//! node:crypto natives: streaming digests, HMAC, OS randomness, and
//! constant-time comparison. Pure-CPU sync work — these run on the isolate
//! thread directly, no op channel.
//!
//! State model: in-flight hashers live in an isolate-slot registry keyed
//! by handle (zero-capture callbacks can't hold Rust state). digest()
//! consumes the entry; copy() clones it (every RustCrypto hasher is
//! Clone, which is what makes Node's hash.copy() cheap here).

use aes_gcm::aead::{Aead, KeyInit as AeadKeyInit};
use cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, StreamCipher};
use hmac::Mac;
use rsa::signature::SignatureEncoding;
use sha2::Digest;
use std::collections::HashMap;

#[derive(Clone)]
pub(crate) enum Hasher {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha224(sha2::Sha224),
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
    HmacMd5(hmac::Hmac<md5::Md5>),
    HmacSha1(hmac::Hmac<sha1::Sha1>),
    HmacSha224(hmac::Hmac<sha2::Sha224>),
    HmacSha256(hmac::Hmac<sha2::Sha256>),
    HmacSha384(hmac::Hmac<sha2::Sha384>),
    HmacSha512(hmac::Hmac<sha2::Sha512>),
}

pub(crate) const SUPPORTED_HASHES: [&str; 6] =
    ["md5", "sha1", "sha224", "sha256", "sha384", "sha512"];

impl Hasher {
    fn new(algorithm: &str) -> Option<Self> {
        Some(match algorithm {
            "md5" => Hasher::Md5(md5::Md5::new()),
            "sha1" => Hasher::Sha1(sha1::Sha1::new()),
            "sha224" => Hasher::Sha224(sha2::Sha224::new()),
            "sha256" => Hasher::Sha256(sha2::Sha256::new()),
            "sha384" => Hasher::Sha384(sha2::Sha384::new()),
            "sha512" => Hasher::Sha512(sha2::Sha512::new()),
            _ => return None,
        })
    }

    fn new_hmac(algorithm: &str, key: &[u8]) -> Option<Self> {
        // new_from_slice is infallible for HMAC (any key length is legal).
        Some(match algorithm {
            "md5" => Hasher::HmacMd5(Mac::new_from_slice(key).ok()?),
            "sha1" => Hasher::HmacSha1(Mac::new_from_slice(key).ok()?),
            "sha224" => Hasher::HmacSha224(Mac::new_from_slice(key).ok()?),
            "sha256" => Hasher::HmacSha256(Mac::new_from_slice(key).ok()?),
            "sha384" => Hasher::HmacSha384(Mac::new_from_slice(key).ok()?),
            "sha512" => Hasher::HmacSha512(Mac::new_from_slice(key).ok()?),
            _ => return None,
        })
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Md5(h) => Digest::update(h, data),
            Hasher::Sha1(h) => Digest::update(h, data),
            Hasher::Sha224(h) => Digest::update(h, data),
            Hasher::Sha256(h) => Digest::update(h, data),
            Hasher::Sha384(h) => Digest::update(h, data),
            Hasher::Sha512(h) => Digest::update(h, data),
            Hasher::HmacMd5(h) => Mac::update(h, data),
            Hasher::HmacSha1(h) => Mac::update(h, data),
            Hasher::HmacSha224(h) => Mac::update(h, data),
            Hasher::HmacSha256(h) => Mac::update(h, data),
            Hasher::HmacSha384(h) => Mac::update(h, data),
            Hasher::HmacSha512(h) => Mac::update(h, data),
        }
    }

    fn finalize(self) -> Vec<u8> {
        match self {
            Hasher::Md5(h) => h.finalize().to_vec(),
            Hasher::Sha1(h) => h.finalize().to_vec(),
            Hasher::Sha224(h) => h.finalize().to_vec(),
            Hasher::Sha256(h) => h.finalize().to_vec(),
            Hasher::Sha384(h) => h.finalize().to_vec(),
            Hasher::Sha512(h) => h.finalize().to_vec(),
            Hasher::HmacMd5(h) => h.finalize().into_bytes().to_vec(),
            Hasher::HmacSha1(h) => h.finalize().into_bytes().to_vec(),
            Hasher::HmacSha224(h) => h.finalize().into_bytes().to_vec(),
            Hasher::HmacSha256(h) => h.finalize().into_bytes().to_vec(),
            Hasher::HmacSha384(h) => h.finalize().into_bytes().to_vec(),
            Hasher::HmacSha512(h) => h.finalize().into_bytes().to_vec(),
        }
    }
}

#[derive(Default)]
pub(crate) struct CryptoState {
    next: u64,
    map: HashMap<u64, Hasher>,
    ciphers: HashMap<u64, CipherInstance>,
}

impl CryptoState {
    fn insert(&mut self, hasher: Hasher) -> u64 {
        self.next += 1;
        self.map.insert(self.next, hasher);
        self.next
    }
}

/// Normalize Node's algorithm spellings: case-insensitive, dashes dropped
/// ('SHA-256' == 'sha256'; WebCrypto names ride the same path).
fn normalize_algorithm(raw: &str) -> String {
    raw.to_ascii_lowercase().replace('-', "")
}

fn throw_unknown_digest(scope: &mut v8::PinScope<'_, '_>, algorithm: &str) {
    let message = v8::String::new(
        scope,
        &format!(
            "Digest method not supported: '{algorithm}' (oam ships {})",
            SUPPORTED_HASHES.join(", ")
        ),
    )
    .unwrap();
    let exception = v8::Exception::error(scope, message);
    scope.throw_exception(exception);
}

pub(crate) fn op_crypto_hash_create(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(algorithm) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "createHash requires an algorithm");
        return;
    };
    let normalized = normalize_algorithm(&algorithm);
    let Some(hasher) = Hasher::new(&normalized) else {
        throw_unknown_digest(scope, &algorithm);
        return;
    };
    let id = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed")
        .insert(hasher);
    rv.set_double(id as f64);
}

pub(crate) fn op_crypto_hmac_create(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(algorithm) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "createHmac requires an algorithm");
        return;
    };
    let Some(key) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "createHmac requires a key");
        return;
    };
    let normalized = normalize_algorithm(&algorithm);
    let Some(hasher) = Hasher::new_hmac(&normalized, &key) else {
        throw_unknown_digest(scope, &algorithm);
        return;
    };
    let id = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed")
        .insert(hasher);
    rv.set_double(id as f64);
}

pub(crate) fn op_crypto_hash_update(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(bytes) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "hash update requires data");
        return;
    };
    let state = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed");
    match state.map.get_mut(&id) {
        Some(hasher) => hasher.update(&bytes),
        None => crate::node_ops::throw_type_error(scope, "Digest already called"),
    }
}

pub(crate) fn op_crypto_hash_digest(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let hasher = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed")
        .map
        .remove(&id);
    match hasher {
        Some(hasher) => {
            let bytes = hasher.finalize();
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, bytes) {
                rv.set(value);
            }
        }
        None => crate::node_ops::throw_type_error(scope, "Digest already called"),
    }
}

pub(crate) fn op_crypto_hash_copy(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let state = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed");
    match state.map.get(&id).cloned() {
        Some(clone) => {
            let new_id = state.insert(clone);
            rv.set_double(new_id as f64);
        }
        None => crate::node_ops::throw_type_error(scope, "Digest already called"),
    }
}

pub(crate) fn op_crypto_random_fill(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let len = args.get(0).number_value(scope).unwrap_or(0.0);
    if !(0.0..=65536.0).contains(&len) {
        crate::node_ops::throw_type_error(
            scope,
            "random fill length must be between 0 and 65536 bytes per call",
        );
        return;
    }
    let mut bytes = vec![0u8; len as usize];
    if getrandom::fill(&mut bytes).is_err() {
        crate::node_ops::throw_type_error(scope, "OS randomness source unavailable");
        return;
    }
    if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, bytes) {
        rv.set(value);
    }
}

pub(crate) fn op_crypto_timing_safe_equal(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let (Some(a), Some(b)) = (
        crate::node_ops::arg_bytes(scope, &args, 0),
        crate::node_ops::arg_bytes(scope, &args, 1),
    ) else {
        crate::node_ops::throw_type_error(scope, "timingSafeEqual requires two buffers");
        return;
    };
    if a.len() != b.len() {
        let message =
            v8::String::new(scope, "Input buffers must have the same byte length").unwrap();
        let exception = v8::Exception::range_error(scope, message);
        scope.throw_exception(exception);
        return;
    }
    // Constant-time: accumulate XOR over every byte, no early exit.
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    rv.set_bool(acc == 0);
}

// ===================================================== key derivation

pub(crate) fn op_crypto_pbkdf2_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(password) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "pbkdf2: password required");
        return;
    };
    let Some(salt) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "pbkdf2: salt required");
        return;
    };
    let iterations = args.get(2).number_value(scope).unwrap_or(0.0) as u32;
    let keylen = args.get(3).number_value(scope).unwrap_or(0.0) as usize;
    let Some(digest) = crate::node_ops::arg_string(scope, &args, 4) else {
        crate::node_ops::throw_type_error(scope, "pbkdf2: digest required");
        return;
    };
    if iterations == 0 {
        crate::node_ops::throw_type_error(scope, "pbkdf2: iterations must be > 0");
        return;
    }
    let mut dk = vec![0u8; keylen];
    let normalized = normalize_algorithm(&digest);
    match normalized.as_str() {
        "sha256" => pbkdf2::pbkdf2_hmac::<sha2::Sha256>(&password, &salt, iterations, &mut dk),
        "sha384" => pbkdf2::pbkdf2_hmac::<sha2::Sha384>(&password, &salt, iterations, &mut dk),
        "sha512" => pbkdf2::pbkdf2_hmac::<sha2::Sha512>(&password, &salt, iterations, &mut dk),
        "sha1" => pbkdf2::pbkdf2_hmac::<sha1::Sha1>(&password, &salt, iterations, &mut dk),
        "md5" => pbkdf2::pbkdf2_hmac::<md5::Md5>(&password, &salt, iterations, &mut dk),
        _ => {
            throw_unknown_digest(scope, &digest);
            return;
        }
    }
    if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, dk) {
        rv.set(value);
    }
}

pub(crate) fn op_crypto_scrypt_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(password) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "scrypt: password required");
        return;
    };
    let Some(salt) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "scrypt: salt required");
        return;
    };
    let keylen = args.get(2).number_value(scope).unwrap_or(0.0) as usize;
    let n = args.get(3).number_value(scope).unwrap_or(16384.0) as u64;
    let r = args.get(4).number_value(scope).unwrap_or(8.0) as u32;
    let p = args.get(5).number_value(scope).unwrap_or(1.0) as u32;

    if n == 0 || !n.is_power_of_two() {
        crate::node_ops::throw_type_error(scope, "scrypt: N must be a power of 2");
        return;
    }
    let log_n = 63 - n.leading_zeros() as u8;
    let params = match scrypt::Params::new(log_n, r, p, keylen) {
        Ok(params) => params,
        Err(e) => {
            crate::node_ops::throw_type_error(scope, &format!("scrypt: invalid parameters: {e}"));
            return;
        }
    };
    let mut dk = vec![0u8; keylen];
    if scrypt::scrypt(&password, &salt, &params, &mut dk).is_err() {
        crate::node_ops::throw_type_error(scope, "scrypt: derivation failed");
        return;
    }
    if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, dk) {
        rv.set(value);
    }
}

pub(crate) fn op_crypto_hkdf_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(digest) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "hkdf: digest required");
        return;
    };
    let Some(ikm) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "hkdf: ikm required");
        return;
    };
    let Some(salt) = crate::node_ops::arg_bytes(scope, &args, 2) else {
        crate::node_ops::throw_type_error(scope, "hkdf: salt required");
        return;
    };
    let Some(info) = crate::node_ops::arg_bytes(scope, &args, 3) else {
        crate::node_ops::throw_type_error(scope, "hkdf: info required");
        return;
    };
    let keylen = args.get(4).number_value(scope).unwrap_or(0.0) as usize;

    let normalized = normalize_algorithm(&digest);
    let salt_opt = if salt.is_empty() {
        None
    } else {
        Some(&salt[..])
    };
    macro_rules! hkdf_expand {
        ($hash:ty) => {{
            let hk = hkdf::Hkdf::<$hash>::new(salt_opt, &ikm);
            let mut okm = vec![0u8; keylen];
            match hk.expand(&info, &mut okm) {
                Ok(()) => okm,
                Err(_) => {
                    crate::node_ops::throw_type_error(
                        scope,
                        "hkdf: output length too large for digest",
                    );
                    return;
                }
            }
        }};
    }
    let okm = match normalized.as_str() {
        "sha256" => hkdf_expand!(sha2::Sha256),
        "sha384" => hkdf_expand!(sha2::Sha384),
        "sha512" => hkdf_expand!(sha2::Sha512),
        "sha1" => hkdf_expand!(sha1::Sha1),
        _ => {
            throw_unknown_digest(scope, &digest);
            return;
        }
    };
    if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, okm) {
        rv.set(value);
    }
}

// ===================================================== symmetric ciphers

pub(crate) const SUPPORTED_CIPHERS: [&str; 6] = [
    "aes-128-cbc",
    "aes-256-cbc",
    "aes-128-ctr",
    "aes-256-ctr",
    "aes-128-gcm",
    "aes-256-gcm",
];

enum CipherMode {
    Aes128Cbc,
    Aes256Cbc,
    Aes128Ctr,
    Aes256Ctr,
    Aes128Gcm,
    Aes256Gcm,
}

struct CipherInstance {
    mode: CipherMode,
    key: Vec<u8>,
    iv: Vec<u8>,
    encrypt: bool,
    buffer: Vec<u8>,
    aad: Vec<u8>,
    auth_tag: Option<Vec<u8>>,
    auto_padding: bool,
}

impl CipherMode {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "aes-128-cbc" => Self::Aes128Cbc,
            "aes-256-cbc" => Self::Aes256Cbc,
            "aes-128-ctr" => Self::Aes128Ctr,
            "aes-256-ctr" => Self::Aes256Ctr,
            "aes-128-gcm" => Self::Aes128Gcm,
            "aes-256-gcm" => Self::Aes256Gcm,
            _ => return None,
        })
    }

    fn key_len(&self) -> usize {
        match self {
            Self::Aes128Cbc | Self::Aes128Ctr | Self::Aes128Gcm => 16,
            Self::Aes256Cbc | Self::Aes256Ctr | Self::Aes256Gcm => 32,
        }
    }

    fn iv_len(&self) -> usize {
        match self {
            Self::Aes128Cbc | Self::Aes256Cbc => 16,
            Self::Aes128Ctr | Self::Aes256Ctr => 16,
            Self::Aes128Gcm | Self::Aes256Gcm => 12,
        }
    }
}

pub(crate) fn op_crypto_cipher_create(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(algorithm) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "createCipheriv: algorithm required");
        return;
    };
    let Some(key) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "createCipheriv: key required");
        return;
    };
    let Some(iv) = crate::node_ops::arg_bytes(scope, &args, 2) else {
        crate::node_ops::throw_type_error(scope, "createCipheriv: iv required");
        return;
    };
    let encrypt = args.get(3).boolean_value(scope);
    let name = algorithm.to_ascii_lowercase();
    let Some(mode) = CipherMode::from_name(&name) else {
        let message = format!(
            "Unknown cipher: '{}' (oam ships {})",
            algorithm,
            SUPPORTED_CIPHERS.join(", ")
        );
        crate::node_ops::throw_type_error(scope, &message);
        return;
    };
    if key.len() != mode.key_len() {
        crate::node_ops::throw_type_error(
            scope,
            &format!(
                "Invalid key length: expected {} bytes, got {}",
                mode.key_len(),
                key.len()
            ),
        );
        return;
    }
    if iv.len() != mode.iv_len() {
        crate::node_ops::throw_type_error(
            scope,
            &format!(
                "Invalid IV length: expected {} bytes, got {}",
                mode.iv_len(),
                iv.len()
            ),
        );
        return;
    }
    let instance = CipherInstance {
        mode,
        key,
        iv,
        encrypt,
        buffer: Vec::new(),
        aad: Vec::new(),
        auth_tag: None,
        auto_padding: true,
    };
    let state = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed");
    state.next += 1;
    let id = state.next;
    state.ciphers.insert(id, instance);
    rv.set_double(id as f64);
}

pub(crate) fn op_crypto_cipher_update(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "cipher update requires data");
        return;
    };
    let state = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed");
    match state.ciphers.get_mut(&id) {
        Some(c) => c.buffer.extend_from_slice(&data),
        None => crate::node_ops::throw_type_error(scope, "cipher: invalid handle"),
    }
}

pub(crate) fn op_crypto_cipher_final(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let instance = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed")
        .ciphers
        .remove(&id);
    let Some(instance) = instance else {
        crate::node_ops::throw_type_error(scope, "cipher: invalid handle");
        return;
    };
    let result = if instance.encrypt {
        cipher_encrypt(instance)
    } else {
        cipher_decrypt(instance)
    };
    match result {
        Ok(bytes) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, bytes) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_cipher_set_aad(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "setAAD requires data");
        return;
    };
    let state = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed");
    match state.ciphers.get_mut(&id) {
        Some(c) => c.aad = data,
        None => crate::node_ops::throw_type_error(scope, "cipher: invalid handle"),
    }
}

pub(crate) fn op_crypto_cipher_get_auth_tag(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let tag_data = {
        let state = scope
            .get_slot_mut::<CryptoState>()
            .expect("crypto state installed");
        match state.ciphers.get(&id) {
            Some(c) => c.auth_tag.clone(),
            None => {
                crate::node_ops::throw_type_error(scope, "cipher: invalid handle");
                return;
            }
        }
    };
    match tag_data {
        Some(tag) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, tag) {
                rv.set(value);
            }
        }
        None => crate::node_ops::throw_type_error(
            scope,
            "getAuthTag: not available (call final() first for GCM encrypt)",
        ),
    }
}

pub(crate) fn op_crypto_cipher_set_auth_tag(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "setAuthTag requires data");
        return;
    };
    let state = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed");
    match state.ciphers.get_mut(&id) {
        Some(c) => c.auth_tag = Some(data),
        None => crate::node_ops::throw_type_error(scope, "cipher: invalid handle"),
    }
}

pub(crate) fn op_crypto_cipher_set_auto_padding(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let auto_pad = args.get(1).boolean_value(scope);
    let state = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed");
    match state.ciphers.get_mut(&id) {
        Some(c) => c.auto_padding = auto_pad,
        None => crate::node_ops::throw_type_error(scope, "cipher: invalid handle"),
    }
}

fn cipher_encrypt(instance: CipherInstance) -> Result<Vec<u8>, String> {
    match instance.mode {
        CipherMode::Aes128Cbc | CipherMode::Aes256Cbc => cbc_encrypt(&instance),
        CipherMode::Aes128Ctr | CipherMode::Aes256Ctr => ctr_process(&instance),
        CipherMode::Aes128Gcm | CipherMode::Aes256Gcm => {
            Err("GCM encrypt uses cipher_encrypt_gcm".into())
        }
    }
}

fn cipher_decrypt(instance: CipherInstance) -> Result<Vec<u8>, String> {
    match instance.mode {
        CipherMode::Aes128Cbc | CipherMode::Aes256Cbc => cbc_decrypt(&instance),
        CipherMode::Aes128Ctr | CipherMode::Aes256Ctr => ctr_process(&instance),
        CipherMode::Aes128Gcm | CipherMode::Aes256Gcm => {
            Err("GCM decrypt uses cipher_decrypt_gcm".into())
        }
    }
}

fn cbc_encrypt(c: &CipherInstance) -> Result<Vec<u8>, String> {
    let data = &c.buffer;
    if !c.auto_padding && !data.len().is_multiple_of(16) {
        return Err(format!(
            "data length {} not a multiple of block size 16 (autoPadding is off)",
            data.len()
        ));
    }
    macro_rules! do_cbc_enc {
        ($aes:ty) => {{
            let enc = cbc::Encryptor::<$aes>::new_from_slices(&c.key, &c.iv)
                .map_err(|e| format!("cbc: {e}"))?;
            if c.auto_padding {
                Ok(enc.encrypt_padded_vec_mut::<cipher::block_padding::Pkcs7>(data))
            } else {
                let mut buf = data.to_vec();
                enc.encrypt_padded_mut::<cipher::block_padding::NoPadding>(&mut buf, data.len())
                    .map_err(|e| format!("cbc encrypt: {e}"))?;
                Ok(buf)
            }
        }};
    }
    match c.key.len() {
        16 => do_cbc_enc!(aes::Aes128),
        32 => do_cbc_enc!(aes::Aes256),
        _ => Err("invalid key length for AES-CBC".into()),
    }
}

fn cbc_decrypt(c: &CipherInstance) -> Result<Vec<u8>, String> {
    let data = &c.buffer;
    if !data.len().is_multiple_of(16) {
        return Err(format!(
            "ciphertext length {} not a multiple of block size 16",
            data.len()
        ));
    }
    macro_rules! do_cbc_dec {
        ($aes:ty) => {{
            let dec = cbc::Decryptor::<$aes>::new_from_slices(&c.key, &c.iv)
                .map_err(|e| format!("cbc: {e}"))?;
            if c.auto_padding {
                dec.decrypt_padded_vec_mut::<cipher::block_padding::Pkcs7>(data)
                    .map_err(|_| "cbc decrypt: invalid padding".into())
            } else {
                dec.decrypt_padded_vec_mut::<cipher::block_padding::NoPadding>(data)
                    .map_err(|_| "cbc decrypt: decryption failed".into())
            }
        }};
    }
    match c.key.len() {
        16 => do_cbc_dec!(aes::Aes128),
        32 => do_cbc_dec!(aes::Aes256),
        _ => Err("invalid key length for AES-CBC".into()),
    }
}

fn ctr_process(c: &CipherInstance) -> Result<Vec<u8>, String> {
    let mut buf = c.buffer.clone();
    macro_rules! do_ctr {
        ($aes:ty) => {{
            let mut cipher = ctr::Ctr128BE::<$aes>::new_from_slices(&c.key, &c.iv)
                .map_err(|e| format!("ctr: {e}"))?;
            cipher.apply_keystream(&mut buf);
        }};
    }
    match c.key.len() {
        16 => do_ctr!(aes::Aes128),
        32 => do_ctr!(aes::Aes256),
        _ => return Err("invalid key length for AES-CTR".into()),
    }
    Ok(buf)
}

pub(crate) fn op_crypto_cipher_final_gcm(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let mut instance = {
        let state = scope
            .get_slot_mut::<CryptoState>()
            .expect("crypto state installed");
        match state.ciphers.remove(&id) {
            Some(i) => i,
            None => {
                crate::node_ops::throw_type_error(scope, "cipher: invalid handle");
                return;
            }
        }
    };
    let result = if instance.encrypt {
        gcm_encrypt(&instance)
    } else {
        gcm_decrypt(&instance)
    };
    match result {
        Ok((data, tag)) => {
            if instance.encrypt
                && let Some(tag) = tag
            {
                instance.auth_tag = Some(tag);
                scope
                    .get_slot_mut::<CryptoState>()
                    .expect("crypto state installed")
                    .ciphers
                    .insert(id, instance);
            }
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, data) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

fn gcm_encrypt(c: &CipherInstance) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
    use aes_gcm::aead::Payload;
    let nonce = aes_gcm::Nonce::from_slice(&c.iv);
    let payload = Payload {
        msg: &c.buffer,
        aad: &c.aad,
    };
    macro_rules! do_gcm_enc {
        ($gcm:ty) => {{
            let cipher = <$gcm>::new_from_slice(&c.key).map_err(|e| format!("gcm: {e}"))?;
            let mut ct = cipher
                .encrypt(nonce, payload)
                .map_err(|e| format!("gcm encrypt: {e}"))?;
            let tag = ct.split_off(ct.len() - 16);
            Ok((ct, Some(tag)))
        }};
    }
    match c.key.len() {
        16 => do_gcm_enc!(aes_gcm::Aes128Gcm),
        32 => do_gcm_enc!(aes_gcm::Aes256Gcm),
        _ => Err("invalid key length for AES-GCM".into()),
    }
}

fn gcm_decrypt(c: &CipherInstance) -> Result<(Vec<u8>, Option<Vec<u8>>), String> {
    use aes_gcm::aead::Payload;
    let nonce = aes_gcm::Nonce::from_slice(&c.iv);
    let tag = c
        .auth_tag
        .as_deref()
        .ok_or("gcm decrypt: auth tag required (call setAuthTag before final)")?;
    let mut ciphertext_with_tag = c.buffer.clone();
    ciphertext_with_tag.extend_from_slice(tag);
    let payload = Payload {
        msg: &ciphertext_with_tag,
        aad: &c.aad,
    };
    macro_rules! do_gcm_dec {
        ($gcm:ty) => {{
            let cipher = <$gcm>::new_from_slice(&c.key).map_err(|e| format!("gcm: {e}"))?;
            let pt = cipher
                .decrypt(nonce, payload)
                .map_err(|_| "gcm decrypt: authentication failed".to_string())?;
            Ok((pt, None))
        }};
    }
    match c.key.len() {
        16 => do_gcm_dec!(aes_gcm::Aes128Gcm),
        32 => do_gcm_dec!(aes_gcm::Aes256Gcm),
        _ => Err("invalid key length for AES-GCM".into()),
    }
}

// ===================================================== asymmetric sign/verify
// Wave 3: RSA (PKCS#1 v1.5, PSS) and ECDSA (P-256, P-384) via ring.

use ring::signature as ring_sig;
use ring_sig::KeyPair as _;

// Wave 4: RSA encrypt/decrypt via the `rsa` crate (ring doesn't do encryption).
use rand_core::OsRng;
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey, EncodeRsaPublicKey};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{Oaep, Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};

fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    base64::engine::general_purpose::STANDARD
        .decode(&body)
        .map_err(|e| format!("PEM base64 decode: {e}"))
}

fn pem_label(pem: &str) -> &str {
    for line in pem.lines() {
        if let Some(rest) = line.strip_prefix("-----BEGIN ")
            && let Some(label) = rest.strip_suffix("-----")
        {
            return label.trim();
        }
    }
    ""
}

fn resolve_sign_algorithm(algo: &str) -> Result<&'static dyn ring_sig::RsaEncoding, String> {
    match algo {
        "sha256" | "rsasha256" => Ok(&ring_sig::RSA_PKCS1_SHA256),
        "sha384" | "rsasha384" => Ok(&ring_sig::RSA_PKCS1_SHA384),
        "sha512" | "rsasha512" => Ok(&ring_sig::RSA_PKCS1_SHA512),
        _ => Err(format!("unsupported RSA signing algorithm: '{algo}'")),
    }
}

fn resolve_verify_algorithm(
    algo: &str,
    key_bits: usize,
) -> Result<&'static ring_sig::RsaParameters, String> {
    let _ = key_bits;
    match algo {
        "sha256" | "rsasha256" => Ok(&ring_sig::RSA_PKCS1_2048_8192_SHA256),
        "sha384" | "rsasha384" => Ok(&ring_sig::RSA_PKCS1_2048_8192_SHA384),
        "sha512" | "rsasha512" => Ok(&ring_sig::RSA_PKCS1_2048_8192_SHA512),
        _ => Err(format!("unsupported RSA verify algorithm: '{algo}'")),
    }
}

fn extract_spki_pubkey(der: &[u8]) -> Result<Vec<u8>, String> {
    if der.len() < 4 {
        return Err("SPKI DER too short".into());
    }
    let (_, seq_body) = parse_asn1_element(der)?;
    let (rest, _algo_seq) = parse_asn1_element(seq_body)?;
    let (_, bit_string_body) = parse_asn1_element(rest)?;
    if bit_string_body.is_empty() {
        return Err("empty BIT STRING in SPKI".into());
    }
    Ok(bit_string_body[1..].to_vec())
}

fn parse_asn1_element(data: &[u8]) -> Result<(&[u8], &[u8]), String> {
    if data.len() < 2 {
        return Err("ASN.1: truncated".into());
    }
    let _tag = data[0];
    let (len, header_size) = if data[1] & 0x80 == 0 {
        (data[1] as usize, 2)
    } else {
        let num_bytes = (data[1] & 0x7f) as usize;
        if data.len() < 2 + num_bytes {
            return Err("ASN.1: truncated length".into());
        }
        let mut len: usize = 0;
        for i in 0..num_bytes {
            len = (len << 8) | data[2 + i] as usize;
        }
        (len, 2 + num_bytes)
    };
    if data.len() < header_size + len {
        return Err("ASN.1: truncated body".into());
    }
    let body = &data[header_size..header_size + len];
    let rest = &data[header_size + len..];
    Ok((rest, body))
}

fn crypto_sign_rsa(algo: &str, data: &[u8], key_pem: &str) -> Result<Vec<u8>, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let key_pair = match label {
        "RSA PRIVATE KEY" => ring_sig::RsaKeyPair::from_der(&der),
        "PRIVATE KEY" => ring_sig::RsaKeyPair::from_pkcs8(&der),
        _ => return Err(format!("unsupported PEM label: '{label}'")),
    }
    .map_err(|e| format!("RSA key parse: {e}"))?;
    let encoding = resolve_sign_algorithm(algo)?;
    let rng = ring::rand::SystemRandom::new();
    let mut signature = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(encoding, &rng, data, &mut signature)
        .map_err(|e| format!("RSA sign: {e}"))?;
    Ok(signature)
}

fn crypto_verify_rsa(
    algo: &str,
    data: &[u8],
    key_pem: &str,
    signature: &[u8],
) -> Result<bool, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let params = resolve_verify_algorithm(algo, 0)?;
    let pub_key_bytes = match label {
        "PUBLIC KEY" => extract_spki_pubkey(&der)?,
        "RSA PUBLIC KEY" => der,
        _ => return Err(format!("unsupported public key PEM label: '{label}'")),
    };
    let pub_key = ring_sig::UnparsedPublicKey::new(params, &pub_key_bytes);
    match pub_key.verify(data, signature) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

fn crypto_sign_ec(algo: &str, data: &[u8], key_pem: &str) -> Result<Vec<u8>, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    if label != "PRIVATE KEY" && label != "EC PRIVATE KEY" {
        return Err(format!("unsupported EC PEM label: '{label}'"));
    }
    let signing_alg: &ring_sig::EcdsaSigningAlgorithm = match algo {
        "sha256" | "ecdsasha256" | "p256" => &ring_sig::ECDSA_P256_SHA256_FIXED_SIGNING,
        "sha384" | "ecdsasha384" | "p384" => &ring_sig::ECDSA_P384_SHA384_FIXED_SIGNING,
        _ => return Err(format!("unsupported ECDSA algorithm: '{algo}'")),
    };
    let rng = ring::rand::SystemRandom::new();
    let key_pair = if label == "PRIVATE KEY" {
        ring_sig::EcdsaKeyPair::from_pkcs8(signing_alg, &der, &rng)
    } else {
        return Err("SEC1 EC private key format not supported; use PKCS#8".into());
    }
    .map_err(|e| format!("EC key parse: {e}"))?;
    let sig = key_pair
        .sign(&rng, data)
        .map_err(|e| format!("ECDSA sign: {e}"))?;
    Ok(sig.as_ref().to_vec())
}

fn crypto_verify_ec(
    algo: &str,
    data: &[u8],
    key_pem: &str,
    signature: &[u8],
) -> Result<bool, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let verify_alg: &ring_sig::EcdsaVerificationAlgorithm = match algo {
        "sha256" | "ecdsasha256" | "p256" => &ring_sig::ECDSA_P256_SHA256_FIXED,
        "sha384" | "ecdsasha384" | "p384" => &ring_sig::ECDSA_P384_SHA384_FIXED,
        _ => return Err(format!("unsupported ECDSA verify algorithm: '{algo}'")),
    };
    if label != "PUBLIC KEY" {
        return Err(format!("unsupported EC public key PEM label: '{label}'"));
    }
    let spki_pub = extract_spki_pubkey(&der)?;
    let pub_key = ring_sig::UnparsedPublicKey::new(verify_alg, &spki_pub);
    match pub_key.verify(data, signature) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

fn crypto_sign_ed25519(data: &[u8], key_pem: &str) -> Result<Vec<u8>, String> {
    let der = pem_to_der(key_pem)?;
    // Node/OpenSSL emit PKCS#8 v1 (RFC 5208: version 0, no public-key attribute).
    // ring's from_pkcs8 requires the v2 (RFC 5958) template carrying the public
    // key, so it rejects Node keys with "VersionNotSupported". from_pkcs8_maybe_unchecked
    // accepts BOTH v1 and v2 -> Node cross-compat, while oam's own generate_pkcs8
    // (v2) output still parses.
    let key_pair = ring_sig::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der)
        .map_err(|e| format!("Ed25519 key parse: {e}"))?;
    Ok(key_pair.sign(data).as_ref().to_vec())
}

fn crypto_verify_ed25519(data: &[u8], key_pem: &str, signature: &[u8]) -> Result<bool, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    if label != "PUBLIC KEY" {
        return Err(format!("unsupported Ed25519 public key PEM: '{label}'"));
    }
    let pub_bytes = extract_spki_pubkey(&der)?;
    let pub_key = ring_sig::UnparsedPublicKey::new(&ring_sig::ED25519, &pub_bytes);
    match pub_key.verify(data, signature) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

fn crypto_generate_ed25519() -> Result<(Vec<u8>, Vec<u8>), String> {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8_doc = ring_sig::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|e| format!("Ed25519 keygen: {e}"))?;
    let key_pair = ring_sig::Ed25519KeyPair::from_pkcs8(pkcs8_doc.as_ref())
        .map_err(|e| format!("Ed25519 parse: {e}"))?;
    let priv_pem = der_to_pem(pkcs8_doc.as_ref(), "PRIVATE KEY");
    let pub_der = key_pair.public_key().as_ref();
    let pub_spki = wrap_ed25519_spki(pub_der);
    let pub_pem = der_to_pem(&pub_spki, "PUBLIC KEY");
    Ok((priv_pem.into_bytes(), pub_pem.into_bytes()))
}

fn der_to_pem(der: &[u8], label: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap());
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {label}-----\n"));
    pem
}

fn wrap_ed25519_spki(pub_key: &[u8]) -> Vec<u8> {
    // Ed25519 OID: 1.3.101.112 = 06 03 2b 65 70
    let algo_id: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];
    let bit_string_len = 1 + pub_key.len(); // 1 byte for unused-bits count
    let seq_content_len = algo_id.len() + 2 + bit_string_len; // 2 = tag + length of BIT STRING
    let total_len = 2 + seq_content_len; // 2 = SEQUENCE tag + length

    let mut der = Vec::with_capacity(total_len + 4);
    der.push(0x30); // SEQUENCE
    encode_asn1_length(&mut der, seq_content_len);
    der.extend_from_slice(algo_id);
    der.push(0x03); // BIT STRING
    encode_asn1_length(&mut der, bit_string_len);
    der.push(0x00); // unused bits
    der.extend_from_slice(pub_key);
    der
}

fn encode_asn1_length(buf: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        buf.push(len as u8);
    } else if len < 0x100 {
        buf.push(0x81);
        buf.push(len as u8);
    } else {
        buf.push(0x82);
        buf.push((len >> 8) as u8);
        buf.push((len & 0xff) as u8);
    }
}

pub(crate) fn op_crypto_generate_keypair(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(key_type) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "generateKeyPair: type required");
        return;
    };
    let result = match key_type.as_str() {
        "ed25519" => crypto_generate_ed25519(),
        "rsa" => {
            let bits = args.get(1).number_value(scope).unwrap_or(2048.0) as usize;
            if !(512..=16384).contains(&bits) {
                crate::node_ops::throw_type_error(
                    scope,
                    "generateKeyPairSync: modulusLength must be between 512 and 16384",
                );
                return;
            }
            crypto_generate_rsa(bits)
        }
        "ec" => {
            let curve_raw = crate::node_ops::arg_string(scope, &args, 2).filter(|s| !s.is_empty());
            let Some(curve_raw) = curve_raw else {
                crate::node_ops::throw_type_error(
                    scope,
                    "generateKeyPairSync: namedCurve required for EC keys",
                );
                return;
            };
            match normalize_curve(&curve_raw) {
                "p256" => crypto_generate_ec_p256(),
                "p384" => crypto_generate_ec_p384(),
                _ => {
                    let msg = format!(
                        "generateKeyPairSync: unsupported EC curve '{}' (oam supports P-256, P-384)",
                        curve_raw
                    );
                    crate::node_ops::throw_type_error(scope, &msg);
                    return;
                }
            }
        }
        _ => {
            let msg = format!(
                "generateKeyPairSync: unsupported type '{}' (oam supports rsa, ec, ed25519)",
                key_type
            );
            crate::node_ops::throw_type_error(scope, &msg);
            return;
        }
    };
    match result {
        Ok((priv_pem, pub_pem)) => {
            let obj = v8::Object::new(scope);
            let priv_str = String::from_utf8_lossy(&priv_pem);
            if let Some(pk) = v8::String::new(scope, &priv_str) {
                let key = v8::String::new(scope, "privateKey").unwrap();
                obj.set(scope, key.into(), pk.into());
            }
            let pub_str = String::from_utf8_lossy(&pub_pem);
            if let Some(pk) = v8::String::new(scope, &pub_str) {
                let key = v8::String::new(scope, "publicKey").unwrap();
                obj.set(scope, key.into(), pk.into());
            }
            rv.set(obj.into());
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_sign(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(algo) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "sign: algorithm required");
        return;
    };
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "sign: data required");
        return;
    };
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 2) else {
        crate::node_ops::throw_type_error(scope, "sign: key required");
        return;
    };
    let Some(key_type) = crate::node_ops::arg_string(scope, &args, 3) else {
        crate::node_ops::throw_type_error(scope, "sign: key type required");
        return;
    };
    let normalized = normalize_algorithm(&algo);
    let result = match key_type.as_str() {
        "rsa" => crypto_sign_rsa(&normalized, &data, &key_pem),
        "ec" => crypto_sign_ec(&normalized, &data, &key_pem),
        "ed25519" => crypto_sign_ed25519(&data, &key_pem),
        _ => Err(format!("unsupported key type: '{key_type}'")),
    };
    match result {
        Ok(sig) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, sig) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_verify(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(algo) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "verify: algorithm required");
        return;
    };
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "verify: data required");
        return;
    };
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 2) else {
        crate::node_ops::throw_type_error(scope, "verify: key required");
        return;
    };
    let Some(signature) = crate::node_ops::arg_bytes(scope, &args, 3) else {
        crate::node_ops::throw_type_error(scope, "verify: signature required");
        return;
    };
    let Some(key_type) = crate::node_ops::arg_string(scope, &args, 4) else {
        crate::node_ops::throw_type_error(scope, "verify: key type required");
        return;
    };
    let normalized = normalize_algorithm(&algo);
    let result = match key_type.as_str() {
        "rsa" => crypto_verify_rsa(&normalized, &data, &key_pem, &signature),
        "ec" => crypto_verify_ec(&normalized, &data, &key_pem, &signature),
        "ed25519" => crypto_verify_ed25519(&data, &key_pem, &signature),
        _ => Err(format!("unsupported key type: '{key_type}'")),
    };
    match result {
        Ok(valid) => rv.set_bool(valid),
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

// ===================================================== RSA encrypt/decrypt
// Wave 4: publicEncrypt / privateDecrypt via the `rsa` crate.

fn parse_rsa_public_key(der: &[u8], label: &str) -> Result<RsaPublicKey, String> {
    match label {
        "PUBLIC KEY" => {
            let pkcs1_bytes = extract_spki_pubkey(der)?;
            RsaPublicKey::from_pkcs1_der(&pkcs1_bytes)
                .map_err(|e| format!("RSA public key parse: {e}"))
        }
        "RSA PUBLIC KEY" => {
            RsaPublicKey::from_pkcs1_der(der).map_err(|e| format!("RSA public key parse: {e}"))
        }
        "PRIVATE KEY" => {
            let priv_key =
                RsaPrivateKey::from_pkcs8_der(der).map_err(|e| format!("RSA key parse: {e}"))?;
            Ok(RsaPublicKey::from(&priv_key))
        }
        "RSA PRIVATE KEY" => {
            let priv_key =
                RsaPrivateKey::from_pkcs1_der(der).map_err(|e| format!("RSA key parse: {e}"))?;
            Ok(RsaPublicKey::from(&priv_key))
        }
        _ => Err(format!(
            "unsupported PEM label for RSA public key: '{label}'"
        )),
    }
}

fn parse_rsa_private_key(der: &[u8], label: &str) -> Result<RsaPrivateKey, String> {
    match label {
        "PRIVATE KEY" => {
            RsaPrivateKey::from_pkcs8_der(der).map_err(|e| format!("RSA private key parse: {e}"))
        }
        "RSA PRIVATE KEY" => {
            RsaPrivateKey::from_pkcs1_der(der).map_err(|e| format!("RSA private key parse: {e}"))
        }
        _ => Err(format!(
            "unsupported PEM label for RSA private key: '{label}'"
        )),
    }
}

fn crypto_public_encrypt(
    data: &[u8],
    key_pem: &str,
    padding_type: &str,
    oaep_hash: &str,
) -> Result<Vec<u8>, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let pub_key = parse_rsa_public_key(&der, label)?;
    let mut rng = OsRng;

    match padding_type {
        "oaep" => {
            let padding = match oaep_hash {
                "sha1" => Oaep::new::<sha1::Sha1>(),
                "sha256" => Oaep::new::<sha2::Sha256>(),
                "sha384" => Oaep::new::<sha2::Sha384>(),
                "sha512" => Oaep::new::<sha2::Sha512>(),
                _ => return Err(format!("unsupported OAEP hash: '{oaep_hash}'")),
            };
            pub_key
                .encrypt(&mut rng, padding, data)
                .map_err(|e| format!("RSA OAEP encrypt: {e}"))
        }
        "pkcs1" => pub_key
            .encrypt(&mut rng, Pkcs1v15Encrypt, data)
            .map_err(|e| format!("RSA PKCS1v15 encrypt: {e}")),
        _ => Err(format!("unsupported RSA padding: '{padding_type}'")),
    }
}

fn crypto_private_decrypt(
    data: &[u8],
    key_pem: &str,
    padding_type: &str,
    oaep_hash: &str,
) -> Result<Vec<u8>, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let priv_key = parse_rsa_private_key(&der, label)?;

    match padding_type {
        "oaep" => {
            let padding = match oaep_hash {
                "sha1" => Oaep::new::<sha1::Sha1>(),
                "sha256" => Oaep::new::<sha2::Sha256>(),
                "sha384" => Oaep::new::<sha2::Sha384>(),
                "sha512" => Oaep::new::<sha2::Sha512>(),
                _ => return Err(format!("unsupported OAEP hash: '{oaep_hash}'")),
            };
            priv_key
                .decrypt(padding, data)
                .map_err(|e| format!("RSA OAEP decrypt: {e}"))
        }
        "pkcs1" => priv_key
            .decrypt(Pkcs1v15Encrypt, data)
            .map_err(|e| format!("RSA PKCS1v15 decrypt: {e}")),
        _ => Err(format!("unsupported RSA padding: '{padding_type}'")),
    }
}

fn crypto_private_encrypt(data: &[u8], key_pem: &str) -> Result<Vec<u8>, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let priv_key = parse_rsa_private_key(&der, label)?;
    let n = priv_key.n();
    let d = priv_key.d();
    let key_len = n.bits().div_ceil(8);
    if data.len() + 11 > key_len {
        return Err("privateEncrypt: data too large for key size".into());
    }
    let ps_len = key_len - data.len() - 3;
    let mut em = vec![0u8; key_len];
    em[1] = 0x01;
    for i in 0..ps_len {
        em[2 + i] = 0xFF;
    }
    em[2 + ps_len] = 0x00;
    em[3 + ps_len..].copy_from_slice(data);
    let m = BigUint::from_bytes_be(&em);
    let c = m.modpow(d, n);
    Ok(pad_be(c.to_bytes_be(), key_len))
}

fn crypto_public_decrypt(data: &[u8], key_pem: &str) -> Result<Vec<u8>, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let (n, e) = if label == "PUBLIC KEY" || label == "RSA PUBLIC KEY" {
        let pk = parse_rsa_public_key(&der, label)?;
        (pk.n().clone(), pk.e().clone())
    } else {
        let sk = parse_rsa_private_key(&der, label)?;
        (sk.n().clone(), sk.e().clone())
    };
    let key_len = n.bits().div_ceil(8);
    if data.len() != key_len {
        return Err(format!("publicDecrypt: input must be {} bytes", key_len));
    }
    let c = BigUint::from_bytes_be(data);
    let m = c.modpow(&e, &n);
    let em = pad_be(m.to_bytes_be(), key_len);
    if em.len() < 11 || em[0] != 0x00 || em[1] != 0x01 {
        return Err("publicDecrypt: invalid PKCS#1 v1.5 padding".into());
    }
    let mut sep = None;
    for (i, &byte) in em.iter().enumerate().skip(2) {
        if byte == 0x00 {
            sep = Some(i);
            break;
        }
        if byte != 0xFF {
            return Err("publicDecrypt: invalid PKCS#1 v1.5 padding".into());
        }
    }
    match sep {
        Some(idx) if idx >= 10 => Ok(em[idx + 1..].to_vec()),
        _ => Err("publicDecrypt: invalid PKCS#1 v1.5 padding".into()),
    }
}

pub(crate) fn op_crypto_public_encrypt(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "publicEncrypt: data required");
        return;
    };
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "publicEncrypt: key required");
        return;
    };
    let padding =
        crate::node_ops::arg_string(scope, &args, 2).unwrap_or_else(|| "oaep".to_string());
    let oaep_hash =
        crate::node_ops::arg_string(scope, &args, 3).unwrap_or_else(|| "sha1".to_string());

    match crypto_public_encrypt(&data, &key_pem, &padding, &oaep_hash) {
        Ok(ct) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, ct) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_private_decrypt(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "privateDecrypt: data required");
        return;
    };
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "privateDecrypt: key required");
        return;
    };
    let padding =
        crate::node_ops::arg_string(scope, &args, 2).unwrap_or_else(|| "oaep".to_string());
    let oaep_hash =
        crate::node_ops::arg_string(scope, &args, 3).unwrap_or_else(|| "sha1".to_string());

    match crypto_private_decrypt(&data, &key_pem, &padding, &oaep_hash) {
        Ok(pt) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, pt) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_private_encrypt(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "privateEncrypt: data required");
        return;
    };
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "privateEncrypt: key required");
        return;
    };
    match crypto_private_encrypt(&data, &key_pem) {
        Ok(ct) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, ct) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_public_decrypt(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "publicDecrypt: data required");
        return;
    };
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "publicDecrypt: key required");
        return;
    };
    match crypto_public_decrypt(&data, &key_pem) {
        Ok(pt) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, pt) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_extract_public_pem(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "extractPublicPem: key required");
        return;
    };
    let der = match pem_to_der(&key_pem) {
        Ok(d) => d,
        Err(e) => {
            crate::node_ops::throw_type_error(scope, &e);
            return;
        }
    };
    let label = pem_label(&key_pem);
    let priv_key = match parse_rsa_private_key(&der, label) {
        Ok(k) => k,
        Err(e) => {
            crate::node_ops::throw_type_error(scope, &e);
            return;
        }
    };
    let pub_key = RsaPublicKey::from(&priv_key);
    let pub_pkcs1 = match pub_key.to_pkcs1_der() {
        Ok(d) => d,
        Err(e) => {
            crate::node_ops::throw_type_error(scope, &format!("RSA pub encode: {e}"));
            return;
        }
    };
    let pub_spki = wrap_rsa_spki(pub_pkcs1.as_ref());
    let pem_str = der_to_pem(&pub_spki, "PUBLIC KEY");
    let val = v8::String::new(scope, &pem_str).unwrap();
    rv.set(val.into());
}

pub(crate) fn op_crypto_rsa_jwk_components(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "rsaJwkComponents: key required");
        return;
    };
    let is_private = args.get(1).boolean_value(scope);
    let der = match pem_to_der(&key_pem) {
        Ok(d) => d,
        Err(e) => {
            crate::node_ops::throw_type_error(scope, &e);
            return;
        }
    };
    let label = pem_label(&key_pem);
    let obj = v8::Object::new(scope);

    macro_rules! set_bytes_prop {
        ($name:expr, $bytes:expr) => {{
            let k = v8::String::new(scope, $name).unwrap();
            if let Some(val) = crate::node_ops::bytes_to_uint8array(scope, $bytes) {
                obj.set(scope, k.into(), val);
            }
        }};
    }

    if is_private {
        let priv_key = match parse_rsa_private_key(&der, label) {
            Ok(k) => k,
            Err(e) => {
                crate::node_ops::throw_type_error(scope, &e);
                return;
            }
        };
        set_bytes_prop!("n", priv_key.n().to_bytes_be());
        set_bytes_prop!("e", priv_key.e().to_bytes_be());
        set_bytes_prop!("d", priv_key.d().to_bytes_be());
        let primes = priv_key.primes();
        if primes.len() >= 2 {
            set_bytes_prop!("p", primes[0].to_bytes_be());
            set_bytes_prop!("q", primes[1].to_bytes_be());
            let one = BigUint::from(1u32);
            let dp = priv_key.d() % (&primes[0] - &one);
            let dq = priv_key.d() % (&primes[1] - &one);
            set_bytes_prop!("dp", dp.to_bytes_be());
            set_bytes_prop!("dq", dq.to_bytes_be());
            let qi = primes[1].modpow(&(&primes[0] - BigUint::from(2u32)), &primes[0]);
            set_bytes_prop!("qi", qi.to_bytes_be());
        }
    } else {
        let pub_key = match parse_rsa_public_key(&der, label) {
            Ok(k) => k,
            Err(e) => {
                crate::node_ops::throw_type_error(scope, &e);
                return;
            }
        };
        set_bytes_prop!("n", pub_key.n().to_bytes_be());
        set_bytes_prop!("e", pub_key.e().to_bytes_be());
    }

    rv.set(obj.into());
}

fn crypto_generate_rsa(bits: usize) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut rng = OsRng;
    let priv_key = RsaPrivateKey::new(&mut rng, bits).map_err(|e| format!("RSA keygen: {e}"))?;
    let pub_key = RsaPublicKey::from(&priv_key);

    let priv_doc = priv_key
        .to_pkcs8_der()
        .map_err(|e| format!("RSA private key encode: {e}"))?;
    let pub_pkcs1 = pub_key
        .to_pkcs1_der()
        .map_err(|e| format!("RSA public key encode: {e}"))?;
    let pub_spki = wrap_rsa_spki(pub_pkcs1.as_ref());

    let priv_pem = der_to_pem(priv_doc.as_bytes(), "PRIVATE KEY");
    let pub_pem = der_to_pem(&pub_spki, "PUBLIC KEY");

    Ok((priv_pem.into_bytes(), pub_pem.into_bytes()))
}

fn wrap_rsa_spki(pkcs1_pub: &[u8]) -> Vec<u8> {
    // RSA OID: 1.2.840.113549.1.1.1 + NULL params
    let algo_id: &[u8] = &[
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
    ];
    let bit_string_len = 1 + pkcs1_pub.len();
    let seq_content_len = algo_id.len() + 1 + asn1_length_size(bit_string_len) + bit_string_len;

    let mut der = Vec::with_capacity(1 + asn1_length_size(seq_content_len) + seq_content_len);
    der.push(0x30); // SEQUENCE
    encode_asn1_length(&mut der, seq_content_len);
    der.extend_from_slice(algo_id);
    der.push(0x03); // BIT STRING
    encode_asn1_length(&mut der, bit_string_len);
    der.push(0x00); // unused bits
    der.extend_from_slice(pkcs1_pub);
    der
}

fn asn1_length_size(len: usize) -> usize {
    if len < 0x80 {
        1
    } else if len < 0x100 {
        2
    } else {
        3
    }
}

// ===================================================== ECDH key agreement
// Wave 5: createECDH — P-256, P-384 via the p256/p384 RustCrypto crates.

macro_rules! impl_ecdh {
    ($mod:ident, $gen:ident, $compute:ident, $pub_from_priv:ident) => {
        fn $gen() -> Result<(Vec<u8>, Vec<u8>), String> {
            use $mod::elliptic_curve::sec1::ToEncodedPoint;
            let sk = $mod::SecretKey::random(&mut OsRng);
            let pk = sk.public_key();
            let priv_bytes = sk.to_bytes().to_vec();
            let pub_bytes = pk.to_encoded_point(false).as_bytes().to_vec();
            Ok((pub_bytes, priv_bytes))
        }

        fn $compute(my_private: &[u8], their_public: &[u8]) -> Result<Vec<u8>, String> {
            let sk = $mod::SecretKey::from_slice(my_private)
                .map_err(|e| format!("ECDH private key: {e}"))?;
            let pk = $mod::PublicKey::from_sec1_bytes(their_public)
                .map_err(|e| format!("ECDH public key: {e}"))?;
            let shared = $mod::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
            Ok(shared.raw_secret_bytes().to_vec())
        }

        fn $pub_from_priv(my_private: &[u8]) -> Result<Vec<u8>, String> {
            use $mod::elliptic_curve::sec1::ToEncodedPoint;
            let sk = $mod::SecretKey::from_slice(my_private)
                .map_err(|e| format!("ECDH private key: {e}"))?;
            let pk = sk.public_key();
            Ok(pk.to_encoded_point(false).as_bytes().to_vec())
        }
    };
}

impl_ecdh!(p256, ecdh_gen_p256, ecdh_compute_p256, ecdh_pub_p256);
impl_ecdh!(p384, ecdh_gen_p384, ecdh_compute_p384, ecdh_pub_p384);

macro_rules! impl_ec_keygen {
    ($mod:ident, $fn_name:ident) => {
        fn $fn_name() -> Result<(Vec<u8>, Vec<u8>), String> {
            use $mod::elliptic_curve::pkcs8::EncodePrivateKey;
            use $mod::elliptic_curve::pkcs8::spki::EncodePublicKey;
            let sk = $mod::SecretKey::random(&mut OsRng);
            let pk = sk.public_key();
            let priv_doc = sk
                .to_pkcs8_der()
                .map_err(|e| format!("EC keygen private: {e}"))?;
            let priv_pem = der_to_pem(priv_doc.as_bytes(), "PRIVATE KEY");
            let pub_doc = pk
                .to_public_key_der()
                .map_err(|e| format!("EC keygen public: {e}"))?;
            let pub_pem = der_to_pem(pub_doc.as_ref(), "PUBLIC KEY");
            Ok((priv_pem.into_bytes(), pub_pem.into_bytes()))
        }
    };
}

impl_ec_keygen!(p256, crypto_generate_ec_p256);
impl_ec_keygen!(p384, crypto_generate_ec_p384);

// ===================================================== EC JWK import/export
// Wave 9: import EC private/public keys from JWK components (crv, x, y, d)
// and export EC keys to JWK components.

/// Import an EC private key from JWK components and return a PKCS#8 PEM.
fn ec_jwk_to_pkcs8_pem(crv: &str, x: &[u8], y: &[u8], d: &[u8]) -> Result<String, String> {
    match crv {
        "P-256" => {
            use p256::elliptic_curve::pkcs8::EncodePrivateKey;
            let sk = p256::SecretKey::from_slice(d)
                .map_err(|e| format!("EC P-256 JWK import private: {e}"))?;
            {
                use p256::elliptic_curve::sec1::ToEncodedPoint;
                let pk = sk.public_key();
                let pt = pk.to_encoded_point(false);
                if pt.x().map(|v| v.as_slice()) != Some(x)
                    || pt.y().map(|v| v.as_slice()) != Some(y)
                {
                    return Err("EC P-256 JWK: x/y do not match private key d".into());
                }
            }
            let doc = sk
                .to_pkcs8_der()
                .map_err(|e| format!("EC P-256 PKCS#8 encode: {e}"))?;
            Ok(der_to_pem(doc.as_bytes(), "PRIVATE KEY"))
        }
        "P-384" => {
            use p384::elliptic_curve::pkcs8::EncodePrivateKey;
            let sk = p384::SecretKey::from_slice(d)
                .map_err(|e| format!("EC P-384 JWK import private: {e}"))?;
            {
                use p384::elliptic_curve::sec1::ToEncodedPoint;
                let pk = sk.public_key();
                let pt = pk.to_encoded_point(false);
                if pt.x().map(|v| v.as_slice()) != Some(x)
                    || pt.y().map(|v| v.as_slice()) != Some(y)
                {
                    return Err("EC P-384 JWK: x/y do not match private key d".into());
                }
            }
            let doc = sk
                .to_pkcs8_der()
                .map_err(|e| format!("EC P-384 PKCS#8 encode: {e}"))?;
            Ok(der_to_pem(doc.as_bytes(), "PRIVATE KEY"))
        }
        _ => Err(format!("unsupported EC curve for JWK import: '{crv}'")),
    }
}

/// Import an EC public key from JWK components and return an SPKI PEM.
fn ec_jwk_to_spki_pem(crv: &str, x: &[u8], y: &[u8]) -> Result<String, String> {
    match crv {
        "P-256" => {
            use p256::elliptic_curve::pkcs8::spki::EncodePublicKey;
            let mut uncompressed = Vec::with_capacity(1 + x.len() + y.len());
            uncompressed.push(0x04);
            uncompressed.extend_from_slice(x);
            uncompressed.extend_from_slice(y);
            let pk = p256::PublicKey::from_sec1_bytes(&uncompressed)
                .map_err(|e| format!("EC P-256 JWK import public: {e}"))?;
            let doc = pk
                .to_public_key_der()
                .map_err(|e| format!("EC P-256 SPKI encode: {e}"))?;
            Ok(der_to_pem(doc.as_ref(), "PUBLIC KEY"))
        }
        "P-384" => {
            use p384::elliptic_curve::pkcs8::spki::EncodePublicKey;
            let mut uncompressed = Vec::with_capacity(1 + x.len() + y.len());
            uncompressed.push(0x04);
            uncompressed.extend_from_slice(x);
            uncompressed.extend_from_slice(y);
            let pk = p384::PublicKey::from_sec1_bytes(&uncompressed)
                .map_err(|e| format!("EC P-384 JWK import public: {e}"))?;
            let doc = pk
                .to_public_key_der()
                .map_err(|e| format!("EC P-384 SPKI encode: {e}"))?;
            Ok(der_to_pem(doc.as_ref(), "PUBLIC KEY"))
        }
        _ => Err(format!("unsupported EC curve for JWK import: '{crv}'")),
    }
}

struct EcJwkComponents {
    crv: String,
    x: Vec<u8>,
    y: Vec<u8>,
    d: Option<Vec<u8>>,
}

/// Export EC key PEM to JWK components {crv, x, y, d?}.
fn ec_pem_to_jwk_components(pem: &str, is_private: bool) -> Result<EcJwkComponents, String> {
    let der = pem_to_der(pem)?;
    let label = pem_label(pem);

    if is_private {
        if let Ok(sk) = p256::SecretKey::from_pkcs8_der(&der) {
            use p256::elliptic_curve::sec1::ToEncodedPoint;
            let pk = sk.public_key();
            let pt = pk.to_encoded_point(false);
            return Ok(EcJwkComponents {
                crv: "P-256".into(),
                x: pt.x().map(|v| v.as_slice().to_vec()).unwrap_or_default(),
                y: pt.y().map(|v| v.as_slice().to_vec()).unwrap_or_default(),
                d: Some(sk.to_bytes().to_vec()),
            });
        }
        if let Ok(sk) = p384::SecretKey::from_pkcs8_der(&der) {
            use p384::elliptic_curve::sec1::ToEncodedPoint;
            let pk = sk.public_key();
            let pt = pk.to_encoded_point(false);
            return Ok(EcJwkComponents {
                crv: "P-384".into(),
                x: pt.x().map(|v| v.as_slice().to_vec()).unwrap_or_default(),
                y: pt.y().map(|v| v.as_slice().to_vec()).unwrap_or_default(),
                d: Some(sk.to_bytes().to_vec()),
            });
        }
        Err(format!(
            "EC private key parse failed (tried P-256, P-384), label='{label}'"
        ))
    } else {
        let spki_pub = extract_spki_pubkey(&der)?;
        if let Ok(pk) = p256::PublicKey::from_sec1_bytes(&spki_pub) {
            use p256::elliptic_curve::sec1::ToEncodedPoint;
            let pt = pk.to_encoded_point(false);
            return Ok(EcJwkComponents {
                crv: "P-256".into(),
                x: pt.x().map(|v| v.as_slice().to_vec()).unwrap_or_default(),
                y: pt.y().map(|v| v.as_slice().to_vec()).unwrap_or_default(),
                d: None,
            });
        }
        if let Ok(pk) = p384::PublicKey::from_sec1_bytes(&spki_pub) {
            use p384::elliptic_curve::sec1::ToEncodedPoint;
            let pt = pk.to_encoded_point(false);
            return Ok(EcJwkComponents {
                crv: "P-384".into(),
                x: pt.x().map(|v| v.as_slice().to_vec()).unwrap_or_default(),
                y: pt.y().map(|v| v.as_slice().to_vec()).unwrap_or_default(),
                d: None,
            });
        }
        Err(format!(
            "EC public key parse failed (tried P-256, P-384), label='{label}'"
        ))
    }
}

pub(crate) fn op_crypto_ec_jwk_import(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(crv) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "ecJwkImport: crv required");
        return;
    };
    let Some(x) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "ecJwkImport: x required");
        return;
    };
    let Some(y) = crate::node_ops::arg_bytes(scope, &args, 2) else {
        crate::node_ops::throw_type_error(scope, "ecJwkImport: y required");
        return;
    };
    let d = crate::node_ops::arg_bytes(scope, &args, 3);
    let is_private = d.is_some();

    let result = if is_private {
        ec_jwk_to_pkcs8_pem(&crv, &x, &y, d.as_ref().unwrap())
    } else {
        ec_jwk_to_spki_pem(&crv, &x, &y)
    };

    match result {
        Ok(pem) => {
            let val = v8::String::new(scope, &pem).unwrap();
            rv.set(val.into());
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_ec_jwk_export(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "ecJwkExport: key required");
        return;
    };
    let is_private = args.get(1).boolean_value(scope);

    match ec_pem_to_jwk_components(&key_pem, is_private) {
        Ok(comps) => {
            let obj = v8::Object::new(scope);
            macro_rules! set_str {
                ($name:expr, $val:expr) => {{
                    let k = v8::String::new(scope, $name).unwrap();
                    let v = v8::String::new(scope, $val).unwrap();
                    obj.set(scope, k.into(), v.into());
                }};
            }
            macro_rules! set_bytes {
                ($name:expr, $val:expr) => {{
                    let k = v8::String::new(scope, $name).unwrap();
                    if let Some(arr) = crate::node_ops::bytes_to_uint8array(scope, $val) {
                        obj.set(scope, k.into(), arr);
                    }
                }};
            }
            set_str!("crv", &comps.crv);
            set_bytes!("x", comps.x);
            set_bytes!("y", comps.y);
            if let Some(d) = comps.d {
                set_bytes!("d", d);
            }
            rv.set(obj.into());
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

// ===================================================== RSA-PSS sign/verify
// Wave 9: RSA-PSS via the `rsa` crate (ring doesn't support custom salt lengths).

fn crypto_sign_rsa_pss(
    algo: &str,
    data: &[u8],
    key_pem: &str,
    salt_length: usize,
) -> Result<Vec<u8>, String> {
    use rsa::pss::BlindedSigningKey;
    use rsa::signature::RandomizedSigner;

    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let priv_key = parse_rsa_private_key(&der, label)?;
    let mut rng = OsRng;

    match algo {
        "sha256" => {
            let signing_key =
                BlindedSigningKey::<sha2::Sha256>::new_with_salt_len(priv_key, salt_length);
            Ok(signing_key
                .sign_with_rng(&mut rng, data)
                .to_bytes()
                .to_vec())
        }
        "sha384" => {
            let signing_key =
                BlindedSigningKey::<sha2::Sha384>::new_with_salt_len(priv_key, salt_length);
            Ok(signing_key
                .sign_with_rng(&mut rng, data)
                .to_bytes()
                .to_vec())
        }
        "sha512" => {
            let signing_key =
                BlindedSigningKey::<sha2::Sha512>::new_with_salt_len(priv_key, salt_length);
            Ok(signing_key
                .sign_with_rng(&mut rng, data)
                .to_bytes()
                .to_vec())
        }
        "sha1" => {
            let signing_key =
                BlindedSigningKey::<sha1::Sha1>::new_with_salt_len(priv_key, salt_length);
            Ok(signing_key
                .sign_with_rng(&mut rng, data)
                .to_bytes()
                .to_vec())
        }
        _ => Err(format!("unsupported RSA-PSS hash: '{algo}'")),
    }
}

fn crypto_verify_rsa_pss(
    algo: &str,
    data: &[u8],
    key_pem: &str,
    signature: &[u8],
    salt_length: usize,
) -> Result<bool, String> {
    use rsa::pss::VerifyingKey;
    use rsa::signature::Verifier;

    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);

    let pub_key = if label == "PRIVATE KEY" || label == "RSA PRIVATE KEY" {
        let priv_key = parse_rsa_private_key(&der, label)?;
        RsaPublicKey::from(&priv_key)
    } else {
        parse_rsa_public_key(&der, label)?
    };

    let sig = rsa::pss::Signature::try_from(signature)
        .map_err(|e| format!("RSA-PSS signature parse: {e}"))?;

    match algo {
        "sha256" => {
            let vk = VerifyingKey::<sha2::Sha256>::new_with_salt_len(pub_key, salt_length);
            Ok(vk.verify(data, &sig).is_ok())
        }
        "sha384" => {
            let vk = VerifyingKey::<sha2::Sha384>::new_with_salt_len(pub_key, salt_length);
            Ok(vk.verify(data, &sig).is_ok())
        }
        "sha512" => {
            let vk = VerifyingKey::<sha2::Sha512>::new_with_salt_len(pub_key, salt_length);
            Ok(vk.verify(data, &sig).is_ok())
        }
        "sha1" => {
            let vk = VerifyingKey::<sha1::Sha1>::new_with_salt_len(pub_key, salt_length);
            Ok(vk.verify(data, &sig).is_ok())
        }
        _ => Err(format!("unsupported RSA-PSS verify hash: '{algo}'")),
    }
}

pub(crate) fn op_crypto_sign_pss(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(algo) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "signPss: algorithm required");
        return;
    };
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "signPss: data required");
        return;
    };
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 2) else {
        crate::node_ops::throw_type_error(scope, "signPss: key required");
        return;
    };
    let salt_length = args.get(3).number_value(scope).unwrap_or(32.0) as usize;
    let normalized = normalize_algorithm(&algo);

    match crypto_sign_rsa_pss(&normalized, &data, &key_pem, salt_length) {
        Ok(sig) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, sig) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_verify_pss(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(algo) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "verifyPss: algorithm required");
        return;
    };
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "verifyPss: data required");
        return;
    };
    let Some(key_pem) = crate::node_ops::arg_string(scope, &args, 2) else {
        crate::node_ops::throw_type_error(scope, "verifyPss: key required");
        return;
    };
    let Some(signature) = crate::node_ops::arg_bytes(scope, &args, 3) else {
        crate::node_ops::throw_type_error(scope, "verifyPss: signature required");
        return;
    };
    let salt_length = args.get(4).number_value(scope).unwrap_or(32.0) as usize;
    let normalized = normalize_algorithm(&algo);

    match crypto_verify_rsa_pss(&normalized, &data, &key_pem, &signature, salt_length) {
        Ok(valid) => rv.set_bool(valid),
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

fn normalize_curve(name: &str) -> &str {
    match name {
        "prime256v1" | "secp256r1" | "P-256" | "p256" => "p256",
        "secp384r1" | "P-384" | "p384" => "p384",
        _ => name,
    }
}

pub(crate) fn op_crypto_ecdh_generate_keys(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(curve) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "ECDH generateKeys: curve required");
        return;
    };
    let result = match normalize_curve(&curve) {
        "p256" => ecdh_gen_p256(),
        "p384" => ecdh_gen_p384(),
        _ => {
            crate::node_ops::throw_type_error(
                scope,
                &format!("ECDH: unsupported curve '{curve}' (oam supports prime256v1, secp384r1)"),
            );
            return;
        }
    };
    match result {
        Ok((pub_bytes, priv_bytes)) => {
            let obj = v8::Object::new(scope);
            if let Some(pub_arr) = crate::node_ops::bytes_to_uint8array(scope, pub_bytes) {
                let key = v8::String::new(scope, "publicKey").unwrap();
                obj.set(scope, key.into(), pub_arr);
            }
            if let Some(priv_arr) = crate::node_ops::bytes_to_uint8array(scope, priv_bytes) {
                let key = v8::String::new(scope, "privateKey").unwrap();
                obj.set(scope, key.into(), priv_arr);
            }
            rv.set(obj.into());
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_ecdh_compute_secret(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(curve) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "ECDH computeSecret: curve required");
        return;
    };
    let Some(my_private) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "ECDH computeSecret: private key required");
        return;
    };
    let Some(their_public) = crate::node_ops::arg_bytes(scope, &args, 2) else {
        crate::node_ops::throw_type_error(scope, "ECDH computeSecret: other public key required");
        return;
    };
    let result = match normalize_curve(&curve) {
        "p256" => ecdh_compute_p256(&my_private, &their_public),
        "p384" => ecdh_compute_p384(&my_private, &their_public),
        _ => Err(format!("ECDH: unsupported curve '{curve}'")),
    };
    match result {
        Ok(secret) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, secret) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_ecdh_get_public_key(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(curve) = crate::node_ops::arg_string(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "ECDH getPublicKey: curve required");
        return;
    };
    let Some(my_private) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "ECDH getPublicKey: private key required");
        return;
    };
    let result = match normalize_curve(&curve) {
        "p256" => ecdh_pub_p256(&my_private),
        "p384" => ecdh_pub_p384(&my_private),
        _ => Err(format!("ECDH: unsupported curve '{curve}'")),
    };
    match result {
        Ok(pub_bytes) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, pub_bytes) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

// ── Wave 6: classic Diffie-Hellman ──────────────────────────────────

use num_bigint_dig::BigUint;

fn pad_be(bytes: Vec<u8>, target_len: usize) -> Vec<u8> {
    if bytes.len() >= target_len {
        return bytes;
    }
    let mut padded = vec![0u8; target_len - bytes.len()];
    padded.extend_from_slice(&bytes);
    padded
}

fn crypto_dh_generate_keys(prime: &[u8], generator: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    use rand_core::RngCore;
    let p = BigUint::from_bytes_be(prime);
    let g = BigUint::from_bytes_be(generator);
    if p <= BigUint::from(2u32) {
        return Err("DH prime too small".into());
    }
    let p_minus_2 = &p - BigUint::from(2u32);
    let mut rand_bytes = vec![0u8; prime.len()];
    OsRng.fill_bytes(&mut rand_bytes);
    let priv_val = BigUint::from_bytes_be(&rand_bytes) % &p_minus_2 + BigUint::from(2u32);
    let pub_val = g.modpow(&priv_val, &p);
    let p_len = prime.len();
    Ok((
        pad_be(pub_val.to_bytes_be(), p_len),
        pad_be(priv_val.to_bytes_be(), p_len),
    ))
}

fn crypto_dh_compute_secret(
    prime: &[u8],
    private_key: &[u8],
    other_public_key: &[u8],
) -> Result<Vec<u8>, String> {
    let p = BigUint::from_bytes_be(prime);
    let priv_val = BigUint::from_bytes_be(private_key);
    let other_pub = BigUint::from_bytes_be(other_public_key);
    if other_pub >= p || other_pub <= BigUint::from(1u32) {
        return Err("DH: invalid peer public key".into());
    }
    let secret = other_pub.modpow(&priv_val, &p);
    Ok(pad_be(secret.to_bytes_be(), prime.len()))
}

pub(crate) fn op_crypto_dh_generate_keys(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(prime) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "DH generateKeys: prime required");
        return;
    };
    let Some(generator) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "DH generateKeys: generator required");
        return;
    };
    match crypto_dh_generate_keys(&prime, &generator) {
        Ok((pub_bytes, priv_bytes)) => {
            let obj = v8::Object::new(scope);
            if let Some(pub_arr) = crate::node_ops::bytes_to_uint8array(scope, pub_bytes) {
                let key = v8::String::new(scope, "publicKey").unwrap();
                obj.set(scope, key.into(), pub_arr);
            }
            if let Some(priv_arr) = crate::node_ops::bytes_to_uint8array(scope, priv_bytes) {
                let key = v8::String::new(scope, "privateKey").unwrap();
                obj.set(scope, key.into(), priv_arr);
            }
            rv.set(obj.into());
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

pub(crate) fn op_crypto_dh_compute_secret(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(prime) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "DH computeSecret: prime required");
        return;
    };
    let Some(private_key) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "DH computeSecret: private key required");
        return;
    };
    let Some(other_pub) = crate::node_ops::arg_bytes(scope, &args, 2) else {
        crate::node_ops::throw_type_error(scope, "DH computeSecret: peer public key required");
        return;
    };
    match crypto_dh_compute_secret(&prime, &private_key, &other_pub) {
        Ok(secret) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, secret) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

// ===================================================== X.509 certificate parsing

fn format_colon_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// OpenSSL's short name for an X.500 attribute type -- the keys of Node's
/// `subject` / `issuer`, in the newline-joined string and the legacy object
/// alike. An unknown type prints as its dotted OID, as OBJ_obj2txt does.
fn x509_attr_short_name(oid: &str) -> Option<&'static str> {
    Some(match oid {
        "2.5.4.3" => "CN",
        "2.5.4.4" => "SN",
        "2.5.4.5" => "serialNumber",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "ST",
        "2.5.4.9" => "street",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        "2.5.4.12" => "title",
        "2.5.4.13" => "description",
        "2.5.4.15" => "businessCategory",
        "2.5.4.17" => "postalCode",
        "2.5.4.41" => "name",
        "2.5.4.42" => "GN",
        "2.5.4.43" => "initials",
        "2.5.4.44" => "generationQualifier",
        "2.5.4.46" => "dnQualifier",
        "2.5.4.65" => "pseudonym",
        "2.5.4.97" => "organizationIdentifier",
        "0.9.2342.19200300.100.1.1" => "UID",
        "0.9.2342.19200300.100.1.25" => "DC",
        "1.2.840.113549.1.9.1" => "emailAddress",
        "1.3.6.1.4.1.311.60.2.1.1" => "jurisdictionL",
        "1.3.6.1.4.1.311.60.2.1.2" => "jurisdictionST",
        "1.3.6.1.4.1.311.60.2.1.3" => "jurisdictionC",
        _ => return None,
    })
}

/// A name's (type, value) pairs in certificate order.
fn x509_name_entries(name: &x509_parser::x509::X509Name<'_>) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for rdn in name.iter() {
        for attr in rdn.iter() {
            let oid = attr.attr_type().to_id_string();
            let label = x509_attr_short_name(&oid).map_or(oid, str::to_string);
            let value = attr.as_str().unwrap_or("(invalid)").to_string();
            entries.push((label, value));
        }
    }
    entries
}

/// Node's `X509Certificate#subject` string: one `type=value` per line.
fn format_x509_name(entries: &[(String, String)]) -> String {
    entries
        .iter()
        .map(|(label, value)| format!("{label}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// BN_bn2hex: uppercase hex with the leading zero BYTES dropped -- a leading
/// zero nibble stays, so a serial of 0x0abc prints "0ABC", as Node prints it
/// (probed on v22.22.2) -- and "0" for zero.
fn bn_hex(bytes: &[u8]) -> String {
    match bytes.iter().position(|b| *b != 0) {
        None => "0".to_string(),
        Some(i) => bytes[i..].iter().map(|b| format!("{b:02X}")).collect(),
    }
}

/// BN_print, which Node's `exponent` goes through (unlike the serial and the
/// modulus, which go through BN_bn2hex): leading zero NIBBLES dropped too, so
/// 65537 prints "10001" (probed: exponent "0x10001").
fn bn_print_hex(bytes: &[u8]) -> String {
    let hex = bn_hex(bytes);
    let trimmed = hex.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A DER OBJECT IDENTIFIER's content octets as the dotted string OBJ_obj2txt
/// prints: the first sub-identifier folds the first two arcs.
fn oid_to_string(content: &[u8]) -> String {
    let mut arcs: Vec<u128> = Vec::new();
    let mut acc: u128 = 0;
    let mut first = true;
    for b in content {
        acc = (acc << 7) | u128::from(b & 0x7f);
        if b & 0x80 != 0 {
            continue;
        }
        if first {
            first = false;
            if acc < 80 {
                arcs.push(acc / 40);
                arcs.push(acc % 40);
            } else {
                arcs.push(2);
                arcs.push(acc - 80);
            }
        } else {
            arcs.push(acc);
        }
        acc = 0;
    }
    arcs.iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// OpenSSL's ASN1_TIME_print of a UTC instant -- "Jun 15 12:30:07 2026 GMT",
/// the day space-padded ("Sep  5") -- which is Node's `validFrom` /
/// `valid_from` (probed on v22.22.2).
fn asn1_time_openssl(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days over the proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{} {day:>2} {hour:02}:{minute:02}:{second:02} {year} GMT",
        MONTHS[(month - 1) as usize]
    )
}

/// Node's spelling of a GeneralName in subjectAltName and infoAccess
/// (probed): "DNS:", "IP Address:" -- a dotted quad, or IPv6 as uppercase
/// hex groups with no zero compression ("0:0:0:0:0:0:0:1") -- "email:",
/// "URI:". "Registered ID:" and "othername:<unsupported>" follow Node's
/// printer unprobed; the rest keep the parser's Debug form.
fn general_name_string(gn: &x509_parser::extensions::GeneralName<'_>) -> String {
    use x509_parser::extensions::GeneralName;
    match gn {
        GeneralName::DNSName(s) => format!("DNS:{s}"),
        GeneralName::IPAddress(b) if b.len() == 4 => {
            format!("IP Address:{}.{}.{}.{}", b[0], b[1], b[2], b[3])
        }
        GeneralName::IPAddress(b) if b.len() == 16 => {
            let groups: Vec<String> = b
                .chunks(2)
                .map(|pair| format!("{:X}", u16::from_be_bytes([pair[0], pair[1]])))
                .collect();
            format!("IP Address:{}", groups.join(":"))
        }
        GeneralName::RFC822Name(s) => format!("email:{s}"),
        GeneralName::URI(s) => format!("URI:{s}"),
        GeneralName::RegisteredID(oid) => format!("Registered ID:{}", oid.to_id_string()),
        GeneralName::OtherName(..) => "othername:<unsupported>".to_string(),
        other => format!("{other:?}"),
    }
}

/// The extended-key-usage OIDs in certificate order -- Node's `keyUsage` and
/// the legacy `ext_key_usage`. x509-parser's parsed form keeps flags, not
/// order, so the SEQUENCE OF OBJECT IDENTIFIER is read by hand.
fn ext_key_usage_oids(value: &[u8]) -> Vec<String> {
    fn tlv(bytes: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let tag = *bytes.first()?;
        let first_len = *bytes.get(1)?;
        let (len, header) = if first_len & 0x80 == 0 {
            (usize::from(first_len), 2)
        } else {
            let n = usize::from(first_len & 0x7f);
            if n == 0 || n > 4 || bytes.len() < 2 + n {
                return None;
            }
            let len = bytes[2..2 + n]
                .iter()
                .fold(0usize, |len, b| (len << 8) | usize::from(*b));
            (len, 2 + n)
        };
        let end = header.checked_add(len)?;
        if bytes.len() < end {
            return None;
        }
        Some((tag, &bytes[header..end], &bytes[end..]))
    }
    let mut oids = Vec::new();
    let Some((0x30, mut body, _)) = tlv(value) else {
        return oids;
    };
    while let Some((tag, content, rest)) = tlv(body) {
        if tag == 0x06 {
            oids.push(oid_to_string(content));
        }
        body = rest;
    }
    oids
}

/// Node's legacy key fields (X509ToObject): RSA gets modulus/bits/exponent
/// and pubkey (the SubjectPublicKeyInfo DER); EC gets bits, pubkey (the raw
/// point), asn1Curve and nistCurve (absent for a curve NIST did not name);
/// other key types get none.
enum X509KeyFields {
    Rsa {
        modulus: String,
        exponent: String,
        bits: u32,
        spki: Vec<u8>,
    },
    Ec {
        bits: u32,
        point: Vec<u8>,
        asn1_curve: &'static str,
        nist_curve: Option<&'static str>,
    },
    None,
}

fn x509_key_fields(spki: &x509_parser::x509::SubjectPublicKeyInfo<'_>) -> X509KeyFields {
    use x509_parser::public_key::PublicKey;
    match spki.parsed() {
        Ok(PublicKey::RSA(rsa)) => {
            let skip = rsa
                .modulus
                .iter()
                .position(|b| *b != 0)
                .unwrap_or(rsa.modulus.len());
            let significant = &rsa.modulus[skip..];
            let bits =
                significant.len() as u32 * 8 - significant.first().map_or(0, |b| b.leading_zeros());
            X509KeyFields::Rsa {
                modulus: bn_hex(rsa.modulus),
                exponent: format!("0x{}", bn_print_hex(rsa.exponent)),
                bits,
                spki: spki.raw.to_vec(),
            }
        }
        Ok(PublicKey::EC(point)) => {
            let curve = spki
                .algorithm
                .parameters
                .as_ref()
                .map(|p| oid_to_string(p.data));
            let (asn1_curve, nist_curve, bits) = match curve.as_deref() {
                Some("1.2.840.10045.3.1.7") => ("prime256v1", Some("P-256"), 256),
                Some("1.3.132.0.34") => ("secp384r1", Some("P-384"), 384),
                Some("1.3.132.0.35") => ("secp521r1", Some("P-521"), 521),
                Some("1.3.132.0.10") => ("secp256k1", None, 256),
                _ => return X509KeyFields::None,
            };
            X509KeyFields::Ec {
                bits,
                point: point.data().to_vec(),
                asn1_curve,
                nist_curve,
            }
        }
        _ => X509KeyFields::None,
    }
}

pub(crate) fn op_crypto_x509_parse(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(input) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "X509Certificate: certificate data required");
        return;
    };

    let der_bytes: Vec<u8> = if input.starts_with(b"-----BEGIN") {
        match x509_parser::pem::parse_x509_pem(&input) {
            Ok((_, pem)) => pem.contents,
            Err(e) => {
                crate::node_ops::throw_type_error(scope, &format!("X509 PEM parse error: {e}"));
                return;
            }
        }
    } else {
        input
    };

    let der_for_raw = der_bytes.clone();

    let cert = match x509_parser::parse_x509_certificate(&der_bytes) {
        Ok((_, cert)) => cert,
        Err(e) => {
            crate::node_ops::throw_type_error(scope, &format!("X509 parse error: {e}"));
            return;
        }
    };

    let obj = v8::Object::new(scope);

    macro_rules! set_str {
        ($name:expr, $val:expr) => {{
            let k = v8::String::new(scope, $name).unwrap();
            let v = v8::String::new(scope, $val).unwrap();
            obj.set(scope, k.into(), v.into());
        }};
    }

    macro_rules! set_bool {
        ($name:expr, $val:expr) => {{
            let k = v8::String::new(scope, $name).unwrap();
            obj.set(scope, k.into(), v8::Boolean::new(scope, $val).into());
        }};
    }
    macro_rules! set_str_array {
        ($name:expr, $items:expr) => {{
            let items = $items;
            let arr = v8::Array::new(scope, items.len() as i32);
            for (i, s) in items.iter().enumerate() {
                let val = v8::String::new(scope, s).unwrap();
                arr.set_index(scope, i as u32, val.into());
            }
            let k = v8::String::new(scope, $name).unwrap();
            obj.set(scope, k.into(), arr.into());
        }};
    }
    macro_rules! set_bytes {
        ($name:expr, $bytes:expr) => {{
            if let Some(val) = crate::node_ops::bytes_to_uint8array(scope, $bytes) {
                let k = v8::String::new(scope, $name).unwrap();
                obj.set(scope, k.into(), val);
            }
        }};
    }

    let subject_entries = x509_name_entries(cert.subject());
    let issuer_entries = x509_name_entries(cert.issuer());
    set_str!("subject", &format_x509_name(&subject_entries));
    set_str!("issuer", &format_x509_name(&issuer_entries));
    // The (type, value) pairs the legacy object's null-prototype subject and
    // issuer are built from on the JS side (a repeated type becomes an array).
    for (name, entries) in [
        ("subjectEntries", &subject_entries),
        ("issuerEntries", &issuer_entries),
    ] {
        let arr = v8::Array::new(scope, entries.len() as i32);
        for (i, (label, value)) in entries.iter().enumerate() {
            let pair = v8::Array::new(scope, 2);
            let l = v8::String::new(scope, label).unwrap();
            let v = v8::String::new(scope, value).unwrap();
            pair.set_index(scope, 0, l.into());
            pair.set_index(scope, 1, v.into());
            arr.set_index(scope, i as u32, pair.into());
        }
        let k = v8::String::new(scope, name).unwrap();
        obj.set(scope, k.into(), arr.into());
    }

    set_str!("serialNumber", &bn_hex(cert.tbs_certificate.raw_serial()));

    set_str!(
        "validFrom",
        &asn1_time_openssl(cert.validity().not_before.timestamp())
    );
    set_str!(
        "validTo",
        &asn1_time_openssl(cert.validity().not_after.timestamp())
    );

    {
        let hash = <sha1::Sha1 as Digest>::digest(&der_bytes);
        set_str!("fingerprint", &format_colon_hex(&hash));
    }
    {
        let hash = <sha2::Sha256 as Digest>::digest(&der_bytes);
        set_str!("fingerprint256", &format_colon_hex(&hash));
    }
    {
        let hash = <sha2::Sha512 as Digest>::digest(&der_bytes);
        set_str!("fingerprint512", &format_colon_hex(&hash));
    }

    let mut basic_constraints: Option<bool> = None;
    let mut san_formatted: Option<String> = None;
    let mut ku_list: Vec<&str> = Vec::new();
    // The pieces of OpenSSL's X509_check_issued that decide whether one
    // certificate issued another (the legacy object's issuerCertificate
    // links): the names, the key identifiers, and keyCertSign when a
    // KeyUsage extension is present at all.
    let mut key_cert_sign: Option<bool> = None;
    let mut ext_key_usage: Option<Vec<String>> = None;
    let mut info_access: Option<String> = None;
    let mut subject_key_id: Option<String> = None;
    let mut authority_key_id: Option<String> = None;

    for ext in cert.extensions() {
        use x509_parser::extensions::ParsedExtension;
        match ext.parsed_extension() {
            ParsedExtension::BasicConstraints(bc) => {
                basic_constraints = Some(bc.ca);
            }
            ParsedExtension::SubjectAlternativeName(san) => {
                let names: Vec<String> =
                    san.general_names.iter().map(general_name_string).collect();
                san_formatted = Some(names.join(", "));
            }
            ParsedExtension::KeyUsage(ku) => {
                if ku.digital_signature() {
                    ku_list.push("digitalSignature");
                }
                if ku.non_repudiation() {
                    ku_list.push("nonRepudiation");
                }
                if ku.key_encipherment() {
                    ku_list.push("keyEncipherment");
                }
                if ku.data_encipherment() {
                    ku_list.push("dataEncipherment");
                }
                if ku.key_agreement() {
                    ku_list.push("keyAgreement");
                }
                if ku.key_cert_sign() {
                    ku_list.push("keyCertSign");
                }
                if ku.crl_sign() {
                    ku_list.push("cRLSign");
                }
                key_cert_sign = Some(ku.key_cert_sign());
            }
            ParsedExtension::ExtendedKeyUsage(_) => {
                ext_key_usage = Some(ext_key_usage_oids(ext.value));
            }
            ParsedExtension::AuthorityInfoAccess(aia) => {
                // Node's X509Certificate#infoAccess: "METHOD - LOCATION" per
                // line, OpenSSL's long names for the two well-known methods.
                let lines: Vec<String> = aia
                    .accessdescs
                    .iter()
                    .map(|ad| {
                        let method = match ad.access_method.to_id_string().as_str() {
                            "1.3.6.1.5.5.7.48.1" => "OCSP".to_string(),
                            "1.3.6.1.5.5.7.48.2" => "CA Issuers".to_string(),
                            other => other.to_string(),
                        };
                        format!("{method} - {}", general_name_string(&ad.access_location))
                    })
                    .collect();
                info_access = Some(lines.join("\n"));
            }
            ParsedExtension::SubjectKeyIdentifier(id) => {
                subject_key_id = Some(hex_lower(id.0));
            }
            ParsedExtension::AuthorityKeyIdentifier(akid) => {
                if let Some(id) = &akid.key_identifier {
                    authority_key_id = Some(hex_lower(id.0));
                }
            }
            _ => {}
        }
    }

    // OpenSSL's X509_check_ca, which Node's `ca` reports: a KeyUsage without
    // keyCertSign says no (probed on v22.22.2: a CA:TRUE self-signed
    // certificate issued with keyUsage=digitalSignature reads ca:false);
    // otherwise BasicConstraints decides; without one, a v1 self-issued root
    // or a KeyUsage that allows certificate signing still counts (those two
    // follow check_ca's source, unprobed).
    let self_issued = subject_entries == issuer_entries;
    let is_v1 = cert.tbs_certificate.version == x509_parser::x509::X509Version::V1;
    let ca = match (key_cert_sign, basic_constraints) {
        (Some(false), _) => false,
        (_, Some(constrained)) => constrained,
        (Some(true), None) => true,
        (None, None) => is_v1 && self_issued,
    };
    set_bool!("ca", ca);
    if let Some(ref san) = san_formatted {
        set_str!("subjectAltName", san);
    }
    if !ku_list.is_empty() {
        set_str_array!("keyUsage", &ku_list);
    }
    if let Some(ref oids) = ext_key_usage {
        set_str_array!("extKeyUsage", oids);
    }
    if let Some(ref text) = info_access {
        set_str!("infoAccess", text);
    }
    if let Some(flag) = key_cert_sign {
        set_bool!("keyCertSign", flag);
    }
    if let Some(ref id) = subject_key_id {
        set_str!("subjectKeyId", id);
    }
    if let Some(ref id) = authority_key_id {
        set_str!("authorityKeyId", id);
    }

    match x509_key_fields(cert.public_key()) {
        X509KeyFields::Rsa {
            modulus,
            exponent,
            bits,
            spki,
        } => {
            set_str!("keyType", "rsa");
            set_str!("modulus", &modulus);
            set_str!("exponent", &exponent);
            let k = v8::String::new(scope, "bits").unwrap();
            obj.set(
                scope,
                k.into(),
                v8::Number::new(scope, f64::from(bits)).into(),
            );
            set_bytes!("pubkey", spki);
        }
        X509KeyFields::Ec {
            bits,
            point,
            asn1_curve,
            nist_curve,
        } => {
            set_str!("keyType", "ec");
            let k = v8::String::new(scope, "bits").unwrap();
            obj.set(
                scope,
                k.into(),
                v8::Number::new(scope, f64::from(bits)).into(),
            );
            set_bytes!("pubkey", point);
            set_str!("asn1Curve", asn1_curve);
            if let Some(nist) = nist_curve {
                set_str!("nistCurve", nist);
            }
        }
        X509KeyFields::None => {
            set_str!("keyType", "");
        }
    }

    set_bytes!("raw", der_for_raw);

    rv.set(obj.into());
}

// ── Wave 8: prime generation & testing ────────────────────────────────

pub(crate) fn op_crypto_generate_prime(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    use num_bigint_dig::RandPrime;
    let bits = args.get(0).int32_value(scope).unwrap_or(0) as usize;
    if bits < 2 {
        crate::node_ops::throw_type_error(scope, "generatePrime: bits must be >= 2");
        return;
    }
    let prime = OsRng.gen_prime(bits);
    let bytes = prime.to_bytes_be();
    if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, bytes) {
        rv.set(value);
    }
}

pub(crate) fn op_crypto_check_prime(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    use num_bigint_dig::prime::probably_prime;
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "checkPrime: data required");
        return;
    };
    let n = BigUint::from_bytes_be(&data);
    let is_prime = probably_prime(&n, 25);
    rv.set(v8::Boolean::new(scope, is_prime).into());
}

#[cfg(test)]
mod x509_legacy_fields {
    use super::*;

    // BN_bn2hex drops leading zero BYTES only (probed on v22.22.2: a serial
    // of 0x0abc reads "0ABC" from both X509Certificate#serialNumber and
    // getPeerCertificate().serialNumber).
    #[test]
    fn bn_hex_keeps_a_leading_zero_nibble() {
        assert_eq!(bn_hex(&[0x00, 0x0a, 0xbc]), "0ABC");
        assert_eq!(bn_hex(&[0x26, 0xc7, 0x11]), "26C711");
        assert_eq!(bn_hex(&[0x00, 0x00]), "0");
        assert_eq!(bn_hex(&[]), "0");
        // No leading zero byte: the top nibble stays, unlike BN_print below.
        assert_eq!(bn_hex(&[0x01, 0x00, 0x01]), "010001");
    }

    // BN_print (the exponent's printer) drops the zero nibble bn2hex keeps.
    #[test]
    fn bn_print_hex_drops_a_leading_zero_nibble() {
        assert_eq!(bn_print_hex(&[0x01, 0x00, 0x01]), "10001");
        assert_eq!(bn_print_hex(&[0x00, 0x0a, 0xbc]), "ABC");
        assert_eq!(bn_print_hex(&[0x00]), "0");
        assert_eq!(bn_print_hex(&[0x03]), "3");
    }

    // OpenSSL's ASN1_TIME_print, as Node's validFrom reads: a space-padded
    // day, a four-digit year, "GMT".
    #[test]
    fn asn1_time_prints_like_openssl() {
        assert_eq!(asn1_time_openssl(1_781_526_607), "Jun 15 12:30:07 2026 GMT");
        assert_eq!(asn1_time_openssl(1_788_570_123), "Sep  5 01:02:03 2026 GMT");
        assert_eq!(asn1_time_openssl(2_104_743_976), "Sep 11 11:06:16 2036 GMT");
        assert_eq!(asn1_time_openssl(946_684_799), "Dec 31 23:59:59 1999 GMT");
        assert_eq!(asn1_time_openssl(0), "Jan  1 00:00:00 1970 GMT");
        assert_eq!(asn1_time_openssl(951_782_400), "Feb 29 00:00:00 2000 GMT");
    }

    #[test]
    fn oid_content_prints_dotted() {
        assert_eq!(
            oid_to_string(&[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01]),
            "1.3.6.1.5.5.7.3.1"
        );
        assert_eq!(oid_to_string(&[0x55, 0x1d, 0x25, 0x00]), "2.5.29.37.0");
        assert_eq!(
            oid_to_string(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01]),
            "1.2.840.113549.1.1.1"
        );
        assert_eq!(
            oid_to_string(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]),
            "1.2.840.10045.3.1.7"
        );
    }

    // The EKU OIDs come back in certificate order, which the parsed
    // extension does not keep.
    #[test]
    fn ext_key_usage_oids_keep_certificate_order() {
        let client_then_server = [
            0x30, 0x14, 0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02, 0x06, 0x08,
            0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01,
        ];
        assert_eq!(
            ext_key_usage_oids(&client_then_server),
            vec![
                "1.3.6.1.5.5.7.3.2".to_string(),
                "1.3.6.1.5.5.7.3.1".to_string()
            ]
        );
        assert!(ext_key_usage_oids(&[0x04, 0x00]).is_empty());
        assert!(ext_key_usage_oids(&[0x30, 0x05, 0x06]).is_empty());
    }

    // Node's subjectAltName spellings (probed): IPv6 as uppercase groups with
    // no zero compression.
    #[test]
    fn general_names_print_like_node() {
        use x509_parser::extensions::GeneralName;
        let v6 = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        assert_eq!(
            general_name_string(&GeneralName::IPAddress(&v6)),
            "IP Address:2001:DB8:0:0:0:0:0:1"
        );
        assert_eq!(
            general_name_string(&GeneralName::IPAddress(&[127, 0, 0, 1])),
            "IP Address:127.0.0.1"
        );
        assert_eq!(
            general_name_string(&GeneralName::DNSName("localhost")),
            "DNS:localhost"
        );
    }
}
