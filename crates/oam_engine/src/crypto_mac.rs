//! node 26 crypto natives: `crypto.createMac` (every family in node's
//! `getMacs()`: HMAC, CMAC, GMAC, KMAC128/256, keyed BLAKE2b/BLAKE2s,
//! SipHash, Poly1305) and `crypto.argon2` / `argon2Sync` (RFC 9106).
//!
//! The JS side (js/node_compat.js, `createMac` / `argon2Sync`) does node's
//! argument validation -- types, required and unsupported options -- and
//! maps the error KINDS this module reports back onto node's OpenSSL-shaped
//! errors. What is checked here is what OpenSSL checks when node hands it
//! the parameters: key, customization, salt and output lengths, and which
//! cipher a CMAC or GMAC names. Limits are OpenSSL 3.5's, measured through
//! node v26.13.1 (KMAC key 4..=512 bytes, customization <= 512 bytes,
//! output <= 2097151 bytes; BLAKE2b key 1..=64, salt and personal <= 16,
//! output 1..=64; BLAKE2s halves those; SipHash key 16, output 8 or 16;
//! Poly1305 key 32).
//!
//! A MAC instance lives in the crypto isolate slot's `macs` map (see
//! `crypto_ops::CryptoState`), keyed by a handle the JS `Mac` holds; final
//! consumes it, and `cryptoHandleFree` releases one dropped unfinished.

use cipher::KeyInit;
use sha3::digest::{ExtendableOutput, Update, XofReader};
use std::hash::Hasher as _;

use crate::crypto_ops::{CryptoState, Hasher};

/// KMAC's cSHAKE state (KMAC128 or KMAC256).
pub(crate) enum Kmac {
    K128(sha3::CShake128),
    K256(sha3::CShake256),
}

pub(crate) enum MacInstance {
    Hmac(Box<Hasher>),
    Cmac128(Box<cmac::Cmac<aes::Aes128>>),
    Cmac192(Box<cmac::Cmac<aes::Aes192>>),
    Cmac256(Box<cmac::Cmac<aes::Aes256>>),
    CmacTdes(Box<cmac::Cmac<des::TdesEde3>>),
    Kmac(Box<Kmac>, usize),
    Blake2b(Box<blake2b_simd::State>),
    Blake2s(Box<blake2s_simd::State>),
    Sip64(siphasher::sip::SipHasher24),
    Sip128(siphasher::sip128::SipHasher24),
    /// Poly1305 and GMAC are one-shot over the whole message: the data is
    /// buffered and the tag computed at final.
    Poly1305([u8; 32], Vec<u8>),
    Gmac {
        key: Vec<u8>,
        iv: Vec<u8>,
        data: Vec<u8>,
    },
}

/// Why a MAC could not be set up, in OpenSSL's terms; the JS side builds
/// node's error from the kind.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MacError {
    /// Unknown MAC name (ERR_CRYPTO_INVALID_MAC).
    InvalidMac,
    /// Unknown digest or cipher (ERR_OSSL_EVP_UNSUPPORTED).
    Unsupported,
    /// A known cipher in the wrong mode for this MAC (ERR_OSSL_INVALID_MODE).
    InvalidMode,
    KeyLength,
    CustomLength,
    OutputLength,
    SaltLength,
    /// BLAKE2's output length out of range (ERR_OSSL_NOT_XOF_OR_INVALID_LENGTH).
    XofLength,
    /// SipHash refusing its key or output size (ERR_CRYPTO_OPERATION_FAILED).
    InitFailed,
}

impl MacError {
    fn kind(&self) -> &'static str {
        match self {
            MacError::InvalidMac => "invalid-mac",
            MacError::Unsupported => "unsupported",
            MacError::InvalidMode => "invalid-mode",
            MacError::KeyLength => "key-length",
            MacError::CustomLength => "custom-length",
            MacError::OutputLength => "output-length",
            MacError::SaltLength => "salt-length",
            MacError::XofLength => "xof-length",
            MacError::InitFailed => "init-failed",
        }
    }
}

/// The parameters node passes to its native Mac.
#[derive(Default)]
pub(crate) struct MacParams<'a> {
    pub digest: Option<&'a str>,
    pub cipher: Option<&'a str>,
    pub iv: Option<&'a [u8]>,
    pub customization: Option<&'a [u8]>,
    pub salt: Option<&'a [u8]>,
    pub output_length: Option<usize>,
}

/// A block-cipher name split into its AES key size and mode, or the 3DES
/// CBC name OpenSSL's CMAC also takes. `None` for a cipher oam does not
/// know at all.
enum CipherName {
    Aes(usize, String),
    TdesCbc,
}

fn parse_cipher(name: &str) -> Option<CipherName> {
    let lower = name.to_ascii_lowercase();
    if lower == "des-ede3-cbc" {
        return Some(CipherName::TdesCbc);
    }
    let rest = lower.strip_prefix("aes-")?;
    let (bits, mode) = rest.split_once('-')?;
    let key_len = match bits {
        "128" => 16,
        "192" => 24,
        "256" => 32,
        _ => return None,
    };
    const MODES: [&str; 9] = [
        "cbc", "ecb", "ctr", "cfb", "ofb", "gcm", "ccm", "ocb", "xts",
    ];
    if !MODES.contains(&mode) {
        return None;
    }
    Some(CipherName::Aes(key_len, mode.to_string()))
}

/// SP 800-185 left_encode / right_encode: `value` big-endian in the fewest
/// bytes (at least one), with the byte count before / after it.
fn encode_len(value: u64, left: bool) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take(7).take_while(|b| **b == 0).count();
    let body = &bytes[skip..];
    let n = body.len() as u8;
    let mut out = Vec::with_capacity(body.len() + 1);
    if left {
        out.push(n);
        out.extend_from_slice(body);
    } else {
        out.extend_from_slice(body);
        out.push(n);
    }
    out
}

/// bytepad(encode_string(key), rate): the first cSHAKE input of KMAC.
fn kmac_key_block(key: &[u8], rate: usize) -> Vec<u8> {
    let mut out = encode_len(rate as u64, true);
    out.extend(encode_len((key.len() as u64) * 8, true));
    out.extend_from_slice(key);
    while !out.len().is_multiple_of(rate) {
        out.push(0);
    }
    out
}

impl MacInstance {
    pub(crate) fn new(name: &str, key: &[u8], p: &MacParams<'_>) -> Result<Self, MacError> {
        match name.to_ascii_lowercase().as_str() {
            "hmac" => {
                let digest = p.digest.unwrap_or_default();
                let normalized = digest.to_ascii_lowercase().replace('-', "");
                Hasher::new_hmac(&normalized, key)
                    .map(|h| MacInstance::Hmac(Box::new(h)))
                    .ok_or(MacError::Unsupported)
            }
            "cmac" => match parse_cipher(p.cipher.unwrap_or_default()) {
                None => Err(MacError::Unsupported),
                Some(CipherName::TdesCbc) => {
                    <cmac::Cmac<des::TdesEde3> as KeyInit>::new_from_slice(key)
                        .map(|m| MacInstance::CmacTdes(Box::new(m)))
                        .map_err(|_| MacError::KeyLength)
                }
                Some(CipherName::Aes(_, mode)) if mode != "cbc" => Err(MacError::InvalidMode),
                Some(CipherName::Aes(len, _)) if len != key.len() => Err(MacError::KeyLength),
                Some(CipherName::Aes(16, _)) => Ok(MacInstance::Cmac128(Box::new(
                    <cmac::Cmac<aes::Aes128> as KeyInit>::new_from_slice(key)
                        .map_err(|_| MacError::KeyLength)?,
                ))),
                Some(CipherName::Aes(24, _)) => Ok(MacInstance::Cmac192(Box::new(
                    <cmac::Cmac<aes::Aes192> as KeyInit>::new_from_slice(key)
                        .map_err(|_| MacError::KeyLength)?,
                ))),
                Some(CipherName::Aes(_, _)) => Ok(MacInstance::Cmac256(Box::new(
                    <cmac::Cmac<aes::Aes256> as KeyInit>::new_from_slice(key)
                        .map_err(|_| MacError::KeyLength)?,
                ))),
            },
            "gmac" => match parse_cipher(p.cipher.unwrap_or_default()) {
                None => Err(MacError::Unsupported),
                Some(CipherName::TdesCbc) => Err(MacError::InvalidMode),
                Some(CipherName::Aes(_, mode)) if mode != "gcm" => Err(MacError::InvalidMode),
                Some(CipherName::Aes(len, _)) if len != key.len() => Err(MacError::KeyLength),
                Some(CipherName::Aes(_, _)) => Ok(MacInstance::Gmac {
                    key: key.to_vec(),
                    iv: p.iv.unwrap_or_default().to_vec(),
                    data: Vec::new(),
                }),
            },
            n @ ("kmac128" | "kmac-128" | "kmac256" | "kmac-256") => {
                let wide = n.contains("256");
                if !(4..=512).contains(&key.len()) {
                    return Err(MacError::KeyLength);
                }
                let custom = p.customization.unwrap_or_default();
                if custom.len() > 512 {
                    return Err(MacError::CustomLength);
                }
                let out_len = p.output_length.unwrap_or(if wide { 64 } else { 32 });
                if out_len > 0xFF_FFFF / 8 {
                    return Err(MacError::OutputLength);
                }
                let kmac = if wide {
                    let mut x = sha3::CShake256::from_core(
                        sha3::CShake256Core::new_with_function_name(b"KMAC", custom),
                    );
                    x.update(&kmac_key_block(key, 136));
                    Kmac::K256(x)
                } else {
                    let mut x = sha3::CShake128::from_core(
                        sha3::CShake128Core::new_with_function_name(b"KMAC", custom),
                    );
                    x.update(&kmac_key_block(key, 168));
                    Kmac::K128(x)
                };
                Ok(MacInstance::Kmac(Box::new(kmac), out_len))
            }
            "blake2bmac" => {
                if key.is_empty() || key.len() > blake2b_simd::KEYBYTES {
                    return Err(MacError::KeyLength);
                }
                let salt = p.salt.unwrap_or_default();
                if salt.len() > blake2b_simd::SALTBYTES {
                    return Err(MacError::SaltLength);
                }
                let personal = p.customization.unwrap_or_default();
                if personal.len() > blake2b_simd::PERSONALBYTES {
                    return Err(MacError::CustomLength);
                }
                let out_len = p.output_length.unwrap_or(blake2b_simd::OUTBYTES);
                if !(1..=blake2b_simd::OUTBYTES).contains(&out_len) {
                    return Err(MacError::XofLength);
                }
                let state = blake2b_simd::Params::new()
                    .hash_length(out_len)
                    .key(key)
                    .salt(salt)
                    .personal(personal)
                    .to_state();
                Ok(MacInstance::Blake2b(Box::new(state)))
            }
            "blake2smac" => {
                if key.is_empty() || key.len() > blake2s_simd::KEYBYTES {
                    return Err(MacError::KeyLength);
                }
                let salt = p.salt.unwrap_or_default();
                if salt.len() > blake2s_simd::SALTBYTES {
                    return Err(MacError::SaltLength);
                }
                let personal = p.customization.unwrap_or_default();
                if personal.len() > blake2s_simd::PERSONALBYTES {
                    return Err(MacError::CustomLength);
                }
                let out_len = p.output_length.unwrap_or(blake2s_simd::OUTBYTES);
                if !(1..=blake2s_simd::OUTBYTES).contains(&out_len) {
                    return Err(MacError::XofLength);
                }
                let state = blake2s_simd::Params::new()
                    .hash_length(out_len)
                    .key(key)
                    .salt(salt)
                    .personal(personal)
                    .to_state();
                Ok(MacInstance::Blake2s(Box::new(state)))
            }
            "siphash" => {
                let key: &[u8; 16] = key.try_into().map_err(|_| MacError::InitFailed)?;
                match p.output_length.unwrap_or(16) {
                    8 => Ok(MacInstance::Sip64(
                        siphasher::sip::SipHasher24::new_with_key(key),
                    )),
                    16 => Ok(MacInstance::Sip128(
                        siphasher::sip128::SipHasher24::new_with_key(key),
                    )),
                    _ => Err(MacError::InitFailed),
                }
            }
            "poly1305" => {
                let key: [u8; 32] = key.try_into().map_err(|_| MacError::KeyLength)?;
                Ok(MacInstance::Poly1305(key, Vec::new()))
            }
            _ => Err(MacError::InvalidMac),
        }
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        match self {
            MacInstance::Hmac(h) => h.update(data),
            MacInstance::Cmac128(m) => cmac::Mac::update(m.as_mut(), data),
            MacInstance::Cmac192(m) => cmac::Mac::update(m.as_mut(), data),
            MacInstance::Cmac256(m) => cmac::Mac::update(m.as_mut(), data),
            MacInstance::CmacTdes(m) => cmac::Mac::update(m.as_mut(), data),
            MacInstance::Kmac(k, _) => match k.as_mut() {
                Kmac::K128(x) => x.update(data),
                Kmac::K256(x) => x.update(data),
            },
            MacInstance::Blake2b(s) => {
                s.update(data);
            }
            MacInstance::Blake2s(s) => {
                s.update(data);
            }
            MacInstance::Sip64(s) => s.write(data),
            MacInstance::Sip128(s) => s.write(data),
            MacInstance::Poly1305(_, buf) => buf.extend_from_slice(data),
            MacInstance::Gmac { data: buf, .. } => buf.extend_from_slice(data),
        }
    }

    pub(crate) fn finalize(self) -> Vec<u8> {
        match self {
            MacInstance::Hmac(h) => (*h).finalize(),
            MacInstance::Cmac128(m) => cmac::Mac::finalize(*m).into_bytes().to_vec(),
            MacInstance::Cmac192(m) => cmac::Mac::finalize(*m).into_bytes().to_vec(),
            MacInstance::Cmac256(m) => cmac::Mac::finalize(*m).into_bytes().to_vec(),
            MacInstance::CmacTdes(m) => cmac::Mac::finalize(*m).into_bytes().to_vec(),
            MacInstance::Kmac(k, out_len) => {
                let mut out = vec![0u8; out_len];
                let tail = encode_len((out_len as u64) * 8, false);
                match *k {
                    Kmac::K128(mut x) => {
                        x.update(&tail);
                        x.finalize_xof().read(&mut out);
                    }
                    Kmac::K256(mut x) => {
                        x.update(&tail);
                        x.finalize_xof().read(&mut out);
                    }
                }
                out
            }
            MacInstance::Blake2b(s) => s.finalize().as_bytes().to_vec(),
            MacInstance::Blake2s(s) => s.finalize().as_bytes().to_vec(),
            MacInstance::Sip64(s) => s.finish().to_le_bytes().to_vec(),
            MacInstance::Sip128(s) => {
                use siphasher::sip128::Hasher128 as _;
                s.finish128().as_bytes().to_vec()
            }
            MacInstance::Poly1305(key, data) => {
                use poly1305::universal_hash::KeyInit as _;
                poly1305::Poly1305::new(&key.into())
                    .compute_unpadded(&data)
                    .to_vec()
            }
            MacInstance::Gmac { key, iv, data } => gmac(&key, &iv, &data),
        }
    }
}

/// GMAC (SP 800-38D with an empty plaintext): the GCM tag of `aad` alone.
/// Any IV length: 96 bits is the counter block directly, anything else is
/// GHASHed into one, as GCM does.
fn gmac(key: &[u8], iv: &[u8], aad: &[u8]) -> Vec<u8> {
    use aes::cipher::BlockEncrypt;
    use ghash::universal_hash::UniversalHash;

    fn encrypt_block(key: &[u8], block: &mut [u8; 16]) {
        let b = aes::Block::from_mut_slice(block);
        match key.len() {
            16 => aes::Aes128::new_from_slice(key)
                .expect("checked")
                .encrypt_block(b),
            24 => aes::Aes192::new_from_slice(key)
                .expect("checked")
                .encrypt_block(b),
            _ => aes::Aes256::new_from_slice(key)
                .expect("checked")
                .encrypt_block(b),
        }
    }
    let mut h = [0u8; 16];
    encrypt_block(key, &mut h);
    let ghash_key = ghash::Key::from(h);

    let mut j0 = [0u8; 16];
    if iv.len() == 12 {
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;
    } else {
        let mut g = ghash::GHash::new(&ghash_key);
        g.update_padded(iv);
        let mut lens = [0u8; 16];
        lens[8..].copy_from_slice(&((iv.len() as u64) * 8).to_be_bytes());
        g.update(&[lens.into()]);
        j0.copy_from_slice(&g.finalize());
    }

    let mut g = ghash::GHash::new(&ghash_key);
    g.update_padded(aad);
    let mut lens = [0u8; 16];
    lens[..8].copy_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
    g.update(&[lens.into()]);
    let s = g.finalize();

    encrypt_block(key, &mut j0);
    j0.iter().zip(s.iter()).map(|(a, b)| a ^ b).collect()
}

fn optional_str(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    index: i32,
) -> Option<String> {
    crate::node_ops::arg_string(scope, args, index)
}

/// cryptoMacCreate(name, key, digest?, cipher?, iv?, customization?, salt?,
/// outputLength?) -> handle (number), or an error kind (string) the JS side
/// turns into node's error.
pub(crate) fn op_crypto_mac_create(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(name) = optional_str(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "cryptoMacCreate: name required");
        return;
    };
    let key = crate::node_ops::arg_bytes(scope, &args, 1).unwrap_or_default();
    let digest = optional_str(scope, &args, 2);
    let cipher = optional_str(scope, &args, 3);
    let iv = crate::node_ops::arg_bytes(scope, &args, 4);
    let customization = crate::node_ops::arg_bytes(scope, &args, 5);
    let salt = crate::node_ops::arg_bytes(scope, &args, 6);
    let output_length = {
        let v = args.get(7);
        if v.is_undefined() {
            None
        } else {
            v.number_value(scope).map(|n| n as usize)
        }
    };
    let params = MacParams {
        digest: digest.as_deref(),
        cipher: cipher.as_deref(),
        iv: iv.as_deref(),
        customization: customization.as_deref(),
        salt: salt.as_deref(),
        output_length,
    };
    match MacInstance::new(&name, &key, &params) {
        Ok(mac) => {
            let id = scope
                .get_slot_mut::<CryptoState>()
                .expect("crypto state installed")
                .insert_mac(mac);
            rv.set_double(id as f64);
        }
        Err(e) => {
            let kind = v8::String::new(scope, e.kind()).unwrap();
            rv.set(kind.into());
        }
    }
}

pub(crate) fn op_crypto_mac_update(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(bytes) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "mac update requires data");
        return;
    };
    let state = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed");
    match state.macs.get_mut(&id) {
        Some(mac) => mac.update(&bytes),
        None => crate::node_ops::throw_type_error(scope, "MAC already finalized"),
    }
}

pub(crate) fn op_crypto_mac_final(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let mac = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed")
        .macs
        .remove(&id);
    match mac {
        Some(mac) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, mac.finalize()) {
                rv.set(value);
            }
        }
        None => crate::node_ops::throw_type_error(scope, "MAC already finalized"),
    }
}

/// OpenSSL's long name (`OBJ_nid2ln`) for a certificate signature algorithm
/// OID, as node 26's X509Certificate#signatureAlgorithm reports it (measured
/// on v26.13.1 for RSA, RSA-PSS, ECDSA and EdDSA certificates; every row
/// checked against OpenSSL 3.5.9's objects table with `openssl asn1parse
/// -genstr OID:<oid>`). `None` for an OID outside this table, which oam
/// then reports as undefined, as node does for an OID OpenSSL has no
/// object for.
pub(crate) fn signature_algorithm_long_name(oid: &str) -> Option<&'static str> {
    Some(match oid {
        "1.2.840.113549.1.1.2" => "md2WithRSAEncryption",
        "1.2.840.113549.1.1.3" => "md4WithRSAEncryption",
        "1.2.840.113549.1.1.4" => "md5WithRSAEncryption",
        "1.2.840.113549.1.1.5" => "sha1WithRSAEncryption",
        "1.2.840.113549.1.1.10" => "rsassaPss",
        "1.2.840.113549.1.1.11" => "sha256WithRSAEncryption",
        "1.2.840.113549.1.1.12" => "sha384WithRSAEncryption",
        "1.2.840.113549.1.1.13" => "sha512WithRSAEncryption",
        "1.2.840.113549.1.1.14" => "sha224WithRSAEncryption",
        "1.2.840.113549.1.1.15" => "sha512-224WithRSAEncryption",
        "1.2.840.113549.1.1.16" => "sha512-256WithRSAEncryption",
        "1.2.840.10045.4.1" => "ecdsa-with-SHA1",
        "1.2.840.10045.4.3.1" => "ecdsa-with-SHA224",
        "1.2.840.10045.4.3.2" => "ecdsa-with-SHA256",
        "1.2.840.10045.4.3.3" => "ecdsa-with-SHA384",
        "1.2.840.10045.4.3.4" => "ecdsa-with-SHA512",
        "1.2.840.10040.4.3" => "dsaWithSHA1",
        "2.16.840.1.101.3.4.3.1" => "dsa_with_SHA224",
        "2.16.840.1.101.3.4.3.2" => "dsa_with_SHA256",
        "2.16.840.1.101.3.4.3.9" => "ecdsa_with_SHA3-224",
        "2.16.840.1.101.3.4.3.10" => "ecdsa_with_SHA3-256",
        "2.16.840.1.101.3.4.3.11" => "ecdsa_with_SHA3-384",
        "2.16.840.1.101.3.4.3.12" => "ecdsa_with_SHA3-512",
        "2.16.840.1.101.3.4.3.13" => "RSA-SHA3-224",
        "2.16.840.1.101.3.4.3.14" => "RSA-SHA3-256",
        "2.16.840.1.101.3.4.3.15" => "RSA-SHA3-384",
        "2.16.840.1.101.3.4.3.16" => "RSA-SHA3-512",
        "2.16.840.1.101.3.4.3.17" => "ML-DSA-44",
        "2.16.840.1.101.3.4.3.18" => "ML-DSA-65",
        "2.16.840.1.101.3.4.3.19" => "ML-DSA-87",
        "1.3.101.112" => "ED25519",
        "1.3.101.113" => "ED448",
        "1.2.156.10197.1.501" => "SM2-with-SM3",
        _ => return None,
    })
}

/// Argon2 (RFC 9106, version 0x13) over node's parameters. `type_id` is
/// 0 = argon2d, 1 = argon2i, 2 = argon2id. The associated data is limited to
/// 32 bytes by the argon2 crate (node takes up to 2^32 - 1); a longer one is
/// refused rather than truncated (docs/node-divergences.md).
#[allow(clippy::too_many_arguments)]
pub(crate) fn argon2_derive(
    type_id: u32,
    message: &[u8],
    nonce: &[u8],
    parallelism: u32,
    tag_length: usize,
    memory: u32,
    passes: u32,
    secret: &[u8],
    associated_data: &[u8],
) -> Result<Vec<u8>, String> {
    let algorithm = match type_id {
        0 => argon2::Algorithm::Argon2d,
        1 => argon2::Algorithm::Argon2i,
        _ => argon2::Algorithm::Argon2id,
    };
    let data = argon2::AssociatedData::new(associated_data).map_err(|_| {
        format!(
            "argon2: associatedData longer than {} bytes is not supported by oam",
            argon2::AssociatedData::MAX_LEN
        )
    })?;
    let mut builder = argon2::ParamsBuilder::new();
    builder
        .m_cost(memory)
        .t_cost(passes)
        .p_cost(parallelism)
        .output_len(tag_length)
        .data(data);
    let params = builder.build().map_err(|e| format!("argon2: {e}"))?;
    let ctx = if secret.is_empty() {
        argon2::Argon2::new(algorithm, argon2::Version::V0x13, params)
    } else {
        argon2::Argon2::new_with_secret(secret, algorithm, argon2::Version::V0x13, params)
            .map_err(|e| format!("argon2: {e}"))?
    };
    let mut out = vec![0u8; tag_length];
    ctx.hash_password_into(message, nonce, &mut out)
        .map_err(|e| format!("argon2: {e}"))?;
    Ok(out)
}

/// cryptoArgon2Sync(type, message, nonce, parallelism, tagLength, memory,
/// passes, secret, associatedData) -> Uint8Array. Arguments arrive validated
/// to node's ranges (js/node_compat.js `argon2Check`).
pub(crate) fn op_crypto_argon2_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let num = |scope: &mut v8::PinScope<'_, '_>, i: i32| -> f64 {
        args.get(i).number_value(scope).unwrap_or(0.0)
    };
    let type_id = num(scope, 0) as u32;
    let message = crate::node_ops::arg_bytes(scope, &args, 1).unwrap_or_default();
    let nonce = crate::node_ops::arg_bytes(scope, &args, 2).unwrap_or_default();
    let parallelism = num(scope, 3) as u32;
    let tag_length = num(scope, 4) as usize;
    let memory = num(scope, 5) as u32;
    let passes = num(scope, 6) as u32;
    let secret = crate::node_ops::arg_bytes(scope, &args, 7).unwrap_or_default();
    let ad = crate::node_ops::arg_bytes(scope, &args, 8).unwrap_or_default();
    match argon2_derive(
        type_id,
        &message,
        &nonce,
        parallelism,
        tag_length,
        memory,
        passes,
        &secret,
        &ad,
    ) {
        Ok(out) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, out) {
                rv.set(value);
            }
        }
        Err(msg) => {
            let message = v8::String::new(scope, &msg).unwrap();
            let exception = v8::Exception::error(scope, message);
            scope.throw_exception(exception);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac_hex(name: &str, key: &[u8], p: &MacParams<'_>, data: &[u8]) -> String {
        let mut m = MacInstance::new(name, key, p).expect("mac");
        m.update(data);
        m.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    const K16: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    // Expected values: node v26.13.1 (OpenSSL 3.5.9), createMac(...)
    // .update('abc').final('hex') with key 000102...0f unless noted.
    #[test]
    fn macs_match_node_26() {
        let none = MacParams::default();
        let cbc = MacParams {
            cipher: Some("aes-128-cbc"),
            ..Default::default()
        };
        assert_eq!(
            mac_hex("cmac", &K16, &cbc, b"abc"),
            "0a54a6a425d48438c3f8bbe09bf944cc"
        );
        assert_eq!(
            mac_hex("kmac128", &K16, &none, b"abc"),
            "4944f7aa41c34bae0ff7c622b51a6af94f298a92fbd5d50670b401b06f067872"
        );
        let cust = MacParams {
            customization: Some(b"cust"),
            output_length: Some(20),
            ..Default::default()
        };
        assert_eq!(
            mac_hex("kmac128", &K16, &cust, b"abc"),
            "6a0246e2c07e8bf33c4141ea3bc6ebbd6af64c65"
        );
        assert_eq!(
            mac_hex("blake2bmac", &K16, &none, b"abc"),
            "4c76bc7ad0fc52e4bde231b38727c331172cfe3eeaf10cd2fa5c65abbdb8faeaad2da338531b382ac103f10ccaf41f74d8870d016c48b269c0d9a5cf5752c8c3"
        );
        assert_eq!(
            mac_hex("blake2smac", &K16, &none, b"abc"),
            "033a40b64c6296fc2e3584d2a06bcf1a8003ebb9b8007940e2f820b645917a4e"
        );
        assert_eq!(
            mac_hex("siphash", &K16, &none, b"abc"),
            "bb96c3a813c9903e3325ef616045abb8"
        );
        let eight = MacParams {
            output_length: Some(8),
            ..Default::default()
        };
        assert_eq!(mac_hex("siphash", &K16, &eight, b"abc"), "a50720aa53fabc5d");
        assert_eq!(
            mac_hex("poly1305", &[7u8; 32], &none, b"abc"),
            "7a902220fdf8f51bfdf8f51bfdf8f51b"
        );
        let gcm = MacParams {
            cipher: Some("aes-128-gcm"),
            iv: Some(&[0u8; 12]),
            ..Default::default()
        };
        assert_eq!(
            mac_hex("gmac", &K16, &gcm, b"abc"),
            "71c97e7265176abb1ea435384743293d"
        );
        let gcm16 = MacParams {
            cipher: Some("aes-128-gcm"),
            iv: Some(&[0u8; 16]),
            ..Default::default()
        };
        assert_eq!(
            mac_hex("gmac", &K16, &gcm16, b"abc"),
            "092ece3bd108876a381e09d6327a9812"
        );
        let des3 = MacParams {
            cipher: Some("des-ede3-cbc"),
            ..Default::default()
        };
        assert_eq!(
            mac_hex("cmac", &[3u8; 24], &des3, b"abc"),
            "691dd3b84cd393f6"
        );
    }

    #[test]
    fn mac_parameter_errors_are_openssls() {
        let none = MacParams::default();
        let err = |name: &str, key: &[u8], p: &MacParams<'_>| MacInstance::new(name, key, p).err();
        assert_eq!(err("nope", &K16, &none), Some(MacError::InvalidMac));
        assert_eq!(err("kmac128", &[1u8; 3], &none), Some(MacError::KeyLength));
        assert_eq!(
            err("kmac128", &[1u8; 513], &none),
            Some(MacError::KeyLength)
        );
        let ecb = MacParams {
            cipher: Some("aes-128-ecb"),
            ..Default::default()
        };
        assert_eq!(err("cmac", &K16, &ecb), Some(MacError::InvalidMode));
        let nosuch = MacParams {
            cipher: Some("nosuch"),
            ..Default::default()
        };
        assert_eq!(err("cmac", &K16, &nosuch), Some(MacError::Unsupported));
        let out0 = MacParams {
            output_length: Some(0),
            ..Default::default()
        };
        assert_eq!(err("blake2bmac", &K16, &out0), Some(MacError::XofLength));
        assert_eq!(err("siphash", &[0u8; 8], &none), Some(MacError::InitFailed));
        assert_eq!(err("poly1305", &K16, &none), Some(MacError::KeyLength));
    }

    #[test]
    fn argon2id_matches_node_26() {
        // node v26.13.1: argon2Sync('argon2id', { message: 'pw', nonce:
        // Buffer.alloc(16, 1), parallelism: 1, tagLength: 32, memory: 64,
        // passes: 3 }).toString('hex').
        let out = argon2_derive(2, b"pw", &[1u8; 16], 1, 32, 64, 3, &[], &[]).unwrap();
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "390819762576b536ee4c141e3689b102f5f4e85022fdf3a99a02e428e55848a8"
        );
    }
}
