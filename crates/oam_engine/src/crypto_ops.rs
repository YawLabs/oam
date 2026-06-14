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
            crate::node_ops::throw_type_error(
                scope,
                &format!("scrypt: invalid parameters: {e}"),
            );
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
    let salt_opt = if salt.is_empty() { None } else { Some(&salt[..]) };
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
    if !c.auto_padding && data.len() % 16 != 0 {
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
    if data.len() % 16 != 0 {
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
            if instance.encrypt {
                if let Some(tag) = tag {
                    instance.auth_tag = Some(tag);
                    scope
                        .get_slot_mut::<CryptoState>()
                        .expect("crypto state installed")
                        .ciphers
                        .insert(id, instance);
                }
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
            let cipher =
                <$gcm>::new_from_slice(&c.key).map_err(|e| format!("gcm: {e}"))?;
            let mut ct =
                cipher.encrypt(nonce, payload).map_err(|e| format!("gcm encrypt: {e}"))?;
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
            let cipher =
                <$gcm>::new_from_slice(&c.key).map_err(|e| format!("gcm: {e}"))?;
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
