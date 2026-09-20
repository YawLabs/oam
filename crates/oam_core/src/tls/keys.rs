//! Reading a TLS server's private key and PKCS#12 bundles the way Node's
//! `createSecureContext` does (measured on v22.22.2 with OpenSSL-made
//! fixtures):
//!
//! - a `key` is PEM: PKCS#8, PKCS#1 (RSA) or SEC1 (EC), or an encrypted
//!   form of one -- PKCS#8 `ENCRYPTED PRIVATE KEY` (PBES2: PBKDF2 or scrypt
//!   with AES-CBC or DES-EDE3-CBC; or PKCS#12's pbeWithSHAAnd3-KeyTripleDES-CBC
//!   / pbeWithSHAAnd2-KeyTripleDES-CBC, what `openssl pkcs8 -topk8 -v1
//!   PBE-SHA1-3DES` and OpenSSL 1.x's default write) or a legacy
//!   `Proc-Type: 4,ENCRYPTED` key (AES-CBC, DES-EDE3-CBC as `openssl rsa
//!   -des3` writes it, DES-EDE-CBC; OpenSSL's EVP_BytesToKey) -- opened with
//!   its `passphrase`. A wrong or missing passphrase is
//!   `ERR_OSSL_BAD_DECRYPT`; a key that cannot be read at all is
//!   `ERR_OSSL_UNSUPPORTED`.
//! - a `pfx` is a PKCS#12 bundle: its MAC is checked with the passphrase
//!   ("mac verify failure" when it does not verify), then its bags are
//!   opened -- PBES2-protected (OpenSSL 3's default), protected with
//!   PKCS#12's own triple-DES PBEs (OpenSSL 1.x's default for keys), or not
//!   protected at all ("bad decrypt" when a bundle without a MAC does not
//!   open).
//!
//! What Node 22 refuses, oam refuses the same way: the ciphers OpenSSL 3
//! keeps in its legacy provider -- single DES (legacy PEM `DES-CBC`, PBES2
//! des-cbc, the PKCS#5 v1.5 PBES1 schemes), RC2, RC4, Blowfish, CAST5, IDEA,
//! SEED -- are `ERR_OSSL_EVP_UNSUPPORTED` for a `key` and "Unsupported
//! PKCS12 PFX data" in a `pfx`, whatever the passphrase.
//! The ASN.1 read here is BER, which covers DER and the indefinite lengths
//! and constructed strings some PKCS#12 writers emit.

use super::server::ContextError;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
// Triple-DES comes from the RustCrypto `des` crate, on the same `cipher`
// traits as `aes`.
use hmac::Mac;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
};
use sha2::Digest;

/// What a PKCS#12 bundle held: its key (if any) and its certificates, in
/// the bundle's order.
pub struct Pkcs12Bundle {
    pub key: Option<PrivateKeyDer<'static>>,
    pub certs: Vec<CertificateDer<'static>>,
}

// ------------------------------------------------------------------ errors

fn unsupported_pfx() -> ContextError {
    ContextError::plain(
        "Unsupported PKCS12 PFX data",
        Some("ERR_CRYPTO_UNSUPPORTED_OPERATION"),
    )
}

fn mac_verify_failure() -> ContextError {
    ContextError::plain("mac verify failure", None)
}

/// A bundle without a MAC whose bags do not open with the passphrase:
/// OpenSSL's cipher error, the reason alone (measured).
fn pfx_bad_decrypt() -> ContextError {
    ContextError::plain("bad decrypt", None)
}

/// OpenSSL's ASN.1 decoder's words for a bundle it cannot parse.
fn malformed_pfx() -> ContextError {
    ContextError::plain("not enough data", None)
}

// --------------------------------------------------------------- BER reader

/// One BER element: its identifier octet and its contents (for a
/// constructed element read with an indefinite length, the contents up to
/// its end-of-contents).
#[derive(Clone, Copy, Debug)]
struct Tlv<'a> {
    tag: u8,
    content: &'a [u8],
}

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const OCTET_STRING: u8 = 0x04;
const OID: u8 = 0x06;
const CONTEXT_0: u8 = 0xa0;
const CONTEXT_0_PRIMITIVE: u8 = 0x80;

/// Read one element off the front of `input`: it and what follows it.
fn read_tlv(input: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    let (&tag, rest) = input.split_first()?;
    // High tag numbers (0x1f) do not occur in these structures.
    if tag & 0x1f == 0x1f {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    if first == 0x80 {
        // Indefinite length: constructed only, ends at 00 00.
        if tag & 0x20 == 0 {
            return None;
        }
        let mut cursor = rest;
        loop {
            if cursor.len() >= 2 && cursor[0] == 0 && cursor[1] == 0 {
                let used = rest.len() - cursor.len();
                return Some((
                    Tlv {
                        tag,
                        content: &rest[..used],
                    },
                    &cursor[2..],
                ));
            }
            let (_, next) = read_tlv(cursor)?;
            cursor = next;
        }
    }
    let (len, rest) = if first & 0x80 == 0 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let len = rest[..count]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (len, &rest[count..])
    };
    if rest.len() < len {
        return None;
    }
    Some((
        Tlv {
            tag,
            content: &rest[..len],
        },
        &rest[len..],
    ))
}

/// The one element `input` holds (trailing bytes are ignored, as OpenSSL
/// ignores them after a PKCS#12 structure).
fn parse(input: &[u8]) -> Option<Tlv<'_>> {
    read_tlv(input).map(|(tlv, _)| tlv)
}

/// The elements inside a constructed element.
fn children<'a>(tlv: &Tlv<'a>) -> Option<Vec<Tlv<'a>>> {
    if tlv.tag & 0x20 == 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut cursor = tlv.content;
    while !cursor.is_empty() {
        let (child, rest) = read_tlv(cursor)?;
        out.push(child);
        cursor = rest;
    }
    Some(out)
}

fn expect<'a>(tlv: Option<&Tlv<'a>>, tag: u8) -> Option<Tlv<'a>> {
    tlv.filter(|t| t.tag == tag).copied()
}

/// An OCTET STRING's bytes (implicitly tagged ones included), a BER
/// constructed string's segments joined.
fn octets(tlv: &Tlv<'_>) -> Option<Vec<u8>> {
    if tlv.tag & 0x20 == 0 {
        return Some(tlv.content.to_vec());
    }
    let mut out = Vec::new();
    for part in children(tlv)? {
        out.extend(octets(&part)?);
    }
    Some(out)
}

fn small_uint(tlv: &Tlv<'_>) -> Option<u64> {
    if tlv.tag != INTEGER || tlv.content.is_empty() || tlv.content.len() > 9 {
        return None;
    }
    if tlv.content[0] & 0x80 != 0 {
        return None;
    }
    Some(
        tlv.content
            .iter()
            .fold(0u64, |acc, &b| acc.wrapping_shl(8) | u64::from(b)),
    )
}

// -------------------------------------------------------------------- OIDs

const OID_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x01];
const OID_ENCRYPTED_DATA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x06];
const OID_KEY_BAG: &[u8] = &[
    0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x0c, 0x0a, 0x01, 0x01,
];
const OID_SHROUDED_KEY_BAG: &[u8] = &[
    0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x0c, 0x0a, 0x01, 0x02,
];
const OID_CERT_BAG: &[u8] = &[
    0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x0c, 0x0a, 0x01, 0x03,
];
const OID_X509_CERTIFICATE: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x16, 0x01];
const OID_PBES2: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0d];
const OID_PBKDF2: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0c];
const OID_SCRYPT: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0xda, 0x47, 0x04, 0x0b];
const OID_HMAC_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x07];
const OID_HMAC_SHA224: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x08];
const OID_HMAC_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x09];
const OID_HMAC_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0a];
const OID_HMAC_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0b];
const OID_AES128_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x02];
const OID_AES192_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x16];
const OID_AES256_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x2a];
const OID_DES_EDE3_CBC: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x03, 0x07];
/// desCBC (1.3.14.3.2.7) and rc2CBC (1.2.840.113549.3.2): PBES2 schemes
/// whose ciphers OpenSSL 3 keeps in its legacy provider.
const OID_DES_CBC: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x07];
const OID_RC2_CBC: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x03, 0x02];
/// pkcs-12PbeIds: 1.2.840.113549.1.12.1.{1..6} (RC4-128, RC4-40, 3DES,
/// 2-key 3DES, RC2-128, RC2-40).
const OID_PKCS12_PBE_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x0c, 0x01];
/// PKCS#5 v1 PBES1 ids: 1.2.840.113549.1.5.{1,3,4,6,10,11}.
const OID_PBES1_PREFIX: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05];
const OID_SHA1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];
const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
const OID_SHA512: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03];
const OID_SHA224: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x04];

// ------------------------------------------------------------ key derivation

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hash {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

fn pbkdf2(hash: Hash, password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) {
    match hash {
        Hash::Sha1 => pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, salt, rounds, out),
        Hash::Sha224 => pbkdf2::pbkdf2_hmac::<sha2::Sha224>(password, salt, rounds, out),
        Hash::Sha256 => pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, rounds, out),
        Hash::Sha384 => pbkdf2::pbkdf2_hmac::<sha2::Sha384>(password, salt, rounds, out),
        Hash::Sha512 => pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password, salt, rounds, out),
    }
}

/// RFC 7292 appendix B.2: PKCS#12's own key derivation, over the password as
/// a BMPString. `id` 1 is a key, 2 an IV, 3 a MAC key.
fn pkcs12_kdf<D: Digest + sha2::digest::core_api::BlockSizeUser>(
    password: &[u8],
    salt: &[u8],
    id: u8,
    iterations: u32,
    len: usize,
) -> Vec<u8> {
    let u = <D as Digest>::output_size();
    let v = <D as sha2::digest::core_api::BlockSizeUser>::block_size();
    let fill = |src: &[u8]| -> Vec<u8> {
        if src.is_empty() {
            return Vec::new();
        }
        let n = v * src.len().div_ceil(v);
        src.iter().copied().cycle().take(n).collect()
    };
    let mut i_block = fill(salt);
    i_block.extend(fill(password));
    let d_block = vec![id; v];
    let mut out = Vec::with_capacity(len + u);
    while out.len() < len {
        let mut a = D::new()
            .chain_update(&d_block)
            .chain_update(&i_block)
            .finalize();
        for _ in 1..iterations.max(1) {
            a = D::digest(&a);
        }
        out.extend_from_slice(&a);
        if out.len() >= len {
            break;
        }
        let b: Vec<u8> = a.iter().copied().cycle().take(v).collect();
        for chunk in i_block.chunks_mut(v) {
            // chunk = (chunk + b + 1) mod 2^(8v)
            let mut carry = 1u16;
            for (x, y) in chunk.iter_mut().rev().zip(b.iter().rev()) {
                let sum = u16::from(*x) + u16::from(*y) + carry;
                *x = sum as u8;
                carry = sum >> 8;
            }
        }
    }
    out.truncate(len);
    out
}

fn pkcs12_kdf_for(
    hash: Hash,
    password: &[u8],
    salt: &[u8],
    id: u8,
    iterations: u32,
    len: usize,
) -> Vec<u8> {
    match hash {
        Hash::Sha1 => pkcs12_kdf::<sha1::Sha1>(password, salt, id, iterations, len),
        Hash::Sha224 => pkcs12_kdf::<sha2::Sha224>(password, salt, id, iterations, len),
        Hash::Sha256 => pkcs12_kdf::<sha2::Sha256>(password, salt, id, iterations, len),
        Hash::Sha384 => pkcs12_kdf::<sha2::Sha384>(password, salt, id, iterations, len),
        Hash::Sha512 => pkcs12_kdf::<sha2::Sha512>(password, salt, id, iterations, len),
    }
}

fn hmac_verify(hash: Hash, key: &[u8], data: &[u8], tag: &[u8]) -> bool {
    macro_rules! verify {
        ($d:ty) => {{
            let Ok(mut mac) = hmac::Hmac::<$d>::new_from_slice(key) else {
                return false;
            };
            mac.update(data);
            mac.verify_slice(tag).is_ok()
        }};
    }
    match hash {
        Hash::Sha1 => verify!(sha1::Sha1),
        Hash::Sha224 => verify!(sha2::Sha224),
        Hash::Sha256 => verify!(sha2::Sha256),
        Hash::Sha384 => verify!(sha2::Sha384),
        Hash::Sha512 => verify!(sha2::Sha512),
    }
}

fn digest_output(hash: Hash) -> usize {
    match hash {
        Hash::Sha1 => 20,
        Hash::Sha224 => 28,
        Hash::Sha256 => 32,
        Hash::Sha384 => 48,
        Hash::Sha512 => 64,
    }
}

// ------------------------------------------------------------- decryption

/// The CBC block ciphers keys and bundles are protected with, among those
/// OpenSSL 3's default provider has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockCipher {
    Aes128,
    Aes192,
    Aes256,
    /// Three-key triple DES (DES-EDE3-CBC).
    DesEde3,
    /// Two-key triple DES (DES-EDE-CBC: the third key is the first).
    DesEde,
}

impl BlockCipher {
    fn key_len(self) -> usize {
        match self {
            BlockCipher::Aes128 | BlockCipher::DesEde => 16,
            BlockCipher::Aes192 | BlockCipher::DesEde3 => 24,
            BlockCipher::Aes256 => 32,
        }
    }

    fn iv_len(self) -> usize {
        match self {
            BlockCipher::Aes128 | BlockCipher::Aes192 | BlockCipher::Aes256 => 16,
            BlockCipher::DesEde3 | BlockCipher::DesEde => 8,
        }
    }
}

/// CBC with PKCS#7 padding. None when the key or IV has the wrong length,
/// or the padding does not check out -- the wrong key, which is how OpenSSL
/// learns the passphrase was wrong.
fn cbc_decrypt(cipher: BlockCipher, key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    macro_rules! decrypt {
        ($c:ty) => {
            cbc::Decryptor::<$c>::new_from_slices(key, iv)
                .ok()?
                .decrypt_padded_vec_mut::<Pkcs7>(data)
                .ok()
        };
    }
    if key.len() != cipher.key_len() || iv.len() != cipher.iv_len() {
        return None;
    }
    match cipher {
        BlockCipher::Aes128 => decrypt!(aes::Aes128),
        BlockCipher::Aes192 => decrypt!(aes::Aes192),
        BlockCipher::Aes256 => decrypt!(aes::Aes256),
        BlockCipher::DesEde3 => decrypt!(des::TdesEde3),
        BlockCipher::DesEde => decrypt!(des::TdesEde2),
    }
}

/// Why something encrypted could not be opened.
enum Refusal {
    /// The password does not open it.
    BadDecrypt,
    /// A cipher OpenSSL 3 keeps in its legacy provider (DES, RC2, RC4, ...),
    /// which Node 22 does not load: refused whatever the password.
    Legacy,
    /// An algorithm that is not known here.
    Unsupported,
    /// Not a structure that can be read.
    Malformed,
}

/// A password as each scheme reads it: PBES2 (PBKDF2 or scrypt) and
/// OpenSSL's legacy PEM KDF take its bytes; PKCS#12's own KDF takes it as a
/// BMPString -- with its two-byte terminator, or, for a bundle opened with
/// no password at all, as nothing (OpenSSL's NULL password).
struct Password {
    bytes: Vec<u8>,
    bmp: Vec<u8>,
}

impl Password {
    fn new(password: &str) -> Self {
        Password {
            bytes: password.as_bytes().to_vec(),
            bmp: bmp_password(password),
        }
    }
}

/// Decrypt `data` under an AlgorithmIdentifier: PBES2 (PBKDF2 or scrypt
/// with AES-CBC or DES-EDE3-CBC), or PKCS#12's own triple-DES PBEs.
fn decrypt(algorithm: &Tlv<'_>, data: &[u8], password: &Password) -> Result<Vec<u8>, Refusal> {
    let parts = children(algorithm).ok_or(Refusal::Malformed)?;
    let oid = expect(parts.first(), OID).ok_or(Refusal::Malformed)?;
    if oid.content == OID_PBES2 {
        let params = expect(parts.get(1), SEQUENCE).ok_or(Refusal::Malformed)?;
        return pbes2_decrypt(&params, data, &password.bytes);
    }
    if oid.content.len() == OID_PKCS12_PBE_PREFIX.len() + 1
        && oid.content.starts_with(OID_PKCS12_PBE_PREFIX)
    {
        let cipher = match oid.content[OID_PKCS12_PBE_PREFIX.len()] {
            3 => BlockCipher::DesEde3,
            4 => BlockCipher::DesEde,
            // RC4-128, RC4-40, RC2-128, RC2-40.
            1 | 2 | 5 | 6 => return Err(Refusal::Legacy),
            _ => return Err(Refusal::Unsupported),
        };
        let params = expect(parts.get(1), SEQUENCE).ok_or(Refusal::Malformed)?;
        return pkcs12_pbe_decrypt(&params, cipher, data, &password.bmp);
    }
    if oid.content.len() == OID_PBES1_PREFIX.len() + 1 && oid.content.starts_with(OID_PBES1_PREFIX)
    {
        // PKCS#5 v1.5 PBES1: pbeWith{MD2,MD5,SHA1}And{DES,RC2}-CBC, all of it
        // single DES or RC2.
        return Err(match oid.content[OID_PBES1_PREFIX.len()] {
            1 | 3 | 4 | 6 | 10 | 11 => Refusal::Legacy,
            _ => Refusal::Unsupported,
        });
    }
    Err(Refusal::Unsupported)
}

/// PKCS#12's PBE (RFC 7292 appendix C): the key and IV derived from the
/// BMPString password with SHA-1 (ids 1 and 2), the salt and iteration count
/// from `params` (pkcs-12PbeParams).
fn pkcs12_pbe_decrypt(
    params: &Tlv<'_>,
    cipher: BlockCipher,
    data: &[u8],
    bmp_password: &[u8],
) -> Result<Vec<u8>, Refusal> {
    let fields = children(params).ok_or(Refusal::Malformed)?;
    let salt = expect(fields.first(), OCTET_STRING).ok_or(Refusal::Malformed)?;
    let iterations = fields
        .get(1)
        .and_then(small_uint)
        .ok_or(Refusal::Malformed)?;
    let iterations = u32::try_from(iterations).map_err(|_| Refusal::Malformed)?;
    let key = pkcs12_kdf_for(
        Hash::Sha1,
        bmp_password,
        salt.content,
        1,
        iterations,
        cipher.key_len(),
    );
    let iv = pkcs12_kdf_for(
        Hash::Sha1,
        bmp_password,
        salt.content,
        2,
        iterations,
        cipher.iv_len(),
    );
    cbc_decrypt(cipher, &key, &iv, data).ok_or(Refusal::BadDecrypt)
}

fn pbes2_decrypt(params: &Tlv<'_>, data: &[u8], password: &[u8]) -> Result<Vec<u8>, Refusal> {
    let parts = children(params).ok_or(Refusal::Malformed)?;
    let kdf = expect(parts.first(), SEQUENCE).ok_or(Refusal::Malformed)?;
    let scheme = expect(parts.get(1), SEQUENCE).ok_or(Refusal::Malformed)?;
    let scheme_parts = children(&scheme).ok_or(Refusal::Malformed)?;
    let scheme_oid = expect(scheme_parts.first(), OID).ok_or(Refusal::Malformed)?;
    let cipher = match scheme_oid.content {
        OID_AES128_CBC => BlockCipher::Aes128,
        OID_AES192_CBC => BlockCipher::Aes192,
        OID_AES256_CBC => BlockCipher::Aes256,
        OID_DES_EDE3_CBC => BlockCipher::DesEde3,
        OID_DES_CBC | OID_RC2_CBC => return Err(Refusal::Legacy),
        _ => return Err(Refusal::Unsupported),
    };
    let key_len = cipher.key_len();
    let iv = expect(scheme_parts.get(1), OCTET_STRING).ok_or(Refusal::Malformed)?;

    let kdf_parts = children(&kdf).ok_or(Refusal::Malformed)?;
    let kdf_oid = expect(kdf_parts.first(), OID).ok_or(Refusal::Malformed)?;
    let kdf_params = expect(kdf_parts.get(1), SEQUENCE).ok_or(Refusal::Malformed)?;
    let kdf_fields = children(&kdf_params).ok_or(Refusal::Malformed)?;
    let salt = expect(kdf_fields.first(), OCTET_STRING).ok_or(Refusal::Malformed)?;
    let mut key = vec![0u8; key_len];
    match kdf_oid.content {
        OID_PBKDF2 => {
            let rounds = kdf_fields
                .get(1)
                .and_then(small_uint)
                .ok_or(Refusal::Malformed)?;
            let rounds = u32::try_from(rounds).map_err(|_| Refusal::Malformed)?;
            // keyLength (optional INTEGER), then prf (optional, default
            // hmacWithSHA1).
            let mut hash = Hash::Sha1;
            for field in kdf_fields.iter().skip(2) {
                if field.tag == SEQUENCE {
                    let prf = children(field).ok_or(Refusal::Malformed)?;
                    let prf_oid = expect(prf.first(), OID).ok_or(Refusal::Malformed)?;
                    hash = match prf_oid.content {
                        OID_HMAC_SHA1 => Hash::Sha1,
                        OID_HMAC_SHA224 => Hash::Sha224,
                        OID_HMAC_SHA256 => Hash::Sha256,
                        OID_HMAC_SHA384 => Hash::Sha384,
                        OID_HMAC_SHA512 => Hash::Sha512,
                        _ => return Err(Refusal::Unsupported),
                    };
                }
            }
            pbkdf2(hash, password, salt.content, rounds, &mut key);
        }
        OID_SCRYPT => {
            let n = kdf_fields
                .get(1)
                .and_then(small_uint)
                .ok_or(Refusal::Malformed)?;
            let r = kdf_fields
                .get(2)
                .and_then(small_uint)
                .ok_or(Refusal::Malformed)?;
            let p = kdf_fields
                .get(3)
                .and_then(small_uint)
                .ok_or(Refusal::Malformed)?;
            if !n.is_power_of_two() || n < 2 {
                return Err(Refusal::Malformed);
            }
            let log_n = n.trailing_zeros() as u8;
            let params = scrypt::Params::new(
                log_n,
                u32::try_from(r).map_err(|_| Refusal::Malformed)?,
                u32::try_from(p).map_err(|_| Refusal::Malformed)?,
                key_len,
            )
            .map_err(|_| Refusal::Malformed)?;
            scrypt::scrypt(password, salt.content, &params, &mut key)
                .map_err(|_| Refusal::Malformed)?;
        }
        _ => return Err(Refusal::Unsupported),
    }
    cbc_decrypt(cipher, &key, iv.content, data).ok_or(Refusal::BadDecrypt)
}

/// An EncryptedPrivateKeyInfo opened with `password`: the PKCS#8 key inside.
fn decrypt_private_key_info(der: &[u8], password: &Password) -> Result<Vec<u8>, Refusal> {
    let info = parse(der)
        .filter(|t| t.tag == SEQUENCE)
        .ok_or(Refusal::Malformed)?;
    let parts = children(&info).ok_or(Refusal::Malformed)?;
    let algorithm = expect(parts.first(), SEQUENCE).ok_or(Refusal::Malformed)?;
    let encrypted = parts.get(1).ok_or(Refusal::Malformed)?;
    let data = octets(encrypted).ok_or(Refusal::Malformed)?;
    let plain = decrypt(&algorithm, &data, password)?;
    // The right padding by chance under a wrong key: the result must also be
    // a key.
    if !is_private_key_info(&plain) {
        return Err(Refusal::BadDecrypt);
    }
    Ok(plain)
}

/// Whether `der` is a PKCS#8 PrivateKeyInfo (SEQUENCE { INTEGER, SEQUENCE,
/// OCTET STRING, ... }).
fn is_private_key_info(der: &[u8]) -> bool {
    parse(der)
        .filter(|t| t.tag == SEQUENCE)
        .and_then(|t| children(&t))
        .is_some_and(|parts| {
            parts.len() >= 3
                && parts[0].tag == INTEGER
                && parts[1].tag == SEQUENCE
                && parts[2].tag == OCTET_STRING
        })
}

/// Whether `der` is a SEQUENCE starting with a version INTEGER (PKCS#1 and
/// SEC1 keys both are).
fn is_versioned_sequence(der: &[u8]) -> bool {
    parse(der)
        .filter(|t| t.tag == SEQUENCE)
        .and_then(|t| children(&t))
        .is_some_and(|parts| parts.first().is_some_and(|p| p.tag == INTEGER))
}

// ---------------------------------------------------------------------- PEM

/// One PEM block: its label, its RFC 1421 headers, its decoded body.
struct PemBlock {
    label: String,
    headers: Vec<(String, String)>,
    der: Vec<u8>,
}

/// The first PEM block whose label is a private key's.
fn first_key_block(text: &str) -> Option<PemBlock> {
    let mut lines = text.lines().map(str::trim_end);
    while let Some(line) = lines.next() {
        let Some(label) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|l| l.strip_suffix("-----"))
        else {
            continue;
        };
        if !label.ends_with("PRIVATE KEY") {
            continue;
        }
        let end = format!("-----END {label}-----");
        let mut headers = Vec::new();
        let mut body = String::new();
        let mut in_headers = true;
        let mut closed = false;
        for line in lines.by_ref() {
            if line == end {
                closed = true;
                break;
            }
            if in_headers {
                if let Some((name, value)) = line.split_once(':')
                    && !name.contains(' ')
                    && body.is_empty()
                {
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                    continue;
                }
                in_headers = false;
                if line.trim().is_empty() {
                    continue;
                }
            }
            body.push_str(line.trim());
        }
        if !closed {
            return None;
        }
        use base64::Engine;
        let der = base64::engine::general_purpose::STANDARD
            .decode(body.as_bytes())
            .ok()?;
        return Some(PemBlock {
            label: label.to_string(),
            headers,
            der,
        });
    }
    None
}

/// OpenSSL's EVP_BytesToKey with MD5 and one round: the key a legacy
/// encrypted PEM key's `DEK-Info` names, from the passphrase and the IV's
/// first eight bytes.
fn evp_bytes_to_key(password: &[u8], salt: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 16);
    let mut previous: Vec<u8> = Vec::new();
    while out.len() < len {
        let block = md5::Md5::new()
            .chain_update(&previous)
            .chain_update(password)
            .chain_update(salt)
            .finalize();
        previous = block.to_vec();
        out.extend_from_slice(&block);
    }
    out.truncate(len);
    out
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A legacy PEM key's `DEK-Info` cipher: one this reads, or None for one
/// OpenSSL 3 keeps in its legacy provider (Node refuses those whatever the
/// passphrase) -- and Err for a name that is neither.
fn dek_info_cipher(name: &str) -> Result<Option<BlockCipher>, ContextError> {
    Ok(Some(match name {
        "AES-128-CBC" => BlockCipher::Aes128,
        "AES-192-CBC" => BlockCipher::Aes192,
        "AES-256-CBC" => BlockCipher::Aes256,
        "DES-EDE3-CBC" => BlockCipher::DesEde3,
        "DES-EDE-CBC" => BlockCipher::DesEde,
        "DES-CBC" | "DESX-CBC" | "RC2-CBC" | "RC2-40-CBC" | "RC2-64-CBC" | "BF-CBC"
        | "CAST5-CBC" | "CAST-CBC" | "IDEA-CBC" | "SEED-CBC" => return Ok(None),
        _ => return Err(ContextError::unsupported_key()),
    }))
}

fn refusal_error(refusal: Refusal) -> ContextError {
    match refusal {
        Refusal::BadDecrypt => ContextError::bad_decrypt(),
        Refusal::Legacy => ContextError::evp_unsupported(),
        Refusal::Unsupported | Refusal::Malformed => ContextError::unsupported_key(),
    }
}

/// A `key` entry, opened with `passphrase` when it is encrypted.
pub fn load_private_key(
    pem: &[u8],
    passphrase: Option<&str>,
) -> Result<PrivateKeyDer<'static>, ContextError> {
    let text = String::from_utf8_lossy(pem);
    let block = first_key_block(&text).ok_or_else(ContextError::unsupported_key)?;
    // No passphrase at all: OpenSSL's password callback gives it nothing
    // (an empty one), and the decryption fails the same way.
    let password = Password::new(passphrase.unwrap_or(""));
    match block.label.as_str() {
        "PRIVATE KEY" if is_private_key_info(&block.der) => {
            Ok(PrivatePkcs8KeyDer::from(block.der).into())
        }
        "ENCRYPTED PRIVATE KEY" => {
            let plain = decrypt_private_key_info(&block.der, &password).map_err(refusal_error)?;
            Ok(PrivatePkcs8KeyDer::from(plain).into())
        }
        label @ ("RSA PRIVATE KEY" | "EC PRIVATE KEY") => {
            let encrypted = block
                .headers
                .iter()
                .any(|(name, value)| name == "Proc-Type" && value.ends_with("ENCRYPTED"));
            let der = if encrypted {
                let dek = block
                    .headers
                    .iter()
                    .find(|(name, _)| name == "DEK-Info")
                    .map(|(_, value)| value.as_str())
                    .ok_or_else(ContextError::unsupported_key)?;
                let (cipher, iv_hex) = dek
                    .split_once(',')
                    .ok_or_else(ContextError::unsupported_key)?;
                let cipher =
                    dek_info_cipher(cipher.trim())?.ok_or_else(ContextError::evp_unsupported)?;
                let iv = hex_bytes(iv_hex.trim()).ok_or_else(ContextError::unsupported_key)?;
                if iv.len() != cipher.iv_len() {
                    return Err(ContextError::unsupported_key());
                }
                // OpenSSL's EVP_DecryptFinal refuses a ciphertext that is not
                // whole blocks before it looks at the padding (measured: a
                // truncated key is this, not a bad decrypt).
                if block.der.len() % cipher.iv_len() != 0 {
                    return Err(ContextError::wrong_final_block_length());
                }
                let key = evp_bytes_to_key(&password.bytes, &iv[..8], cipher.key_len());
                cbc_decrypt(cipher, &key, &iv, &block.der)
                    .filter(|plain| is_versioned_sequence(plain))
                    .ok_or_else(ContextError::bad_decrypt)?
            } else {
                block.der
            };
            if !is_versioned_sequence(&der) {
                return Err(ContextError::unsupported_key());
            }
            Ok(if label == "RSA PRIVATE KEY" {
                PrivatePkcs1KeyDer::from(der).into()
            } else {
                PrivateSec1KeyDer::from(der).into()
            })
        }
        _ => Err(ContextError::unsupported_key()),
    }
}

// ------------------------------------------------------------------ PKCS#12

/// A ContentInfo's content: the octets of a `data` one.
fn content_info_data(info: &Tlv<'_>) -> Option<Vec<u8>> {
    let parts = children(info)?;
    let oid = expect(parts.first(), OID)?;
    if oid.content != OID_DATA {
        return None;
    }
    let explicit = expect(parts.get(1), CONTEXT_0)?;
    let inner = children(&explicit)?;
    octets(inner.first()?)
}

/// The password as PKCS#12's KDF takes it: a BMPString (UTF-16BE) with its
/// two-byte terminator.
fn bmp_password(password: &str) -> Vec<u8> {
    let mut out: Vec<u8> = password.encode_utf16().flat_map(u16::to_be_bytes).collect();
    out.extend_from_slice(&[0, 0]);
    out
}

/// Check a PFX's MAC with `password`, and say which form of it the MAC was
/// made with -- the one its bags are then opened with. OpenSSL's
/// PKCS12_parse: an empty password is tried as no password at all (NULL)
/// first, then as the empty BMPString.
fn verify_mac(
    mac_data: &Tlv<'_>,
    auth_safe: &[u8],
    password: &str,
) -> Result<Vec<u8>, ContextError> {
    let parts = children(mac_data).ok_or_else(malformed_pfx)?;
    let digest_info = expect(parts.first(), SEQUENCE).ok_or_else(malformed_pfx)?;
    let salt = expect(parts.get(1), OCTET_STRING).ok_or_else(malformed_pfx)?;
    let iterations = match parts.get(2) {
        Some(tlv) => u32::try_from(small_uint(tlv).ok_or_else(malformed_pfx)?)
            .map_err(|_| malformed_pfx())?,
        None => 1,
    };
    let info = children(&digest_info).ok_or_else(malformed_pfx)?;
    let algorithm = expect(info.first(), SEQUENCE).ok_or_else(malformed_pfx)?;
    let tag = expect(info.get(1), OCTET_STRING).ok_or_else(malformed_pfx)?;
    let oid = children(&algorithm)
        .and_then(|a| expect(a.first(), OID))
        .ok_or_else(malformed_pfx)?;
    let hash = match oid.content {
        OID_SHA1 => Hash::Sha1,
        OID_SHA224 => Hash::Sha224,
        OID_SHA256 => Hash::Sha256,
        OID_SHA384 => Hash::Sha384,
        OID_SHA512 => Hash::Sha512,
        _ => return Err(unsupported_pfx()),
    };
    let candidates = if password.is_empty() {
        vec![Vec::new(), bmp_password(password)]
    } else {
        vec![bmp_password(password)]
    };
    for candidate in candidates {
        let key = pkcs12_kdf_for(
            hash,
            &candidate,
            salt.content,
            3,
            iterations,
            digest_output(hash),
        );
        if hmac_verify(hash, &key, auth_safe, tag.content) {
            return Ok(candidate);
        }
    }
    Err(mac_verify_failure())
}

/// A bag or SafeContents that did not open, as Node reports it for a
/// bundle.
fn pfx_refusal(refusal: Refusal) -> ContextError {
    match refusal {
        Refusal::Legacy | Refusal::Unsupported => unsupported_pfx(),
        Refusal::BadDecrypt => pfx_bad_decrypt(),
        Refusal::Malformed => malformed_pfx(),
    }
}

/// The bags of one SafeContents.
fn read_bags(
    safe_contents: &[u8],
    password: &Password,
    bundle: &mut Pkcs12Bundle,
) -> Result<(), ContextError> {
    let bags = parse(safe_contents)
        .filter(|t| t.tag == SEQUENCE)
        .and_then(|t| children(&t))
        .ok_or_else(malformed_pfx)?;
    for bag in bags {
        let parts = children(&bag).ok_or_else(malformed_pfx)?;
        let bag_id = expect(parts.first(), OID).ok_or_else(malformed_pfx)?;
        let value = expect(parts.get(1), CONTEXT_0)
            .and_then(|v| children(&v))
            .and_then(|v| v.first().copied())
            .ok_or_else(malformed_pfx)?;
        match bag_id.content {
            OID_KEY_BAG => {
                if bundle.key.is_none() {
                    let der = value_bytes(&value);
                    if !is_private_key_info(&der) {
                        return Err(malformed_pfx());
                    }
                    bundle.key = Some(PrivatePkcs8KeyDer::from(der).into());
                }
            }
            OID_SHROUDED_KEY_BAG => {
                if bundle.key.is_none() {
                    let der = value_bytes(&value);
                    let plain = decrypt_private_key_info(&der, password).map_err(pfx_refusal)?;
                    bundle.key = Some(PrivatePkcs8KeyDer::from(plain).into());
                }
            }
            OID_CERT_BAG => {
                let cert_parts = children(&value).ok_or_else(malformed_pfx)?;
                let cert_type = expect(cert_parts.first(), OID).ok_or_else(malformed_pfx)?;
                if cert_type.content == OID_X509_CERTIFICATE {
                    let cert = expect(cert_parts.get(1), CONTEXT_0)
                        .and_then(|c| children(&c))
                        .and_then(|c| c.first().copied())
                        .and_then(|c| octets(&c))
                        .ok_or_else(malformed_pfx)?;
                    bundle.certs.push(CertificateDer::from(cert));
                }
            }
            // CRL bags, secret bags, nested safe contents: nothing a TLS
            // server serves.
            _ => {}
        }
    }
    Ok(())
}

/// The DER of a bag value: the element itself, header included.
fn value_bytes(value: &Tlv<'_>) -> Vec<u8> {
    // Re-encode the header: the value is a SEQUENCE whose contents we hold.
    let mut out = vec![value.tag];
    let len = value.content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(value.content);
    out
}

/// A `pfx` entry, opened with `password`.
pub fn load_pkcs12(der: &[u8], password: &str) -> Result<Pkcs12Bundle, ContextError> {
    let pfx = parse(der)
        .filter(|t| t.tag == SEQUENCE)
        .ok_or_else(malformed_pfx)?;
    let parts = children(&pfx).ok_or_else(malformed_pfx)?;
    if parts.first().and_then(small_uint) != Some(3) {
        return Err(malformed_pfx());
    }
    let auth_safe_info = expect(parts.get(1), SEQUENCE).ok_or_else(malformed_pfx)?;
    // Only password integrity mode (authSafe is `data`); public-key mode
    // (signedData) is not something a server's pfx uses.
    let auth_safe = content_info_data(&auth_safe_info).ok_or_else(unsupported_pfx)?;
    // The password the bags are opened with: the form the MAC verified
    // with; without a MAC, an empty password is no password at all (NULL),
    // as in OpenSSL's PKCS12_parse.
    let bmp = match parts.get(2).filter(|t| t.tag == SEQUENCE) {
        Some(mac_data) => verify_mac(mac_data, &auth_safe, password)?,
        None if password.is_empty() => Vec::new(),
        None => bmp_password(password),
    };
    let password = Password {
        bytes: password.as_bytes().to_vec(),
        bmp,
    };
    let infos = parse(&auth_safe)
        .filter(|t| t.tag == SEQUENCE)
        .and_then(|t| children(&t))
        .ok_or_else(malformed_pfx)?;
    let mut bundle = Pkcs12Bundle {
        key: None,
        certs: Vec::new(),
    };
    for info in infos {
        let info_parts = children(&info).ok_or_else(malformed_pfx)?;
        let kind = expect(info_parts.first(), OID).ok_or_else(malformed_pfx)?;
        match kind.content {
            OID_DATA => {
                let contents = content_info_data(&info).ok_or_else(malformed_pfx)?;
                read_bags(&contents, &password, &mut bundle)?;
            }
            OID_ENCRYPTED_DATA => {
                // EncryptedData { version, EncryptedContentInfo { type,
                // algorithm, [0] IMPLICIT encryptedContent } }
                let explicit = expect(info_parts.get(1), CONTEXT_0)
                    .and_then(|e| children(&e))
                    .and_then(|e| e.first().copied())
                    .ok_or_else(malformed_pfx)?;
                let encrypted_data = children(&explicit).ok_or_else(malformed_pfx)?;
                let content_info =
                    expect(encrypted_data.get(1), SEQUENCE).ok_or_else(malformed_pfx)?;
                let ci = children(&content_info).ok_or_else(malformed_pfx)?;
                let algorithm = expect(ci.get(1), SEQUENCE).ok_or_else(malformed_pfx)?;
                let content = ci
                    .get(2)
                    .filter(|t| t.tag == CONTEXT_0_PRIMITIVE || t.tag == CONTEXT_0)
                    .and_then(octets)
                    .ok_or_else(malformed_pfx)?;
                let plain = decrypt(&algorithm, &content, &password).map_err(pfx_refusal)?;
                read_bags(&plain, &password, &mut bundle)?;
            }
            _ => return Err(unsupported_pfx()),
        }
    }
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ber_reads_definite_and_indefinite_lengths() {
        // SEQUENCE { INTEGER 3 } in DER, and the same with an indefinite
        // length.
        let der = [0x30, 0x03, 0x02, 0x01, 0x03];
        let ber = [0x30, 0x80, 0x02, 0x01, 0x03, 0x00, 0x00];
        for input in [&der[..], &ber[..]] {
            let seq = parse(input).unwrap();
            let items = children(&seq).unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(small_uint(&items[0]), Some(3));
        }
        // A constructed OCTET STRING's segments are joined.
        let constructed = [0x24, 0x80, 0x04, 0x01, 0xaa, 0x04, 0x01, 0xbb, 0x00, 0x00];
        assert_eq!(
            octets(&parse(&constructed).unwrap()),
            Some(vec![0xaa, 0xbb])
        );
        assert!(parse(&[0x30, 0x05, 0x02]).is_none());
    }

    // RFC 7292's KDF against a vector computed with OpenSSL's
    // PKCS12_key_gen_uni (password "smeg", salt 0A58CF64530D823F, id 1,
    // iterations 1, SHA-1, 24 bytes).
    #[test]
    fn pkcs12_kdf_matches_openssl() {
        let key = pkcs12_kdf::<sha1::Sha1>(
            &bmp_password("smeg"),
            &[0x0a, 0x58, 0xcf, 0x64, 0x53, 0x0d, 0x82, 0x3f],
            1,
            1,
            24,
        );
        assert_eq!(
            hex(&key),
            "8aaae6297b6cb04642ab5b077851284eb7128f1a2a7fbca3"
        );
    }

    // `openssl enc -aes-{128,256}-cbc -k hunter2 -S 0102030405060708 -P -md
    // md5` (OpenSSL 3.5).
    #[test]
    fn evp_bytes_to_key_matches_openssl() {
        let salt = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(
            hex(&evp_bytes_to_key(b"hunter2", &salt, 16)),
            "dd076b4bcd49c33676d8185c3dd67e93"
        );
        assert_eq!(
            hex(&evp_bytes_to_key(b"hunter2", &salt, 32)),
            "dd076b4bcd49c33676d8185c3dd67e935d3b7324ff7d8e1074d9734059f0971e"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // The fixtures of conformance case 144, made with OpenSSL 3.5: the
    // case 141 server key (P-256) and certificate, the key encrypted every
    // way `openssl pkcs8 -topk8` / `openssl ec -aesN|-des3` write it, and the
    // pair bundled every way `openssl pkcs12 -export` writes it. The
    // passphrase is "hunter2" throughout (PFX_NOPASS: the empty one).
    const KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgQLidYpqFITu5wno8\n\
Fw5b5Ahrg5eTwH0UqA7RU57egNKhRANCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0b\n\
j0ngz5dpnyIRlajsUptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1V\n\
-----END PRIVATE KEY-----\n";

    const CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBvjCCAWWgAwIBAgIUOy7BLDqzc+0IZz2NWG95hnXgrd4wCgYIKoZIzj0EAwIw\n\
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx\n\
MjUwMTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIB\n\
BggqhkjOPQMBBwNCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0bj0ngz5dpnyIRlajs\n\
UptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1Vo4GMMIGJMBoGA1UdEQQTMBGC\n\
CWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDATBgNVHSUE\n\
DDAKBggrBgEFBQcDATAdBgNVHQ4EFgQUmxnUU2rP4FwgoXrkCkeRxNgCVycwHwYD\n\
VR0jBBgwFoAUOlIo6O4tIFNjD7vXJV51FU2DLQcwCgYIKoZIzj0EAwIDRwAwRAIg\n\
ItB5f9aIsf9D8cXBvJvvr5ahB57RK7DgAsIVf5uJ0zcCIBPOR2Z+ycbeeByMKH2v\n\
shKfeR1QdaoQHwJKJln0q1fo\n\
-----END CERTIFICATE-----\n";

    const CA: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBmjCCAUGgAwIBAgIUHjF3aO/Nr2SNMEQNV9GNuumIljswCgYIKoZIzj0EAwIw\n\
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx\n\
MjUwMTAxMDAwMDAwWjAaMRgwFgYDVQQDDA9vYW0gaDJzIHRlc3QgQ0EwWTATBgcq\n\
hkjOPQIBBggqhkjOPQMBBwNCAAR6EfahtynuI8VLuixWn6GiZ3BYWFdJEqP1FfLE\n\
lCBVF/69Rm6fDrzSVP/GWO7qsNhAZmyIVWyRQJcQiBv55omto2MwYTAdBgNVHQ4E\n\
FgQUOlIo6O4tIFNjD7vXJV51FU2DLQcwHwYDVR0jBBgwFoAUOlIo6O4tIFNjD7vX\n\
JV51FU2DLQcwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYwCgYIKoZI\n\
zj0EAwIDRwAwRAIgFFCfCAiuzT1cHBF7zAQEVxSrWsoco8cOD49S6whO4vsCIC/T\n\
xtSxdoSsByDfaJz7qxOrhJzSD5lDwUdNMe3EoP9l\n\
-----END CERTIFICATE-----\n";

    const ENC_PKCS8_AES256_SHA256: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBD/NfAlfZ9uL1N7wVVt\n\
wnhUAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQ3SzJ6yMRKmFSVr12\n\
3lg/JgSBkCYCOxV1KkXDgOlVCVslFI3opLMP2/WqW9SV8O6w22TT1dvuQZCA2M/h\n\
fBFdD8vg+8BI5E/79HcL+4pcD/K3fC2Zt0bKlKaHfpKGGJT2McuRaLFX9QUvEv+K\n\
neMEQmUOsMDcb+pfmQDQuVe8pUFvDFXp3IdCpdj7USyaW01vPEjygoSuwG2QRMZV\n\
PIcp7mnM0w==\n\
-----END ENCRYPTED PRIVATE KEY-----\n";

    const ENC_PKCS8_AES128_SHA1: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
MIHmMFEGCSqGSIb3DQEFDTBEMCMGCSqGSIb3DQEFDDAWBBDecnaqkm18vLtlYSAd\n\
Ui+MAgIIADAdBglghkgBZQMEAQIEEKVfKGNqr/uy0I80w+T6g6UEgZCfws3veonN\n\
EcFTNrXvR/pg62V6ZlVHctUFPepdsEgsJFZ5BmXn7HCqz9PkmN/R8L9pFI65Lqds\n\
hBFPl5LX9x2UlFSl/lM/nFyiIF050ofzNGJBKL/xzqMLJFOXyFQ7KLpg8KYn97AX\n\
BTjHkZQty/FzFKgNn3H401gDo2HwgLVmdUVW8OIKwQuWnfQIP1Rw2+Q=\n\
-----END ENCRYPTED PRIVATE KEY-----\n";

    const ENC_PKCS8_SCRYPT: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
MIHsMFcGCSqGSIb3DQEFDTBKMCkGCSsGAQQB2kcECzAcBBDizhYu2Ebr218D8t1X\n\
ORUwAgJAAAIBCAIBATAdBglghkgBZQMEASoEECoZDRx3OebYeGtCW+4dKa8EgZAl\n\
nvRYfvGg86rfYTkOJgg438VPCqX0X2e66rWWLkPWoatPxsR22GQmcCtLlSI3cl3F\n\
adhIA1yeee+BrG+90hpjFVHchFsDpY2xmCWUp15QhLTNJ2rdXf6DzAXKYwPlrKrO\n\
C8UQGBvR6qcOn8NJQmqINUbnjqu57GfMdUeUOcCD8yW1MHUx3vCfLZFdpnPLrmc=\n\
-----END ENCRYPTED PRIVATE KEY-----\n";

    const ENC_PKCS8_3DES: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
MIGxMBwGCiqGSIb3DQEMAQMwDgQI/vUeQeQSOx8CAggABIGQPkrDaGeC2+Ulgpmv\n\
q1Wx++nEQoSLb+vEUG9r46sFnsLEECUOjUi5zRinZKAysJGVWTCVgO3moQ/iifIG\n\
aWDtCtwkYiLc1YOzQyisN9uaL99qpJBm0Ivu+1Jmctq0FAop4/g9jOsmiaQ3aNKS\n\
ejHKTEs+5wrLnKHfbhswafAHeCrKbMGIk/C9Mfw8yFsSX19J\n\
-----END ENCRYPTED PRIVATE KEY-----\n";

    const ENC_TRAD_AES256: &str = "-----BEGIN EC PRIVATE KEY-----\n\
Proc-Type: 4,ENCRYPTED\n\
DEK-Info: AES-256-CBC,596FA7EB54954C9E933AB0263BF73EFC\n\
\n\
tmbMlvPgoBrWUneETUkHZcFnXn8SF0+YRPqL5DYe/oIIb6JdP+vZpzJ27DRhC6yf\n\
0S/oznLFevOugicDJxR0HjfOIbIVCuaJKJdIdW/HWP4TbFjlLY3AcwTQv62GO7NF\n\
3Re+qGxfvOX7j+u+hi0D/kyvRFVU+H0zp9zuVi7YuTM=\n\
-----END EC PRIVATE KEY-----\n";

    const ENC_TRAD_AES128: &str = "-----BEGIN EC PRIVATE KEY-----\n\
Proc-Type: 4,ENCRYPTED\n\
DEK-Info: AES-128-CBC,A7D5EF6FE5E600818D7D321D64B2B6AD\n\
\n\
QSqMSiDDp37W+a9qRZ3bxwz77jReAONHk3hwTkzl55CYP7JbHWZNjY8imch6BYVt\n\
cQKEff2zC/gAq7+BsEB5wVn7YC7LWWafYG4mNaLNRA43K+v0DBIogpU3qOFwQuuq\n\
NL4MJ/XdtjGSxOUfiP6EXeIfgS5XX56ljMiWJKtnKyk=\n\
-----END EC PRIVATE KEY-----\n";

    const ENC_TRAD_DES3: &str = "-----BEGIN EC PRIVATE KEY-----\n\
Proc-Type: 4,ENCRYPTED\n\
DEK-Info: DES-EDE3-CBC,C1E2F7A24C169E65\n\
\n\
3JR/ujj4YQ8R4vPFFSyoeG7st+JfzyiXaUVXb/CkHFOwu117w5wueL6tJ6UzPxDk\n\
1rreYgVAqIK8Ri5YnR23a0RB4l2nSn9DPu292APm3E4zONjj7XnPl+Nnxp3EQXGN\n\
ZlTzTyhb1sadpIQoDcaznS6wUNTDR7R3vBk8NWz3yxA=\n\
-----END EC PRIVATE KEY-----\n";

    const PFX_AES256: &str = "MIIGLAIBAzCCBeIGCSqGSIb3DQEHAaCCBdMEggXPMIIFyzCCBHoGCSqGSIb3DQEHBqCCBGswggRn\
AgEAMIIEYAYJKoZIhvcNAQcBMF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBCqXjV7M0Dy\
D175Fp7IFUoeAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQ5HAXDV9vpEEjTvv76jMa\
uoCCA/DCCZFpH7ORl8ZmegoXzaHB3EsnXdvl4/gVgNXV/j/cKsV3vutKV+EV2fl5kvgjxpj2YFWi\
RFmt0Droa9ShAghFe3HdE9EOJbRFpnRkUhd5l9Zv4bAIn53sXYDCl5WCfteJhuSyPupmL0YCiEBZ\
6WU3uy2ZWky0emJKhuz2RgRLLR6Sv/XV50/pTo7cG+RatY7HDrX2e1z7NkUd84dRfVo8zm/HEBDi\
Z4/m+bTSRW9ZK3Yi8HaH+7tmXPeoH5ijuM2NfDfCEKOhgpkqVwuei+Hx/Pt/gSZ0FWjZ608sn9en\
v7dvIzozrrlv2HaiVWRf/qbUpg/SE5cd8VfG/DuF+YFWDMHyJ7w9Y9eV/XVmxktCf94cInxUsawj\
+5xbzZsiZfeOUj54/v1ex8vyMBqIf0qZG8E+xBzv0GUiVfLxEP0+K76/cilcpZPR11ZCQBlZr9DM\
Cljm3sk0CaOScxh5YAt9338nuYjwWc1efaac/07i3k5Oa8oxbZB37zp/UoYCaXe4sRqfRq/sZnsm\
UdZeAloA429lNjx/RBeh5iOiAUS7NpXTKS7lq3IFPKCUfK/hW+xd6i02knuwlU3Flqpxyn3Avh8k\
GkECgyb77ushQ0pzexHrbDfhlgRYjuSQ1Wzbtua1Xq2jHUgosYencZdjFPbH4e8S7pfjLk47L8lQ\
5RmQ6N6lMWpUkUaZYTeZkkfRpdvbtP4R8lzeWNYvmJexQeVPEWbO3fKHMhdzzmGzHlU56xGb4vi1\
fFI5i8n4GEyfLt3KYlcboBleNMCV/AiLlDupqS+2a1GXRZbEODySUEt0M2Dt7yJGosAS3xpjIWon\
yHpcpffjwv0mq0lkqdggjJjRQj417XC2yXP7MjK6V9M06xEpSRRQH4ons1v9P0ckn7uHEkmsvr+W\
QElB8y6AUT53+oc0Q89+QK47o6Pg9P0hlpg7ZHUNSVzHpoD8+m3PaL3juaie+nBIfRxqt1Rn2YKb\
lFrsOOTBgkAYLU6aSPPjzlC+cwHBCJBwTXEFxqyqQiighpPXIRhN2LthwU76zPq2rHsCe9yH0P2X\
hrnTB8lQHoDs1SyoCzUtHOYE5YCGYgGnPhxb2G89K8fTIoXyz8zAly+gtPW+++voQgN8V5u7xefp\
V9K1Q5BxkBwg2k5Llx8OElJ4b5BzZOFkmXbj1eppKMC2TiZPJmt2hooJ8D5RX2g3pCJ2I62hHvu4\
VuwL8zDkrRNyHnFK1HvqcQRxKGIL3X6svOXQjp4eqgum0o6XDvYsX7xsYd16CeT3L/gfBq6Bao+X\
oVj/xwPag2podNZtjhWZch01e8ZbqnaOPg/Rj5sjgQwFKceN7HbPNzRv+0wwggFJBgkqhkiG9w0B\
BwGgggE6BIIBNjCCATIwggEuBgsqhkiG9w0BDAoBAqCB9zCB9DBfBgkqhkiG9w0BBQ0wUjAxBgkq\
hkiG9w0BBQwwJAQQnKCybfmc+9/qjal/9Hcn/gICCAAwDAYIKoZIhvcNAgkFADAdBglghkgBZQME\
ASoEEFsnj+SjrA/u16BzbYV6ue0EgZCkWy9pwOfy6yeqJAKptSp+DXIMTLlZXD7LVy/awrAl6Aqn\
IuNQXgR8cm1sqRM9nD+PruhpYLNn58XeJPo5Sh/mCPFVfFPEKnPs5UiS6gpWr87SeuWNcCTt1Ast\
oLceLDBARYCohjvXseOlQHNgFlL8nyEZlUDg5tl6Q2UOsenPwNPd13JNqyJ8W2Bz924rff0xJTAj\
BgkqhkiG9w0BCRUxFgQUC7Q/+dbHzTKclTiclZB4IHjDb00wQTAxMA0GCWCGSAFlAwQCAQUABCAQ\
4001cVDDlYPFTMg3YtCiMzYEGh5IDdPD5rpectd9sgQItOZBv2/HIQ4CAggA";

    const PFX_AES128_SHA1MAC: &str = "MIIETAIBAzCCBBIGCSqGSIb3DQEHAaCCBAMEggP/MIID+zCCAqoGCSqGSIb3DQEHBqCCApswggKX\
AgEAMIICkAYJKoZIhvcNAQcBMF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBBbFcg70Omw\
DloTEuXgLH0cAgID6DAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBAgQQjWvHHKRJzYdMTi/87gIf\
m4CCAiDwqKGC7hH9L8DRPl2ZLRqPf7v1xklZjIX5tlvRqnz1EsMdl7N0USVz9dsqVrFp31OMTsZf\
KJQI3iamogGxvgGnw4MnJGGRpickK0RRu8Q58kvP2tPaYSEvfM21NVzGJwn30bEl6Xwz8BnLRhJO\
iTE/B+BAPjLiGway7db5HpQeziHRKIK5aGit8MlE9icjWjDnwt417bGpBHt4yqbHLffj7kvqm0pD\
PzYO3MxA72/RL8s9IMCK+A3E73m5rhktXfmqBJwflXb2tbE+2OFuosF7rjgLQwhzwdT+fX8SfRZD\
eByhbojCWauJ0RjcUBb8yX/u9AoMIaW52Bs360+q/dMmTFKKjJvOMJ3bJHNxVCxA1MVEuL9V4pmX\
/rhGCz6ruhUenGK+jLsAb9GLG3GpKvXYo8ancXAbF+5LKxLvDIa3R7qpGMpQweLeaaWEAL9bcFMl\
YtBmHLPs2EYTmJhrWjZQTNr2aKk/5QE7YXe3QgOymHNyl0rrseY8eVSyWxX1Jw/Uj9k0XTTc3zOA\
ZYJcR8LxoQHOfVnfdKn64HBnypLyM70EwFvgBOUn/Y/sDjDhy4fNkYyNxzags6aKgTHYFdsWo0TL\
fh2LObQZo3dPXGe+VhsiGGlN8JNYCMxCAPyvCYE07YXAguPvPpqgjf5F7tZZ291a1prQAGO3Pjga\
87ia+YGbjJ+KEqffzMQch+LvGGS2bDpTylCyOQsUHLOKVdauMIIBSQYJKoZIhvcNAQcBoIIBOgSC\
ATYwggEyMIIBLgYLKoZIhvcNAQwKAQKggfcwgfQwXwYJKoZIhvcNAQUNMFIwMQYJKoZIhvcNAQUM\
MCQEEE3TuIXD+brW4JeDsTuV2nECAgPoMAwGCCqGSIb3DQIJBQAwHQYJYIZIAWUDBAECBBBGFd0E\
P/wZeXA18IIMQXUzBIGQOp0sj4pXYex+dHR09yJ8duUkLKs8zsbtf4cAp5qkCMco3KwhnYm+DFZO\
zt9Xe14r8gECHxytbLqyDzlNhLbjbRK6q6jS0ny4TzE9R5zOiJV/EoX/oRkSwruyBPloxcDd2SaS\
I8IFciOVHoyHR3nBEX0eWb5YSIJCecdED71BjvDZpsjY8SLCA27EShHFavl1MSUwIwYJKoZIhvcN\
AQkVMRYEFAu0P/nWx80ynJU4nJWQeCB4w29NMDEwITAJBgUrDgMCGgUABBRCWOY5jUX1qxAmK8pf\
LL2cEGsbZgQILr2XnsrDbCkCAgPo";

    const PFX_NOMAC: &str = "MIIDnAIBAzCCA5UGCSqGSIb3DQEHAaCCA4YEggOCMIIDfjCCAi0GCSqGSIb3DQEHAaCCAh4EggIa\
MIICFjCCAhIGCyqGSIb3DQEMCgEDoIIB2jCCAdYGCiqGSIb3DQEJFgGgggHGBIIBwjCCAb4wggFl\
oAMCAQICFDsuwSw6s3PtCGc9jVhveYZ14K3eMAoGCCqGSM49BAMCMBoxGDAWBgNVBAMMD29hbSBo\
MnMgdGVzdCBDQTAgFw0yNTAxMDEwMDAwMDBaGA8yMTI1MDEwMTAwMDAwMFowFDESMBAGA1UEAwwJ\
bG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEyGUkTjD3DV3JrGpnAEOlTOTcwpEd\
G49J4M+XaZ8iEZWo7FKbTf6j4rEaFSeQamY38+DEtb1TdChoO7w0aAjtVaOBjDCBiTAaBgNVHREE\
EzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMCB4AwEwYDVR0lBAwwCgYI\
KwYBBQUHAwEwHQYDVR0OBBYEFJsZ1FNqz+BcIKF65ApHkcTYAlcnMB8GA1UdIwQYMBaAFDpSKOju\
LSBTYw+71yVedRVNgy0HMAoGCCqGSM49BAMCA0cAMEQCICLQeX/WiLH/Q/HFwbyb76+WoQee0Suw\
4ALCFX+bidM3AiATzkdmfsnG3ngcjCh9r7ISn3kdUHWqEB8CSiZZ9KtX6DElMCMGCSqGSIb3DQEJ\
FTEWBBQLtD/51sfNMpyVOJyVkHggeMNvTTCCAUkGCSqGSIb3DQEHAaCCAToEggE2MIIBMjCCAS4G\
CyqGSIb3DQEMCgECoIH3MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBD7cB7smCSl\
u1dNz/y2bAUGAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQj5YFmUxgopzmP4sQm3V4\
kgSBkGLIoTrO1vyswgMJGoqeqxvarf+r9ikdAvgATeRaBhJpJ4K0Q1VG302yBD6qQy8rFESmM/CO\
J5owX8wD5PIHRmQPrpsZYfxlv2cJmRYVffN+Nhp/kqSyacL5SXbDxb7IgY1tNHUIvsfir3zQVTqw\
gdF1ik80p52XsLXZ9ZaAzfuhN4JFC1dOe1rWOF4XZyGeGjElMCMGCSqGSIb3DQEJFTEWBBQLtD/5\
1sfNMpyVOJyVkHggeMNvTQ==";

    const PFX_NOENC: &str = "MIIDbQIBAzCCAyMGCSqGSIb3DQEHAaCCAxQEggMQMIIDDDCCAi0GCSqGSIb3DQEHAaCCAh4EggIa\
MIICFjCCAhIGCyqGSIb3DQEMCgEDoIIB2jCCAdYGCiqGSIb3DQEJFgGgggHGBIIBwjCCAb4wggFl\
oAMCAQICFDsuwSw6s3PtCGc9jVhveYZ14K3eMAoGCCqGSM49BAMCMBoxGDAWBgNVBAMMD29hbSBo\
MnMgdGVzdCBDQTAgFw0yNTAxMDEwMDAwMDBaGA8yMTI1MDEwMTAwMDAwMFowFDESMBAGA1UEAwwJ\
bG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEyGUkTjD3DV3JrGpnAEOlTOTcwpEd\
G49J4M+XaZ8iEZWo7FKbTf6j4rEaFSeQamY38+DEtb1TdChoO7w0aAjtVaOBjDCBiTAaBgNVHREE\
EzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMCB4AwEwYDVR0lBAwwCgYI\
KwYBBQUHAwEwHQYDVR0OBBYEFJsZ1FNqz+BcIKF65ApHkcTYAlcnMB8GA1UdIwQYMBaAFDpSKOju\
LSBTYw+71yVedRVNgy0HMAoGCCqGSM49BAMCA0cAMEQCICLQeX/WiLH/Q/HFwbyb76+WoQee0Suw\
4ALCFX+bidM3AiATzkdmfsnG3ngcjCh9r7ISn3kdUHWqEB8CSiZZ9KtX6DElMCMGCSqGSIb3DQEJ\
FTEWBBQLtD/51sfNMpyVOJyVkHggeMNvTTCB2AYJKoZIhvcNAQcBoIHKBIHHMIHEMIHBBgsqhkiG\
9w0BDAoBAaCBijCBhwIBADATBgcqhkjOPQIBBggqhkjOPQMBBwRtMGsCAQEEIEC4nWKahSE7ucJ6\
PBcOW+QIa4OXk8B9FKgO0VOe3oDSoUQDQgAEyGUkTjD3DV3JrGpnAEOlTOTcwpEdG49J4M+XaZ8i\
EZWo7FKbTf6j4rEaFSeQamY38+DEtb1TdChoO7w0aAjtVTElMCMGCSqGSIb3DQEJFTEWBBQLtD/5\
1sfNMpyVOJyVkHggeMNvTTBBMDEwDQYJYIZIAWUDBAIBBQAEIDIyeh+r5mijac76FRwsa99ud3cz\
mbR2GsIVrrRR+j/YBAg55WW1SGhdgAICCAA=";

    const PFX_NOPASS: &str = "MIIEXAIBAzCCBBIGCSqGSIb3DQEHAaCCBAMEggP/MIID+zCCAqoGCSqGSIb3DQEHBqCCApswggKX\
AgEAMIICkAYJKoZIhvcNAQcBMF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBA9OzcK4X+L\
wKqlBXnLoTC9AgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQankWFtM7eT1h3w3VzJNx\
CoCCAiBugLifxUz34UsVRRl9l2eij6rer9ZA5D3RvFzFJjeiyMG4z81xq5GI6sPYgxdcpFam/KR8\
AyrxPeaz4AcvfGyt0K6Z+VhBSsxP5QfopLDXdMQCJWiC0ofL+1pAp/PKxA4bt59naFo0KJKuSTCv\
aeP57oa7k/MmsJKrnwdV4WcM5abSZE6ca0vD5G53QbNW8j0wyKB+4YgYsowKhVX5X5chYNEDQ31E\
Q16LLXc9PQZj+oU+DGo801kJ9o8yn2MTUF94RptaIAMDdk97Yxl5g/SwfrTlProSA/M9muJIpXl4\
aiq8BOjt76cFAIGSnyIyNAhftpynnyCGwg8gE9KyXb4d73ZNV+/D1EoHbIAxF5Ivn7SgwWk0caw0\
YAUiQG5dQwtcDfL4QD6nwZhBKqLqJ+wUuTUdNeP8fS5z1gBpKLdgDuPRezolLvzgcuEy2l4s9TV2\
3JmQ7N3CkHls/E7WIWsXmh29y460ItJk4K+Gak+X4WXHFLEZXoPqye2ZWujTs4nsJ8LF54JDpprx\
1wEx1nAQ3KSOQNVb5yqWKXOVH5oLSRnaTp1jfzgPeiiH7XjLMPqvNb3t6BYfNnqoeBeKPIwg4ohY\
5fs/w6sQVSIqa7PvDkGyIK98Qvp9aVyFCAx2bd36OHXtMsSJYDMTs0p4ULpdgPZMprU1niox2tvj\
8Ep4r0wYxCAvRZb8aCUGquL3KXu4g0+LVBTB7aimpWccnbwKMIIBSQYJKoZIhvcNAQcBoIIBOgSC\
ATYwggEyMIIBLgYLKoZIhvcNAQwKAQKggfcwgfQwXwYJKoZIhvcNAQUNMFIwMQYJKoZIhvcNAQUM\
MCQEEGi7U5cEkfDjG9i3tDgehnECAggAMAwGCCqGSIb3DQIJBQAwHQYJYIZIAWUDBAEqBBCBeZET\
pDYcii5piMK3/I6ABIGQl6sJ7zIfeX2LOC+mAGJXCH1Z4EN/iJo1tPM1nCuwrQbwKrosweV7IEX5\
/8cdMXIcIeZkQ74VM6BtFTtbplNZfG9uH3HfshNcWOIAv+C2xo3CXjvCb1yczSsSDa8qLccwBL4j\
IOHHH2i0yHxMa4fJa9CleHtCLlAPtOarr4RWHmj7WraomwUQr520xx2izGMgMSUwIwYJKoZIhvcN\
AQkVMRYEFAu0P/nWx80ynJU4nJWQeCB4w29NMEEwMTANBglghkgBZQMEAgEFAAQgzVZKCOL8ajrv\
qfdPMbCYORhi7bSzXfg5g2bNP+bKJlIECIS2a6C8HGvYAgIIAA==";

    const PFX_3DES: &str = "MIIDwgIBAzCCA4gGCSqGSIb3DQEHAaCCA3kEggN1MIIDcTCCAmcGCSqGSIb3DQEHBqCCAlgwggJU\
AgEAMIICTQYJKoZIhvcNAQcBMBwGCiqGSIb3DQEMAQMwDgQIGSNFq6YWisICAggAgIICINR5hWWu\
zB8Vx0YujNx/Ypn1KH4PBcEmgmkgdT3Zv0e7b8Q2PLh5lCd7cyCrW0gZbvrlK7Zt21KSfNEAnNrJ\
ARTor2CihXgCvDzXRJc+2T6RpLzUe61hxsYTNMSh4PyiyY7B1J/07t0XPtA4ZUdlMB6o9StqnUv9\
6vVC/Apvd9eK4rwY+p4I5Tp1bItMJpwyYIeOJetIhavK0A123/+xtZ01n6zeFHSiuQoP2Vp1MVHk\
aT9d4gAn5pBSA3K10QoxrsKJ43AX9JK7WaybAjJBa/0AXOvv+XUx393rTLbrRdU88RU/ux2kC2wp\
KsQeAlNFOL2gjI2AYYaLXCKEP24AhDzJAkVouze9/Y+X5dkki4jyaw/ikeHtG7PrNrkgzMumPTmn\
BBxw4BpiqXx3ISXvnEQOAql+kEzv1T9ifliMmAfeea28vOpb2s/xPcdAv/FEgQrvGwvZrkgON6uM\
itflri9sR8cap66OuJIb3BafsxNDhhR/QUmwsarrptgqfhqUOw1SyIYofO30ZgCMhEJ8TUbgGfpK\
lYgIsHe4Hms8CY0YSiNnEPDlZ1/8eLgsmDMQ3CC1fXZN2BzzYEQ3osFH/h5EoqUKFRXI/YcA1rzt\
soQoQiUAPm1muppo2nBK0nTg5+9EMOqzE7QO6q1KvX6Fe5JeNDKo+ngU8n5fDgD0qXJ+PVLa4oJ0\
ICGjXb4ShvitEnW5n8B0LUzwpaDCjTsKdhgwggECBgkqhkiG9w0BBwGggfQEgfEwge4wgesGCyqG\
SIb3DQEMCgECoIG0MIGxMBwGCiqGSIb3DQEMAQMwDgQIryWjhH+2hb4CAggABIGQIcIK5iLXWWfP\
DUKnPxd/WOdLK0oHfaHoorY08Ebc/Z0XtivDeimWtjsUnTszO9MJGMCY+0jdH0a32hklMWFcCfMF\
iCJwrt+RG7i9BI0jjbpTEu3pu35P/8dNFBzWi4ApiKGD3vGIPVa5BU2ALl3mZYQngAwIcIX9dUyc\
l1JJO7Lq0qWpEd2OP1akV+o+H0+ZMSUwIwYJKoZIhvcNAQkVMRYEFAu0P/nWx80ynJU4nJWQeCB4\
w29NMDEwITAJBgUrDgMCGgUABBROsKMLdQ9LI52ittkvIiG5GRBeZgQIuf3SxWpq678CAggA";

    const PFX_LEGACY: &str = "MIIDwgIBAzCCA4gGCSqGSIb3DQEHAaCCA3kEggN1MIIDcTCCAmcGCSqGSIb3DQEHBqCCAlgwggJU\
AgEAMIICTQYJKoZIhvcNAQcBMBwGCiqGSIb3DQEMAQYwDgQIHFb1l3GgbRsCAggAgIICIJCKXGog\
5ahV0Rnvnrps+hhBl/HebB6r+1QX4tqc8uR9VnY83kWJWSJV5SdFwQrdo4dAYjuyHNjLM8kQYHdV\
ScWfrGosei7xIsnIl491yCCnIvDViLVjIzrv+vT7OyzgH5p/GcbQ8HElIUTDCwUMjkM2pD4OZCxs\
pv6zAiHxai79jWRDe0k6jg0/SwFQ7tk3E2rg0TYDxlQN+byZ2Kh2lzhEKLkndnQ4NWGwmGYmsDDd\
GCo+3HiI2EEU/kbcEro0HjYvoD83JoWI6fMXIixez+kn2n2XxPlmf4fYnH+0J0dbHX/NPMBBnM9P\
iRyi3+Nm/w0yX7EvbKYyiRBfhGXbNiPL/TKG0QrTjHm81bPc4v9EK0wEqYucNzlHi4i3T+vAdRBR\
/QZDEPE02fAjuu5h+wDHA1gAsxF0y2Yc2VbkZs8S8aRfrYE/tyMi6lX2M5aR7QzVwUmBtL8p4KuU\
+aXaere7ygXZSGw86zKTJNcgjqY+T+PbnEGLqrM8bSU87yg34kmr7u3Oz0bl0H39z1NT6y1TOYc2\
mGsOuRP+aBn1ogLFjfuYUAXXBOHBE9TJ96U6mY3DrOOMqzxSdnjHbohi980EjwxfzUXyV09cY512\
+f1HNNKrjWZZe6p48hkZX5Go5N1jBdDSjc8xdTzwqbZWqOg4H95754slZF1eecK86SGnO7B1CSd1\
vqiYYVoQV6fu8G/Cka0z2oOlEfUK5OCy5okwggECBgkqhkiG9w0BBwGggfQEgfEwge4wgesGCyqG\
SIb3DQEMCgECoIG0MIGxMBwGCiqGSIb3DQEMAQMwDgQIkyb0mXIdttICAggABIGQ7PaU40D5/H/l\
vQ3X/yxmd5kvqhBL+/kVVYKt+nCNBif4OiEhH1Y7BVx++nn3B68o0tz4ujGkCVhBr7ciHEkO9MGZ\
0/IO2W5+DQc+oEQsCwG8W+LWAyDtFz72VZPXy/vaJ7G2JOUO/slMNM8RTc1EJEihOSP0EHn7FmGc\
FRIyKlhZaN7deKk6b3KCpnE0BrRdMSUwIwYJKoZIhvcNAQkVMRYEFAu0P/nWx80ynJU4nJWQeCB4\
w29NMDEwITAJBgUrDgMCGgUABBS37T1BnrJ+DyZm5AbR8yO+OwohTAQIUrF+Vk8e3TsCAggA";

    // The Triple-DES forms Node reads, and the legacy ones it refuses, made
    // with OpenSSL 3.5 from the same key and certificate ("hunter2"; the
    // single-DES, RC4 and Blowfish ones with its legacy provider): `openssl
    // pkcs8 -topk8 -v2 des3`, `-v1 PBE-SHA1-2DES`, `-v2 des-cbc`, `-v1
    // PBE-SHA1-DES`, `-v1 PBE-SHA1-RC4-128`; `openssl ec -des-ede-cbc`,
    // `-des`, `-bf`; `openssl pkcs12 -export` with `-keypbe/-certpbe`
    // PBE-SHA1-2DES, PBE-SHA1-3DES (with `-nomac`, and with an empty
    // password), DES-EDE3-CBC (PBES2), PBE-SHA1-RC4-128 for the key, DES-CBC
    // for the certificates. RSA_CERT is a self-signed RSA-2048 localhost
    // certificate (2025-2125) and ENC_RSA_TRAD_DES3 its key under
    // `openssl rsa -traditional -des3`.
    const RSA_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDOjCCAiKgAwIBAgIUf1VzZQLeHSMvtbWoevGmWX5gK6swDQYJKoZIhvcNAQEL\n\
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUw\n\
MTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwggEiMA0GCSqGSIb3DQEB\n\
AQUAA4IBDwAwggEKAoIBAQDGp86nCMj9sUkOCDNHNFtrWOpKWk88GdDGl8LyL/Zm\n\
bpZF5I0uQzuha+i+Iw+bcveNV4I/oEqlsiT0kxZ6DWf2p7rRi2MPAVIyKXeJZ2rS\n\
2FrTmPEsE5qGQrjtS7xe946+vg18BsW/88d/SriUOs2JlyUPwBCPsczS5h7duSWf\n\
OryWIdChwd8HL2OdLOhThsJnLykwLb2L313Exa3/BUpAoJLd7IMTKxqfljJ2SDqg\n\
qeZhJ0owgfmO3HaUWCr1KEO6VinbaNoiG2XgLbt1u7kI9Mxe+Vf774NLxYtC/txx\n\
X63wAZSJGMxVoeU1u5bHglx4m8VxB+f4Mc+n+uDnhQ9nAgMBAAGjgYEwfzAdBgNV\n\
HQ4EFgQUISnpzi3j+HUmwVrlbBgKO9+sfCcwHwYDVR0jBBgwFoAUISnpzi3j+HUm\n\
wVrlbBgKO9+sfCcwGgYDVR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAwGA1UdEwEB\n\
/wQCMAAwEwYDVR0lBAwwCgYIKwYBBQUHAwEwDQYJKoZIhvcNAQELBQADggEBAEj4\n\
P36dacT99r1ufz4gZbaxQsnJNakKjR0padxYmhBUEMOYS379zI99V5CQgU2HBxsg\n\
SI7r9mpg1HZMadU3h6jwXMqoB24yQqAsDMvzb9RQJRhmsdDh9OA+s6GD6kQ/A3B4\n\
7c+TZ1tKb3C4+MT8rXHMwbVCdvRvXZVQEvYqgHXkO3rpxS7plIJYEzp+fJabIE0W\n\
7+7ZJkdYLKVewrMqBAgTTeon1PpDI3Mm15isrNGVUWUHmRdFTvgGiqPx88fPZgQU\n\
mD/x+mHTitbbNtAS318UHtGXruOBUR+xNhf6pxe9hi2A+wW4nTv29ZsdoXSfCh38\n\
fNThpA0GAZlUS/aJE3g=\n\
-----END CERTIFICATE-----\n";

    const ENC_RSA_TRAD_DES3: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
Proc-Type: 4,ENCRYPTED\n\
DEK-Info: DES-EDE3-CBC,46AAEB545508ED53\n\
\n\
Ovspc0v14Ml+wwbVpv0j+asHnISUL0GJR1qLG3cikRitgkDFobcz7xR7JzpyChLZ\n\
hhV/j/q69mmzHEylYCEeMya06bYmYyMXsulCih5VDKljBDJp9PpJpOVGfjNvGugJ\n\
H6WxHYFEaHrxRtpxhwHCZ0Xw6zejMc0IKQ7WROgHJ8W9OM3ePqCgryWBtyvcPMxB\n\
s28t8cw34jOjhSGpwR0EmJyTEnS8SkX91cwXmWVmgOhCYrPhTd0se1J+nFdZItwd\n\
Ru1woqNGWkrkjEtIC/uNY4BAjCyx7y6udJm/TV6l2xKFIaOMPyh2gjRQ+SnSCIst\n\
yy2cvlQsdj9St/TOWlU8PVew17yQSKnxvUs7uuleMHwSxUz4K7sFS+h5G/BdJU7/\n\
rw5N+v1/N5InPDbiywaMbV9M8lZCqC43kJB8j4JCRgwC/pQavSXF8m6A2gfFaEvF\n\
qbhwgr/5DdXXHUfGR1ZFYeSu9TvJx71KTFgfOnUkBaj/aqwRm6LAIMBi2fKBM/gx\n\
Ozn7w8KT1yOB7KuRALeZv9ceu8nYfI4hH/ikwTSfqcTFEUUZsAe2lUiJHvQ9fyxp\n\
Og6cUzOvJBLyHeoReLkYiyog4Qw3AqQF5NWOeiTGgaVq9TrFGyi2bkscNfkOwulE\n\
4BR6P3TTpYELJLCRQVoC3ocHSCST4nDAS7Tw2/DvB76LsWbj9aUSNMMypK76TCMq\n\
c8ggKYgmKszTeGWf/lerwLelNM2R0vWApM5sZ3TUJAS+w04cNhzLbyp5+HbfT7Aj\n\
5zYCFZHsYdPuLslXWbgJj80FY6oBbvzpWChnhZSh7vma0w9cingbjiTpiOeppHUJ\n\
rraKxub0lpczQ2mwvSYP4jUY4xMJ4gz+jAZq6gwsUGMi0dJL02d8wyiPuSek/V6p\n\
gYJqKTGh0npPBFWpdIMeUdWIhbRPwyf1cO2t3CrSDbVyayi8pZTioGG1948iQxzJ\n\
+bEfXqbgg7QeIRgcgAb1x1jF1gJe1h8krurfdGwQZRel9u96x/BnSCaXP21lpIzZ\n\
r+P6eFQZ32o9Ms5ft3szYNvJMjCcUVg+66ekCfs+LxNbaBr4p3b06lOL/8Pm1MDL\n\
gGvcLTRCjojBMU0y8nKSnP6GcQ7w+gv+59IQuYKYTQZSvFSUUU9gN9H3t4gYYMfm\n\
IkRUv6jWPu9ikKdXNVLAXNvLNnzvalZqOu/q4TUTelYvtYd4ctH8DiFlnyyvg7QQ\n\
BYC0VT0shb6oosvzHK7qnJfUvvJIFdYY/FhkDzOFhN+NkTTW1f43e37rGrIEylZ6\n\
+y9W5vZEO0PS+/fp7eavAOwDjHYiUOv2tur/tePxoyZ3RgcXKBdM35l4884D4YGK\n\
U5WTDpoqjQK/jrOMOWeADJWqw0V3+8ldLOMNPCWHxGEDWjCEwYUEKHeQQCw/FIYV\n\
Zq94PmIsTWzyHj5780lsRhDumOcxSQbVmmcDLGf6wEz8UrD35jpCm0Gj6UPDnkZj\n\
whl/IY0hwv6Sgu8+bgWNsm8wBJr9odSF5LvBmCusxuIPUU6wl2u01iK7NC+ZWC/c\n\
NTtYepmISqR6miBpc/UGSkhHXCkMkoNHNnVbBKkpKS5s+azOj8Y9ag==\n\
-----END RSA PRIVATE KEY-----\n";

    const ENC_TRAD_DES_EDE: &str = "-----BEGIN EC PRIVATE KEY-----\n\
Proc-Type: 4,ENCRYPTED\n\
DEK-Info: DES-EDE-CBC,7DD01BED1D39F781\n\
\n\
nSV0BqKkdId7aC/nC4ZnZKBUheSMSBGTO9V5Zt3F1UqS5pTI70+BKCZsZh3FPXVK\n\
uW4xZHg0/0HVt7/ze2xm8L9G8q8iWY3+FFgygOKqMMDEhnYuB4EVFdNFafplcsdB\n\
2sIP+EE8+1iBG24VTPQUqmAiqotkgq5khof8sf+Nkcs=\n\
-----END EC PRIVATE KEY-----\n";

    const ENC_PKCS8_PBES2_DES3: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
MIHrMFYGCSqGSIb3DQEFDTBJMDEGCSqGSIb3DQEFDDAkBBD9aACEjjKNdZrgKR0K\n\
ezkHAgIIADAMBggqhkiG9w0CCQUAMBQGCCqGSIb3DQMHBAiEh9vtokF2RwSBkHxG\n\
VnoOwJGBIajQPEWWAN8CzemN272fEIlmQ8l4/G3pk3k1RQOWkX4H8cPTDHX9AaZG\n\
1M41J4HRbA6w8Fxy8OMlyB7l0jEU3SXMZfjMGKvHSqsPHm4ts90bj3EjbJsGga9m\n\
o1HR+/uVpqC2pHbjoQYGrEnKp7Y6MJC93M8C6DezrW4cta0NampF+cMT8mf2dg==\n\
-----END ENCRYPTED PRIVATE KEY-----\n";

    const ENC_PKCS8_2DES: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
MIGxMBwGCiqGSIb3DQEMAQQwDgQIInyAOdpEAsECAggABIGQHIxRKjWcb5ZoLlwi\n\
JGBUs1ql3RJxK/ULXT+e3M7rAPqjfepcU47/dzlow9HOnb9Ckzd24mPlWoWBNIRi\n\
fE3u5rHMn1ylnGEr+ht/XRC3wjR+3ErrbmSgn0UdPMo6nicaXqFbnMx1opnz8d6v\n\
ftNCTVUaRgTfONvO0Ag8att3GtTD6H5pL/pvYPzLgEocX7sn\n\
-----END ENCRYPTED PRIVATE KEY-----\n";

    const ENC_TRAD_DES: &str = "-----BEGIN EC PRIVATE KEY-----\n\
Proc-Type: 4,ENCRYPTED\n\
DEK-Info: DES-CBC,68B04ACA935C95A8\n\
\n\
0hWLqyoOsPJQgh3jSCHSg96QE0LAM24j6h4R0X+a8vsFd3yV1QCm0y3PxCNM4Tg9\n\
lkQg4c00M4W2DAzRIZuZ7pqMLu9Fg9flze+QCooiTpHXq5tkawqqj4Hstycm8hc2\n\
OImsx90xG5EtczetVVsdzuFyRU+YkGFxaWYJGyf116M=\n\
-----END EC PRIVATE KEY-----\n";

    const ENC_TRAD_BF: &str = "-----BEGIN EC PRIVATE KEY-----\n\
Proc-Type: 4,ENCRYPTED\n\
DEK-Info: BF-CBC,865316BF1D3B275B\n\
\n\
C/Xbl59c05g1nsU+K87CBKcUG5t7+zjE1eC0+tfSdS13WOcmL25ZXrqYvVoS1lFj\n\
4Te4mvIh866moMLojCY9VWASQmXaMy3qO7wSHad1VangIovx+2HPtUDGQT+dAQ7d\n\
wN5xTslXN3GbKDOhkuycYzC/vegPpKxi/8RVgD2hg/o=\n\
-----END EC PRIVATE KEY-----\n";

    const ENC_PKCS8_PBES1_DES: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
MIGwMBsGCSqGSIb3DQEFCjAOBAhhL5nI8v6y/AICCAAEgZBgYgCJhMKKmqwZGZTb\n\
8M4FMar8GnmEXLIuRh9nD/ZTGIhR31MrxCpm8cV9Zr8iYD/YKPrpASXnrs1fZTHA\n\
cjMHgR7o8gw5CckS3VajvLamYBfKxe6uUbz+KOKsjzXoLvtBc8AsfCHBsgscwN6w\n\
1OllsIg0VXcmbm/t1b9J65GBx95AwyROPdFChNXPTum8B7M=\n\
-----END ENCRYPTED PRIVATE KEY-----\n";

    const ENC_PKCS8_PBES2_DES: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
MIHoMFMGCSqGSIb3DQEFDTBGMDEGCSqGSIb3DQEFDDAkBBDQ5u4JuzsgPqXuWrV1\n\
p3yYAgIIADAMBggqhkiG9w0CCQUAMBEGBSsOAwIHBAhqi0xMpw6oXASBkII7H2pX\n\
Oi6LOky+Jr9azWmEZZw7/gjNDlS5g4cXhRU+4G+XK3VpK20LveIJ/n6GxLnFEkMc\n\
UpdqV9IebQ5fSGuD4zqwqu3EXzM1xLta/pGtrzl3CmNGGxuHUnBGPKFllwi5EEpD\n\
G1fpgaL4BIhFkrnVRBXZUnKMJpryxEIxIamqQuTn37xn4189BpDuCAoojA==\n\
-----END ENCRYPTED PRIVATE KEY-----\n";

    const ENC_PKCS8_RC4: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----\n\
MIGrMBwGCiqGSIb3DQEMAQEwDgQItUHaSygujMsCAggABIGK2Qky4zUqUqBP2rNT\n\
eFWML8NSfPkze5ckk3TKYbH6INFUGVsIEGGOo9XN53NoHQhP8KQcsqAW4VuaMHJC\n\
yZ5FoTJZd97yyKmMRXc92rdigu+eIBxvg8nH+22dc4P5ZdBCBZHvbo1IFqefccd/\n\
NvcE3LqERfmwpHZeI8OV4JY0+9Z8X9rMc4W7ZHbX\n\
-----END ENCRYPTED PRIVATE KEY-----\n";

    const PFX_2DES: &str = "MIIFigIBAzCCBVAGCSqGSIb3DQEHAaCCBUEEggU9MIIFOTCCBC8GCSqGSIb3DQEHBqCCBCAwggQc\
AgEAMIIEFQYJKoZIhvcNAQcBMBwGCiqGSIb3DQEMAQQwDgQIy3yzG4l6NawCAggAgIID6HkL9F1N\
XzWPqMyMUG8WpWsAyLrnpERMC3K4tKrOgzoUXu4RuNqgm9KZyPSCQxXyCDsHz+a04iW3QgoZvlze\
UMxa4xLqwvtJS5IzcYJQ+XBqH02T/hZZ8bKiKSA6+BdWU7vbeqyCzm8O2sdRINzFncO5ItIb5JOR\
Uy22oG5JiTrdi8Q4hf6EwB2gFh/32xcRHRXnjpXZohu1ShvwZ1sRZsfXvm6Mv3NBqc4PWJjMS7Je\
McgRZJfI45woTa+FuAQlKe8cGWFncTsytdohfJEglklkyx6Ip5bbjkQtSaNcQQ3JCMb80HiPi32L\
fLEXa7Km/yqeOg7aRZ+Iuw4Vg9GGyIWFMar4VhSpUn+HjazLhhTiCDh06Q10CraAxRkrNYhWSdOB\
dOeN9bn9W+010i0iqm2zcIBOcjbDGsRwl8E0MFcsy4/9iazChVYxlrzkR3sAsBfmMkgpV1/68Xb+\
1h5T0x39b/OhP6oqdZkTmINq4RyoiTav5bdBR6tqXwaQR94y6+QcxPZDBVm6fYKzr009eH1teAUj\
B3x3s0RwKVuwRXai5Vg4l/rnuQkQZZWTZx706X0zNckb2YI5GSfU1tmkNcYsy7HrCs9EO39WBJYr\
UOQ/Y7kxyEDF8VRT4jL7FaNOkGlsZtMLYBsT23E1drKFpNShDhZvCd32yuw1gxkFEGbDwy5ha0PR\
+iVDya0i/CodaplYRh78jzYwBrg0UgBWpmBpbviV+WjXoS4d3uWM851wbNmAhQiZdYcjWsA4FocJ\
ZvEIBa0Tk2NVPszpUknhimiqwpDlzgp6az/Z9JkewfbgJXVx5SK6Z8F5z5wxsX5j+uHMBCzSmBNU\
qzjQgJXvPXVkUIjkQs9Ew0+IoETOcZN6izGWNFAsvDht6CFxA9GExG2boQbNRnjssWE57051idsY\
oaHHnDrHCy9guZ/sHOKF2mcs9GzaUgyLWcX65ZjguXiAcAS1ImXQ/Emu8Q/C976/Vr0j2jNELnTw\
bPIzpxRTcAlQnrE4oufV7bMVmDo42VEBW/dr0zyoEZWnJzHiU9Ek5xjoST8PwlyPXO4XCj6SrF4o\
wYOpvP7neXVVRSOfdMwLOM8RoZfr0+1nfVZKFb7FB1EnRTn2MKpDeUQQT78JeFRbf5cJAaJ17Bpr\
vnD5fX/pD5OkeT9J49RJFJfAI5v/qjL4xtS2sSvHYucWPlnP0PFiOTGaj7LHb6gpRB6kjJgCGMix\
GDPZX0wUJ42Hz+T/zLMPk1VM+wa4tCSvaXpt+1rpnWaT+bDcQB8Ve7MA6knAxEo0bHmtFRmDpwIT\
kYNBFyoKqbEYk2CplmCWw+HXAnzY7AjZHV0wggECBgkqhkiG9w0BBwGggfQEgfEwge4wgesGCyqG\
SIb3DQEMCgECoIG0MIGxMBwGCiqGSIb3DQEMAQQwDgQI0P2+SJbJr2cCAggABIGQUGEDEbGHFm/h\
MwwvDSqN2TyYtsUWPZZasUVUMY7dKC56zoIppud9duHGr9tIrNCXS5dlHSg2q0oHd5pbfSkljIJW\
FFikONOyUkd1g4nScPFratJjUm9N11Mc1u+vV+QNbNB8AOzPp77MCFQkk4h9QFxz6erw4ozQilmP\
mhlfxnQ3A3sffMmNNkjAOzcOdMk+MSUwIwYJKoZIhvcNAQkVMRYEFAu0P/nWx80ynJU4nJWQeCB4\
w29NMDEwITAJBgUrDgMCGgUABBRb+PcNjXjKXqQG9UcRdXQWJWn7DgQIzm3nwbPo68cCAggA";

    const PFX_3DES_NOMAC: &str = "MIIFIAIBAzCCBRkGCSqGSIb3DQEHAaCCBQoEggUGMIIFAjCCA/gGCSqGSIb3DQEHAaCCA+kEggPl\
MIID4TCCAhIGCyqGSIb3DQEMCgEDoIIB2jCCAdYGCiqGSIb3DQEJFgGgggHGBIIBwjCCAb4wggFl\
oAMCAQICFDsuwSw6s3PtCGc9jVhveYZ14K3eMAoGCCqGSM49BAMCMBoxGDAWBgNVBAMMD29hbSBo\
MnMgdGVzdCBDQTAgFw0yNTAxMDEwMDAwMDBaGA8yMTI1MDEwMTAwMDAwMFowFDESMBAGA1UEAwwJ\
bG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEyGUkTjD3DV3JrGpnAEOlTOTcwpEd\
G49J4M+XaZ8iEZWo7FKbTf6j4rEaFSeQamY38+DEtb1TdChoO7w0aAjtVaOBjDCBiTAaBgNVHREE\
EzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMCB4AwEwYDVR0lBAwwCgYI\
KwYBBQUHAwEwHQYDVR0OBBYEFJsZ1FNqz+BcIKF65ApHkcTYAlcnMB8GA1UdIwQYMBaAFDpSKOju\
LSBTYw+71yVedRVNgy0HMAoGCCqGSM49BAMCA0cAMEQCICLQeX/WiLH/Q/HFwbyb76+WoQee0Suw\
4ALCFX+bidM3AiATzkdmfsnG3ngcjCh9r7ISn3kdUHWqEB8CSiZZ9KtX6DElMCMGCSqGSIb3DQEJ\
FTEWBBQLtD/51sfNMpyVOJyVkHggeMNvTTCCAccGCyqGSIb3DQEMCgEDoIIBtjCCAbIGCiqGSIb3\
DQEJFgGgggGiBIIBnjCCAZowggFBoAMCAQICFB4xd2jvza9kjTBEDVfRjbrpiJY7MAoGCCqGSM49\
BAMCMBoxGDAWBgNVBAMMD29hbSBoMnMgdGVzdCBDQTAgFw0yNTAxMDEwMDAwMDBaGA8yMTI1MDEw\
MTAwMDAwMFowGjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0D\
AQcDQgAEehH2obcp7iPFS7osVp+homdwWFhXSRKj9RXyxJQgVRf+vUZunw680lT/xlju6rDYQGZs\
iFVskUCXEIgb+eaJraNjMGEwHQYDVR0OBBYEFDpSKOjuLSBTYw+71yVedRVNgy0HMB8GA1UdIwQY\
MBaAFDpSKOjuLSBTYw+71yVedRVNgy0HMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgEG\
MAoGCCqGSM49BAMCA0cAMEQCIBRQnwgIrs09XBwRe8wEBFcUq1rKHKPHDg+PUusITuL7AiAv08bU\
sXaErAcg32ic+6sTq4Sc0g+ZQ8FHTTHtxKD/ZTCCAQIGCSqGSIb3DQEHAaCB9ASB8TCB7jCB6wYL\
KoZIhvcNAQwKAQKggbQwgbEwHAYKKoZIhvcNAQwBAzAOBAil76FKmuQbKQICCAAEgZDnKE9qC/eM\
Kolc6B/O4pXIO6sIESewbwgiepM7cXsHm9uipMGvr4IJcvW78rWOf4awKjpMsXMPeCIDalzQ1VIS\
0vbaofrv6E9SBr9r0ey5C5nrOnIYUsR0AJzSaYhFbHAg2iQEtrkSbG2sH7c3ggnOfYK69uMIp+J3\
EdDaiJZ40yDoDFAwkI6w3g9ANpKZcYMxJTAjBgkqhkiG9w0BCRUxFgQUC7Q/+dbHzTKclTiclZB4\
IHjDb00=";

    const PFX_PBES2_DES3: &str = "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\
I9+Gtk8ECIboZqmh3Vg7AgIIAA==";

    const PFX_3DES_EMPTY: &str = "MIIFmgIBAzCCBVAGCSqGSIb3DQEHAaCCBUEEggU9MIIFOTCCBC8GCSqGSIb3DQEHBqCCBCAwggQc\
AgEAMIIEFQYJKoZIhvcNAQcBMBwGCiqGSIb3DQEMAQMwDgQIKlj83O3wO20CAggAgIID6BFP3elJ\
h5arbuUflg8VPXjbrs9YoWcWyOZzWqZXw9vO5sRoAqU5UCzwuD87WghyS3GhVk16tTWFOJ+ZQFqI\
vPJzJ7r2LjU7up5xvuVis14veokjn8pD/WpYZQqAWHUN5/qpTW9a7EbbFF6/44jvfNLlCp3RP111\
tm2SLOjg3qQLtYit9HQPl/daGMiD1SB0LtSx3fAb3ipGQXLGe6M+evmaE45VJeKehXVXfTA1bFYF\
02nugVnQBMgq2DrXKDv+LvhLy8S03cdxL9BHg6Zk3hhNWQ0JSqVSlLpspfHmGEY6+JFdbnl1vObx\
m+xv3Rk6YVcLndEW6VIbxWuCLpetXPQ2vRASB7zWuDYmA4A+v/eRDrlfSMBAKEeS2vLu47/vwEYi\
9CSozeerUa782naOpUS+T+Kanxd/Mtl+/3ZIApzMZQAd3rvLsSp0ZS1mHa9Y2LQLlYEtFE8s1weW\
a8/8XsEZMOGdv9V2Lc8R3P4u/BgCIBbKxSe/UxKpZKC+6z+1WVg18NO0cjIrOMKnf+WN1It6e70e\
nqTURqJfiKGT3t1ib1LbTwLmKcVv348h4y/S/mjLXecOY3IUKPPWFGjqugImLXxCDCUJC8Yx+vbx\
vPg2N2FxMTV+u1av+6TrKuAiz0GD58sCZe1VAQhGziMSbsE1hvsudDerhSuakIUY7MXWq7phYN8s\
uG2z3/tgajP8vXG4aGyAb2xt+e+Hb5EKe0PfJdWpXJM6DIt+JWI9b6Wh0D3RvZpcbT4sNE06MxSs\
/BvdZnOuLoVZ5PAgNjH8Vwn05Yo1F7LZGm6BQ27aZvB9vQmEPt28bbiw+9NEHUWgZ3bG1oTlXuOe\
Cj4AWbuJabfR0mTd8870PosEt4gNNPWjgWjhdRqeZvYvTEHvyyltc6DDHk0v/1IKNxm5ad54OQR6\
QXVuLN48L0qf0TvBKQyqhj06PPHpWEgG2XFYN9z8KH8VxBwr2JXc99Jixv7H17lwbtbO+5cDkNE8\
SfkAJLCvfjCLHaccMx08e8bDw7WcAC1n4ydDD7iYkIRN4oxF7AFn4YM316tr/TJZY63Phzlbsse9\
/bORrNNveJ/maa/qYIs/VFPjSzqF1gG14TBDQW6Kq5vs6yvjGtgG1WPn/F1qnC91KGm7+G/s5kbH\
DLqhZHILmvx9PRXLOfr8y/nwzL3fQpt/XH4I5J/KltB3NqgF2HazVmKEsjnFPRj2xKxGGQ0MdTrH\
E72AtmEpHufmn6DclZ3gaZVTmuVNtcOjyxihD19rmrNNjn06hmKJ3R23X0HwkA6Na+ALGkBxrlDL\
NySdaWYGPE85CaIi3j1Z9j6dP7Qgx/2KXfswggECBgkqhkiG9w0BBwGggfQEgfEwge4wgesGCyqG\
SIb3DQEMCgECoIG0MIGxMBwGCiqGSIb3DQEMAQMwDgQI+IRq4BpZX+gCAggABIGQD/fNQOXloJhO\
LVKduFF7rGRpkQnMqAEWCayqrPvbGnFFeGVQBdnI8u+khAI2VxLsXoUfhYISkPQZVVK+AuX4I2Zz\
feayNjcPWAZLiPOeY5YbIPUY2/RPuejMH1kqKnXAnX4oB6kgtt1V1jFIT/Z6887KDEaVzc9hKyNs\
d7DjpKb1vQNq8cmyiYt0G4A8RV5YMSUwIwYJKoZIhvcNAQkVMRYEFAu0P/nWx80ynJU4nJWQeCB4\
w29NMEEwMTANBglghkgBZQMEAgEFAAQgiqEZZmGaeQpCG8xnn3AzQt3r6xVgJPDfNcuvTJOU9psE\
CP7IU6qiZftRAgIIAA==";

    const PFX_RC4_KEY: &str = "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\
KzAm7d7NC+fKsAQIrBzlHPJI1EsCAggA";

    const PFX_DES_CERTS: &str = "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\
sHru29UzKFvQ6BoECGJ2YEXz/KNcAgIIAA==";

    fn b64(text: &str) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .unwrap()
    }

    fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
        rustls_pemfile::certs(&mut std::io::BufReader::new(pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    /// Whether `key` is the private key of CERT.
    fn is_server_key(key: PrivateKeyDer<'static>) -> bool {
        let provider = rustls::crypto::ring::default_provider();
        let signing = provider.key_provider.load_private_key(key).unwrap();
        rustls::sign::CertifiedKey::new(certs(CERT), signing)
            .keys_match()
            .is_ok()
    }

    fn code(result: Result<PrivateKeyDer<'static>, ContextError>) -> Option<&'static str> {
        result.err().and_then(|e| e.code)
    }

    #[test]
    fn encrypted_keys_open_with_their_passphrase() {
        for (label, pem) in [
            ("plain", KEY),
            ("pkcs8 aes-256 pbkdf2-sha256", ENC_PKCS8_AES256_SHA256),
            ("pkcs8 aes-128 pbkdf2-sha1", ENC_PKCS8_AES128_SHA1),
            ("pkcs8 scrypt", ENC_PKCS8_SCRYPT),
            ("legacy aes-256", ENC_TRAD_AES256),
            ("legacy aes-128", ENC_TRAD_AES128),
        ] {
            let key = load_private_key(pem.as_bytes(), Some("hunter2"))
                .unwrap_or_else(|e| panic!("{label}: {e:?}"));
            assert!(is_server_key(key), "{label}");
        }
        for pem in [
            ENC_PKCS8_AES256_SHA256,
            ENC_PKCS8_AES128_SHA1,
            ENC_PKCS8_SCRYPT,
            ENC_TRAD_AES256,
            ENC_TRAD_AES128,
        ] {
            for passphrase in [Some("nope"), None] {
                assert_eq!(
                    code(load_private_key(pem.as_bytes(), passphrase)),
                    Some("ERR_OSSL_BAD_DECRYPT")
                );
            }
        }
        assert_eq!(
            code(load_private_key(b"not a key", None)),
            Some("ERR_OSSL_UNSUPPORTED")
        );
    }

    /// A legacy key cut short of a whole cipher block is OpenSSL's "wrong
    /// final block length"; cut at a block boundary, its padding fails:
    /// "bad decrypt" (both measured on v22.22.2).
    #[test]
    fn truncated_legacy_keys_fail_as_openssl_fails_them() {
        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD;
        for pem in [ENC_TRAD_DES3, ENC_TRAD_AES256] {
            let (head, rest) = pem
                .split_once(
                    "

",
                )
                .unwrap();
            let body: String = rest.lines().filter(|l| !l.starts_with("-----")).collect();
            let der = engine.decode(body).unwrap();
            let footer = rest.lines().last().unwrap();
            let cut = |len: usize| {
                let pem = format!(
                    "{head}

{}
{footer}
",
                    engine.encode(&der[..len])
                );
                code(load_private_key(pem.as_bytes(), Some("hunter2")))
            };
            assert_eq!(
                cut(der.len() - 1),
                Some("ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH")
            );
            assert_eq!(cut(1), Some("ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH"));
            assert_eq!(cut(der.len() - 16), Some("ERR_OSSL_BAD_DECRYPT"));
        }
    }

    // `openssl enc -des-ede3-cbc` / `-des-ede-cbc` (OpenSSL 3.5) of "oam
    // triple-DES known answer" under key 0123456789abcdef fedcba9876543210
    // [89abcdef01234567] and IV 1122334455667788.
    #[test]
    fn triple_des_cbc_matches_openssl() {
        let plain = b"oam triple-DES known answer";
        let iv = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let key3 = hex_bytes("0123456789abcdeffedcba987654321089abcdef01234567").unwrap();
        let ede3 =
            hex_bytes("dd1831dc20e09d6361941aad64320d3ec8bb5db6cebb999b75f8741806d04166").unwrap();
        assert_eq!(
            cbc_decrypt(BlockCipher::DesEde3, &key3, &iv, &ede3).as_deref(),
            Some(&plain[..])
        );
        let key2 = hex_bytes("0123456789abcdeffedcba9876543210").unwrap();
        let ede2 =
            hex_bytes("2eb4228b63ea1456a477b92fc9bc3ecb61ceaa7266cf9fca5731c526cc32020d").unwrap();
        assert_eq!(
            cbc_decrypt(BlockCipher::DesEde, &key2, &iv, &ede2).as_deref(),
            Some(&plain[..])
        );
        // A key or IV of the wrong length is no decryption at all.
        assert_eq!(cbc_decrypt(BlockCipher::DesEde3, &key2, &iv, &ede3), None);
        assert_eq!(cbc_decrypt(BlockCipher::DesEde3, &key3, &key2, &ede3), None);
    }

    #[test]
    fn triple_des_keys_open_as_node_opens_them() {
        for (label, pem) in [
            ("pkcs8 pbeWithSHAAnd3-KeyTripleDES-CBC", ENC_PKCS8_3DES),
            ("pkcs8 pbeWithSHAAnd2-KeyTripleDES-CBC", ENC_PKCS8_2DES),
            ("pkcs8 pbes2 des-ede3-cbc", ENC_PKCS8_PBES2_DES3),
            ("legacy DES-EDE3-CBC", ENC_TRAD_DES3),
            ("legacy DES-EDE-CBC", ENC_TRAD_DES_EDE),
        ] {
            let key = load_private_key(pem.as_bytes(), Some("hunter2"))
                .unwrap_or_else(|e| panic!("{label}: {e:?}"));
            assert!(is_server_key(key), "{label}");
            for passphrase in [Some("nope"), None, Some("")] {
                assert_eq!(
                    code(load_private_key(pem.as_bytes(), passphrase)),
                    Some("ERR_OSSL_BAD_DECRYPT"),
                    "{label} {passphrase:?}"
                );
            }
        }
        // `openssl rsa -traditional -des3`: an RSA key, the PKCS#1 form.
        let rsa = load_private_key(ENC_RSA_TRAD_DES3.as_bytes(), Some("hunter2")).unwrap();
        assert!(matches!(rsa, PrivateKeyDer::Pkcs1(_)));
        let provider = rustls::crypto::ring::default_provider();
        let signing = provider.key_provider.load_private_key(rsa).unwrap();
        assert!(
            rustls::sign::CertifiedKey::new(certs(RSA_CERT), signing)
                .keys_match()
                .is_ok()
        );
        assert_eq!(
            code(load_private_key(ENC_RSA_TRAD_DES3.as_bytes(), Some("nope"))),
            Some("ERR_OSSL_BAD_DECRYPT")
        );
        // What OpenSSL 3 keeps in its legacy provider Node refuses whatever
        // the passphrase: single DES (legacy PEM, PBES2, PKCS#5 v1.5 PBES1),
        // RC4, Blowfish.
        for (label, pem) in [
            ("legacy DES-CBC", ENC_TRAD_DES),
            ("legacy BF-CBC", ENC_TRAD_BF),
            ("pkcs8 pbeWithSHA1AndDES-CBC", ENC_PKCS8_PBES1_DES),
            ("pkcs8 pbes2 des-cbc", ENC_PKCS8_PBES2_DES),
            ("pkcs8 pbeWithSHAAnd128BitRC4", ENC_PKCS8_RC4),
        ] {
            for passphrase in [Some("hunter2"), Some("nope"), None] {
                let error = load_private_key(pem.as_bytes(), passphrase).unwrap_err();
                assert_eq!(
                    (error.code, error.message.as_str()),
                    (
                        Some("ERR_OSSL_EVP_UNSUPPORTED"),
                        "error:0308010C:digital envelope routines::unsupported"
                    ),
                    "{label} {passphrase:?}"
                );
            }
        }
    }

    #[test]
    fn pkcs12_bundles_open_as_node_opens_them() {
        for (label, der, passphrase) in [
            ("aes-256, sha-256 mac", PFX_AES256, "hunter2"),
            ("aes-128, sha-1 mac", PFX_AES128_SHA1MAC, "hunter2"),
            ("no mac", PFX_NOMAC, "hunter2"),
            ("no encryption", PFX_NOENC, "hunter2"),
            ("empty password", PFX_NOPASS, ""),
            ("3des", PFX_3DES, "hunter2"),
            ("2-key 3des", PFX_2DES, "hunter2"),
            ("3des, no mac", PFX_3DES_NOMAC, "hunter2"),
            ("pbes2 des-ede3-cbc", PFX_PBES2_DES3, "hunter2"),
            ("3des, empty password", PFX_3DES_EMPTY, ""),
        ] {
            let bundle =
                load_pkcs12(&b64(der), passphrase).unwrap_or_else(|e| panic!("{label}: {e:?}"));
            assert!(is_server_key(bundle.key.unwrap()), "{label}");
            assert_eq!(bundle.certs.first(), certs(CERT).first(), "{label}");
        }
        // The bundles made with -certfile carry the CA after the leaf.
        for der in [PFX_AES256, PFX_2DES, PFX_PBES2_DES3] {
            let full = load_pkcs12(&b64(der), "hunter2").unwrap();
            assert_eq!(full.certs, [certs(CERT), certs(CA)].concat());
        }

        let message = |der: &str, passphrase: &str| {
            load_pkcs12(&b64(der), passphrase)
                .err()
                .map(|e| (e.message, e.code))
        };
        let mac = Some(("mac verify failure".to_string(), None));
        assert_eq!(message(PFX_AES256, "nope"), mac);
        assert_eq!(message(PFX_AES256, ""), mac);
        assert_eq!(message(PFX_NOENC, "nope"), mac);
        assert_eq!(message(PFX_NOPASS, "x"), mac);
        assert_eq!(message(PFX_3DES, "nope"), mac);
        assert_eq!(message(PFX_3DES, ""), mac);
        assert_eq!(message(PFX_3DES_EMPTY, "hunter2"), mac);
        // Without a MAC, the wrong passphrase is the cipher's error.
        let bad_decrypt = Some(("bad decrypt".to_string(), None));
        for passphrase in ["nope", ""] {
            assert_eq!(message(PFX_NOMAC, passphrase), bad_decrypt);
            assert_eq!(message(PFX_3DES_NOMAC, passphrase), bad_decrypt);
        }
        // RC2, RC4 and single DES: refused as Node 22 refuses them, once the
        // MAC says the passphrase is right.
        let unsupported = Some((
            "Unsupported PKCS12 PFX data".to_string(),
            Some("ERR_CRYPTO_UNSUPPORTED_OPERATION"),
        ));
        for der in [PFX_LEGACY, PFX_RC4_KEY, PFX_DES_CERTS] {
            assert_eq!(message(der, "hunter2"), unsupported);
            assert_eq!(message(der, "nope"), mac);
        }
        assert!(load_pkcs12(b"garbage", "").is_err());
    }
}
