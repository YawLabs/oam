//! node:crypto natives: streaming digests, HMAC, OS randomness, and
//! constant-time comparison. Pure-CPU sync work — these run on the isolate
//! thread directly, no op channel.
//!
//! State model: in-flight hashers live in an isolate-slot registry keyed
//! by handle (zero-capture callbacks can't hold Rust state). digest()
//! consumes the entry; copy() clones it (every RustCrypto hasher is
//! Clone, which is what makes Node's hash.copy() cheap here). Cipher
//! instances live in the same registry and are consumed by final(); they
//! are incremental too (see "symmetric ciphers" below).
//!
//! A handle whose JS owner is dropped before digest()/final() is released
//! by `op_crypto_handle_free`, which the JS side calls from a
//! FinalizationRegistry (js/node_compat.js, `cryptoHandles`). Handles are
//! never reused, so a late free of a consumed handle is a no-op.

use cipher::{BlockDecryptMut, BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit, StreamCipher};
use ghash::universal_hash::UniversalHash;
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
    /// The tag of a finished GCM encryption, keyed by the cipher's handle.
    /// final() consumes the instance (key schedule, counter, GHASH state)
    /// and leaves only this; the one getAuthTag read that follows removes it.
    gcm_tags: HashMap<u64, Vec<u8>>,
}

impl CryptoState {
    fn insert(&mut self, hasher: Hasher) -> u64 {
        self.next += 1;
        self.map.insert(self.next, hasher);
        self.next
    }

    /// Drop whatever `id` still names: an in-flight hasher, a cipher
    /// instance, or a pending GCM tag. Unknown ids are ignored.
    fn release(&mut self, id: u64) {
        self.map.remove(&id);
        self.ciphers.remove(&id);
        self.gcm_tags.remove(&id);
    }
}

/// cryptoHandleFree(id) -> void. Releases a hash / HMAC / cipher handle that
/// was never finalized; called by the JS FinalizationRegistry when the
/// owning object is collected.
pub(crate) fn op_crypto_handle_free(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed")
        .release(id);
}

/// A numeric argument with a real default: `default` when the argument is
/// absent or `undefined`. (`number_value` alone converts `undefined` to NaN,
/// so `.unwrap_or(default)` on it never supplies the default.)
fn arg_number_or(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    index: i32,
    default: f64,
) -> f64 {
    let value = args.get(index);
    if value.is_undefined() {
        return default;
    }
    value.number_value(scope).unwrap_or(default)
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
    let normalized = normalize_algorithm(&digest);
    let Some(dk) = pbkdf2_derive(&normalized, &password, &salt, iterations, keylen) else {
        throw_unknown_digest(scope, &digest);
        return;
    };
    if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, dk) {
        rv.set(value);
    }
}

/// PBKDF2-HMAC over every digest in `SUPPORTED_HASHES` (`digest` is already
/// normalized). `None` for a digest outside that list.
fn pbkdf2_derive(
    digest: &str,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    keylen: usize,
) -> Option<Vec<u8>> {
    let mut dk = vec![0u8; keylen];
    match digest {
        "sha256" => pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, iterations, &mut dk),
        "sha384" => pbkdf2::pbkdf2_hmac::<sha2::Sha384>(password, salt, iterations, &mut dk),
        "sha512" => pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password, salt, iterations, &mut dk),
        "sha224" => pbkdf2::pbkdf2_hmac::<sha2::Sha224>(password, salt, iterations, &mut dk),
        "sha1" => pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, salt, iterations, &mut dk),
        "md5" => pbkdf2::pbkdf2_hmac::<md5::Md5>(password, salt, iterations, &mut dk),
        _ => return None,
    }
    Some(dk)
}

/// scrypt with the output length taken from `keylen` alone. The crate's
/// `Params` carries a `len` field it range-checks (10..=64) but that only
/// its password-hash API reads; `scrypt::scrypt` sizes the output by the
/// buffer, so the recommended constant is passed there and `keylen` is free.
fn scrypt_derive(
    password: &[u8],
    salt: &[u8],
    keylen: usize,
    n: u64,
    r: u32,
    p: u32,
) -> Result<Vec<u8>, String> {
    if n == 0 || !n.is_power_of_two() {
        return Err("scrypt: N must be a power of 2".into());
    }
    let log_n = 63 - n.leading_zeros() as u8;
    let params = scrypt::Params::new(log_n, r, p, scrypt::Params::RECOMMENDED_LEN)
        .map_err(|e| format!("scrypt: invalid parameters: {e}"))?;
    let mut dk = vec![0u8; keylen];
    // The crate refuses an empty output buffer; node returns an empty Buffer
    // for keylen 0 (after validating the cost parameters, as above).
    if keylen == 0 {
        return Ok(dk);
    }
    scrypt::scrypt(password, salt, &params, &mut dk)
        .map_err(|_| "scrypt: derivation failed".to_string())?;
    Ok(dk)
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
    // Node's defaults (N 16384, r 8, p 1) for an argument the caller left out.
    let n = arg_number_or(scope, &args, 3, 16384.0) as u64;
    let r = arg_number_or(scope, &args, 4, 8.0) as u32;
    let p = arg_number_or(scope, &args, 5, 1.0) as u32;

    match scrypt_derive(&password, &salt, keylen, n, r, p) {
        Ok(dk) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, dk) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
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
    let okm = match hkdf_derive(&normalized, salt_opt, &ikm, &info, keylen) {
        Some(Ok(okm)) => okm,
        Some(Err(msg)) => {
            crate::node_ops::throw_type_error(scope, msg);
            return;
        }
        None => {
            throw_unknown_digest(scope, &digest);
            return;
        }
    };
    if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, okm) {
        rv.set(value);
    }
}

/// HKDF over every digest in `SUPPORTED_HASHES` (`digest` is already
/// normalized). `None` for a digest outside that list; `Some(Err)` when
/// `keylen` exceeds what the digest can expand to.
fn hkdf_derive(
    digest: &str,
    salt: Option<&[u8]>,
    ikm: &[u8],
    info: &[u8],
    keylen: usize,
) -> Option<Result<Vec<u8>, &'static str>> {
    macro_rules! hkdf_expand {
        ($hash:ty) => {{
            let hk = hkdf::Hkdf::<$hash>::new(salt, ikm);
            let mut okm = vec![0u8; keylen];
            match hk.expand(info, &mut okm) {
                Ok(()) => Ok(okm),
                Err(_) => Err("hkdf: output length too large for digest"),
            }
        }};
    }
    Some(match digest {
        "sha256" => hkdf_expand!(sha2::Sha256),
        "sha384" => hkdf_expand!(sha2::Sha384),
        "sha512" => hkdf_expand!(sha2::Sha512),
        "sha224" => hkdf_expand!(sha2::Sha224),
        "sha1" => hkdf_expand!(sha1::Sha1),
        "md5" => hkdf_expand!(md5::Md5),
        _ => return None,
    })
}

// ===================================================== symmetric ciphers
//
// Every mode is incremental, as OpenSSL's EVP layer is: update() returns
// the bytes that are final at that point and the instance keeps at most one
// block of input, so memory does not grow with the message.
//
//   * CTR and GCM are stream modes: every update() returns as many bytes as
//     it was given.
//   * CBC and ECB return whole blocks. A decrypt with padding on holds the
//     last whole block back when the input so far ends on a block boundary
//     (it may be the padding block); final() returns what is left.
//   * GCM authenticates as it goes (GHASH over the ciphertext), so a
//     decrypt releases plaintext before the tag is checked, as Node's does;
//     final() is where a bad tag is refused.

pub(crate) const SUPPORTED_CIPHERS: [&str; 8] = [
    "aes-128-cbc",
    "aes-256-cbc",
    "aes-128-ecb",
    "aes-256-ecb",
    "aes-128-ctr",
    "aes-256-ctr",
    "aes-128-gcm",
    "aes-256-gcm",
];

const AES_BLOCK: usize = 16;

#[derive(Clone, Copy)]
enum CipherMode {
    Aes128Cbc,
    Aes256Cbc,
    Aes128Ecb,
    Aes256Ecb,
    Aes128Ctr,
    Aes256Ctr,
    Aes128Gcm,
    Aes256Gcm,
}

impl CipherMode {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "aes-128-cbc" => Self::Aes128Cbc,
            "aes-256-cbc" => Self::Aes256Cbc,
            "aes-128-ecb" => Self::Aes128Ecb,
            "aes-256-ecb" => Self::Aes256Ecb,
            "aes-128-ctr" => Self::Aes128Ctr,
            "aes-256-ctr" => Self::Aes256Ctr,
            "aes-128-gcm" => Self::Aes128Gcm,
            "aes-256-gcm" => Self::Aes256Gcm,
            _ => return None,
        })
    }

    fn key_len(&self) -> usize {
        match self {
            Self::Aes128Cbc | Self::Aes128Ecb | Self::Aes128Ctr | Self::Aes128Gcm => 16,
            Self::Aes256Cbc | Self::Aes256Ecb | Self::Aes256Ctr | Self::Aes256Gcm => 32,
        }
    }

    fn iv_len(&self) -> usize {
        match self {
            Self::Aes128Cbc | Self::Aes256Cbc => 16,
            Self::Aes128Ecb | Self::Aes256Ecb => 0,
            Self::Aes128Ctr | Self::Aes256Ctr => 16,
            Self::Aes128Gcm | Self::Aes256Gcm => 12,
        }
    }
}

struct CipherInstance {
    engine: CipherEngine,
    /// The tag a GCM decrypt is checked against (setAuthTag).
    auth_tag: Option<Vec<u8>>,
}

/// Boxed: an AES-256 key schedule is several hundred bytes, and the
/// variants differ a lot in size.
enum CipherEngine {
    Block(Box<BlockState>),
    Ctr(Box<CtrCore>),
    Gcm(Box<GcmState>),
}

impl CipherInstance {
    fn new(mode: CipherMode, key: &[u8], iv: &[u8], encrypt: bool) -> Result<Self, String> {
        let engine = match mode {
            CipherMode::Aes128Cbc
            | CipherMode::Aes256Cbc
            | CipherMode::Aes128Ecb
            | CipherMode::Aes256Ecb => {
                CipherEngine::Block(Box::new(BlockState::new(mode, key, iv, encrypt)?))
            }
            CipherMode::Aes128Ctr | CipherMode::Aes256Ctr => {
                CipherEngine::Ctr(Box::new(CtrCore::new(key, iv)?))
            }
            CipherMode::Aes128Gcm | CipherMode::Aes256Gcm => {
                CipherEngine::Gcm(Box::new(GcmState::new(key, iv, encrypt)?))
            }
        };
        Ok(Self {
            engine,
            auth_tag: None,
        })
    }

    /// The output that is final after `data`.
    fn update(&mut self, data: Vec<u8>) -> Result<Vec<u8>, String> {
        match &mut self.engine {
            CipherEngine::Block(state) => Ok(state.update(&data)),
            CipherEngine::Ctr(core) => {
                let mut buf = data;
                core.apply(&mut buf)?;
                Ok(buf)
            }
            CipherEngine::Gcm(state) => state.update(data),
        }
    }
}

// ---- CBC / ECB

enum BlockCore {
    CbcEnc128(cbc::Encryptor<aes::Aes128>),
    CbcEnc256(cbc::Encryptor<aes::Aes256>),
    CbcDec128(cbc::Decryptor<aes::Aes128>),
    CbcDec256(cbc::Decryptor<aes::Aes256>),
    Ecb128(aes::Aes128),
    Ecb256(aes::Aes256),
}

/// Encrypt `buf`, a whole number of blocks, in place. The chaining state
/// (CBC) carries over to the next call.
fn encrypt_blocks<C: BlockEncryptMut>(cipher: &mut C, buf: &mut [u8]) {
    let (blocks, tail) = cipher::inout::InOutBuf::from(buf).into_chunks();
    debug_assert!(tail.is_empty(), "block modes are fed whole blocks");
    cipher.encrypt_blocks_inout_mut(blocks);
}

/// Decrypt `buf`, a whole number of blocks, in place.
fn decrypt_blocks<C: BlockDecryptMut>(cipher: &mut C, buf: &mut [u8]) {
    let (blocks, tail) = cipher::inout::InOutBuf::from(buf).into_chunks();
    debug_assert!(tail.is_empty(), "block modes are fed whole blocks");
    cipher.decrypt_blocks_inout_mut(blocks);
}

struct BlockState {
    core: BlockCore,
    /// "cbc" / "ecb", for the error messages.
    label: &'static str,
    encrypt: bool,
    auto_padding: bool,
    /// Input not yet processed: less than a block, or exactly the one block
    /// a padded decrypt holds back.
    pending: Vec<u8>,
    /// Bytes given to update() so far (the length the errors report).
    total: u64,
}

impl BlockState {
    fn new(mode: CipherMode, key: &[u8], iv: &[u8], encrypt: bool) -> Result<Self, String> {
        let cbc_err = |e: cipher::InvalidLength| format!("cbc: {e}");
        let ecb_err = |e: cipher::InvalidLength| format!("ecb: {e}");
        let (core, label) = match (mode, encrypt) {
            (CipherMode::Aes128Cbc, true) => (
                BlockCore::CbcEnc128(cbc::Encryptor::new_from_slices(key, iv).map_err(cbc_err)?),
                "cbc",
            ),
            (CipherMode::Aes256Cbc, true) => (
                BlockCore::CbcEnc256(cbc::Encryptor::new_from_slices(key, iv).map_err(cbc_err)?),
                "cbc",
            ),
            (CipherMode::Aes128Cbc, false) => (
                BlockCore::CbcDec128(cbc::Decryptor::new_from_slices(key, iv).map_err(cbc_err)?),
                "cbc",
            ),
            (CipherMode::Aes256Cbc, false) => (
                BlockCore::CbcDec256(cbc::Decryptor::new_from_slices(key, iv).map_err(cbc_err)?),
                "cbc",
            ),
            (CipherMode::Aes128Ecb, _) => (
                BlockCore::Ecb128(aes::Aes128::new_from_slice(key).map_err(ecb_err)?),
                "ecb",
            ),
            (CipherMode::Aes256Ecb, _) => (
                BlockCore::Ecb256(aes::Aes256::new_from_slice(key).map_err(ecb_err)?),
                "ecb",
            ),
            _ => return Err("not a block-mode cipher".into()),
        };
        Ok(Self {
            core,
            label,
            encrypt,
            auto_padding: true,
            pending: Vec::with_capacity(AES_BLOCK),
            total: 0,
        })
    }

    /// Run whole blocks through the cipher in place.
    fn apply(&mut self, buf: &mut [u8]) {
        match &mut self.core {
            BlockCore::CbcEnc128(c) => encrypt_blocks(c, buf),
            BlockCore::CbcEnc256(c) => encrypt_blocks(c, buf),
            BlockCore::CbcDec128(c) => decrypt_blocks(c, buf),
            BlockCore::CbcDec256(c) => decrypt_blocks(c, buf),
            BlockCore::Ecb128(c) if self.encrypt => encrypt_blocks(c, buf),
            BlockCore::Ecb128(c) => decrypt_blocks(c, buf),
            BlockCore::Ecb256(c) if self.encrypt => encrypt_blocks(c, buf),
            BlockCore::Ecb256(c) => decrypt_blocks(c, buf),
        }
    }

    fn update(&mut self, data: &[u8]) -> Vec<u8> {
        self.total += data.len() as u64;
        let available = self.pending.len() + data.len();
        let mut ready = available - available % AES_BLOCK;
        // A padded decrypt cannot release the last whole block yet: if no
        // more input comes, that block is the one carrying the padding.
        if !self.encrypt && self.auto_padding && ready == available && ready > 0 {
            ready -= AES_BLOCK;
        }
        if ready == 0 {
            self.pending.extend_from_slice(data);
            return Vec::new();
        }
        // `pending` is at most one block and `ready` at least one, so the
        // split point always falls inside `data`.
        let take = ready - self.pending.len();
        let mut out = Vec::with_capacity(ready);
        out.extend_from_slice(&self.pending);
        out.extend_from_slice(&data[..take]);
        self.pending.clear();
        self.pending.extend_from_slice(&data[take..]);
        self.apply(&mut out);
        out
    }

    fn finish(mut self) -> Result<Vec<u8>, String> {
        let mut last = std::mem::take(&mut self.pending);
        if self.encrypt {
            if !self.auto_padding {
                if !last.is_empty() {
                    return Err(format!(
                        "data length {} not a multiple of block size 16 (autoPadding is off)",
                        self.total
                    ));
                }
                return Ok(last);
            }
            // PKCS#7: 1..=16 bytes, each the pad length.
            let pad = AES_BLOCK - last.len();
            last.resize(AES_BLOCK, pad as u8);
            self.apply(&mut last);
            return Ok(last);
        }
        if !self.total.is_multiple_of(AES_BLOCK as u64) {
            return Err(format!(
                "ciphertext length {} not a multiple of block size 16",
                self.total
            ));
        }
        if !self.auto_padding {
            // Padding was switched off after a block had been held back.
            self.apply(&mut last);
            return Ok(last);
        }
        let label = self.label;
        let invalid = || format!("{label} decrypt: invalid padding");
        if last.len() != AES_BLOCK {
            // No input at all: there is no padding block to remove.
            return Err(invalid());
        }
        self.apply(&mut last);
        let pad = last[AES_BLOCK - 1] as usize;
        if pad == 0 || pad > AES_BLOCK || last[AES_BLOCK - pad..].iter().any(|&b| b as usize != pad)
        {
            return Err(invalid());
        }
        last.truncate(AES_BLOCK - pad);
        Ok(last)
    }
}

// ---- CTR

// Both variants are large (an AES key schedule each) and the enum only ever
// lives behind `CipherEngine`'s Box, so boxing the bigger one again would
// buy nothing.
#[allow(clippy::large_enum_variant)]
enum CtrCore {
    Aes128(ctr::Ctr128BE<aes::Aes128>),
    Aes256(ctr::Ctr128BE<aes::Aes256>),
}

impl CtrCore {
    fn new(key: &[u8], iv: &[u8]) -> Result<Self, String> {
        let err = |e: cipher::InvalidLength| format!("ctr: {e}");
        match key.len() {
            16 => Ok(Self::Aes128(
                ctr::Ctr128BE::new_from_slices(key, iv).map_err(err)?,
            )),
            32 => Ok(Self::Aes256(
                ctr::Ctr128BE::new_from_slices(key, iv).map_err(err)?,
            )),
            _ => Err("invalid key length for AES-CTR".into()),
        }
    }

    /// XOR the next keystream bytes into `buf`. The position carries over,
    /// including mid-block.
    fn apply(&mut self, buf: &mut [u8]) -> Result<(), String> {
        match self {
            Self::Aes128(c) => c.try_apply_keystream(buf),
            Self::Aes256(c) => c.try_apply_keystream(buf),
        }
        .map_err(|_| "ctr: keystream exhausted".to_string())
    }
}

// ---- GCM
//
// The `aes-gcm` crate is one-shot only (it needs the whole message for one
// call), so the streaming path assembles GCM from its parts (NIST SP
// 800-38D): AES-CTR with a 32-bit counter for the data, and GHASH over the
// AAD and the ciphertext for the tag. `gcm_one_shot` below still goes
// through `aes-gcm`, which is also the oracle the unit tests hold this
// implementation to.

/// GHASH (SP 800-38D section 6.4), incremental: the `ghash` crate (the one
/// `aes-gcm` itself authenticates with) does the field arithmetic, and this
/// wrapper holds the partial block the crate's block-at-a-time API leaves to
/// its caller.
struct Ghash {
    inner: ghash::GHash,
    /// A partial block waiting for more input (or for `pad`).
    partial: [u8; AES_BLOCK],
    partial_len: usize,
}

impl Ghash {
    /// `h` is the hash subkey, AES_K(0^128).
    fn new(h: &[u8; AES_BLOCK]) -> Self {
        Self {
            inner: ghash::GHash::new(ghash::Key::from_slice(h)),
            partial: [0; AES_BLOCK],
            partial_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        if self.partial_len > 0 {
            let take = (AES_BLOCK - self.partial_len).min(data.len());
            self.partial[self.partial_len..self.partial_len + take].copy_from_slice(&data[..take]);
            self.partial_len += take;
            data = &data[take..];
            if self.partial_len < AES_BLOCK {
                return;
            }
            self.inner.update_padded(&self.partial);
            self.partial_len = 0;
        }
        // Whole blocks only, so `update_padded` pads nothing here.
        let (blocks, rest) = data.split_at(data.len() - data.len() % AES_BLOCK);
        self.inner.update_padded(blocks);
        self.partial[..rest.len()].copy_from_slice(rest);
        self.partial_len = rest.len();
    }

    /// Zero-fill a partial block and absorb it: the AAD and the ciphertext
    /// are each padded to a block boundary.
    fn pad(&mut self) {
        if self.partial_len > 0 {
            self.inner.update_padded(&self.partial[..self.partial_len]);
            self.partial_len = 0;
        }
    }

    /// Absorb the closing lengths block and return the hash. Call after
    /// `pad`: a pending partial block is not part of the output.
    fn finish(mut self, lengths: &[u8; AES_BLOCK]) -> [u8; AES_BLOCK] {
        self.inner.update_padded(lengths);
        self.inner.finalize().into()
    }
}

// As `CtrCore`: two key schedules, held inside the boxed `GcmState`.
#[allow(clippy::large_enum_variant)]
enum GcmCtr {
    Aes128(ctr::Ctr32BE<aes::Aes128>),
    Aes256(ctr::Ctr32BE<aes::Aes256>),
}

struct GcmState {
    ctr: GcmCtr,
    ghash: Ghash,
    /// AES_K(J0), XORed into the GHASH output to make the tag.
    tag_mask: [u8; AES_BLOCK],
    encrypt: bool,
    aad_len: u64,
    data_len: u64,
    /// Set by the first update() / final(): the AAD is closed (padded) and
    /// setAAD is refused from then on.
    data_started: bool,
}

impl GcmState {
    fn new(key: &[u8], iv: &[u8], encrypt: bool) -> Result<Self, String> {
        if iv.len() != 12 {
            return Err(format!(
                "Invalid IV length: expected 12 bytes, got {}",
                iv.len()
            ));
        }
        // J0 = IV || 0^31 || 1 masks the tag; the data starts one counter on.
        let mut j0 = [0u8; AES_BLOCK];
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;
        let mut first = j0;
        first[15] = 2;
        let mut h = [0u8; AES_BLOCK];
        let mut tag_mask = j0;
        let err = |e: cipher::InvalidLength| format!("gcm: {e}");
        let ctr = match key.len() {
            16 => {
                let aes = aes::Aes128::new_from_slice(key).map_err(err)?;
                aes.encrypt_block(aes::Block::from_mut_slice(&mut h));
                aes.encrypt_block(aes::Block::from_mut_slice(&mut tag_mask));
                GcmCtr::Aes128(ctr::Ctr32BE::new_from_slices(key, &first).map_err(err)?)
            }
            32 => {
                let aes = aes::Aes256::new_from_slice(key).map_err(err)?;
                aes.encrypt_block(aes::Block::from_mut_slice(&mut h));
                aes.encrypt_block(aes::Block::from_mut_slice(&mut tag_mask));
                GcmCtr::Aes256(ctr::Ctr32BE::new_from_slices(key, &first).map_err(err)?)
            }
            _ => return Err("invalid key length for AES-GCM".into()),
        };
        Ok(Self {
            ctr,
            ghash: Ghash::new(&h),
            tag_mask,
            encrypt,
            aad_len: 0,
            data_len: 0,
            data_started: false,
        })
    }

    /// setAAD: successive calls append, as OpenSSL's do.
    fn add_aad(&mut self, aad: &[u8]) -> Result<(), String> {
        if self.data_started {
            return Err("Invalid state for operation setAAD".into());
        }
        self.aad_len += aad.len() as u64;
        self.ghash.update(aad);
        Ok(())
    }

    fn start_data(&mut self) {
        if !self.data_started {
            self.data_started = true;
            self.ghash.pad();
        }
    }

    fn update(&mut self, mut data: Vec<u8>) -> Result<Vec<u8>, String> {
        self.start_data();
        self.data_len += data.len() as u64;
        // GHASH always runs over the ciphertext: the input of a decrypt,
        // the output of an encrypt.
        if !self.encrypt {
            self.ghash.update(&data);
        }
        match &mut self.ctr {
            GcmCtr::Aes128(c) => c.try_apply_keystream(&mut data),
            GcmCtr::Aes256(c) => c.try_apply_keystream(&mut data),
        }
        // The 32-bit block counter ran out: past GCM's 2^36 - 32 byte limit.
        .map_err(|_| "gcm: message too long".to_string())?;
        if self.encrypt {
            self.ghash.update(&data);
        }
        Ok(data)
    }

    /// The tag over everything seen so far.
    fn tag(mut self) -> [u8; AES_BLOCK] {
        self.start_data();
        self.ghash.pad();
        let mut lengths = [0u8; AES_BLOCK];
        lengths[..8].copy_from_slice(&(self.aad_len * 8).to_be_bytes());
        lengths[8..].copy_from_slice(&(self.data_len * 8).to_be_bytes());
        let mut tag = self.ghash.finish(&lengths);
        for (t, m) in tag.iter_mut().zip(self.tag_mask) {
            *t ^= m;
        }
        tag
    }

    /// final(): an encrypt yields its tag; a decrypt checks `expected`. No
    /// data is left to return either way (every update() returned its own).
    fn finish(self, expected: Option<&[u8]>) -> Result<Option<Vec<u8>>, String> {
        if self.encrypt {
            return Ok(Some(self.tag().to_vec()));
        }
        let expected =
            expected.ok_or("gcm decrypt: auth tag required (call setAuthTag before final)")?;
        let tag = self.tag();
        // Constant-time over the tag bytes; the length is not secret.
        let differs = expected.len() != tag.len()
            || tag
                .iter()
                .zip(expected)
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                != 0;
        if differs {
            return Err("gcm decrypt: authentication failed".into());
        }
        Ok(None)
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
    let instance = match CipherInstance::new(mode, &key, &iv, encrypt) {
        Ok(instance) => instance,
        Err(msg) => {
            crate::node_ops::throw_type_error(scope, &msg);
            return;
        }
    };
    let state = scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed");
    state.next += 1;
    let id = state.next;
    state.ciphers.insert(id, instance);
    rv.set_double(id as f64);
}

/// cryptoCipherUpdate(id, data) -> Uint8Array: the output that is final
/// after `data` (see the section comment for what each mode returns).
pub(crate) fn op_crypto_cipher_update(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(data) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "cipher update requires data");
        return;
    };
    let result = match scope
        .get_slot_mut::<CryptoState>()
        .expect("crypto state installed")
        .ciphers
        .get_mut(&id)
    {
        Some(c) => c.update(data),
        None => Err("cipher: invalid handle".into()),
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

/// cryptoCipherFinal(id) -> Uint8Array: what a CBC / ECB cipher still owes
/// (the padding block, or the block a padded decrypt held back); empty for
/// CTR. Consumes the handle.
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
    let result = match instance.engine {
        CipherEngine::Block(state) => state.finish(),
        CipherEngine::Ctr(_) => Ok(Vec::new()),
        CipherEngine::Gcm(_) => {
            Err("GCM ciphers are finalized by cryptoCipherFinalGcm, not cryptoCipherFinal".into())
        }
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
    let result = match state.ciphers.get_mut(&id) {
        Some(c) => match &mut c.engine {
            CipherEngine::Gcm(gcm) => gcm.add_aad(&data),
            // Only GCM authenticates; the other modes have nothing to do
            // with AAD and take the call as a no-op, as they always have.
            CipherEngine::Block(_) | CipherEngine::Ctr(_) => Ok(()),
        },
        None => Err("cipher: invalid handle".into()),
    };
    if let Err(msg) = result {
        crate::node_ops::throw_type_error(scope, &msg);
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
        // A finished GCM encryption left only its tag; this read takes it,
        // so nothing of the cipher outlives the final()/getAuthTag pair (the
        // JS Cipher keeps its own copy for repeated getAuthTag() calls).
        let finished = state.gcm_tags.remove(&id);
        if finished.is_some() {
            finished
        } else {
            match state.ciphers.get(&id) {
                Some(c) => c.auth_tag.clone(),
                None => {
                    crate::node_ops::throw_type_error(scope, "cipher: invalid handle");
                    return;
                }
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
        Some(c) => {
            // Padding is a block-mode notion; a stream mode ignores it.
            if let CipherEngine::Block(block) = &mut c.engine {
                block.auto_padding = auto_pad;
            }
        }
        None => crate::node_ops::throw_type_error(scope, "cipher: invalid handle"),
    }
}

/// cryptoCipherFinalGcm(id) -> Uint8Array (always empty: every update()
/// returned its own output). An encrypt leaves its tag for the
/// cryptoCipherGetAuthTag read that follows; a decrypt throws here when the
/// tag does not match. Consumes the handle.
pub(crate) fn op_crypto_cipher_final_gcm(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let instance = {
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
    let result = match instance.engine {
        CipherEngine::Gcm(gcm) => gcm.finish(instance.auth_tag.as_deref()),
        CipherEngine::Block(_) | CipherEngine::Ctr(_) => {
            Err("cryptoCipherFinalGcm: not a GCM cipher".into())
        }
    };
    match result {
        Ok(tag) => {
            if let Some(tag) = tag {
                // Keep the tag alone for the getAuthTag read that follows;
                // the instance (key schedule, counter, GHASH state) ends
                // here.
                scope
                    .get_slot_mut::<CryptoState>()
                    .expect("crypto state installed")
                    .gcm_tags
                    .insert(id, tag);
            }
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, Vec::new()) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

/// cryptoGcmOneShot(key, iv, aad, data, encrypt) -> Uint8Array. WebCrypto's
/// AES-GCM has the whole message in hand, so it takes the `aes-gcm` crate's
/// one-shot path (hardware GHASH, and a decrypt that releases nothing unless
/// the tag holds). Encrypt returns ciphertext || 16-byte tag; decrypt takes
/// that same layout and returns the plaintext.
pub(crate) fn op_crypto_gcm_one_shot(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let (Some(key), Some(iv), Some(data)) = (
        crate::node_ops::arg_bytes(scope, &args, 0),
        crate::node_ops::arg_bytes(scope, &args, 1),
        crate::node_ops::arg_bytes(scope, &args, 3),
    ) else {
        crate::node_ops::throw_type_error(scope, "cryptoGcmOneShot: key, iv and data required");
        return;
    };
    let aad = crate::node_ops::arg_bytes(scope, &args, 2).unwrap_or_default();
    let encrypt = args.get(4).boolean_value(scope);
    match gcm_one_shot(&key, &iv, &aad, &data, encrypt) {
        Ok(bytes) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, bytes) {
                rv.set(value);
            }
        }
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

fn gcm_one_shot(
    key: &[u8],
    iv: &[u8],
    aad: &[u8],
    data: &[u8],
    encrypt: bool,
) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, KeyInit as AeadKeyInit, Payload};
    if iv.len() != 12 {
        return Err(format!(
            "Invalid IV length: expected 12 bytes, got {}",
            iv.len()
        ));
    }
    let nonce = aes_gcm::Nonce::from_slice(iv);
    let payload = Payload { msg: data, aad };
    macro_rules! run {
        ($gcm:ty) => {{
            let cipher = <$gcm>::new_from_slice(key).map_err(|e| format!("gcm: {e}"))?;
            if encrypt {
                cipher
                    .encrypt(nonce, payload)
                    .map_err(|e| format!("gcm encrypt: {e}"))
            } else {
                cipher
                    .decrypt(nonce, payload)
                    .map_err(|_| "gcm decrypt: authentication failed".to_string())
            }
        }};
    }
    match key.len() {
        16 => run!(aes_gcm::Aes128Gcm),
        32 => run!(aes_gcm::Aes256Gcm),
        n => Err(format!(
            "Invalid key length: expected 16 or 32 bytes, got {n}"
        )),
    }
}

// ===================================================== asymmetric sign/verify
// Wave 3: RSA (PKCS#1 v1.5, PSS) and ECDSA (P-256, P-384) via ring.

use ring::signature as ring_sig;
use ring_sig::KeyPair as _;

// Wave 4: RSA encrypt/decrypt via the `rsa` crate (ring doesn't do encryption).
// `SysRng` is the operating-system generator on the rand_core 0.10 traits the
// `rsa` crate takes; `OsRng` is the rand_core 0.6 one p256/p384 and classic DH
// still take.
use crypto_common::getrandom::SysRng;
use rand_core::OsRng;
use rsa::pkcs1::EncodeRsaPublicKey;
// The EC keys (p256/p384 0.13) decode through the pkcs8 0.10 trait; the RSA
// keys through the pkcs8 0.11 one the `rsa` crate re-exports.
use p256::pkcs8::DecodePrivateKey as _;
use rsa::pkcs8::EncodePrivateKey;
use rsa::sha2::digest::Digest as RsaDigest;
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{
    BoxedUint, Oaep, Pkcs1v15Encrypt, PrivateKey as RsaPrivateKey, PublicKey as RsaPublicKey,
};

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

fn resolve_verify_algorithm(algo: &str) -> Result<&'static ring_sig::RsaParameters, String> {
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

/// One DER element: `(rest, body)`. Every length is bounds-checked against
/// `data` before it is used: a long-form length is at most four bytes (far
/// beyond any key this file reads), the indefinite form (0x80) is refused,
/// and the end offset is computed with a checked add.
fn parse_asn1_element(data: &[u8]) -> Result<(&[u8], &[u8]), String> {
    if data.len() < 2 {
        return Err("ASN.1: truncated".into());
    }
    let (len, header_size) = if data[1] & 0x80 == 0 {
        (data[1] as usize, 2usize)
    } else {
        let num_bytes = (data[1] & 0x7f) as usize;
        if num_bytes == 0 || num_bytes > 4 {
            return Err("ASN.1: unsupported length encoding".into());
        }
        let Some(len_bytes) = data.get(2..2 + num_bytes) else {
            return Err("ASN.1: truncated length".into());
        };
        let mut len: usize = 0;
        for &byte in len_bytes {
            len = (len << 8) | byte as usize;
        }
        (len, 2 + num_bytes)
    };
    let end = header_size
        .checked_add(len)
        .filter(|&end| end <= data.len())
        .ok_or_else(|| "ASN.1: truncated body".to_string())?;
    let body = &data[header_size..end];
    let rest = &data[end..];
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
    let params = resolve_verify_algorithm(algo)?;
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum EcCurve {
    P256,
    P384,
}

impl EcCurve {
    fn name(self) -> &'static str {
        match self {
            EcCurve::P256 => "P-256",
            EcCurve::P384 => "P-384",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EcHash {
    Sha256,
    Sha384,
}

impl EcHash {
    /// The digest an ECDSA call names. The digest is all the name selects:
    /// the curve is the key's.
    fn from_algo(algo: &str) -> Option<Self> {
        match algo {
            "sha256" | "ecdsasha256" | "p256" => Some(EcHash::Sha256),
            "sha384" | "ecdsasha384" | "p384" => Some(EcHash::Sha384),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            EcHash::Sha256 => "sha256",
            EcHash::Sha384 => "sha384",
        }
    }
}

/// ECDSA sign. `der_encoding` picks the signature format: DER (node's
/// default `dsaEncoding`) or the fixed-width r||s of IEEE P1363 (WebCrypto,
/// and node's `dsaEncoding: 'ieee-p1363'`).
///
/// ring signs P-256 with SHA-256 and P-384 with SHA-384 only; the two mixed
/// pairs are refused by name (docs/node-divergences.md).
fn crypto_sign_ec(
    algo: &str,
    data: &[u8],
    key_pem: &str,
    der_encoding: bool,
) -> Result<Vec<u8>, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    match label {
        "PRIVATE KEY" => {}
        "EC PRIVATE KEY" => {
            return Err("SEC1 EC private key format not supported; use PKCS#8".into());
        }
        _ => return Err(format!("unsupported EC PEM label: '{label}'")),
    }
    let hash =
        EcHash::from_algo(algo).ok_or_else(|| format!("unsupported ECDSA algorithm: '{algo}'"))?;
    let p256_alg: &'static ring_sig::EcdsaSigningAlgorithm = if der_encoding {
        &ring_sig::ECDSA_P256_SHA256_ASN1_SIGNING
    } else {
        &ring_sig::ECDSA_P256_SHA256_FIXED_SIGNING
    };
    let p384_alg: &'static ring_sig::EcdsaSigningAlgorithm = if der_encoding {
        &ring_sig::ECDSA_P384_SHA384_ASN1_SIGNING
    } else {
        &ring_sig::ECDSA_P384_SHA384_FIXED_SIGNING
    };
    let rng = ring::rand::SystemRandom::new();
    // The curve is read off the key: ring parses a PKCS#8 key only under an
    // algorithm of the key's own curve.
    let (curve, key_pair) = match ring_sig::EcdsaKeyPair::from_pkcs8(p256_alg, &der, &rng) {
        Ok(key_pair) => (EcCurve::P256, key_pair),
        Err(_) => match ring_sig::EcdsaKeyPair::from_pkcs8(p384_alg, &der, &rng) {
            Ok(key_pair) => (EcCurve::P384, key_pair),
            Err(e) => return Err(format!("EC key parse: {e}")),
        },
    };
    let paired = matches!(
        (curve, hash),
        (EcCurve::P256, EcHash::Sha256) | (EcCurve::P384, EcHash::Sha384)
    );
    if !paired {
        return Err(format!(
            "ECDSA sign: a {} key with {} is not supported \
             (oam signs P-256 with sha256 and P-384 with sha384)",
            curve.name(),
            hash.name()
        ));
    }
    let sig = key_pair
        .sign(&rng, data)
        .map_err(|e| format!("ECDSA sign: {e}"))?;
    Ok(sig.as_ref().to_vec())
}

/// r||s (IEEE P1363) to the DER `SEQUENCE { INTEGER r, INTEGER s }`. `None`
/// for an input that is not two equal-width halves.
fn ecdsa_p1363_to_der(sig: &[u8]) -> Option<Vec<u8>> {
    if sig.is_empty() || !sig.len().is_multiple_of(2) {
        return None;
    }
    let (r, s) = sig.split_at(sig.len() / 2);
    let mut body = Vec::with_capacity(sig.len() + 6);
    for half in [r, s] {
        // Minimal big-endian: no leading zero bytes (one byte stays for the
        // value zero), plus a zero pad when the top bit is set.
        let first = half.iter().position(|&b| b != 0).unwrap_or(half.len() - 1);
        let digits = &half[first..];
        let pad = digits[0] & 0x80 != 0;
        body.push(0x02);
        encode_asn1_length(&mut body, digits.len() + usize::from(pad));
        if pad {
            body.push(0x00);
        }
        body.extend_from_slice(digits);
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(0x30);
    encode_asn1_length(&mut out, body.len());
    out.extend_from_slice(&body);
    Some(out)
}

/// ECDSA verify; `der_encoding` as for `crypto_sign_ec`. The curve is the
/// key's (read off the public point's length) and the digest is the named
/// one, in any of the four P-256/P-384 x SHA-256/SHA-384 pairs.
fn crypto_verify_ec(
    algo: &str,
    data: &[u8],
    key_pem: &str,
    signature: &[u8],
    der_encoding: bool,
) -> Result<bool, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let hash = EcHash::from_algo(algo)
        .ok_or_else(|| format!("unsupported ECDSA verify algorithm: '{algo}'"))?;
    if label != "PUBLIC KEY" {
        return Err(format!("unsupported EC public key PEM label: '{label}'"));
    }
    let spki_pub = extract_spki_pubkey(&der)?;
    // An uncompressed point: 0x04 || X || Y.
    let curve = match spki_pub.len() {
        65 => EcCurve::P256,
        97 => EcCurve::P384,
        _ => {
            return Err("unsupported EC public key (oam verifies P-256 and P-384)".into());
        }
    };
    // ring has a fixed-width verifier only for a curve with its own digest;
    // a P1363 signature over a mixed pair is re-encoded and checked by the
    // DER verifier instead.
    let fixed_alg: Option<&'static ring_sig::EcdsaVerificationAlgorithm> = match (curve, hash) {
        (EcCurve::P256, EcHash::Sha256) => Some(&ring_sig::ECDSA_P256_SHA256_FIXED),
        (EcCurve::P384, EcHash::Sha384) => Some(&ring_sig::ECDSA_P384_SHA384_FIXED),
        _ => None,
    };
    let asn1_alg: &'static ring_sig::EcdsaVerificationAlgorithm = match (curve, hash) {
        (EcCurve::P256, EcHash::Sha256) => &ring_sig::ECDSA_P256_SHA256_ASN1,
        (EcCurve::P256, EcHash::Sha384) => &ring_sig::ECDSA_P256_SHA384_ASN1,
        (EcCurve::P384, EcHash::Sha256) => &ring_sig::ECDSA_P384_SHA256_ASN1,
        (EcCurve::P384, EcHash::Sha384) => &ring_sig::ECDSA_P384_SHA384_ASN1,
    };
    let verified = if der_encoding {
        ring_sig::UnparsedPublicKey::new(asn1_alg, &spki_pub)
            .verify(data, signature)
            .is_ok()
    } else if let Some(fixed_alg) = fixed_alg {
        ring_sig::UnparsedPublicKey::new(fixed_alg, &spki_pub)
            .verify(data, signature)
            .is_ok()
    } else {
        let width = match curve {
            EcCurve::P256 => 64,
            EcCurve::P384 => 96,
        };
        match ecdsa_p1363_to_der(signature) {
            Some(der_sig) if signature.len() == width => {
                ring_sig::UnparsedPublicKey::new(asn1_alg, &spki_pub)
                    .verify(data, &der_sig)
                    .is_ok()
            }
            _ => false,
        }
    };
    Ok(verified)
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
            // No default: node requires modulusLength, and a missing one
            // (NaN, cast to 0) is refused by crypto_generate_rsa's range check.
            let bits = args.get(1).number_value(scope).unwrap_or(0.0) as usize;
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
    // Optional fifth argument, ECDSA only: "der" for a DER signature (what
    // node:crypto asks for by default); absent means r||s (WebCrypto).
    let der_encoding = crate::node_ops::arg_string(scope, &args, 4).as_deref() == Some("der");
    let normalized = normalize_algorithm(&algo);
    let result = match key_type.as_str() {
        "rsa" => crypto_sign_rsa(&normalized, &data, &key_pem),
        "ec" => crypto_sign_ec(&normalized, &data, &key_pem, der_encoding),
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
    // Optional sixth argument, ECDSA only: as for cryptoSign's fifth.
    let der_encoding = crate::node_ops::arg_string(scope, &args, 5).as_deref() == Some("der");
    let normalized = normalize_algorithm(&algo);
    let result = match key_type.as_str() {
        "rsa" => crypto_verify_rsa(&normalized, &data, &key_pem, &signature),
        "ec" => crypto_verify_ec(&normalized, &data, &key_pem, &signature, der_encoding),
        "ed25519" => crypto_verify_ed25519(&data, &key_pem, &signature),
        _ => Err(format!("unsupported key type: '{key_type}'")),
    };
    match result {
        Ok(valid) => rv.set_bool(valid),
        Err(msg) => crate::node_ops::throw_type_error(scope, &msg),
    }
}

// ===================================================== RSA encrypt/decrypt
// Wave 4: publicEncrypt / privateDecrypt / privateEncrypt / publicDecrypt via
// the `rsa` crate.

/// The modulus sizes, in bits, a decoded RSA public key may have. Node reads
/// a public key of any size and its public-key operations work up to 16384
/// bits (measured on v22.22.2: 80- to 16384-bit keys encrypt and verify, a
/// 16392-bit one is `modulus too large`). The `rsa` crate's own default is
/// 1024..=8192, so the range is passed to it explicitly.
const RSA_PUBLIC_MODULUS_BITS: std::ops::RangeInclusive<usize> = 1..=16384;

/// The modulus sizes `generateKeyPair('rsa')` produces. The lower bound is
/// node's (511 bits is `key size too small` there, 512 generates).
const RSA_GENERATE_MODULUS_BITS: std::ops::RangeInclusive<usize> = 512..=16384;

// Node's messages for the conditions below, as OpenSSL words them. The JS
// layer (`rsaOpError` in js/node_compat.js) turns a message of this shape
// into an `Error` carrying node's `code`, `library` and `reason`.
const RSA_ERR_DATA_TOO_LARGE_FOR_KEY_SIZE: &str =
    "error:0200006E:rsa routines::data too large for key size";
const RSA_ERR_DATA_TOO_SMALL_FOR_KEY_SIZE: &str =
    "error:0200007A:rsa routines::data too small for key size";
const RSA_ERR_DATA_TOO_LARGE_FOR_MODULUS: &str =
    "error:02000084:rsa routines::data too large for modulus";
const RSA_ERR_DATA_GREATER_THAN_MOD_LEN: &str =
    "error:0200006C:rsa routines::data greater than mod len";
const RSA_ERR_DATA_TOO_SMALL: &str = "error:0200006F:rsa routines::data too small";
const RSA_ERR_OAEP_DECODING: &str = "error:02000079:rsa routines::oaep decoding error";
const RSA_ERR_INVALID_PADDING: &str = "error:0200008A:rsa routines::invalid padding";
const RSA_ERR_BLOCK_TYPE_IS_NOT_01: &str = "error:0200006A:rsa routines::block type is not 01";
const RSA_ERR_BAD_FIXED_HEADER: &str = "error:02000066:rsa routines::bad fixed header decrypt";
const RSA_ERR_NULL_BEFORE_BLOCK_MISSING: &str =
    "error:02000071:rsa routines::null before block missing";
const RSA_ERR_BAD_PAD_BYTE_COUNT: &str = "error:02000067:rsa routines::bad pad byte count";
const RSA_ERR_UNKNOWN_PADDING_TYPE: &str = "error:02000076:rsa routines::unknown padding type";
const RSA_ERR_MODULUS_TOO_LARGE: &str = "error:02000069:rsa routines::modulus too large";
const RSA_ERR_UNSUPPORTED_PADDING_MODE: &str =
    "error:1C8000A5:Provider routines::illegal or unsupported padding mode";
const RSA_ERR_EMPTY_RESULT: &str = "error:1C880004:Provider routines::RSA lib";

/// An `RSAPublicKey` (PKCS#1) structure. The integers are read with the
/// `pkcs1` crate and handed to the key constructor that takes the size range,
/// which is the route the `rsa` crate documents for a modulus outside its
/// default limits.
fn rsa_public_key_from_pkcs1(der: &[u8]) -> Result<RsaPublicKey, String> {
    use rsa::pkcs1::der::Decode;
    let raw = rsa::pkcs1::RsaPublicKeyRef::from_der(der)
        .map_err(|e| format!("RSA public key parse: {e}"))?;
    rsa_public_key_from_integers(raw.modulus.as_bytes(), raw.public_exponent.as_bytes())
}

/// A public key from its modulus and exponent, each big-endian.
fn rsa_public_key_from_integers(n: &[u8], e: &[u8]) -> Result<RsaPublicKey, String> {
    let n = BoxedUint::from_be_slice_vartime(n);
    let e = BoxedUint::from_be_slice_vartime(e);
    RsaPublicKey::new_with_size_limits(n, e, RSA_PUBLIC_MODULUS_BITS).map_err(|e| match e {
        rsa::Error::ModulusTooLarge => RSA_ERR_MODULUS_TOO_LARGE.to_string(),
        e => format!("RSA public key parse: {e}"),
    })
}

/// The `RSAPrivateKey` (PKCS#1) structure a private key carries: the whole
/// input under the `RSA PRIVATE KEY` label, the octet string inside a PKCS#8
/// `PrivateKeyInfo` under `PRIVATE KEY`. `what` names the key in an error.
fn rsa_private_key_structure<'a>(
    der: &'a [u8],
    label: &str,
    what: &str,
) -> Result<rsa::pkcs1::RsaPrivateKeyRef<'a>, String> {
    use rsa::pkcs8::der::Decode;
    let pkcs1 = match label {
        "RSA PRIVATE KEY" => der,
        "PRIVATE KEY" => {
            let info = rsa::pkcs8::PrivateKeyInfoRef::from_der(der)
                .map_err(|e| format!("{what} parse: {e}"))?;
            // rsaEncryption carries NULL parameters, id-RSASSA-PSS none:
            // the two forms the `rsa` crate's own PKCS#8 decoder takes.
            let algorithm = &info.algorithm;
            let known = if algorithm.oid == rsa::pkcs1::ALGORITHM_OID {
                algorithm
                    .parameters
                    .as_ref()
                    .is_some_and(|parameters| parameters.is_null())
            } else if algorithm.oid == RSASSA_PSS_OID {
                algorithm.parameters.is_none()
            } else {
                return Err(format!(
                    "{what} parse: unknown/unsupported algorithm OID: {}",
                    algorithm.oid
                ));
            };
            if !known {
                return Err(format!("{what} parse: malformed algorithm parameters"));
            }
            info.private_key.as_bytes()
        }
        _ => return Err(format!("unsupported PEM label for {what}: '{label}'")),
    };
    let raw =
        rsa::pkcs1::RsaPrivateKeyRef::from_der(pkcs1).map_err(|e| format!("{what} parse: {e}"))?;
    if raw.version() != rsa::pkcs1::Version::TwoPrime {
        return Err(format!("{what} parse: only two-prime keys are supported"));
    }
    Ok(raw)
}

/// id-RSASSA-PSS, the other algorithm a PKCS#8 RSA key may be filed under.
const RSASSA_PSS_OID: rsa::pkcs8::ObjectIdentifier =
    rsa::pkcs8::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.10");

/// The number of significant bits in a big-endian integer.
fn be_bit_length(bytes: &[u8]) -> usize {
    match bytes.iter().position(|&byte| byte != 0) {
        Some(first) => (bytes.len() - first) * 8 - bytes[first].leading_zeros() as usize,
        None => 0,
    }
}

fn parse_rsa_public_key(der: &[u8], label: &str) -> Result<RsaPublicKey, String> {
    match label {
        "PUBLIC KEY" => rsa_public_key_from_pkcs1(&extract_spki_pubkey(der)?),
        "RSA PUBLIC KEY" => rsa_public_key_from_pkcs1(der),
        // A private key stands for its public half: the modulus and public
        // exponent it carries, under the same size range as a public key.
        "PRIVATE KEY" | "RSA PRIVATE KEY" => {
            let raw = rsa_private_key_structure(der, label, "RSA key")?;
            rsa_public_key_from_integers(raw.modulus.as_bytes(), raw.public_exponent.as_bytes())
        }
        _ => Err(format!(
            "unsupported PEM label for RSA public key: '{label}'"
        )),
    }
}

/// A two-prime private key from PKCS#1 or PKCS#8.
///
/// The `rsa` crate stores the two factors at one of two widths: half the
/// modulus length, which is what its own decoders use and what a key from the
/// usual generators fits, or the full modulus length, for a key whose factors
/// differ in length by more than that allows. Node reads both kinds, so the
/// width is picked here from the lengths of the factors in the key file. A
/// key of the second kind is several times slower to use.
fn parse_rsa_private_key(der: &[u8], label: &str) -> Result<RsaPrivateKey, String> {
    let raw = rsa_private_key_structure(der, label, "RSA private key")?;
    let invalid = |e: rsa::Error| format!("RSA private key parse: {e}");

    let modulus = raw.modulus.as_bytes();
    let n = BoxedUint::from_be_slice_vartime(modulus);
    let e = BoxedUint::from_be_slice_vartime(raw.public_exponent.as_bytes());
    // The private integers take the modulus's storage width, as in the
    // crate's decoders; one that does not fit it is not part of this key.
    let width =
        u32::try_from(modulus.len() * 8).map_err(|_| invalid(rsa::Error::InvalidModulus))?;
    let private = |bytes: &[u8], error: rsa::Error| {
        BoxedUint::from_be_slice(bytes, width).map_err(|_| invalid(error))
    };
    let d = private(raw.private_exponent.as_bytes(), rsa::Error::InvalidExponent)?;
    let (p_bytes, q_bytes) = (raw.prime1.as_bytes(), raw.prime2.as_bytes());
    let primes = vec![
        private(p_bytes, rsa::Error::InvalidPrime)?,
        private(q_bytes, rsa::Error::InvalidPrime)?,
    ];

    let half = be_bit_length(modulus).div_ceil(2);
    let key = if be_bit_length(p_bytes) > half || be_bit_length(q_bytes) > half {
        RsaPrivateKey::from_components_with_wide_primes(n, e, d, primes)
    } else {
        RsaPrivateKey::from_components(n, e, d, primes)
    };
    key.map_err(invalid)
}

/// The padding an RSA encrypt/decrypt call names: `RSA_PKCS1_PADDING`,
/// `RSA_PKCS1_OAEP_PADDING` or `RSA_NO_PADDING`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RsaPadding {
    Pkcs1,
    Oaep,
    None,
}

impl RsaPadding {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "pkcs1" => Some(Self::Pkcs1),
            "oaep" => Some(Self::Oaep),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// The octet-string-to-integer step for a block that must be exactly `k`
/// bytes and below the modulus.
fn rsa_block_to_uint<K: PublicKeyParts>(key: &K, block: &[u8]) -> Result<BoxedUint, String> {
    let value = BoxedUint::from_be_slice(block, key.n_bits_precision())
        .map_err(|_| RSA_ERR_DATA_TOO_LARGE_FOR_MODULUS.to_string())?;
    if value >= *key.n().as_ref() {
        return Err(RSA_ERR_DATA_TOO_LARGE_FOR_MODULUS.into());
    }
    Ok(value)
}

/// The integer-to-octet-string step: the low `k` bytes of a value below the
/// modulus. The integer keeps the modulus's storage width whatever its
/// value, so the copy is the same length for every result.
fn rsa_uint_to_block(value: &BoxedUint, k: usize) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    bytes[bytes.len().saturating_sub(k)..].to_vec()
}

/// What `publicEncrypt` / `privateEncrypt` take under `RSA_NO_PADDING`: a
/// block of exactly the modulus length whose value is below the modulus.
fn rsa_unpadded_input<K: PublicKeyParts>(key: &K, data: &[u8]) -> Result<BoxedUint, String> {
    let k = key.size();
    match data.len().cmp(&k) {
        std::cmp::Ordering::Less => Err(RSA_ERR_DATA_TOO_SMALL_FOR_KEY_SIZE.into()),
        std::cmp::Ordering::Greater => Err(RSA_ERR_DATA_TOO_LARGE_FOR_KEY_SIZE.into()),
        std::cmp::Ordering::Equal => rsa_block_to_uint(key, data),
    }
}

/// What `privateDecrypt` / `publicDecrypt` take, under every padding: at most
/// the modulus length, read as a big-endian integer below the modulus. Node
/// accepts an input shorter than the modulus as that same integer written
/// without its leading zero bytes, so the block is left-padded to `k` bytes
/// here. Returns the `k`-byte block.
fn rsa_decrypt_input<K: PublicKeyParts>(key: &K, data: &[u8]) -> Result<Vec<u8>, String> {
    let k = key.size();
    if data.len() > k {
        return Err(RSA_ERR_DATA_GREATER_THAN_MOD_LEN.into());
    }
    let mut block = vec![0u8; k];
    block[k - data.len()..].copy_from_slice(data);
    rsa_block_to_uint(key, &block)?;
    Ok(block)
}

fn oaep_padding<D>(label: &[u8]) -> Oaep<D>
where
    D: RsaDigest + rsa::sha2::digest::FixedOutputReset,
{
    // An absent label and an empty one are the same label.
    if label.is_empty() {
        Oaep::new()
    } else {
        Oaep::new_with_label(label)
    }
}

/// Run `$body` with `$padding` bound to the OAEP scheme for the named hash.
/// The name is matched as `normalize_algorithm` spells it, and node's
/// `RSA-SHA*` spellings name the same hashes.
macro_rules! with_oaep_padding {
    ($hash:expr, $label:expr, |$padding:ident| $body:expr) => {{
        let normalized = normalize_algorithm($hash);
        match normalized.strip_prefix("rsa").unwrap_or(&normalized) {
            "sha1" => {
                let $padding = oaep_padding::<sha1_rsa::Sha1>($label);
                $body
            }
            "sha224" => {
                let $padding = oaep_padding::<rsa::sha2::Sha224>($label);
                $body
            }
            "sha256" => {
                let $padding = oaep_padding::<rsa::sha2::Sha256>($label);
                $body
            }
            "sha384" => {
                let $padding = oaep_padding::<rsa::sha2::Sha384>($label);
                $body
            }
            "sha512" => {
                let $padding = oaep_padding::<rsa::sha2::Sha512>($label);
                $body
            }
            _ => return Err(format!("unsupported OAEP hash: '{}'", $hash)),
        }
    }};
}

fn crypto_public_encrypt(
    data: &[u8],
    key_pem: &str,
    padding_type: &str,
    oaep_hash: &str,
    oaep_label: &[u8],
) -> Result<Vec<u8>, String> {
    let padding =
        RsaPadding::parse(padding_type).ok_or_else(|| RSA_ERR_UNKNOWN_PADDING_TYPE.to_string())?;
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let pub_key = parse_rsa_public_key(&der, label)?;
    fn too_long(what: &'static str) -> impl Fn(rsa::Error) -> String {
        move |e| match e {
            rsa::Error::MessageTooLong => RSA_ERR_DATA_TOO_LARGE_FOR_KEY_SIZE.to_string(),
            e => format!("RSA {what} encrypt: {e}"),
        }
    }

    match padding {
        RsaPadding::Oaep => with_oaep_padding!(oaep_hash, oaep_label, |scheme| pub_key
            .encrypt(&mut SysRng, scheme, data)
            .map_err(too_long("OAEP"))),
        RsaPadding::Pkcs1 => pub_key
            .encrypt(&mut SysRng, Pkcs1v15Encrypt, data)
            .map_err(too_long("PKCS1v15")),
        RsaPadding::None => {
            let m = rsa_unpadded_input(&pub_key, data)?;
            let c =
                rsa::hazmat::rsa_encrypt(&pub_key, &m).map_err(|e| format!("RSA encrypt: {e}"))?;
            Ok(rsa_uint_to_block(&c, pub_key.size()))
        }
    }
}

fn crypto_private_decrypt(
    data: &[u8],
    key_pem: &str,
    padding_type: &str,
    oaep_hash: &str,
    oaep_label: &[u8],
) -> Result<Vec<u8>, String> {
    let padding =
        RsaPadding::parse(padding_type).ok_or_else(|| RSA_ERR_UNKNOWN_PADDING_TYPE.to_string())?;
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let priv_key = parse_rsa_private_key(&der, label)?;
    if data.is_empty() {
        return Err(RSA_ERR_DATA_TOO_SMALL.into());
    }
    let block = rsa_decrypt_input(&priv_key, data)?;

    // `decrypt` draws on the system RNG itself and `rsa_decrypt_and_check`
    // is handed it: the crate randomises the private-key operation with it.
    match padding {
        RsaPadding::Oaep => with_oaep_padding!(oaep_hash, oaep_label, |scheme| priv_key
            .decrypt(scheme, &block)
            .map_err(|e| match e {
                rsa::Error::Decryption => RSA_ERR_OAEP_DECODING.to_string(),
                e => format!("RSA OAEP decrypt: {e}"),
            })),
        // The crate answers a block that does not decode as PKCS#1 v1.5 with
        // a message derived from the key and the block, the same bytes every
        // time, rather than an error. That is node's behaviour too, so it is
        // passed through as the plaintext.
        RsaPadding::Pkcs1 => priv_key
            .decrypt(Pkcs1v15Encrypt, &block)
            .map_err(|e| format!("RSA PKCS1v15 decrypt: {e}")),
        RsaPadding::None => {
            let c = rsa_block_to_uint(&priv_key, &block)?;
            let m = rsa::hazmat::rsa_decrypt_and_check(&priv_key, Some(&mut SysRng), &c)
                .map_err(|e| format!("RSA decrypt: {e}"))?;
            Ok(rsa_uint_to_block(&m, priv_key.size()))
        }
    }
}

fn crypto_private_encrypt(
    data: &[u8],
    key_pem: &str,
    padding_type: &str,
) -> Result<Vec<u8>, String> {
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let priv_key = parse_rsa_private_key(&der, label)?;
    let key_len = priv_key.size();
    match RsaPadding::parse(padding_type) {
        Some(RsaPadding::Pkcs1) => {
            if data.len() + 11 > key_len {
                return Err(RSA_ERR_DATA_TOO_LARGE_FOR_KEY_SIZE.into());
            }
            // privateEncrypt is PKCS#1 v1.5 type-1 padding (00 01 FF.. 00 ||
            // data) with no DigestInfo, which is exactly the `rsa` crate's
            // unprefixed signing scheme. Going through it keeps the
            // private-key operation on the crate's own randomised, CRT,
            // result-checked path.
            priv_key
                .sign(rsa::Pkcs1v15Sign::new_unprefixed(), data)
                .map_err(|e| format!("privateEncrypt: {e}"))
        }
        Some(RsaPadding::None) => {
            let m = rsa_unpadded_input(&priv_key, data)?;
            let c = rsa::hazmat::rsa_decrypt_and_check(&priv_key, Some(&mut SysRng), &m)
                .map_err(|e| format!("privateEncrypt: {e}"))?;
            Ok(rsa_uint_to_block(&c, key_len))
        }
        // OAEP, and any other value, is not a padding this direction has.
        _ => Err(RSA_ERR_UNSUPPORTED_PADDING_MODE.into()),
    }
}

/// Remove PKCS#1 v1.5 type-1 padding (00 01 FF.. 00 || data) from a block of
/// the modulus length. Everything here is public data. Each refusal is the
/// one node reports for that defect.
fn pkcs1_type1_unpad(em: &[u8]) -> Result<Vec<u8>, String> {
    if em.len() < 11 || em[0] != 0x00 {
        return Err(RSA_ERR_INVALID_PADDING.into());
    }
    if em[1] != 0x01 {
        return Err(RSA_ERR_BLOCK_TYPE_IS_NOT_01.into());
    }
    let mut sep = None;
    for (i, &byte) in em.iter().enumerate().skip(2) {
        if byte == 0x00 {
            sep = Some(i);
            break;
        }
        if byte != 0xFF {
            return Err(RSA_ERR_BAD_FIXED_HEADER.into());
        }
    }
    match sep {
        None => Err(RSA_ERR_NULL_BEFORE_BLOCK_MISSING.into()),
        // Fewer than eight 0xFF bytes between the header and the separator.
        Some(idx) if idx < 10 => Err(RSA_ERR_BAD_PAD_BYTE_COUNT.into()),
        // Node reports a padded block that carries no data as a failure.
        Some(idx) if idx + 1 == em.len() => Err(RSA_ERR_EMPTY_RESULT.into()),
        Some(idx) => Ok(em[idx + 1..].to_vec()),
    }
}

fn crypto_public_decrypt(
    data: &[u8],
    key_pem: &str,
    padding_type: &str,
) -> Result<Vec<u8>, String> {
    let padding = match RsaPadding::parse(padding_type) {
        Some(padding @ (RsaPadding::Pkcs1 | RsaPadding::None)) => padding,
        _ => return Err(RSA_ERR_UNSUPPORTED_PADDING_MODE.into()),
    };
    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    // A private key is accepted too: only its public half is used.
    let pub_key = parse_rsa_public_key(&der, label)?;
    let block = rsa_decrypt_input(&pub_key, data)?;
    let c = rsa_block_to_uint(&pub_key, &block)?;
    let m = rsa::hazmat::rsa_encrypt(&pub_key, &c).map_err(|e| format!("publicDecrypt: {e}"))?;
    let em = rsa_uint_to_block(&m, pub_key.size());
    match padding {
        RsaPadding::None => Ok(em),
        _ => pkcs1_type1_unpad(&em),
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
    let oaep_label = crate::node_ops::arg_bytes(scope, &args, 4).unwrap_or_default();

    match crypto_public_encrypt(&data, &key_pem, &padding, &oaep_hash, &oaep_label) {
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
    let oaep_label = crate::node_ops::arg_bytes(scope, &args, 4).unwrap_or_default();

    match crypto_private_decrypt(&data, &key_pem, &padding, &oaep_hash, &oaep_label) {
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
    let padding =
        crate::node_ops::arg_string(scope, &args, 2).unwrap_or_else(|| "pkcs1".to_string());
    match crypto_private_encrypt(&data, &key_pem, &padding) {
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
    let padding =
        crate::node_ops::arg_string(scope, &args, 2).unwrap_or_else(|| "pkcs1".to_string());
    match crypto_public_decrypt(&data, &key_pem, &padding) {
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
    let pub_pkcs1 = match priv_key.as_public_key().to_pkcs1_der() {
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
        let parts = match rsa_private_jwk_parts(&priv_key) {
            Ok(parts) => parts,
            Err(e) => {
                crate::node_ops::throw_type_error(scope, &e);
                return;
            }
        };
        set_bytes_prop!("n", parts.n);
        set_bytes_prop!("e", parts.e);
        set_bytes_prop!("d", parts.d);
        set_bytes_prop!("p", parts.p);
        set_bytes_prop!("q", parts.q);
        set_bytes_prop!("dp", parts.dp);
        set_bytes_prop!("dq", parts.dq);
        set_bytes_prop!("qi", parts.qi);
    } else {
        let pub_key = match parse_rsa_public_key(&der, label) {
            Ok(k) => k,
            Err(e) => {
                crate::node_ops::throw_type_error(scope, &e);
                return;
            }
        };
        set_bytes_prop!("n", pub_key.n_bytes().into_vec());
        set_bytes_prop!("e", pub_key.e_bytes().into_vec());
    }

    rv.set(obj.into());
}

/// The eight integers of an RSA private-key JWK (RFC 7518 section 6.3), each
/// the minimal big-endian encoding the format asks for.
#[derive(Debug, PartialEq, Eq)]
struct RsaPrivateJwkParts {
    n: Vec<u8>,
    e: Vec<u8>,
    d: Vec<u8>,
    p: Vec<u8>,
    q: Vec<u8>,
    dp: Vec<u8>,
    dq: Vec<u8>,
    qi: Vec<u8>,
}

/// The JWK integers of a two-prime key. `dp`, `dq` and `qi` are the key's own
/// CRT parameters as the `rsa` crate holds them: d mod (p-1), d mod (q-1) and
/// the inverse of q modulo p.
fn rsa_private_jwk_parts(key: &RsaPrivateKey) -> Result<RsaPrivateJwkParts, String> {
    let minimal = |value: &BoxedUint| value.to_be_bytes_trimmed_vartime().into_vec();
    let [p, q] = key.primes() else {
        return Err("RSA JWK export: only two-prime keys are supported".into());
    };
    let no_crt = || "RSA JWK export: the key has no CRT parameters".to_string();
    let dp = key.dp().ok_or_else(no_crt)?;
    let dq = key.dq().ok_or_else(no_crt)?;
    let qi = key.crt_coefficient().ok_or_else(no_crt)?;
    Ok(RsaPrivateJwkParts {
        n: key.n_bytes().into_vec(),
        e: key.e_bytes().into_vec(),
        d: minimal(key.d()),
        p: minimal(p.as_ref()),
        q: minimal(q.as_ref()),
        dp: minimal(dp),
        dq: minimal(dq),
        qi: minimal(&qi),
    })
}

fn crypto_generate_rsa(bits: usize) -> Result<(Vec<u8>, Vec<u8>), String> {
    if !RSA_GENERATE_MODULUS_BITS.contains(&bits) {
        return Err(format!(
            "generateKeyPairSync: modulusLength must be between {} and {}",
            RSA_GENERATE_MODULUS_BITS.start(),
            RSA_GENERATE_MODULUS_BITS.end()
        ));
    }
    // `new_unchecked` is `new` without the crate's default 1024-bit floor;
    // the range check above is the floor here, and it is node's.
    let priv_key =
        RsaPrivateKey::new_unchecked(&mut SysRng, bits).map_err(|e| format!("RSA keygen: {e}"))?;

    let priv_doc = priv_key
        .to_pkcs8_der()
        .map_err(|e| format!("RSA private key encode: {e}"))?;
    let pub_pkcs1 = priv_key
        .as_public_key()
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

/// `crypto.constants.RSA_PSS_SALTLEN_DIGEST`: the salt is as long as the
/// digest.
const RSA_PSS_SALTLEN_DIGEST: i64 = -1;
/// `RSA_PSS_SALTLEN_MAX_SIGN` when signing (the longest salt the key holds)
/// and `RSA_PSS_SALTLEN_AUTO` when verifying (read the length off the
/// signature). Node gives both the value -2 and makes each the default of
/// its direction.
const RSA_PSS_SALTLEN_AUTO: i64 = -2;

/// The `saltLength` argument as the JS layer hands it over: `undefined`
/// (NaN here) is Node's default, -1 and -2 are the sentinels above, and a
/// non-negative integer is a literal length. Anything else is refused
/// rather than saturated to 0 by an `as usize` cast.
fn pss_salt_arg(raw: f64) -> Result<i64, String> {
    if raw.is_nan() {
        return Ok(RSA_PSS_SALTLEN_AUTO);
    }
    if raw.fract() != 0.0 || raw < -2.0 || raw > f64::from(i32::MAX) {
        return Err(format!("Invalid saltLength: {raw}"));
    }
    Ok(raw as i64)
}

/// The longest salt EMSA-PSS can encode for a modulus of `mod_bits` bits and
/// a digest of `h_len` bytes: emLen - hLen - 2 (RFC 8017 section 9.1.1).
/// `None` when the key cannot hold even an empty salt.
fn pss_max_salt_len(mod_bits: usize, h_len: usize) -> Option<usize> {
    let em_len = mod_bits.checked_sub(1)?.div_ceil(8);
    em_len.checked_sub(h_len.checked_add(2)?)
}

/// The salt length a signature is made with. A length the key cannot encode
/// is an error here, so it never reaches the signer.
fn pss_sign_salt_len(salt_length: i64, mod_bits: usize, h_len: usize) -> Result<usize, String> {
    let max = pss_max_salt_len(mod_bits, h_len)
        .ok_or_else(|| "RSA-PSS sign: key is too small for this digest".to_string())?;
    let want = match salt_length {
        RSA_PSS_SALTLEN_DIGEST => h_len,
        RSA_PSS_SALTLEN_AUTO => max,
        n if n >= 0 => usize::try_from(n).unwrap_or(usize::MAX),
        n => return Err(format!("Invalid saltLength: {n}")),
    };
    if want > max {
        return Err(format!(
            "RSA-PSS sign: salt length {want} is too large for this key and digest (max {max})"
        ));
    }
    Ok(want)
}

fn crypto_sign_rsa_pss(
    algo: &str,
    data: &[u8],
    key_pem: &str,
    salt_length: i64,
) -> Result<Vec<u8>, String> {
    use rsa::pss::SigningKey;
    use rsa::signature::RandomizedSigner;

    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);
    let priv_key = parse_rsa_private_key(&der, label)?;
    let mod_bits = priv_key.n().bit_width() as usize;

    // `try_sign_with_rng`, not `sign_with_rng`: the latter is the former
    // plus an `expect`, and a refused encoding has to reach JS as a throw.
    // The RNG supplies the salt and randomises the private-key operation.
    macro_rules! pss_sign {
        ($d:ty) => {{
            let h_len = <$d as RsaDigest>::output_size();
            let salt_len = pss_sign_salt_len(salt_length, mod_bits, h_len)?;
            let signing_key = SigningKey::<$d>::new_with_salt_len(priv_key, salt_len);
            signing_key
                .try_sign_with_rng(&mut SysRng, data)
                .map(|sig| sig.to_bytes().to_vec())
                .map_err(|e| format!("RSA-PSS sign: {e}"))
        }};
    }

    match algo {
        "sha256" => pss_sign!(rsa::sha2::Sha256),
        "sha384" => pss_sign!(rsa::sha2::Sha384),
        "sha512" => pss_sign!(rsa::sha2::Sha512),
        "sha1" => pss_sign!(sha1_rsa::Sha1),
        _ => Err(format!("unsupported RSA-PSS hash: '{algo}'")),
    }
}

fn crypto_verify_rsa_pss(
    algo: &str,
    data: &[u8],
    key_pem: &str,
    signature: &[u8],
    salt_length: i64,
) -> Result<bool, String> {
    use rsa::pss::VerifyingKey;
    use rsa::signature::Verifier;

    let der = pem_to_der(key_pem)?;
    let label = pem_label(key_pem);

    // A private key verifies with its public half.
    let pub_key = match parse_rsa_public_key(&der, label) {
        Ok(pub_key) => pub_key,
        // Node answers `false` for a key whose modulus is too large to use.
        Err(e) if e == RSA_ERR_MODULUS_TOO_LARGE => return Ok(false),
        Err(e) => return Err(e),
    };

    // A signature is exactly as long as the modulus; any other length does
    // not verify. The bound is passed explicitly because the type's own
    // default stops at the crate's default 8192-bit modulus.
    let Ok(sig) = rsa::pss::Signature::from_bytes_with_max_len(signature, pub_key.size()) else {
        return Ok(false);
    };

    let mod_bits = pub_key.n().bit_width() as usize;

    // A length the key cannot encode is a signature that does not verify --
    // not an error. RSA_PSS_SALTLEN_AUTO is the crate's own "read the salt
    // length off the signature" verifier.
    macro_rules! pss_verify {
        ($d:ty) => {{
            let h_len = <$d as RsaDigest>::output_size();
            let max = pss_max_salt_len(mod_bits, h_len);
            let fixed = |salt_len: usize| match max {
                Some(max) if salt_len <= max => {
                    let vk = VerifyingKey::<$d>::new_with_salt_len(pub_key.clone(), salt_len);
                    vk.verify(data, &sig).is_ok()
                }
                _ => false,
            };
            Ok(match salt_length {
                RSA_PSS_SALTLEN_DIGEST => fixed(h_len),
                RSA_PSS_SALTLEN_AUTO => {
                    max.is_some()
                        && VerifyingKey::<$d>::new_with_auto_salt_len(pub_key.clone())
                            .verify(data, &sig)
                            .is_ok()
                }
                n => usize::try_from(n).map(fixed).unwrap_or(false),
            })
        }};
    }

    match algo {
        "sha256" => pss_verify!(rsa::sha2::Sha256),
        "sha384" => pss_verify!(rsa::sha2::Sha384),
        "sha512" => pss_verify!(rsa::sha2::Sha512),
        "sha1" => pss_verify!(sha1_rsa::Sha1),
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
    let salt_length = match pss_salt_arg(args.get(3).number_value(scope).unwrap_or(f64::NAN)) {
        Ok(salt_length) => salt_length,
        Err(msg) => {
            crate::node_ops::throw_type_error(scope, &msg);
            return;
        }
    };
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
    let salt_length = match pss_salt_arg(args.get(4).number_value(scope).unwrap_or(f64::NAN)) {
        Ok(salt_length) => salt_length,
        Err(msg) => {
            crate::node_ops::throw_type_error(scope, &msg);
            return;
        }
    };
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
//
// node's DiffieHellman is OpenSSL's DH: the rules below (which groups are
// "named", how long a fresh private value is, which peer values are refused,
// what verifyError reports) follow OpenSSL 3.5's dh_key.c / dh_check.c /
// ffc_dh.c as node v22.22.2 links them, and were measured against that node.
// Every exponentiation with a private exponent runs in crypto-bigint's
// Montgomery form over a fixed exponent width (`dh_exponent_bits`): the
// group's private-value width, or a caller-set key's own byte width when that
// is longer. Values stay fixed-width until the final encode.

use num_bigint_dig::BigUint;

use crypto_bigint::modular::{BoxedMontyForm, BoxedMontyParams};
use crypto_bigint::zeroize::Zeroize;
use crypto_bigint::{CtEq as _, CtLt as _, Odd, Resize as _};

/// OpenSSL's DH_MIN_MODULUS_BITS: a smaller modulus refuses key generation.
const DH_MIN_MODULUS_BITS: u32 = 512;
/// OPENSSL_DH_MAX_MODULUS_BITS: a larger modulus refuses key generation and
/// key agreement.
const DH_MAX_MODULUS_BITS: u32 = 10_000;
/// OPENSSL_DH_CHECK_MAX_MODULUS_BITS: past this, parameter and peer checks
/// fail outright.
const DH_CHECK_MAX_MODULUS_BITS: u32 = 32_768;

/// DH_check flags (crypto.constants carries the first four).
const DH_CHECK_P_NOT_PRIME: i32 = 0x01;
const DH_CHECK_P_NOT_SAFE_PRIME: i32 = 0x02;
const DH_NOT_SUITABLE_GENERATOR: i32 = 0x08;
const DH_MODULUS_TOO_SMALL: i32 = 0x80;
const DH_MODULUS_TOO_LARGE: i32 = 0x100;

/// A group OpenSSL recognises by its (p, g): RFC 7919 ffdhe, RFC 3526 MODP
/// from 1536 bits, and the three RFC 5114 groups. Such a group carries a
/// subgroup order q and a private-value length; `private_bits` 0 means twice
/// the modulus' security strength. A safe-prime group's q is (p - 1) / 2.
struct DhNamedGroup {
    #[cfg_attr(not(test), allow(dead_code))]
    name: &'static str,
    private_bits: u32,
    p: &'static str,
    q: Option<&'static str>,
    g: &'static str,
}

static DH_NAMED_GROUPS: [DhNamedGroup; 14] = [
    DhNamedGroup {
        name: "ffdhe2048",
        private_bits: 225,
        p: "ffffffffffffffffadf85458a2bb4a9aafdc5620273d3cf1d8b9c583ce2d3695\
         a9e13641146433fbcc939dce249b3ef97d2fe363630c75d8f681b202aec4617a\
         d3df1ed5d5fd65612433f51f5f066ed0856365553ded1af3b557135e7f57c935\
         984f0c70e0e68b77e2a689daf3efe8721df158a136ade73530acca4f483a797a\
         bc0ab182b324fb61d108a94bb2c8e3fbb96adab760d7f4681d4f42a3de394df4\
         ae56ede76372bb190b07a7c8ee0a6d709e02fce1cdf7e2ecc03404cd28342f61\
         9172fe9ce98583ff8e4f1232eef28183c3fe3b1b4c6fad733bb5fcbc2ec22005\
         c58ef1837d1683b2c6f34a26c1b2effa886b423861285c97ffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "ffdhe3072",
        private_bits: 275,
        p: "ffffffffffffffffadf85458a2bb4a9aafdc5620273d3cf1d8b9c583ce2d3695\
         a9e13641146433fbcc939dce249b3ef97d2fe363630c75d8f681b202aec4617a\
         d3df1ed5d5fd65612433f51f5f066ed0856365553ded1af3b557135e7f57c935\
         984f0c70e0e68b77e2a689daf3efe8721df158a136ade73530acca4f483a797a\
         bc0ab182b324fb61d108a94bb2c8e3fbb96adab760d7f4681d4f42a3de394df4\
         ae56ede76372bb190b07a7c8ee0a6d709e02fce1cdf7e2ecc03404cd28342f61\
         9172fe9ce98583ff8e4f1232eef28183c3fe3b1b4c6fad733bb5fcbc2ec22005\
         c58ef1837d1683b2c6f34a26c1b2effa886b4238611fcfdcde355b3b6519035b\
         bc34f4def99c023861b46fc9d6e6c9077ad91d2691f7f7ee598cb0fac186d91c\
         aefe130985139270b4130c93bc437944f4fd4452e2d74dd364f2e21e71f54bff\
         5cae82ab9c9df69ee86d2bc522363a0dabc521979b0deada1dbf9a42d5c4484e\
         0abcd06bfa53ddef3c1b20ee3fd59d7c25e41d2b66c62e37ffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "ffdhe4096",
        private_bits: 325,
        p: "ffffffffffffffffadf85458a2bb4a9aafdc5620273d3cf1d8b9c583ce2d3695\
         a9e13641146433fbcc939dce249b3ef97d2fe363630c75d8f681b202aec4617a\
         d3df1ed5d5fd65612433f51f5f066ed0856365553ded1af3b557135e7f57c935\
         984f0c70e0e68b77e2a689daf3efe8721df158a136ade73530acca4f483a797a\
         bc0ab182b324fb61d108a94bb2c8e3fbb96adab760d7f4681d4f42a3de394df4\
         ae56ede76372bb190b07a7c8ee0a6d709e02fce1cdf7e2ecc03404cd28342f61\
         9172fe9ce98583ff8e4f1232eef28183c3fe3b1b4c6fad733bb5fcbc2ec22005\
         c58ef1837d1683b2c6f34a26c1b2effa886b4238611fcfdcde355b3b6519035b\
         bc34f4def99c023861b46fc9d6e6c9077ad91d2691f7f7ee598cb0fac186d91c\
         aefe130985139270b4130c93bc437944f4fd4452e2d74dd364f2e21e71f54bff\
         5cae82ab9c9df69ee86d2bc522363a0dabc521979b0deada1dbf9a42d5c4484e\
         0abcd06bfa53ddef3c1b20ee3fd59d7c25e41d2b669e1ef16e6f52c3164df4fb\
         7930e9e4e58857b6ac7d5f42d69f6d187763cf1d5503400487f55ba57e31cc7a\
         7135c886efb4318aed6a1e012d9e6832a907600a918130c46dc778f971ad0038\
         092999a333cb8b7a1a1db93d7140003c2a4ecea9f98d0acc0a8291cdcec97dcf\
         8ec9b55a7f88a46b4db5a851f44182e1c68a007e5e655f6affffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "ffdhe6144",
        private_bits: 375,
        p: "ffffffffffffffffadf85458a2bb4a9aafdc5620273d3cf1d8b9c583ce2d3695\
         a9e13641146433fbcc939dce249b3ef97d2fe363630c75d8f681b202aec4617a\
         d3df1ed5d5fd65612433f51f5f066ed0856365553ded1af3b557135e7f57c935\
         984f0c70e0e68b77e2a689daf3efe8721df158a136ade73530acca4f483a797a\
         bc0ab182b324fb61d108a94bb2c8e3fbb96adab760d7f4681d4f42a3de394df4\
         ae56ede76372bb190b07a7c8ee0a6d709e02fce1cdf7e2ecc03404cd28342f61\
         9172fe9ce98583ff8e4f1232eef28183c3fe3b1b4c6fad733bb5fcbc2ec22005\
         c58ef1837d1683b2c6f34a26c1b2effa886b4238611fcfdcde355b3b6519035b\
         bc34f4def99c023861b46fc9d6e6c9077ad91d2691f7f7ee598cb0fac186d91c\
         aefe130985139270b4130c93bc437944f4fd4452e2d74dd364f2e21e71f54bff\
         5cae82ab9c9df69ee86d2bc522363a0dabc521979b0deada1dbf9a42d5c4484e\
         0abcd06bfa53ddef3c1b20ee3fd59d7c25e41d2b669e1ef16e6f52c3164df4fb\
         7930e9e4e58857b6ac7d5f42d69f6d187763cf1d5503400487f55ba57e31cc7a\
         7135c886efb4318aed6a1e012d9e6832a907600a918130c46dc778f971ad0038\
         092999a333cb8b7a1a1db93d7140003c2a4ecea9f98d0acc0a8291cdcec97dcf\
         8ec9b55a7f88a46b4db5a851f44182e1c68a007e5e0dd9020bfd64b645036c7a\
         4e677d2c38532a3a23ba4442caf53ea63bb454329b7624c8917bdd64b1c0fd4c\
         b38e8c334c701c3acdad0657fccfec719b1f5c3e4e46041f388147fb4cfdb477\
         a52471f7a9a96910b855322edb6340d8a00ef092350511e30abec1fff9e3a26e\
         7fb29f8c183023c3587e38da0077d9b4763e4e4b94b2bbc194c6651e77caf992\
         eeaac0232a281bf6b3a739c1226116820ae8db5847a67cbef9c9091b462d538c\
         d72b03746ae77f5e62292c311562a846505dc82db854338ae49f5235c95b9117\
         8ccf2dd5cacef403ec9d1810c6272b045b3b71f9dc6b80d63fdd4a8e9adb1e69\
         62a69526d43161c1a41d570d7938dad4a40e329cd0e40e65ffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "ffdhe8192",
        private_bits: 400,
        p: "ffffffffffffffffadf85458a2bb4a9aafdc5620273d3cf1d8b9c583ce2d3695\
         a9e13641146433fbcc939dce249b3ef97d2fe363630c75d8f681b202aec4617a\
         d3df1ed5d5fd65612433f51f5f066ed0856365553ded1af3b557135e7f57c935\
         984f0c70e0e68b77e2a689daf3efe8721df158a136ade73530acca4f483a797a\
         bc0ab182b324fb61d108a94bb2c8e3fbb96adab760d7f4681d4f42a3de394df4\
         ae56ede76372bb190b07a7c8ee0a6d709e02fce1cdf7e2ecc03404cd28342f61\
         9172fe9ce98583ff8e4f1232eef28183c3fe3b1b4c6fad733bb5fcbc2ec22005\
         c58ef1837d1683b2c6f34a26c1b2effa886b4238611fcfdcde355b3b6519035b\
         bc34f4def99c023861b46fc9d6e6c9077ad91d2691f7f7ee598cb0fac186d91c\
         aefe130985139270b4130c93bc437944f4fd4452e2d74dd364f2e21e71f54bff\
         5cae82ab9c9df69ee86d2bc522363a0dabc521979b0deada1dbf9a42d5c4484e\
         0abcd06bfa53ddef3c1b20ee3fd59d7c25e41d2b669e1ef16e6f52c3164df4fb\
         7930e9e4e58857b6ac7d5f42d69f6d187763cf1d5503400487f55ba57e31cc7a\
         7135c886efb4318aed6a1e012d9e6832a907600a918130c46dc778f971ad0038\
         092999a333cb8b7a1a1db93d7140003c2a4ecea9f98d0acc0a8291cdcec97dcf\
         8ec9b55a7f88a46b4db5a851f44182e1c68a007e5e0dd9020bfd64b645036c7a\
         4e677d2c38532a3a23ba4442caf53ea63bb454329b7624c8917bdd64b1c0fd4c\
         b38e8c334c701c3acdad0657fccfec719b1f5c3e4e46041f388147fb4cfdb477\
         a52471f7a9a96910b855322edb6340d8a00ef092350511e30abec1fff9e3a26e\
         7fb29f8c183023c3587e38da0077d9b4763e4e4b94b2bbc194c6651e77caf992\
         eeaac0232a281bf6b3a739c1226116820ae8db5847a67cbef9c9091b462d538c\
         d72b03746ae77f5e62292c311562a846505dc82db854338ae49f5235c95b9117\
         8ccf2dd5cacef403ec9d1810c6272b045b3b71f9dc6b80d63fdd4a8e9adb1e69\
         62a69526d43161c1a41d570d7938dad4a40e329ccff46aaa36ad004cf600c838\
         1e425a31d951ae64fdb23fcec9509d43687feb69edd1cc5e0b8cc3bdf64b10ef\
         86b63142a3ab8829555b2f747c932665cb2c0f1cc01bd70229388839d2af05e4\
         54504ac78b7582822846c0ba35c35f5c59160cc046fd8251541fc68c9c86b022\
         bb7099876a460e7451a8a93109703fee1c217e6c3826e52c51aa691e0e423cfc\
         99e9e31650c1217b624816cdad9a95f9d5b8019488d9c0a0a1fe3075a577e231\
         83f81d4a3f2fa4571efc8ce0ba8a4fe8b6855dfe72b0a66eded2fbabfbe58a30\
         fafabe1c5d71a87e2f741ef8c1fe86fea6bbfde530677f0d97d11d49f7a8443d\
         0822e506a9f4614e011e2a94838ff88cd68c8bb7c5c6424cffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "modp_1536",
        private_bits: 200,
        p: "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74\
         020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f1437\
         4fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7ed\
         ee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf05\
         98da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb\
         9ed529077096966d670c354e4abc9804f1746c08ca237327ffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "modp_2048",
        private_bits: 225,
        p: "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74\
         020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f1437\
         4fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7ed\
         ee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf05\
         98da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb\
         9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3b\
         e39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf695581718\
         3995497cea956ae515d2261898fa051015728e5a8aacaa68ffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "modp_3072",
        private_bits: 275,
        p: "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74\
         020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f1437\
         4fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7ed\
         ee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf05\
         98da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb\
         9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3b\
         e39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf695581718\
         3995497cea956ae515d2261898fa051015728e5a8aaac42dad33170d04507a33\
         a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7\
         abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864\
         d87602733ec86a64521f2b18177b200cbbe117577a615d6c770988c0bad946e2\
         08e24fa074e5ab3143db5bfce0fd108e4b82d120a93ad2caffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "modp_4096",
        private_bits: 325,
        p: "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74\
         020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f1437\
         4fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7ed\
         ee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf05\
         98da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb\
         9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3b\
         e39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf695581718\
         3995497cea956ae515d2261898fa051015728e5a8aaac42dad33170d04507a33\
         a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7\
         abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864\
         d87602733ec86a64521f2b18177b200cbbe117577a615d6c770988c0bad946e2\
         08e24fa074e5ab3143db5bfce0fd108e4b82d120a92108011a723c12a787e6d7\
         88719a10bdba5b2699c327186af4e23c1a946834b6150bda2583e9ca2ad44ce8\
         dbbbc2db04de8ef92e8efc141fbecaa6287c59474e6bc05d99b2964fa090c3a2\
         233ba186515be7ed1f612970cee2d7afb81bdd762170481cd0069127d5b05aa9\
         93b4ea988d8fddc186ffb7dc90a6c08f4df435c934063199ffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "modp_6144",
        private_bits: 375,
        p: "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74\
         020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f1437\
         4fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7ed\
         ee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf05\
         98da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb\
         9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3b\
         e39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf695581718\
         3995497cea956ae515d2261898fa051015728e5a8aaac42dad33170d04507a33\
         a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7\
         abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864\
         d87602733ec86a64521f2b18177b200cbbe117577a615d6c770988c0bad946e2\
         08e24fa074e5ab3143db5bfce0fd108e4b82d120a92108011a723c12a787e6d7\
         88719a10bdba5b2699c327186af4e23c1a946834b6150bda2583e9ca2ad44ce8\
         dbbbc2db04de8ef92e8efc141fbecaa6287c59474e6bc05d99b2964fa090c3a2\
         233ba186515be7ed1f612970cee2d7afb81bdd762170481cd0069127d5b05aa9\
         93b4ea988d8fddc186ffb7dc90a6c08f4df435c93402849236c3fab4d27c7026\
         c1d4dcb2602646dec9751e763dba37bdf8ff9406ad9e530ee5db382f413001ae\
         b06a53ed9027d831179727b0865a8918da3edbebcf9b14ed44ce6cbaced4bb1b\
         db7f1447e6cc254b332051512bd7af426fb8f401378cd2bf5983ca01c64b92ec\
         f032ea15d1721d03f482d7ce6e74fef6d55e702f46980c82b5a84031900b1c9e\
         59e7c97fbec7e8f323a97a7e36cc88be0f1d45b7ff585ac54bd407b22b4154aa\
         cc8f6d7ebf48e1d814cc5ed20f8037e0a79715eef29be32806a1d58bb7c5da76\
         f550aa3d8a1fbff0eb19ccb1a313d55cda56c9ec2ef29632387fe8d76e3c0468\
         043e8f663f4860ee12bf2d5b0b7474d6e694f91e6dcc4024ffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "modp_8192",
        private_bits: 400,
        p: "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74\
         020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f1437\
         4fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7ed\
         ee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf05\
         98da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb\
         9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3b\
         e39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf695581718\
         3995497cea956ae515d2261898fa051015728e5a8aaac42dad33170d04507a33\
         a85521abdf1cba64ecfb850458dbef0a8aea71575d060c7db3970f85a6e1e4c7\
         abf5ae8cdb0933d71e8c94e04a25619dcee3d2261ad2ee6bf12ffa06d98a0864\
         d87602733ec86a64521f2b18177b200cbbe117577a615d6c770988c0bad946e2\
         08e24fa074e5ab3143db5bfce0fd108e4b82d120a92108011a723c12a787e6d7\
         88719a10bdba5b2699c327186af4e23c1a946834b6150bda2583e9ca2ad44ce8\
         dbbbc2db04de8ef92e8efc141fbecaa6287c59474e6bc05d99b2964fa090c3a2\
         233ba186515be7ed1f612970cee2d7afb81bdd762170481cd0069127d5b05aa9\
         93b4ea988d8fddc186ffb7dc90a6c08f4df435c93402849236c3fab4d27c7026\
         c1d4dcb2602646dec9751e763dba37bdf8ff9406ad9e530ee5db382f413001ae\
         b06a53ed9027d831179727b0865a8918da3edbebcf9b14ed44ce6cbaced4bb1b\
         db7f1447e6cc254b332051512bd7af426fb8f401378cd2bf5983ca01c64b92ec\
         f032ea15d1721d03f482d7ce6e74fef6d55e702f46980c82b5a84031900b1c9e\
         59e7c97fbec7e8f323a97a7e36cc88be0f1d45b7ff585ac54bd407b22b4154aa\
         cc8f6d7ebf48e1d814cc5ed20f8037e0a79715eef29be32806a1d58bb7c5da76\
         f550aa3d8a1fbff0eb19ccb1a313d55cda56c9ec2ef29632387fe8d76e3c0468\
         043e8f663f4860ee12bf2d5b0b7474d6e694f91e6dbe115974a3926f12fee5e4\
         38777cb6a932df8cd8bec4d073b931ba3bc832b68d9dd300741fa7bf8afc47ed\
         2576f6936ba424663aab639c5ae4f5683423b4742bf1c978238f16cbe39d652d\
         e3fdb8befc848ad922222e04a4037c0713eb57a81a23f0c73473fc646cea306b\
         4bcbc8862f8385ddfa9d4b7fa2c087e879683303ed5bdd3a062b3cf5b3a278a6\
         6d2a13f83f44f82ddf310ee074ab6a364597e899a0255dc164f31cc50846851d\
         f9ab48195ded7ea1b1d510bd7ee74d73faf36bc31ecfa268359046f4eb879f92\
         4009438b481c6cd7889a002ed5ee382bc9190da6fc026e479558e4475677e9aa\
         9e3050e2765694dfc81f56e880b96e7160c980dd98edd3dfffffffffffffffff",
        q: None,
        g: "02",
    },
    DhNamedGroup {
        name: "dh1024_160",
        private_bits: 0,
        p: "b10b8f96a080e01dde92de5eae5d54ec52c99fbcfb06a3c69a6a9dca52d23b61\
         6073e28675a23d189838ef1e2ee652c013ecb4aea906112324975c3cd49b83bf\
         accbdd7d90c4bd7098488e9c219a73724effd6fae5644738faa31a4ff55bccc0\
         a151af5f0dc8b4bd45bf37df365c1a65e68cfda76d4da708df1fb2bc2e4a4371",
        q: Some("f518aa8781a8df278aba4e7d64b7cb9d49462353"),
        g: "a4d1cbd5c3fd34126765a442efb99905f8104dd258ac507fd6406cff14266d31266fea1e5c41564b777e690f5504f213160217b4b01b886a5e91547f9e2749f4d7fbd7d3b9a92ee1909d0d2263f80a76a6a24c087a091f531dbf0a0169b6a28ad662a4d18e73afa32d779d5918d08bc8858f4dcef97c2a24855e6eeb22b3b2e5",
    },
    DhNamedGroup {
        name: "dh2048_224",
        private_bits: 0,
        p: "ad107e1e9123a9d0d660faa79559c51fa20d64e5683b9fd1b54b1597b61d0a75\
         e6fa141df95a56dbaf9a3c407ba1df15eb3d688a309c180e1de6b85a1274a0a6\
         6d3f8152ad6ac2129037c9edefda4df8d91e8fef55b7394b7ad5b7d0b6c12207\
         c9f98d11ed34dbf6c6ba0b2c8bbc27be6a00e0a0b9c49708b3bf8a3170918836\
         81286130bc8985db1602e714415d9330278273c7de31efdc7310f7121fd5a074\
         15987d9adc0a486dcdf93acc44328387315d75e198c641a480cd86a1b9e587e8\
         be60e69cc928b2b9c52172e413042e9b23f10b0e16e79763c9b53dcf4ba80a29\
         e3fb73c16b8e75b97ef363e2ffa31f71cf9de5384e71b81c0ac4dffe0c10e64f",
        q: Some("801c0d34c58d93fe997177101f80535a4738cebcbf389a99b36371eb"),
        g: "ac4032ef4f2d9ae39df30b5c8ffdac506cdebe7b89998caf74866a08cfe4ffe3a6824a4e10b9a6f0dd921f01a70c4afaab739d7700c29f52c57db17c620a8652be5e9001a8d66ad7c17669101999024af4d027275ac1348bb8a762d0521bc98ae247150422ea1ed409939d54da7460cdb5f6c6b250717cbef180eb34118e98d119529a45d6f834566e3025e316a330efbb77a86f0c1ab15b051ae3d428c8f8acb70a8137150b8eeb10e183edd19963ddd9e263e4770589ef6aa21e7f5f2ff381b539cce3409d13cd566afbb48d6c019181e1bcfe94b30269edfe72fe9b6aa4bd7b5a0f1c71cfff4c19c418e1f6ec017981bc087f2a7065b384b890d3191f2bfa",
    },
    DhNamedGroup {
        name: "dh2048_256",
        private_bits: 0,
        p: "87a8e61db4b6663cffbbd19c651959998ceef608660dd0f25d2ceed4435e3b00\
         e00df8f1d61957d4faf7df4561b2aa3016c3d91134096faa3bf4296d830e9a7c\
         209e0c6497517abd5a8a9d306bcf67ed91f9e6725b4758c022e0b1ef4275bf7b\
         6c5bfc11d45f9088b941f54eb1e59bb8bc39a0bf12307f5c4fdb70c581b23f76\
         b63acae1caa6b7902d52526735488a0ef13c6d9a51bfa4ab3ad8347796524d8e\
         f6a167b5a41825d967e144e5140564251ccacb83e6b486f6b3ca3f7971506026\
         c0b857f689962856ded4010abd0be621c3a3960a54e710c375f26375d7014103\
         a4b54330c198af126116d2276e11715f693877fad7ef09cadb094ae91e1a1597",
        q: Some("8cf83642a709a097b447997640129da299b1a47d1eb3750ba308b0fe64f5fbd3"),
        g: "3fb32c9b73134d0b2e77506660edbd484ca7b18f21ef205407f4793a1a0ba12510dbc15077be463fff4fed4aac0bb555be3a6c1b0c6b47b1bc3773bf7e8c6f62901228f8c28cbb18a55ae31341000a650196f931c77a57f2ddf463e5e9ec144b777de62aaab8a8628ac376d282d6ed3864e67982428ebc831d14348f6f2f9193b5045af2767164e1dfc967c1fb3f2e55a4bd1bffe83b9c80d052b985d182ea0adb2a3b7313d3fe14c8484b1e052588b9b7d2bbd2df016199ecd06e1557cd0915b3353bbb64e0ec377fd028370df92b52c7891428cdc67eb6184b523d1db246c32f63078490f00ef8d647d148d47954515e2327cfef98c582664b4c0f6cc41659",
    },
];

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[start..]
}

/// Bit length of a public big-endian value.
fn public_bit_length(bytes: &[u8]) -> u32 {
    let bytes = strip_leading_zeros(bytes);
    match bytes.first() {
        None => 0,
        Some(&top) => (bytes.len() as u32 - 1) * 8 + (8 - top.leading_zeros()),
    }
}

fn hex_decode_static(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("static hex"))
        .collect()
}

/// The named group a (p, g) pair is, compared as numbers (leading zero
/// bytes do not matter).
fn dh_named_group(p: &[u8], g: &[u8]) -> Option<&'static DhNamedGroup> {
    let p = strip_leading_zeros(p);
    let g = strip_leading_zeros(g);
    if p.is_empty() {
        return None;
    }
    let p_hex = hex_lower(p);
    let g_hex = hex_lower(g);
    DH_NAMED_GROUPS
        .iter()
        .find(|group| group.p == p_hex && group.g == g_hex)
}

/// The group's subgroup order q: listed for RFC 5114, (p - 1) / 2 otherwise.
fn dh_named_group_q(group: &DhNamedGroup) -> Vec<u8> {
    match group.q {
        Some(q) => hex_decode_static(q),
        None => {
            let p = BigUint::from_bytes_be(&hex_decode_static(group.p));
            (p >> 1usize).to_bytes_be()
        }
    }
}

/// OpenSSL's ossl_ifc_ffc_compute_security_bits: the security strength of an
/// n-bit modulus (SP 800-56B rev 2 Appendix D), in its fixed-point form so
/// every n gives OpenSSL's exact value.
fn dh_security_bits(n: u32) -> u32 {
    const SCALE: u64 = 1 << 18;
    const CBRT_SCALE: u64 = 1 << (2 * 18 / 3);
    const LOG_2: u64 = 0x02c5c8;
    const LOG_E: u64 = 0x05c551;
    const C1_923: u64 = 0x07b126;
    const C4_690: u64 = 0x12c28f;
    fn mul2(a: u64, b: u64) -> u64 {
        a.wrapping_mul(b) / SCALE
    }
    fn icbrt64(mut x: u64) -> u64 {
        let mut r: u64 = 0;
        let mut s: i32 = 63;
        while s >= 0 {
            r <<= 1;
            let b = 3u64.wrapping_mul(r).wrapping_mul(r + 1).wrapping_add(1);
            if (x >> s) >= b {
                x = x.wrapping_sub(b << s);
                r += 1;
            }
            s -= 3;
        }
        r.wrapping_mul(CBRT_SCALE)
    }
    fn ilog_e(mut v: u64) -> u64 {
        let mut r: u32 = 0;
        while v >= 2 * SCALE {
            v >>= 1;
            r += SCALE as u32;
        }
        let mut i = (SCALE / 2) as u32;
        while i != 0 {
            v = mul2(v, v);
            if v >= 2 * SCALE {
                v >>= 1;
                r += i;
            }
            i /= 2;
        }
        (u64::from(r) * SCALE / LOG_E) as u32 as u64
    }
    match n {
        2048 => return 112,
        3072 => return 128,
        4096 => return 152,
        6144 => return 176,
        7680 => return 192,
        8192 => return 200,
        15360 => return 256,
        _ => {}
    }
    if n >= 687_737 {
        return 1200;
    }
    if n < 8 {
        return 0;
    }
    let cap: u32 = if n <= 7680 {
        192
    } else if n <= 15360 {
        256
    } else {
        1200
    };
    let x = u64::from(n) * LOG_2;
    let lx = ilog_e(x);
    let y = (mul2(C1_923, icbrt64(mul2(mul2(x, lx), lx))).wrapping_sub(C4_690) / LOG_2) as u16;
    let y = (u32::from(y) + 4) as u16 & !7u16;
    u32::from(y).min(cap)
}

/// The private-value length OpenSSL gives a group it generated for a prime
/// length (dh_builtin_genparams): the RFC 7919 rule, 2 * strength rounded up
/// to a multiple of 25.
fn dh_generated_private_bits(prime_bits: u32) -> u32 {
    (2 * dh_security_bits(prime_bits)).div_ceil(25) * 25
}

/// How a fresh private value is drawn for a group.
#[derive(Debug, PartialEq, Eq)]
enum DhPrivateSpec {
    /// Named group: 1 + a uniform `bits`-bit value, drawn again while it is
    /// not below `upper` (q, when q is shorter than 2^bits) or 2^bits.
    Bounded { bits: u32, upper: Option<Vec<u8>> },
    /// Any other group: exactly `bits` long (top bit set); bit 0 cleared when
    /// g = 2 and p has bit 2 clear.
    TopBitSet { bits: u32, clear_low_bit: bool },
}

impl DhPrivateSpec {
    fn bits(&self) -> u32 {
        match self {
            DhPrivateSpec::Bounded { bits, .. } | DhPrivateSpec::TopBitSet { bits, .. } => *bits,
        }
    }
}

/// The private-value rule for (p, g), or None when OpenSSL refuses to draw
/// one. `generated` marks a group made by createDiffieHellman(primeLength),
/// which carries the generated-group length.
fn dh_private_spec(p: &[u8], g: &[u8], generated: bool) -> Option<DhPrivateSpec> {
    let p = strip_leading_zeros(p);
    let g = strip_leading_zeros(g);
    let p_bits = public_bit_length(p);
    if let Some(group) = dh_named_group(p, g) {
        let strength = dh_security_bits(p_bits);
        let bits = if group.private_bits != 0 {
            group.private_bits
        } else {
            2 * strength
        };
        let q = dh_named_group_q(group);
        let q_bits = public_bit_length(&q);
        if bits < 2 * strength || bits > q_bits {
            return None;
        }
        let upper = (q_bits <= bits).then_some(q);
        return Some(DhPrivateSpec::Bounded { bits, upper });
    }
    let length = if generated {
        dh_generated_private_bits(p_bits)
    } else {
        0
    };
    if length >= p_bits || p_bits < 3 {
        return None;
    }
    let mut bits = p_bits - 2;
    if length != 0 && length < bits {
        bits = length;
    }
    let clear_low_bit = g == [2] && p.last().is_some_and(|&b| b & 0x04 == 0);
    Some(DhPrivateSpec::TopBitSet {
        bits,
        clear_low_bit,
    })
}

/// The exponent width a private exponentiation runs over: the group's
/// private-value width in whole bytes, or the key's own byte width when a
/// caller-set key is longer. It depends on the group and on how many bytes
/// the key is held in, never on the key's value.
fn dh_exponent_bits(group_bits: u32, key_len: usize) -> u32 {
    let group_bits = group_bits.div_ceil(8).saturating_mul(8);
    let key_bits = u32::try_from(key_len.saturating_mul(8)).unwrap_or(u32::MAX);
    group_bits.max(key_bits).max(8)
}

/// The bytes a value of `bits` bits is held in.
fn dh_bytes_for_bits(bits: u32) -> usize {
    bits.div_ceil(8) as usize
}

/// A fresh private value under `spec`, big-endian in exactly
/// dh_bytes_for_bits(spec.bits()) bytes.
fn dh_draw_private(spec: &DhPrivateSpec, rng: &mut impl rand_core::RngCore) -> Vec<u8> {
    let bits = spec.bits();
    let len = dh_bytes_for_bits(bits);
    let top_bits = bits - 8 * (len as u32 - 1);
    let top_mask = if top_bits == 8 {
        0xff
    } else {
        (1u8 << top_bits) - 1
    };
    match spec {
        DhPrivateSpec::TopBitSet { clear_low_bit, .. } => {
            let mut out = vec![0u8; len];
            rng.fill_bytes(&mut out);
            out[0] &= top_mask;
            out[0] |= 1 << (top_bits - 1);
            if *clear_low_bit {
                out[len - 1] &= 0xfe;
            }
            out
        }
        DhPrivateSpec::Bounded { upper, .. } => {
            // One more bit of precision than the value: 1 + (2^bits - 1)
            // must not wrap.
            let precision = bits + 1;
            let one = BoxedUint::one_with_precision(precision);
            let bound = match upper {
                Some(q) => BoxedUint::from_be_slice(q, precision).expect("q fits its own width"),
                None => one.shl(bits),
            };
            let mut raw = vec![0u8; len];
            loop {
                rng.fill_bytes(&mut raw);
                raw[0] &= top_mask;
                let mut candidate =
                    BoxedUint::from_be_slice(&raw, precision).expect("draw fits its width");
                // The slice, not the Vec: the buffer keeps its length for the
                // next draw.
                raw.as_mut_slice().zeroize();
                candidate = candidate.wrapping_add(&one);
                // A draw at or above the bound is drawn again (OpenSSL's
                // ossl_ffc_generate_private_key).
                if candidate.ct_lt(&bound).to_bool() {
                    let wide = candidate.to_be_bytes();
                    candidate.zeroize();
                    let out = wide[wide.len() - len..].to_vec();
                    let mut wide = wide.into_vec();
                    wide.zeroize();
                    return out;
                }
                candidate.zeroize();
            }
        }
    }
}

/// Montgomery parameters for an odd public modulus (None for an even one,
/// which OpenSSL's Montgomery exponentiation refuses too).
fn dh_monty_params(p: &[u8]) -> Option<BoxedMontyParams> {
    let p = BoxedUint::from_be_slice_vartime(strip_leading_zeros(p));
    Odd::new(p).into_option().map(BoxedMontyParams::new_vartime)
}

/// A public value reduced into the group, in Montgomery form.
fn dh_monty_public(value: &[u8], params: &BoxedMontyParams) -> BoxedMontyForm {
    let modulus = params.modulus();
    let precision = modulus.bits_precision();
    let value = BoxedUint::from_be_slice_vartime(strip_leading_zeros(value));
    let value = if value.bits_precision() < precision {
        value.resize(precision)
    } else {
        value
    };
    // Below p, so it fits p's precision.
    let reduced = value.rem_vartime(modulus.as_nz_ref()).resize(precision);
    BoxedMontyForm::new(reduced, params)
}

/// base^exponent mod p over `exponent_bits` exponent bits, whatever the
/// exponent's value. `exponent` must fit in `exponent_bits`.
fn dh_pow(base: &BoxedMontyForm, exponent: &[u8], exponent_bits: u32) -> BoxedUint {
    let mut e = BoxedUint::from_be_slice(exponent, exponent_bits).expect("exponent fits its width");
    // The Montgomery-form result is a heap value with no wipe on drop; clear
    // it here like every other intermediate of a key operation.
    let mut monty = base.pow_bounded_exp(&e, exponent_bits);
    let result = monty.retrieve();
    monty.zeroize();
    e.zeroize();
    result
}

/// A value below p as exactly `len` big-endian bytes.
fn dh_fixed_width(value: &BoxedUint, len: usize) -> Vec<u8> {
    let mut wide = value.to_be_bytes().into_vec();
    let out = wide[wide.len().saturating_sub(len)..].to_vec();
    wide.zeroize();
    if out.len() < len {
        let mut padded = vec![0u8; len - out.len()];
        padded.extend_from_slice(&out);
        return padded;
    }
    out
}

/// The final encode of a key: OpenSSL's BN_bn2bin, no leading zero bytes.
fn dh_encode_minimal(mut fixed: Vec<u8>) -> Vec<u8> {
    let start = fixed.iter().position(|&b| b != 0).unwrap_or(fixed.len());
    let out = fixed[start..].to_vec();
    fixed.zeroize();
    out
}

/// generateKeys: the public value g^x mod p, and x when it was drawn here
/// (the caller set none). Err means node's "Key generation failed". The
/// public key comes back in its final (minimal) encoding, where empty is a
/// zero one, which node also stores and then reports as a failure; a drawn
/// private value comes back fixed-width, for its final encode at
/// getPrivateKey.
#[derive(Debug)]
struct DhKeys {
    public: Vec<u8>,
    private: Option<Vec<u8>>,
}

fn dh_generate_keys(
    prime: &[u8],
    generator: &[u8],
    private_key: Option<&[u8]>,
    generated: bool,
    rng: &mut impl rand_core::RngCore,
) -> Result<DhKeys, ()> {
    let p = strip_leading_zeros(prime);
    let p_bits = public_bit_length(p);
    if !(DH_MIN_MODULUS_BITS..=DH_MAX_MODULUS_BITS).contains(&p_bits) {
        return Err(());
    }
    let spec = dh_private_spec(p, generator, generated);
    let drawn = private_key.is_none();
    let mut private = match private_key {
        Some(key) => key.to_vec(),
        None => dh_draw_private(spec.as_ref().ok_or(())?, rng),
    };
    let group_bits = spec.as_ref().map_or(p_bits, DhPrivateSpec::bits);
    let exponent_bits = dh_exponent_bits(group_bits, private.len());
    let Some(params) = dh_monty_params(p) else {
        private.zeroize();
        return Err(());
    };
    let base = dh_monty_public(generator, &params);
    let public = dh_pow(&base, &private, exponent_bits);
    let public = dh_encode_minimal(dh_fixed_width(&public, p.len()));
    if !drawn {
        private.zeroize();
    }
    Ok(DhKeys {
        public,
        private: drawn.then_some(private),
    })
}

/// Why computeSecret refuses a peer key (node's two RangeErrors).
#[derive(Debug, PartialEq, Eq)]
enum DhPeerError {
    /// ERR_CRYPTO_INVALID_KEYLEN "Supplied key is too small"
    TooSmall,
    /// ERR_CRYPTO_INVALID_KEYTYPE "Unspecified validation error"
    Invalid,
}

/// DH_check_pub_key as node maps it: a peer <= 1 is too small; one >= p - 1,
/// one outside a named group's order-q subgroup, or any peer of a modulus
/// past the check limit is invalid.
fn dh_check_peer(p: &[u8], g: &[u8], peer: &[u8]) -> Result<(), DhPeerError> {
    let p = strip_leading_zeros(p);
    if public_bit_length(p) > DH_CHECK_MAX_MODULUS_BITS {
        return Err(DhPeerError::Invalid);
    }
    let p_num = BigUint::from_bytes_be(p);
    let peer_num = BigUint::from_bytes_be(peer);
    let one = BigUint::from(1u32);
    if peer_num <= one {
        return Err(DhPeerError::TooSmall);
    }
    // p - 1 as a signed value: for p = 0 it is -1, below every peer.
    if p_num == BigUint::from(0u32) || peer_num >= &p_num - &one {
        return Err(DhPeerError::Invalid);
    }
    if let Some(group) = dh_named_group(p, g) {
        let q = dh_named_group_q(group);
        let params = dh_monty_params(p).ok_or(DhPeerError::Invalid)?;
        let base = dh_monty_public(peer, &params);
        let q_bits = public_bit_length(&q);
        let check = dh_pow(&base, strip_leading_zeros(&q), q_bits.div_ceil(8) * 8);
        if !check.is_one().to_bool() {
            return Err(DhPeerError::Invalid);
        }
    }
    Ok(())
}

/// computeSecret: peer^x mod p, as many bytes as p. A peer node refuses is
/// an Err; a key agreement OpenSSL cannot complete comes back as node returns
/// it: empty (no private value, an even or too-large modulus, or a degenerate
/// shared value) or all zeros (a modulus below 512 bits).
fn dh_compute_secret(
    prime: &[u8],
    generator: &[u8],
    private_key: Option<&[u8]>,
    peer: &[u8],
    generated: bool,
) -> Result<Vec<u8>, DhPeerError> {
    let p = strip_leading_zeros(prime);
    dh_check_peer(p, generator, peer)?;
    let p_bits = public_bit_length(p);
    if p_bits > DH_MAX_MODULUS_BITS {
        return Ok(Vec::new());
    }
    if p_bits < DH_MIN_MODULUS_BITS {
        return Ok(vec![0u8; p.len()]);
    }
    let Some(private) = private_key else {
        return Ok(Vec::new());
    };
    let Some(params) = dh_monty_params(p) else {
        return Ok(Vec::new());
    };
    let group_bits = dh_private_spec(p, generator, generated)
        .as_ref()
        .map_or(p_bits, DhPrivateSpec::bits);
    let exponent_bits = dh_exponent_bits(group_bits, private.len());
    let base = dh_monty_public(peer, &params);
    let mut shared = dh_pow(&base, private, exponent_bits);
    // A shared value of 0, 1 or p - 1 is refused (OpenSSL's DH_R_INVALID_SECRET).
    let precision = shared.bits_precision();
    let two = BoxedUint::from_be_slice(&[2], precision).expect("2 fits");
    let p_minus_one = params
        .modulus()
        .as_ref()
        .wrapping_sub(BoxedUint::one_with_precision(precision));
    let degenerate = shared.ct_lt(&two).or(shared.ct_eq(&p_minus_one));
    if degenerate.to_bool() {
        shared.zeroize();
        return Ok(Vec::new());
    }
    let out = dh_fixed_width(&shared, p.len());
    shared.zeroize();
    Ok(out)
}

/// verifyError: DH_check's flags for (p, g), or Err when the check itself
/// fails (a modulus past the check limit). Primality is tested on the public
/// modulus only.
fn dh_check(prime: &[u8], generator: &[u8]) -> Result<i32, ()> {
    use num_bigint_dig::prime::probably_prime;
    let p = strip_leading_zeros(prime);
    let g = strip_leading_zeros(generator);
    if dh_named_group(p, g).is_some() {
        return Ok(0);
    }
    let p_bits = public_bit_length(p);
    if p_bits > DH_CHECK_MAX_MODULUS_BITS {
        return Err(());
    }
    let mut flags = 0;
    let p_num = BigUint::from_bytes_be(p);
    let g_num = BigUint::from_bytes_be(g);
    let one = BigUint::from(1u32);
    if p.last().is_none_or(|&b| b & 1 == 0) {
        flags |= DH_CHECK_P_NOT_PRIME;
    }
    if g_num <= one {
        flags |= DH_NOT_SUITABLE_GENERATOR;
    }
    // g >= p - 1, with p - 1 signed (-1 for p = 0).
    if p_num == BigUint::from(0u32) || g_num >= &p_num - &one {
        flags |= DH_NOT_SUITABLE_GENERATOR;
    }
    if p_bits < DH_MIN_MODULUS_BITS {
        flags |= DH_MODULUS_TOO_SMALL;
    }
    if p_bits > DH_MAX_MODULUS_BITS {
        flags |= DH_MODULUS_TOO_LARGE;
    }
    if !probably_prime(&p_num, 25) {
        flags |= DH_CHECK_P_NOT_PRIME;
    } else if !probably_prime(&(p_num >> 1usize), 25) {
        flags |= DH_CHECK_P_NOT_SAFE_PRIME;
    }
    Ok(flags)
}

/// An optional bytes argument: undefined / null is "not set".
fn dh_optional_bytes(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    index: i32,
) -> Option<Vec<u8>> {
    crate::node_ops::arg_bytes(scope, args, index)
}

/// cryptoDhGenerateKeys(prime, generator, privateKey?, generated) ->
/// { publicKey, privateKey? } (privateKey only when drawn here) or null when
/// key generation fails.
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
    let mut private_key = dh_optional_bytes(scope, &args, 2);
    let generated = args.get(3).boolean_value(scope);
    let result = dh_generate_keys(
        &prime,
        &generator,
        private_key.as_deref(),
        generated,
        &mut OsRng,
    );
    if let Some(key) = private_key.as_mut() {
        key.zeroize();
    }
    match result {
        Ok(keys) => {
            let obj = v8::Object::new(scope);
            if let Some(pub_arr) = crate::node_ops::bytes_to_uint8array(scope, keys.public) {
                let key = v8::String::new(scope, "publicKey").unwrap();
                obj.set(scope, key.into(), pub_arr);
            }
            if let Some(private) = keys.private
                && let Some(priv_arr) = crate::node_ops::bytes_to_uint8array(scope, private)
            {
                let key = v8::String::new(scope, "privateKey").unwrap();
                obj.set(scope, key.into(), priv_arr);
            }
            rv.set(obj.into());
        }
        Err(()) => rv.set(v8::null(scope).into()),
    }
}

/// cryptoDhComputeSecret(prime, generator, privateKey?, peer, generated) ->
/// the shared secret, or 1 (peer too small) / 2 (peer invalid).
pub(crate) fn op_crypto_dh_compute_secret(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(prime) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "DH computeSecret: prime required");
        return;
    };
    let Some(generator) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "DH computeSecret: generator required");
        return;
    };
    let mut private_key = dh_optional_bytes(scope, &args, 2);
    let Some(peer) = crate::node_ops::arg_bytes(scope, &args, 3) else {
        crate::node_ops::throw_type_error(scope, "DH computeSecret: peer public key required");
        return;
    };
    let generated = args.get(4).boolean_value(scope);
    let result = dh_compute_secret(&prime, &generator, private_key.as_deref(), &peer, generated);
    if let Some(key) = private_key.as_mut() {
        key.zeroize();
    }
    match result {
        Ok(secret) => {
            if let Some(value) = crate::node_ops::bytes_to_uint8array(scope, secret) {
                rv.set(value);
            }
        }
        Err(DhPeerError::TooSmall) => rv.set(v8::Integer::new(scope, 1).into()),
        Err(DhPeerError::Invalid) => rv.set(v8::Integer::new(scope, 2).into()),
    }
}

/// cryptoDhCheck(prime, generator) -> DH_check flags, or -1 when the check
/// fails.
pub(crate) fn op_crypto_dh_check(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(prime) = crate::node_ops::arg_bytes(scope, &args, 0) else {
        crate::node_ops::throw_type_error(scope, "DH check: prime required");
        return;
    };
    let Some(generator) = crate::node_ops::arg_bytes(scope, &args, 1) else {
        crate::node_ops::throw_type_error(scope, "DH check: generator required");
        return;
    };
    let flags = dh_check(&prime, &generator).unwrap_or(-1);
    rv.set(v8::Integer::new(scope, flags).into());
}

#[cfg(test)]
mod dh_exponent_width {
    use super::*;

    const MODP1: &str = "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74\
                         020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f1437\
                         4fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6bffffffffffffffff";
    // A 512-bit safe prime with p = 3 (mod 8) and one with bit 2 set.
    const PA: &str = "dda1c2f6e6d302291a445defc55fbf75acd344565f302ca129e491c3e75cd5a8\
                      714ab2c1faf2df3473bba813eb04f32ac64c39ba890d37a2970e75c4fc83581f";
    const PB: &str = "c1b5c3d899bfe0823b965fc1518b67a7ff5fdd2fb3e2324a718e61015dc045d9\
                      d50169ffa45b399c33389a7213db210f562d1861dd472959070fb283dcd49a63";

    fn hex(s: &str) -> Vec<u8> {
        hex_decode_static(s)
    }

    /// A deterministic byte source for the draws.
    struct Counter(u8);
    impl rand_core::RngCore for Counter {
        fn next_u32(&mut self) -> u32 {
            let mut b = [0u8; 4];
            self.fill_bytes(&mut b);
            u32::from_le_bytes(b)
        }
        fn next_u64(&mut self) -> u64 {
            let mut b = [0u8; 8];
            self.fill_bytes(&mut b);
            u64::from_le_bytes(b)
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for b in dest {
                self.0 = self.0.wrapping_mul(29).wrapping_add(71);
                *b = self.0;
            }
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    /// Every byte set: the largest draw a width allows.
    struct Ones;
    impl rand_core::RngCore for Ones {
        fn next_u32(&mut self) -> u32 {
            u32::MAX
        }
        fn next_u64(&mut self) -> u64 {
            u64::MAX
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            dest.fill(0xff);
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    /// All ones for the first fill, then the inner source.
    struct OnesThen(bool, Counter);
    impl rand_core::RngCore for OnesThen {
        fn next_u32(&mut self) -> u32 {
            self.1.next_u32()
        }
        fn next_u64(&mut self) -> u64 {
            self.1.next_u64()
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            if std::mem::take(&mut self.0) {
                dest.fill(0xff);
            } else {
                self.1.fill_bytes(dest);
            }
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    #[test]
    fn security_bits_match_openssl() {
        // OpenSSL's canonical values, and the formula's at the sizes node
        // measured through createDiffieHellman(primeLength).
        for (n, s) in [
            (2048, 112),
            (3072, 128),
            (4096, 152),
            (6144, 176),
            (8192, 200),
            (15360, 256),
        ] {
            assert_eq!(dh_security_bits(n), s, "n={n}");
        }
        assert_eq!(dh_security_bits(1024), 80);
        assert_eq!(dh_security_bits(7), 0);
        assert_eq!(dh_security_bits(700_000), 1200);
        // node v22.22.2: createDiffieHellman(n).generateKeys() private bits.
        for (n, bits) in [
            (512, 125),
            (520, 125),
            (576, 150),
            (600, 150),
            (640, 150),
            (704, 150),
            (768, 150),
            (800, 150),
            (1000, 175),
            (1024, 175),
            (1280, 200),
            (1536, 200),
        ] {
            assert_eq!(dh_generated_private_bits(n), bits, "n={n}");
        }
    }

    #[test]
    fn named_groups_are_recognised_by_p_and_g() {
        let modp14 = &DH_NAMED_GROUPS[6];
        assert_eq!(modp14.name, "modp_2048");
        let p = hex(modp14.p);
        assert!(dh_named_group(&p, &[2]).is_some());
        assert!(dh_named_group(&p, &[0, 0, 2]).is_some());
        let mut padded = vec![0u8];
        padded.extend_from_slice(&p);
        assert!(dh_named_group(&padded, &[2]).is_some());
        assert!(dh_named_group(&p, &[5]).is_none());
        // modp1 (768 bits) and modp2 are not named groups to OpenSSL.
        assert!(dh_named_group(&hex(MODP1), &[2]).is_none());
        for group in &DH_NAMED_GROUPS {
            let q = dh_named_group_q(group);
            let p = BigUint::from_bytes_be(&hex(group.p));
            let q = BigUint::from_bytes_be(&q);
            assert_eq!(
                (&p - BigUint::from(1u32)) % &q,
                BigUint::from(0u32),
                "{}",
                group.name
            );
        }
    }

    #[test]
    fn private_width_rule() {
        // Named groups: the RFC 7919 lengths, or twice the strength (RFC 5114).
        let widths: Vec<(&str, u32)> = DH_NAMED_GROUPS
            .iter()
            .map(|g| {
                let spec = dh_private_spec(&hex(g.p), &hex(g.g), false).unwrap();
                (g.name, spec.bits())
            })
            .collect();
        assert_eq!(
            widths,
            [
                ("ffdhe2048", 225),
                ("ffdhe3072", 275),
                ("ffdhe4096", 325),
                ("ffdhe6144", 375),
                ("ffdhe8192", 400),
                ("modp_1536", 200),
                ("modp_2048", 225),
                ("modp_3072", 275),
                ("modp_4096", 325),
                ("modp_6144", 375),
                ("modp_8192", 400),
                ("dh1024_160", 160),
                ("dh2048_224", 224),
                ("dh2048_256", 224),
            ]
        );
        // RFC 5114 1024/160 and 2048/224: q is no longer than the width, so
        // it bounds the value.
        let g = &DH_NAMED_GROUPS[11];
        match dh_private_spec(&hex(g.p), &hex(g.g), false).unwrap() {
            DhPrivateSpec::Bounded { upper, .. } => assert!(upper.is_some()),
            other => panic!("{other:?}"),
        }
        // Other groups: bits(p) - 2, top bit set; generated groups their length.
        assert_eq!(
            dh_private_spec(&hex(MODP1), &[2], false),
            Some(DhPrivateSpec::TopBitSet {
                bits: 766,
                clear_low_bit: false
            })
        );
        assert_eq!(
            dh_private_spec(&hex(PA), &[5], true),
            Some(DhPrivateSpec::TopBitSet {
                bits: 125,
                clear_low_bit: false
            })
        );
        // g = 2 with bit 2 of p clear: bit 0 of the value is cleared.
        assert_eq!(
            dh_private_spec(&hex(PB), &[2], false),
            Some(DhPrivateSpec::TopBitSet {
                bits: 510,
                clear_low_bit: true
            })
        );
        assert_eq!(
            dh_private_spec(&hex(PB), &[3], false),
            Some(DhPrivateSpec::TopBitSet {
                bits: 510,
                clear_low_bit: false
            })
        );
    }

    #[test]
    fn exponent_width_is_the_groups_unless_the_key_is_wider() {
        // A key held in up to the group's byte width runs at that width
        // whatever its value: 1, 28 or 29 bytes alike.
        assert_eq!(dh_exponent_bits(225, 0), 232);
        assert_eq!(dh_exponent_bits(225, 1), 232);
        assert_eq!(dh_exponent_bits(225, 28), 232);
        assert_eq!(dh_exponent_bits(225, 29), 232);
        assert_eq!(dh_exponent_bits(225, 30), 240);
        assert_eq!(dh_exponent_bits(510, 64), 512);
        assert_eq!(dh_exponent_bits(510, 200), 1600);
        assert_eq!(dh_exponent_bits(400, 50), 400);
    }

    #[test]
    fn draws_have_the_spec_width() {
        let mut rng = Counter(1);
        for spec in [
            DhPrivateSpec::TopBitSet {
                bits: 510,
                clear_low_bit: true,
            },
            DhPrivateSpec::TopBitSet {
                bits: 125,
                clear_low_bit: false,
            },
        ] {
            for _ in 0..16 {
                let x = dh_draw_private(&spec, &mut rng);
                assert_eq!(x.len(), dh_bytes_for_bits(spec.bits()));
                assert_eq!(public_bit_length(&x), spec.bits());
                if let DhPrivateSpec::TopBitSet {
                    clear_low_bit: true,
                    ..
                } = spec
                {
                    assert_eq!(x[x.len() - 1] & 1, 0);
                }
            }
        }
        let spec = DhPrivateSpec::Bounded {
            bits: 225,
            upper: None,
        };
        for _ in 0..16 {
            let x = dh_draw_private(&spec, &mut rng);
            assert_eq!(x.len(), 29);
            assert!(public_bit_length(&x) <= 225);
            assert!(public_bit_length(&x) >= 1);
        }
        // The all-ones draw is 2^225 after the + 1 and must be drawn again;
        // a source that only ever yields it would loop, so check the bound
        // directly on a draw just below it.
        let mut below = vec![0xffu8; 29];
        below[0] = 0x01;
        below[28] = 0xfe;
        let precision = 226;
        let x = BoxedUint::from_be_slice(&below, precision)
            .unwrap()
            .wrapping_add(BoxedUint::one_with_precision(precision));
        assert!(
            x.ct_lt(&BoxedUint::one_with_precision(precision).shl(225))
                .to_bool()
        );
        // q-bounded: every kept value is below q.
        let g = &DH_NAMED_GROUPS[11];
        let spec = dh_private_spec(&hex(g.p), &hex(g.g), false).unwrap();
        let q = BigUint::from_bytes_be(&dh_named_group_q(g));
        for _ in 0..16 {
            let x = BigUint::from_bytes_be(&dh_draw_private(&spec, &mut rng));
            assert!(x < q && x >= BigUint::from(1u32));
        }
        // A refused draw is drawn again: all ones first (2^225 after the
        // + 1, refused), then ordinary bytes.
        let mut first_ones = OnesThen(true, Counter(3));
        let x = dh_draw_private(
            &DhPrivateSpec::Bounded {
                bits: 225,
                upper: None,
            },
            &mut first_ones,
        );
        assert!(!first_ones.0);
        assert_eq!(x.len(), 29);
        assert_ne!(x, vec![0xffu8; 29]);
        // A TopBitSet draw from an all-ones source keeps its exact width.
        let x = dh_draw_private(
            &DhPrivateSpec::TopBitSet {
                bits: 125,
                clear_low_bit: true,
            },
            &mut Ones,
        );
        assert_eq!(public_bit_length(&x), 125);
        assert_eq!(x[x.len() - 1] & 1, 0);
    }

    #[test]
    fn fixed_width_encode_keeps_leading_zeros_until_the_final_encode() {
        let v = BoxedUint::from_be_slice(&[0, 0, 0x12, 0x34], 128).unwrap();
        assert_eq!(dh_fixed_width(&v, 4), vec![0, 0, 0x12, 0x34]);
        assert_eq!(
            dh_fixed_width(&v, 20),
            [vec![0u8; 18], vec![0x12, 0x34]].concat()
        );
        assert_eq!(dh_encode_minimal(vec![0, 0, 0x12, 0x34]), vec![0x12, 0x34]);
        assert_eq!(dh_encode_minimal(vec![0, 0]), Vec::<u8>::new());
    }

    #[test]
    fn exponentiation_matches_reference_arithmetic() {
        // node v22.22.2: createDiffieHellman(PA, g).setPrivateKey(X) with
        // X = sha256("oam-dh-a").
        let x = hex("853b8aa2118738a31def9fa4ba32baf4eb844116ca7194acf6d8cdf5641e8b09");
        let keys = dh_generate_keys(&hex(PA), &[2], Some(&x), false, &mut Counter(0)).unwrap();
        assert_eq!(
            hex_lower(&keys.public),
            "45c53208225eb0ab342b44a0daf53245295a3b1e41889273a868f41dfdfd74cb\
             d9daef977272ec19d4ea3d0083821dfa9a4524f7b39db369ba0745eb1639252c"
        );
        // Against num-bigint-dig's arithmetic for wide and short exponents.
        let p = BigUint::from_bytes_be(&hex(PA));
        for key in [
            vec![0x80u8; 64],
            vec![0xc3u8; 200],
            vec![0, 0, 0, 5],
            vec![1],
        ] {
            let keys =
                dh_generate_keys(&hex(PA), &[5], Some(&key), false, &mut Counter(0)).unwrap();
            let want = BigUint::from(5u32).modpow(&BigUint::from_bytes_be(&key), &p);
            assert_eq!(keys.public, want.to_bytes_be());
        }
    }

    #[test]
    fn generated_keys_round_trip() {
        let mut rng = Counter(9);
        let a = dh_generate_keys(&hex(PB), &[2], None, false, &mut rng).unwrap();
        let b = dh_generate_keys(&hex(PB), &[2], None, false, &mut rng).unwrap();
        let (a_private, b_private) = (a.private.unwrap(), b.private.unwrap());
        assert_eq!(a_private.len(), 64);
        assert_eq!(public_bit_length(&a_private), 510);
        let s1 = dh_compute_secret(&hex(PB), &[2], Some(&a_private), &b.public, false).unwrap();
        let s2 = dh_compute_secret(&hex(PB), &[2], Some(&b_private), &a.public, false).unwrap();
        // A caller-set key is not handed back.
        let c = dh_generate_keys(&hex(PB), &[2], Some(&a_private), false, &mut rng).unwrap();
        assert!(c.private.is_none());
        assert_eq!(c.public, a.public);
        assert_eq!(s1, s2);
        assert_eq!(s1.len(), 64);
    }

    #[test]
    fn peer_validation_and_unusable_moduli() {
        let pa = hex(PA);
        let x = [7u8; 32];
        let p = BigUint::from_bytes_be(&pa);
        let minus = |n: u32| (&p - BigUint::from(n)).to_bytes_be();
        assert_eq!(
            dh_compute_secret(&pa, &[2], Some(&x), &[], false),
            Err(DhPeerError::TooSmall)
        );
        assert_eq!(
            dh_compute_secret(&pa, &[2], Some(&x), &[1], false),
            Err(DhPeerError::TooSmall)
        );
        assert_eq!(
            dh_compute_secret(&pa, &[2], Some(&x), &minus(1), false),
            Err(DhPeerError::Invalid)
        );
        assert_eq!(
            dh_compute_secret(&pa, &[2], Some(&x), &pa, false),
            Err(DhPeerError::Invalid)
        );
        assert!(dh_compute_secret(&pa, &[2], Some(&x), &minus(2), false).is_ok());
        // A named group refuses a peer outside its order-q subgroup.
        let modp14 = hex(DH_NAMED_GROUPS[6].p);
        let m = BigUint::from_bytes_be(&modp14);
        let m2 = (&m - BigUint::from(2u32)).to_bytes_be();
        assert_eq!(
            dh_compute_secret(&modp14, &[2], Some(&x), &m2, false),
            Err(DhPeerError::Invalid)
        );
        assert_eq!(
            dh_compute_secret(&modp14, &[2], Some(&x), &[2], false)
                .unwrap()
                .len(),
            256
        );
        // Unusable moduli: never a fallback, node's empty / zero results.
        let even = (&p + BigUint::from(1u32)).to_bytes_be();
        assert_eq!(
            dh_compute_secret(&even, &[2], Some(&x), &[3], false),
            Ok(Vec::new())
        );
        assert!(dh_generate_keys(&even, &[2], Some(&x), false, &mut Counter(0)).is_err());
        assert_eq!(
            dh_compute_secret(&pa, &[2], None, &[3], false),
            Ok(Vec::new())
        );
        assert_eq!(
            dh_compute_secret(&[23], &[2], Some(&x), &[3], false),
            Ok(vec![0])
        );
        assert!(dh_generate_keys(&[23], &[2], None, false, &mut Counter(0)).is_err());
        // A zero private value makes the shared value 1, which is refused.
        assert_eq!(
            dh_compute_secret(&pa, &[2], Some(&[0]), &[3], false),
            Ok(Vec::new())
        );
    }

    #[test]
    fn check_flags() {
        let pa = hex(PA);
        assert_eq!(dh_check(&pa, &[2]), Ok(0));
        assert_eq!(dh_check(&[3], &[2]), Ok(0x8a));
        assert_eq!(dh_check(&[2], &[2]), Ok(0x8b));
        assert_eq!(dh_check(&[1], &[2]), Ok(0x89));
        assert_eq!(dh_check(&[0], &[2]), Ok(0x89));
        assert_eq!(dh_check(&[], &[2]), Ok(0x89));
        assert_eq!(dh_check(&[4], &[2]), Ok(0x81));
        assert_eq!(dh_check(&[5], &[2]), Ok(0x80));
        assert_eq!(dh_check(&[15], &[2]), Ok(0x81));
        let p = BigUint::from_bytes_be(&pa);
        assert_eq!(
            dh_check(&pa, &(&p - BigUint::from(1u32)).to_bytes_be()),
            Ok(8)
        );
        assert_eq!(
            dh_check(&pa, &(&p - BigUint::from(2u32)).to_bytes_be()),
            Ok(0)
        );
        assert_eq!(
            dh_check(&(&p + BigUint::from(1u32)).to_bytes_be(), &[2]),
            Ok(1)
        );
        assert_eq!(dh_check(&hex(DH_NAMED_GROUPS[0].p), &[2]), Ok(0));
        let huge = vec![0xffu8; 4097];
        assert_eq!(dh_check(&huge, &[2]), Err(()));
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

/// Byte offset of the first PEM `-----BEGIN` marker in `input`, if any.
fn pem_begin_offset(input: &[u8]) -> Option<usize> {
    const MARKER: &[u8] = b"-----BEGIN";
    input.windows(MARKER.len()).position(|w| w == MARKER)
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

    // PEM is recognised by its BEGIN marker wherever it sits, not only at
    // byte 0: a leading newline, a BOM or a text preamble before the block
    // is still PEM. A marker past byte 0 only counts when the whole input
    // is not itself a DER certificate, so DER that happens to carry those
    // ten bytes keeps parsing as DER.
    let pem_at = pem_begin_offset(&input)
        .filter(|&at| at == 0 || x509_parser::parse_x509_certificate(&input).is_err());
    let der_bytes: Vec<u8> = if let Some(at) = pem_at {
        match x509_parser::pem::parse_x509_pem(&input[at..]) {
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
    let mut ku_list: Vec<&str> = Vec::new();
    // The pieces of OpenSSL's X509_check_issued that decide whether one
    // certificate issued another (the legacy object's issuerCertificate
    // links): the names, the key identifiers, and keyCertSign when a
    // KeyUsage extension is present at all.
    let mut key_cert_sign: Option<bool> = None;
    let mut ext_key_usage: Option<Vec<String>> = None;
    let mut subject_key_id: Option<String> = None;
    let mut authority_key_id: Option<String> = None;

    // subjectAltName and infoAccess are printed off the extension's DER as
    // Node prints them (oam_core::tls::names): each name that could be read
    // as more than one -- a comma, a quote, a control character -- is a JSON
    // string literal, so a list split on ", " (tls.checkServerIdentity's)
    // can never find a `DNS:` entry inside a URI or a directory name. The
    // first extension of each kind is the one Node prints; one that does not
    // decode is absent, as in Node.
    let first_extension = |oid: &str| {
        cert.extensions()
            .iter()
            .find(|ext| ext.oid.to_id_string() == oid)
            .map(|ext| ext.value)
    };
    let san_formatted = first_extension("2.5.29.17").and_then(oam_core::tls::names::alt_names_text);
    let info_access =
        first_extension("1.3.6.1.5.5.7.1.1").and_then(oam_core::tls::names::info_access_text);

    for ext in cert.extensions() {
        use x509_parser::extensions::ParsedExtension;
        match ext.parsed_extension() {
            ParsedExtension::BasicConstraints(bc) => {
                basic_constraints = Some(bc.ca);
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

/// The largest prime `generatePrime` (and the DH prime-length constructors,
/// which share the op) will search for. Node bounds `size` only by int32,
/// but the search is synchronous on the JS thread and its cost grows steeply
/// with the size, so anything far past real key sizes is refused up front
/// instead of pinning the thread. 65536 bits is four times the largest RSA
/// modulus OpenSSL accepts by default.
const MAX_GENERATED_PRIME_BITS: usize = 65_536;

/// Validate the bit count for `op_crypto_generate_prime`.
fn prime_bits_arg(raw: f64) -> Result<usize, String> {
    if raw.is_nan() || raw.fract() != 0.0 || raw < 2.0 {
        return Err("generatePrime: bits must be an integer >= 2".into());
    }
    if raw > MAX_GENERATED_PRIME_BITS as f64 {
        return Err(format!(
            "generatePrime: bits must be <= {MAX_GENERATED_PRIME_BITS}"
        ));
    }
    Ok(raw as usize)
}

pub(crate) fn op_crypto_generate_prime(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    use num_bigint_dig::RandPrime;
    // Read as a double and range-check BEFORE the cast: an `int32_value(..)
    // as usize` sign-extends a negative (and 2**31, which ToInt32 wraps
    // negative) into a bit count near 2^64 that passes a `< 2` test.
    let raw = args.get(0).number_value(scope).unwrap_or(f64::NAN);
    let bits = match prime_bits_arg(raw) {
        Ok(bits) => bits,
        Err(msg) => {
            crate::node_ops::throw_type_error(scope, &msg);
            return;
        }
    };
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
mod kdf_der_and_ecdsa {
    use super::*;

    // Expected bytes are node v22.22.2's, for the same inputs:
    // scryptSync('password', 'salt', len, { N: 1024, r: 8, p: 1 }).
    const SCRYPT_70: &str = "16dbc8906763c7f048977a68f9d305f7710e068ca2cd95dab372125bb3f19608\
                             175003c79f9cdee65d2e45fc1f169afde0a6806f5d4f2ba0584249d2e66c2c96\
                             cee540dfeaec";

    // The output length is the caller's: below 10 and above 64 included.
    #[test]
    fn scrypt_output_length_is_the_callers() {
        for keylen in [8usize, 32, 70] {
            let dk = scrypt_derive(b"password", b"salt", keylen, 1024, 8, 1).unwrap();
            assert_eq!(dk.len(), keylen);
            assert_eq!(hex_lower(&dk), SCRYPT_70[..keylen * 2]);
        }
        assert!(
            scrypt_derive(b"password", b"salt", 0, 1024, 8, 1)
                .unwrap()
                .is_empty()
        );
        assert!(scrypt_derive(b"password", b"salt", 32, 1000, 8, 1).is_err());
        assert!(scrypt_derive(b"password", b"salt", 32, 0, 8, 1).is_err());
    }

    // Every digest the unknown-digest message lists is one the KDFs take.
    #[test]
    fn kdfs_take_every_listed_digest() {
        for digest in SUPPORTED_HASHES {
            let derived = pbkdf2_derive(digest, b"password", b"salt", 1, 8);
            assert!(derived.is_some(), "pbkdf2 {digest}");
            let expanded = hkdf_derive(digest, Some(b"salt"), b"key", b"info", 8);
            assert!(matches!(expanded, Some(Ok(_))), "hkdf {digest}");
        }
        assert!(pbkdf2_derive("sha3256", b"password", b"salt", 1, 8).is_none());
        assert!(hkdf_derive("sha3256", None, b"key", b"info", 8).is_none());

        // node: pbkdf2Sync('password', 'salt', 2, 20, 'sha224') and
        // hkdfSync(digest, 'key', 'salt', 'info', 16).
        let dk = pbkdf2_derive("sha224", b"password", b"salt", 2, 20).unwrap();
        assert_eq!(hex_lower(&dk), "93200ffa96c5776d38fa10abdf8f5bfc0054b971");
        let okm = hkdf_derive("md5", Some(b"salt"), b"key", b"info", 16)
            .unwrap()
            .unwrap();
        assert_eq!(hex_lower(&okm), "edd053d0030729baf813c1e0f0e9941d");
        let okm = hkdf_derive("sha224", Some(b"salt"), b"key", b"info", 16)
            .unwrap()
            .unwrap();
        assert_eq!(hex_lower(&okm), "0ff75af37d8684eb5b7a470cd4dedabd");
    }

    // A declared length is checked against the input before it is used.
    #[test]
    fn asn1_lengths_are_bounds_checked() {
        let (rest, body) = parse_asn1_element(&[0x30, 0x03, 0x02, 0x01, 0x05, 0xaa]).unwrap();
        assert_eq!(body, [0x02, 0x01, 0x05]);
        assert_eq!(rest, [0xaa]);
        let (rest, body) = parse_asn1_element(&[0x30, 0x81, 0x03, 0x02, 0x01, 0x05]).unwrap();
        assert_eq!(body, [0x02, 0x01, 0x05]);
        assert!(rest.is_empty());

        // Eight length bytes, all ones.
        let mut wide = vec![0x30, 0x88];
        wide.extend_from_slice(&[0xff; 8]);
        wide.push(0x00);
        assert!(parse_asn1_element(&wide).is_err());
        assert!(extract_spki_pubkey(&wide).is_err());
        // Four length bytes naming more than is there.
        assert!(parse_asn1_element(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff, 0x00]).is_err());
        // The indefinite form, a truncated length, a truncated body.
        assert!(parse_asn1_element(&[0x30, 0x80, 0x00, 0x00]).is_err());
        assert!(parse_asn1_element(&[0x30, 0x82, 0x01]).is_err());
        assert!(parse_asn1_element(&[0x30, 0x05, 0x02, 0x01]).is_err());
        assert!(parse_asn1_element(&[0x30]).is_err());
    }

    #[test]
    fn p1363_reencodes_as_minimal_der_integers() {
        assert_eq!(
            ecdsa_p1363_to_der(&[0x00, 0x80, 0x00, 0x01]).unwrap(),
            [0x30, 0x07, 0x02, 0x02, 0x00, 0x80, 0x02, 0x01, 0x01]
        );
        assert_eq!(
            ecdsa_p1363_to_der(&[0x00, 0x00, 0x7f, 0xff]).unwrap(),
            [0x30, 0x07, 0x02, 0x01, 0x00, 0x02, 0x02, 0x7f, 0xff]
        );
        assert!(ecdsa_p1363_to_der(&[]).is_none());
        assert!(ecdsa_p1363_to_der(&[0x01, 0x02, 0x03]).is_none());
    }

    fn pem_pair(pair: (Vec<u8>, Vec<u8>)) -> (String, String) {
        let private = String::from_utf8(pair.0).unwrap();
        let public = String::from_utf8(pair.1).unwrap();
        (private, public)
    }

    // DER is node:crypto's default and r||s is WebCrypto's; each verifies
    // under its own encoding only, and the curve comes from the key.
    #[test]
    fn ecdsa_encodings_and_curve_from_the_key() {
        let msg = b"the message";
        let (priv256, pub256) = pem_pair(crypto_generate_ec_p256().unwrap());
        let (priv384, pub384) = pem_pair(crypto_generate_ec_p384().unwrap());

        let der = crypto_sign_ec("sha256", msg, &priv256, true).unwrap();
        assert_eq!(der[0], 0x30);
        assert_eq!(der.len(), usize::from(der[1]) + 2);
        assert!(crypto_verify_ec("sha256", msg, &pub256, &der, true).unwrap());
        assert!(!crypto_verify_ec("sha256", b"another", &pub256, &der, true).unwrap());
        assert!(!crypto_verify_ec("sha256", msg, &pub256, &der, false).unwrap());

        let fixed = crypto_sign_ec("sha256", msg, &priv256, false).unwrap();
        assert_eq!(fixed.len(), 64);
        assert!(crypto_verify_ec("sha256", msg, &pub256, &fixed, false).unwrap());
        assert!(!crypto_verify_ec("sha256", msg, &pub256, &fixed, true).unwrap());
        let reencoded = ecdsa_p1363_to_der(&fixed).unwrap();
        assert!(crypto_verify_ec("sha256", msg, &pub256, &reencoded, true).unwrap());

        let fixed384 = crypto_sign_ec("sha384", msg, &priv384, false).unwrap();
        assert_eq!(fixed384.len(), 96);
        assert!(crypto_verify_ec("sha384", msg, &pub384, &fixed384, false).unwrap());
        let der384 = crypto_sign_ec("sha384", msg, &priv384, true).unwrap();
        assert!(crypto_verify_ec("sha384", msg, &pub384, &der384, true).unwrap());

        // A mixed pair: refused by name when signing, and a signature made
        // for another digest does not verify (it is not an error).
        let err = crypto_sign_ec("sha256", msg, &priv384, true).unwrap_err();
        assert!(
            err.contains("P-384") && err.contains("not supported"),
            "{err}"
        );
        assert!(!crypto_verify_ec("sha256", msg, &pub384, &der384, true).unwrap());
        assert!(!crypto_verify_ec("sha256", msg, &pub384, &fixed384, false).unwrap());
        assert!(!crypto_verify_ec("sha384", msg, &pub256, &fixed, false).unwrap());
    }
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

    #[test]
    fn pem_marker_is_found_past_byte_zero() {
        assert_eq!(pem_begin_offset(b"-----BEGIN CERTIFICATE-----\n"), Some(0));
        assert_eq!(
            pem_begin_offset(b"\n-----BEGIN CERTIFICATE-----\n"),
            Some(1)
        );
        assert_eq!(
            pem_begin_offset(b"\xef\xbb\xbf-----BEGIN CERTIFICATE-----\n"),
            Some(3)
        );
        assert_eq!(
            pem_begin_offset(b"Certificate:\n    Data:\n-----BEGIN CERTIFICATE-----\n"),
            Some(23)
        );
        assert_eq!(pem_begin_offset(&[0x30, 0x82, 0x01, 0x0a]), None);
        assert_eq!(pem_begin_offset(b"-----BEGI"), None);
        assert_eq!(pem_begin_offset(b""), None);
    }
}

#[cfg(test)]
mod rsa_argument_bounds {
    use super::*;

    #[test]
    fn salt_length_argument_keeps_the_sentinels() {
        assert_eq!(pss_salt_arg(f64::NAN), Ok(RSA_PSS_SALTLEN_AUTO));
        assert_eq!(pss_salt_arg(-2.0), Ok(RSA_PSS_SALTLEN_AUTO));
        assert_eq!(pss_salt_arg(-1.0), Ok(RSA_PSS_SALTLEN_DIGEST));
        assert_eq!(pss_salt_arg(0.0), Ok(0));
        assert_eq!(pss_salt_arg(32.0), Ok(32));
        assert!(pss_salt_arg(-3.0).is_err());
        assert!(pss_salt_arg(1.5).is_err());
        assert!(pss_salt_arg(f64::INFINITY).is_err());
        assert!(pss_salt_arg(4_294_967_296.0).is_err());
    }

    // emLen - hLen - 2, with emLen = ceil((modBits - 1) / 8).
    #[test]
    fn max_salt_length_follows_the_modulus_and_digest() {
        assert_eq!(pss_max_salt_len(2048, 32), Some(222));
        assert_eq!(pss_max_salt_len(1024, 64), Some(62));
        assert_eq!(pss_max_salt_len(1025, 64), Some(62));
        assert_eq!(pss_max_salt_len(512, 64), None);
        assert_eq!(pss_max_salt_len(0, 32), None);
        assert_eq!(pss_sign_salt_len(RSA_PSS_SALTLEN_DIGEST, 2048, 32), Ok(32));
        assert_eq!(pss_sign_salt_len(RSA_PSS_SALTLEN_AUTO, 2048, 32), Ok(222));
        assert_eq!(pss_sign_salt_len(222, 2048, 32), Ok(222));
        assert!(pss_sign_salt_len(223, 2048, 32).is_err());
        assert!(pss_sign_salt_len(500, 2048, 32).is_err());
        assert!(pss_sign_salt_len(64, 1024, 64).is_err());
    }

    #[test]
    fn prime_bit_count_is_range_checked_before_the_cast() {
        assert_eq!(prime_bits_arg(2.0), Ok(2));
        assert_eq!(prime_bits_arg(2048.0), Ok(2048));
        assert_eq!(
            prime_bits_arg(MAX_GENERATED_PRIME_BITS as f64),
            Ok(MAX_GENERATED_PRIME_BITS)
        );
        assert!(prime_bits_arg(-1.0).is_err());
        assert!(prime_bits_arg(0.0).is_err());
        assert!(prime_bits_arg(1.0).is_err());
        assert!(prime_bits_arg(1.5).is_err());
        assert!(prime_bits_arg(2_147_483_648.0).is_err());
        assert!(prime_bits_arg(f64::NAN).is_err());
        assert!(prime_bits_arg(f64::INFINITY).is_err());
        assert!(prime_bits_arg(MAX_GENERATED_PRIME_BITS as f64 + 1.0).is_err());
    }

    // One 1024-bit key for the round trips: with sha512 its longest salt is
    // 62 bytes, so a 64-byte salt is the "does not fit" case.
    #[test]
    fn pss_and_private_encrypt_round_trip() {
        let (priv_pem, pub_pem) = crypto_generate_rsa(1024).expect("keygen");
        let priv_pem = String::from_utf8(priv_pem).expect("private pem");
        let pub_pem = String::from_utf8(pub_pem).expect("public pem");
        let msg = b"round trip";

        // A salt the key cannot encode is an error, not a panic.
        assert!(crypto_sign_rsa_pss("sha512", msg, &priv_pem, 64).is_err());
        assert!(crypto_sign_rsa_pss("sha256", msg, &priv_pem, 500).is_err());

        // Default (max) salt: AUTO and the explicit maximum verify it, a
        // different fixed length does not.
        let sig = crypto_sign_rsa_pss("sha256", msg, &priv_pem, RSA_PSS_SALTLEN_AUTO)
            .expect("sign with the max salt");
        let max = pss_max_salt_len(1024, 32).expect("max salt") as i64;
        let verify = |salt: i64, sig: &[u8]| {
            crypto_verify_rsa_pss("sha256", msg, &pub_pem, sig, salt).expect("verify")
        };
        assert!(verify(RSA_PSS_SALTLEN_AUTO, &sig));
        assert!(verify(max, &sig));
        assert!(!verify(32, &sig));
        assert!(!verify(RSA_PSS_SALTLEN_DIGEST, &sig));
        assert!(!verify(i64::from(i32::MAX), &sig));

        // Digest-length salt: -1, the literal length and AUTO all agree.
        let sig = crypto_sign_rsa_pss("sha256", msg, &priv_pem, RSA_PSS_SALTLEN_DIGEST)
            .expect("sign with the digest-length salt");
        assert!(verify(RSA_PSS_SALTLEN_DIGEST, &sig));
        assert!(verify(32, &sig));
        assert!(verify(RSA_PSS_SALTLEN_AUTO, &sig));
        assert!(!verify(max, &sig));

        // An empty salt is a real length, distinct from the sentinels.
        let sig = crypto_sign_rsa_pss("sha1", msg, &priv_pem, 0).expect("sign, empty salt");
        assert!(crypto_verify_rsa_pss("sha1", msg, &pub_pem, &sig, 0).expect("verify"));
        assert!(
            crypto_verify_rsa_pss("sha1", msg, &pub_pem, &sig, RSA_PSS_SALTLEN_AUTO)
                .expect("verify")
        );

        // A tampered signature fails under AUTO too.
        let mut bad = sig.clone();
        bad[10] ^= 0x01;
        assert!(
            !crypto_verify_rsa_pss("sha1", msg, &pub_pem, &bad, RSA_PSS_SALTLEN_AUTO)
                .expect("verify")
        );

        // privateEncrypt output is what publicDecrypt reads back.
        let boxed = crypto_private_encrypt(msg, &priv_pem, "pkcs1").expect("privateEncrypt");
        assert_eq!(boxed.len(), 128);
        assert_eq!(
            crypto_public_decrypt(&boxed, &pub_pem, "pkcs1").expect("publicDecrypt"),
            msg.to_vec()
        );
        assert!(crypto_private_encrypt(&[0u8; 118], &priv_pem, "pkcs1").is_err());
    }
}

#[cfg(test)]
mod rsa_keys_and_padding {
    use super::*;

    // A 1024-bit key node v22.22.2 generated, in PKCS#8 and PKCS#1, and its
    // public half. Every expected value below is node's for this key.
    const KEY_PKCS8: &str = "-----BEGIN PRIVATE KEY-----\n\
        MIICdgIBADANBgkqhkiG9w0BAQEFAASCAmAwggJcAgEAAoGBAJ7h+HNnCM++xncJ\n\
        yfFIsHZzRBhPpfkgeCTSorJnKTxnBtg+rxgLFb0QxJmbT3i6bejG0bVQohIMiCNQ\n\
        ZE7K5QM9KESIJ+zES7WHFmEptsre6aaOF1wNzNWXYPaNnQ9ZZEqSF4NKl1WTYNmL\n\
        S2bJxdqCaSWwzaM3aDvS9P+1fKyvAgMBAAECgYAxZ70VTZXM29rlCzMzh1HwE1m6\n\
        YZIPzMvvT1xuM2v8L2FOfvd9d3z0BLHNviDRPs+Mkb6GLiOnjyQKLyZx5d+eS8VI\n\
        Xzj9OCInO/G5ZmlynVBkP3PVYCqVpUNBuRPmmUlPfLExpRU4yoLyCw4/V/Wc4uoF\n\
        sTH9RdvYX6ttgGsbcQJBAM0aI8QHxBQa1nLHv1Yy1j6/AVbyZLAvdMv+1MvCggi6\n\
        5PIVN6oha/CSsia3xMlrsJ+oKaOrJV+LTR2M1TjHP6cCQQDGT5Lvcn852kb6XgQl\n\
        nBY07NVhQEP7JGI9ssRnufhqekeqLPOaCwmCTFtZXibNwNJimRSIJTYt34rcGabQ\n\
        kIu5AkA+cObSl0qqlEEVUJp/s5pmH+BYiBL/UHNOZ5aBMgZEgiGRUcRQlzw1UXPL\n\
        NcdDSvXKpXeHzGLkY5RmmqmPT5mhAkBjqMO3z7t+MGKxGaRN3kYOrOKC8A/52wv/\n\
        46HuWrgrkm4HaHW3eUp7D7Jsk/M+PWO9Jx75WHIE1a1r9PwvHiUJAkEAnlKQrcNS\n\
        kxXXypzx+QgV9ajVqHftNB3MH0hca7WzjFLw7vd+QgGQpsSzz0xEl6sEbcDATfCp\n\
        2XlK/AUTXVY3lA==\n\
        -----END PRIVATE KEY-----\n";
    const KEY_PKCS1: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
        MIICXAIBAAKBgQCe4fhzZwjPvsZ3CcnxSLB2c0QYT6X5IHgk0qKyZyk8ZwbYPq8Y\n\
        CxW9EMSZm094um3oxtG1UKISDIgjUGROyuUDPShEiCfsxEu1hxZhKbbK3ummjhdc\n\
        DczVl2D2jZ0PWWRKkheDSpdVk2DZi0tmycXagmklsM2jN2g70vT/tXysrwIDAQAB\n\
        AoGAMWe9FU2VzNva5QszM4dR8BNZumGSD8zL709cbjNr/C9hTn73fXd89ASxzb4g\n\
        0T7PjJG+hi4jp48kCi8mceXfnkvFSF84/TgiJzvxuWZpcp1QZD9z1WAqlaVDQbkT\n\
        5plJT3yxMaUVOMqC8gsOP1f1nOLqBbEx/UXb2F+rbYBrG3ECQQDNGiPEB8QUGtZy\n\
        x79WMtY+vwFW8mSwL3TL/tTLwoIIuuTyFTeqIWvwkrImt8TJa7CfqCmjqyVfi00d\n\
        jNU4xz+nAkEAxk+S73J/OdpG+l4EJZwWNOzVYUBD+yRiPbLEZ7n4anpHqizzmgsJ\n\
        gkxbWV4mzcDSYpkUiCU2Ld+K3Bmm0JCLuQJAPnDm0pdKqpRBFVCaf7OaZh/gWIgS\n\
        /1BzTmeWgTIGRIIhkVHEUJc8NVFzyzXHQ0r1yqV3h8xi5GOUZpqpj0+ZoQJAY6jD\n\
        t8+7fjBisRmkTd5GDqzigvAP+dsL/+Oh7lq4K5JuB2h1t3lKew+ybJPzPj1jvSce\n\
        +VhyBNWta/T8Lx4lCQJBAJ5SkK3DUpMV18qc8fkIFfWo1ah37TQdzB9IXGu1s4xS\n\
        8O73fkIBkKbEs89MRJerBG3AwE3wqdl5SvwFE11WN5Q=\n\
        -----END RSA PRIVATE KEY-----\n";
    const KEY_SPKI: &str = "-----BEGIN PUBLIC KEY-----\n\
        MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQCe4fhzZwjPvsZ3CcnxSLB2c0QY\n\
        T6X5IHgk0qKyZyk8ZwbYPq8YCxW9EMSZm094um3oxtG1UKISDIgjUGROyuUDPShE\n\
        iCfsxEu1hxZhKbbK3ummjhdcDczVl2D2jZ0PWWRKkheDSpdVk2DZi0tmycXagmkl\n\
        sM2jN2g70vT/tXysrwIDAQAB\n\
        -----END PUBLIC KEY-----\n";

    // A 512-bit public key (node generated it too).
    const SMALL_SPKI: &str = "-----BEGIN PUBLIC KEY-----\n\
        MFwwDQYJKoZIhvcNAQEBBQADSwAwSAJBANm3+2BW8xDRvRNFpSDU7MAoLyapdhj7\n\
        z6B7hNpQdBqn0Uzk+5yNfHPLMGjXsQNVXVJOZTzB5ywHD2ePJZsMY1kCAwEAAQ==\n\
        -----END PUBLIC KEY-----\n";

    // `createPrivateKey(KEY_PKCS8).export({ format: 'jwk' })`, as hex.
    const JWK_N: &[&str] = &[
        "9ee1f8736708cfbec67709c9f148b0767344184fa5f9207824d2a2b267293c67",
        "06d83eaf180b15bd10c4999b4f78ba6de8c6d1b550a2120c882350644ecae503",
        "3d28448827ecc44bb587166129b6cadee9a68e175c0dccd59760f68d9d0f5964",
        "4a9217834a97559360d98b4b66c9c5da826925b0cda337683bd2f4ffb57cacaf",
    ];
    const JWK_D: &[&str] = &[
        "3167bd154d95ccdbdae50b33338751f01359ba61920fcccbef4f5c6e336bfc2f",
        "614e7ef77d777cf404b1cdbe20d13ecf8c91be862e23a78f240a2f2671e5df9e",
        "4bc5485f38fd3822273bf1b96669729d50643f73d5602a95a54341b913e69949",
        "4f7cb131a51538ca82f20b0e3f57f59ce2ea05b131fd45dbd85fab6d806b1b71",
    ];
    const JWK_P: &[&str] = &[
        "cd1a23c407c4141ad672c7bf5632d63ebf0156f264b02f74cbfed4cbc28208ba",
        "e4f21537aa216bf092b226b7c4c96bb09fa829a3ab255f8b4d1d8cd538c73fa7",
    ];
    const JWK_Q: &[&str] = &[
        "c64f92ef727f39da46fa5e04259c1634ecd5614043fb24623db2c467b9f86a7a",
        "47aa2cf39a0b09824c5b595e26cdc0d26299148825362ddf8adc19a6d0908bb9",
    ];
    const JWK_DP: &[&str] = &[
        "3e70e6d2974aaa944115509a7fb39a661fe0588812ff50734e67968132064482",
        "219151c450973c355173cb35c7434af5caa57787cc62e46394669aa98f4f99a1",
    ];
    const JWK_DQ: &[&str] = &[
        "63a8c3b7cfbb7e3062b119a44dde460eace282f00ff9db0bffe3a1ee5ab82b92",
        "6e076875b7794a7b0fb26c93f33e3d63bd271ef9587204d5ad6bf4fc2f1e2509",
    ];
    const JWK_QI: &[&str] = &[
        "9e5290adc3529315d7ca9cf1f90815f5a8d5a877ed341dcc1f485c6bb5b38c52",
        "f0eef77e420190a6c4b3cf4c4497ab046dc0c04df0a9d9794afc05135d563794",
    ];

    // privateEncrypt(KEY_PKCS8, 'pe').
    const PRIVATE_ENCRYPT_PE: &[&str] = &[
        "083d69f88bbad775d9a4f41892df9073b81b56d6394c42e57cdd41a1f5100f6a",
        "87798f2f92af4959a94ef8733e1b83c189f019645fc2cd55a2c210b3bf835775",
        "208480697a8d3748f53eed2621faa25cab0628329bf7b3af00f7076ccae61bb2",
        "e786a558032bec642248d172d32f213a625b6510cad09c8344a11f8a90624073",
    ];

    // RSA_NO_PADDING on the block `unpadded_block()` builds: publicEncrypt
    // and privateEncrypt.
    const UNPADDED_PUBLIC: &[&str] = &[
        "5c5714a444d06821cb195f12d1e236bddd90521c61378d079403fd94499cc1e2",
        "75c3d81b58d9a1ed351bae75c8ff345cf8bef69d0611ff5a5f28064e6a96f437",
        "a97fb8a8165320b3c378a4d0e2f8d4dd61fc65e52be87d716c91a85c2be023f7",
        "6236fd215e2dc4c0c898d3f40f90917a54372c2495e78839baddb0de78b85f45",
    ];
    const UNPADDED_PRIVATE: &[&str] = &[
        "24af20bd61f99f9ab1bd137c27c8838e917065e4820bb6c5a58749ff56dd69e2",
        "acdfb38ffd3b46ed64cbe1adf1406f5e442f83d247ca8281e8953c2d847c2a20",
        "757d7d5005a4fa5146f73c84b01762eb69ce8375f2020e7feefa8853328a3930",
        "cfa9fc7f46d7b5556a2ad69968215afc21295b8f61b737c72577645252b2b240",
    ];

    // privateDecrypt with RSA_PKCS1_PADDING on `undecodable_block()`, and on
    // 127 bytes of 0x01.
    const UNDECODABLE_PLAINTEXT: &[&str] = &[
        "ef359f8c5c60603737c41a1cc6f111897ef81acc65fffd0a10d2287ea8cbed0e",
        "a85810f43cc6e699f2d14cb065abb0f3f79cc23a0ad9ff129383491aa850cd61",
        "91cde646daf3b322ca",
    ];
    const SHORT_BLOCK_PLAINTEXT: &[&str] = &[
        "3cce9b415c455eedc4460c834ec3b4beef554d0edba5c4a92833b6ef7eddb4f9",
        "2bd653453224afc5a0e3567f9f55512ae82188bbf685f7b99cf8d219717d1d07",
        "a80c3562acbcde4c9ef473bea52da61e778377cb6a2523597c13af47a94c7361",
        "537edc03760d0cebc4",
    ];

    // node's publicEncrypt of 'oaep' under sha256 with the label 'oam-label',
    // and its PSS signature (sha256, salt 7) over 'round trip'.
    const NODE_OAEP_SHA256_LABEL: &[&str] = &[
        "96f1a22bec88e8b726ba84bd328b9ca7c214964825d39473e71021e29f887efa",
        "2677f74ed999353b2d1874a847aa95e24bb5be8fe8487b32adc5b9d135468bc4",
        "8424f6a385f1d3103b51a024423d33cd3ee9a0f468d8615c1be5f1906a362e59",
        "7744bbd3e375b88cefde157cdd9e933102415ebe9a5174982ed149e4f9a2cccc",
    ];
    const NODE_PSS_SHA256_SALT7: &[&str] = &[
        "1801828d62984920cf7d3b43bbebb8d1c90c05038270babe1e6e396aa9130a1b",
        "7d5d91a0477d0bed2a1a359dfe34f016ef2b70cb4f04a311e664f66c65896f56",
        "92dad8283412e638be4e90eda782bb1eade49b74802f0fb3c1dabfdeba809507",
        "742c703ee7e17d0e2ee2bb60358a53cb045ffcdf069dd7d4175b53e59f5db821",
    ];

    fn unhex(parts: &[&str]) -> Vec<u8> {
        let hex = parts.concat();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn private_key() -> RsaPrivateKey {
        parse_rsa_private_key(&pem_to_der(KEY_PKCS8).expect("pem"), "PRIVATE KEY").expect("key")
    }

    fn unpadded_block() -> Vec<u8> {
        let mut block: Vec<u8> = (0..128usize).map(|j| ((j * 13 + 5) & 0xff) as u8).collect();
        block[0] = 0;
        block[1] = 1;
        block
    }

    fn undecodable_block() -> Vec<u8> {
        let mut block: Vec<u8> = (0..128usize)
            .map(|j| ((j * 37 + 11) & 0xff) as u8)
            .collect();
        block[0] &= 0x3f;
        block
    }

    #[test]
    fn jwk_parts_are_nodes_for_the_same_key() {
        let parts = rsa_private_jwk_parts(&private_key()).expect("jwk parts");
        assert_eq!(parts.n, unhex(JWK_N));
        assert_eq!(parts.e, vec![0x01, 0x00, 0x01]);
        assert_eq!(parts.d, unhex(JWK_D));
        assert_eq!(parts.p, unhex(JWK_P));
        assert_eq!(parts.q, unhex(JWK_Q));
        assert_eq!(parts.dp, unhex(JWK_DP));
        assert_eq!(parts.dq, unhex(JWK_DQ));
        assert_eq!(parts.qi, unhex(JWK_QI));
        // The PKCS#1 encoding of the key gives the same integers.
        let pkcs1 = parse_rsa_private_key(&pem_to_der(KEY_PKCS1).expect("pem"), "RSA PRIVATE KEY")
            .expect("pkcs1 key");
        assert_eq!(rsa_private_jwk_parts(&pkcs1).expect("jwk parts"), parts);
    }

    // The CRT parameters checked by arithmetic that does not go through the
    // `rsa` crate: dp = d mod (p-1), dq = d mod (q-1), qi * q = 1 mod p.
    fn assert_crt_parameters_hold(parts: &RsaPrivateJwkParts) {
        let int = |bytes: &[u8]| BigUint::from_bytes_be(bytes);
        let one = BigUint::from(1u32);
        let (n, e, d) = (int(&parts.n), int(&parts.e), int(&parts.d));
        let (p, q) = (int(&parts.p), int(&parts.q));
        assert_eq!(&p * &q, n);
        assert_eq!(int(&parts.dp), &d % (&p - &one));
        assert_eq!(int(&parts.dq), &d % (&q - &one));
        assert_eq!((int(&parts.qi) * &q) % &p, one);
        assert_eq!((&d * &e) % (&p - &one), one);
        assert_eq!((&d * &e) % (&q - &one), one);
        // Minimal encodings: no leading zero byte on any integer.
        for bytes in [
            &parts.n, &parts.e, &parts.d, &parts.p, &parts.q, &parts.dp, &parts.dq, &parts.qi,
        ] {
            assert_ne!(bytes.first(), Some(&0));
        }
    }

    #[test]
    fn crt_parameters_hold_for_fixed_and_generated_keys() {
        assert_crt_parameters_hold(&rsa_private_jwk_parts(&private_key()).expect("jwk parts"));
        // Generated keys, including sizes that are not a whole number of
        // bytes and the smallest one generateKeyPair makes.
        for bits in [512usize, 1023, 1024, 1025] {
            let (priv_pem, pub_pem) = crypto_generate_rsa(bits).expect("keygen");
            let priv_pem = String::from_utf8(priv_pem).expect("private pem");
            let pub_pem = String::from_utf8(pub_pem).expect("public pem");
            let key = parse_rsa_private_key(&pem_to_der(&priv_pem).expect("pem"), "PRIVATE KEY")
                .expect("generated key parses");
            let parts = rsa_private_jwk_parts(&key).expect("jwk parts");
            assert_crt_parameters_hold(&parts);
            assert_eq!(
                BigUint::from_bytes_be(&parts.n).bits(),
                bits,
                "modulus bits"
            );
            assert_eq!(parts.e, vec![0x01, 0x00, 0x01]);
            // The public PEM is the private key's public half.
            let public = parse_rsa_public_key(&pem_to_der(&pub_pem).expect("pem"), "PUBLIC KEY")
                .expect("generated public key parses");
            assert_eq!(public.n_bytes().into_vec(), parts.n);
        }
    }

    // JWK integers back to a key: the same PKCS#8 bytes node exported.
    #[test]
    fn jwk_parts_round_trip_to_the_same_pkcs8() {
        let parts = rsa_private_jwk_parts(&private_key()).expect("jwk parts");
        let int = |bytes: &[u8]| BoxedUint::from_be_slice_vartime(bytes);
        let rebuilt = RsaPrivateKey::from_components(
            int(&parts.n),
            int(&parts.e),
            int(&parts.d),
            vec![int(&parts.p), int(&parts.q)],
        )
        .expect("key from its JWK integers");
        let der = rebuilt.to_pkcs8_der().expect("pkcs8");
        assert_eq!(
            der.as_bytes(),
            pem_to_der(KEY_PKCS8).expect("pem").as_slice()
        );
        assert_eq!(rsa_private_jwk_parts(&rebuilt).expect("jwk parts"), parts);
    }

    #[test]
    fn generated_modulus_size_is_range_checked() {
        for bits in [0usize, 256, 511, 16385, usize::MAX] {
            let err = crypto_generate_rsa(bits).expect_err("refused");
            assert_eq!(
                err,
                "generateKeyPairSync: modulusLength must be between 512 and 16384"
            );
        }
    }

    // A public key below the `rsa` crate's default 1024-bit floor decodes,
    // from SPKI and from the bare PKCS#1 structure inside it.
    #[test]
    fn small_public_key_decodes_and_encrypts() {
        let spki = pem_to_der(SMALL_SPKI).expect("pem");
        let key = parse_rsa_public_key(&spki, "PUBLIC KEY").expect("512-bit public key");
        assert_eq!(key.size(), 64);
        let pkcs1 = extract_spki_pubkey(&spki).expect("pkcs1 inside spki");
        let again = parse_rsa_public_key(&pkcs1, "RSA PUBLIC KEY").expect("pkcs1 public key");
        assert_eq!(again.n_bytes(), key.n_bytes());
        for padding in ["pkcs1", "oaep"] {
            let ct =
                crypto_public_encrypt(b"m", SMALL_SPKI, padding, "sha1", b"").expect("encrypt");
            assert_eq!(ct.len(), 64);
        }
    }

    #[test]
    fn padding_names() {
        assert_eq!(RsaPadding::parse("pkcs1"), Some(RsaPadding::Pkcs1));
        assert_eq!(RsaPadding::parse("oaep"), Some(RsaPadding::Oaep));
        assert_eq!(RsaPadding::parse("none"), Some(RsaPadding::None));
        assert_eq!(RsaPadding::parse("unknown"), None);
        assert_eq!(RsaPadding::parse(""), None);
        let block = vec![0u8; 128];
        assert_eq!(
            crypto_public_encrypt(&block, KEY_SPKI, "unknown", "sha1", b""),
            Err(RSA_ERR_UNKNOWN_PADDING_TYPE.to_string())
        );
        assert_eq!(
            crypto_private_decrypt(&block, KEY_PKCS8, "unknown", "sha1", b""),
            Err(RSA_ERR_UNKNOWN_PADDING_TYPE.to_string())
        );
        // privateEncrypt / publicDecrypt have no OAEP.
        for padding in ["oaep", "unknown"] {
            assert_eq!(
                crypto_private_encrypt(b"m", KEY_PKCS8, padding),
                Err(RSA_ERR_UNSUPPORTED_PADDING_MODE.to_string())
            );
            assert_eq!(
                crypto_public_decrypt(&block, KEY_SPKI, padding),
                Err(RSA_ERR_UNSUPPORTED_PADDING_MODE.to_string())
            );
        }
    }

    #[test]
    fn input_blocks_are_length_and_range_checked() {
        let key = private_key();
        let k = key.size();
        assert_eq!(k, 128);
        // Encrypting without padding: exactly k bytes, below the modulus.
        assert_eq!(
            rsa_unpadded_input(&key, &vec![1u8; k - 1]),
            Err(RSA_ERR_DATA_TOO_SMALL_FOR_KEY_SIZE.to_string())
        );
        assert_eq!(
            rsa_unpadded_input(&key, &vec![1u8; k + 1]),
            Err(RSA_ERR_DATA_TOO_LARGE_FOR_KEY_SIZE.to_string())
        );
        assert_eq!(
            rsa_unpadded_input(&key, &vec![0xffu8; k]),
            Err(RSA_ERR_DATA_TOO_LARGE_FOR_MODULUS.to_string())
        );
        assert!(rsa_unpadded_input(&key, &unpadded_block()).is_ok());
        // The modulus itself is not below the modulus; one less is.
        let n = unhex(JWK_N);
        assert_eq!(
            rsa_unpadded_input(&key, &n),
            Err(RSA_ERR_DATA_TOO_LARGE_FOR_MODULUS.to_string())
        );
        let mut n_minus_one = n.clone();
        *n_minus_one.last_mut().expect("byte") -= 1;
        assert!(rsa_unpadded_input(&key, &n_minus_one).is_ok());

        // Decrypting: up to k bytes, left-padded.
        assert_eq!(
            rsa_decrypt_input(&key, &vec![1u8; k + 1]),
            Err(RSA_ERR_DATA_GREATER_THAN_MOD_LEN.to_string())
        );
        assert_eq!(
            rsa_decrypt_input(&key, &vec![0xffu8; k]),
            Err(RSA_ERR_DATA_TOO_LARGE_FOR_MODULUS.to_string())
        );
        let padded = rsa_decrypt_input(&key, &[7, 8, 9]).expect("short block");
        assert_eq!(padded.len(), k);
        assert_eq!(&padded[k - 3..], &[7, 8, 9]);
        assert!(padded[..k - 3].iter().all(|&b| b == 0));
        assert_eq!(
            rsa_decrypt_input(&key, &[]).expect("empty block"),
            vec![0u8; k]
        );

        // Integer to block: always k bytes, whatever the value.
        let small = BoxedUint::from_be_slice(&[5], key.n_bits_precision()).expect("uint");
        let block = rsa_uint_to_block(&small, k);
        assert_eq!(block.len(), k);
        assert_eq!(block[k - 1], 5);
        assert!(block[..k - 1].iter().all(|&b| b == 0));
    }

    #[test]
    fn unpadded_operations_are_nodes() {
        let block = unpadded_block();
        let c = crypto_public_encrypt(&block, KEY_SPKI, "none", "sha1", b"").expect("encrypt");
        assert_eq!(c, unhex(UNPADDED_PUBLIC));
        // A private key stands in for its public half.
        assert_eq!(
            crypto_public_encrypt(&block, KEY_PKCS1, "none", "sha1", b"").expect("encrypt"),
            c
        );
        assert_eq!(
            crypto_private_decrypt(&c, KEY_PKCS8, "none", "sha1", b"").expect("decrypt"),
            block
        );
        let s = crypto_private_encrypt(&block, KEY_PKCS8, "none").expect("privateEncrypt");
        assert_eq!(s, unhex(UNPADDED_PRIVATE));
        assert_eq!(
            crypto_public_decrypt(&s, KEY_SPKI, "none").expect("publicDecrypt"),
            block
        );
        assert_eq!(
            crypto_public_decrypt(&s, KEY_PKCS8, "none").expect("with the private key"),
            block
        );
        // Decrypting takes the block without its leading zero byte too.
        assert_eq!(
            crypto_private_decrypt(&block[1..], KEY_PKCS8, "none", "sha1", b"").expect("short"),
            crypto_private_decrypt(&block, KEY_PKCS8, "none", "sha1", b"").expect("full")
        );
        assert_eq!(
            crypto_private_decrypt(&[], KEY_PKCS8, "none", "sha1", b""),
            Err(RSA_ERR_DATA_TOO_SMALL.to_string())
        );
    }

    #[test]
    fn private_encrypt_is_nodes_and_public_decrypt_reads_it() {
        let boxed = crypto_private_encrypt(b"pe", KEY_PKCS8, "pkcs1").expect("privateEncrypt");
        assert_eq!(boxed, unhex(PRIVATE_ENCRYPT_PE));
        assert_eq!(
            crypto_public_decrypt(&boxed, KEY_SPKI, "pkcs1").expect("read"),
            b"pe"
        );
        assert_eq!(
            crypto_public_decrypt(&boxed, KEY_PKCS8, "pkcs1").expect("read"),
            b"pe"
        );
        // 117 bytes is the most a 1024-bit key carries.
        assert!(crypto_private_encrypt(&[5u8; 117], KEY_PKCS8, "pkcs1").is_ok());
        assert_eq!(
            crypto_private_encrypt(&[5u8; 118], KEY_PKCS8, "pkcs1"),
            Err(RSA_ERR_DATA_TOO_LARGE_FOR_KEY_SIZE.to_string())
        );
    }

    #[test]
    fn type1_unpadding_names_each_defect() {
        let k = 128;
        let block = |head: &[u8], pad: usize, tail: &[u8]| {
            let mut em = head.to_vec();
            em.extend(std::iter::repeat_n(0xffu8, pad));
            em.extend_from_slice(tail);
            em.resize(k, 0x61);
            em
        };
        assert_eq!(
            pkcs1_type1_unpad(&block(&[0, 1], 8, &[0])).map(|m| m.len()),
            Ok(k - 11)
        );
        assert_eq!(
            pkcs1_type1_unpad(&block(&[0, 1], 100, &[0, 9])).map(|m| m[0]),
            Ok(9)
        );
        assert_eq!(
            pkcs1_type1_unpad(&block(&[1, 1], 8, &[0])),
            Err(RSA_ERR_INVALID_PADDING.to_string())
        );
        assert_eq!(
            pkcs1_type1_unpad(&block(&[0, 2], 8, &[0])),
            Err(RSA_ERR_BLOCK_TYPE_IS_NOT_01.to_string())
        );
        assert_eq!(
            pkcs1_type1_unpad(&block(&[0, 1], 20, &[5])),
            Err(RSA_ERR_BAD_FIXED_HEADER.to_string())
        );
        assert_eq!(
            pkcs1_type1_unpad(&block(&[0, 1], k - 2, &[])),
            Err(RSA_ERR_NULL_BEFORE_BLOCK_MISSING.to_string())
        );
        assert_eq!(
            pkcs1_type1_unpad(&block(&[0, 1], 7, &[0])),
            Err(RSA_ERR_BAD_PAD_BYTE_COUNT.to_string())
        );
        assert_eq!(
            pkcs1_type1_unpad(&block(&[0, 1], k - 3, &[0])),
            Err(RSA_ERR_EMPTY_RESULT.to_string())
        );
        assert_eq!(
            pkcs1_type1_unpad(&[0, 1, 0]),
            Err(RSA_ERR_INVALID_PADDING.to_string())
        );
    }

    // RSA_PKCS1_PADDING on a block that does not decode: a buffer, the same
    // one on every call and for either encoding of the key, and node's bytes.
    #[test]
    fn undecodable_pkcs1_block_decrypts_to_nodes_bytes() {
        let block = undecodable_block();
        let first =
            crypto_private_decrypt(&block, KEY_PKCS8, "pkcs1", "sha1", b"").expect("decrypt");
        assert_eq!(first, unhex(UNDECODABLE_PLAINTEXT));
        for _ in 0..3 {
            assert_eq!(
                crypto_private_decrypt(&block, KEY_PKCS8, "pkcs1", "sha1", b"").expect("decrypt"),
                first
            );
        }
        assert_eq!(
            crypto_private_decrypt(&block, KEY_PKCS1, "pkcs1", "sha1", b"").expect("decrypt"),
            first
        );
        // One byte short of the modulus, and the same block with its zero.
        let short =
            crypto_private_decrypt(&[1u8; 127], KEY_PKCS8, "pkcs1", "sha1", b"").expect("short");
        assert_eq!(short, unhex(SHORT_BLOCK_PLAINTEXT));
        let mut padded = vec![1u8; 128];
        padded[0] = 0;
        assert_eq!(
            crypto_private_decrypt(&padded, KEY_PKCS8, "pkcs1", "sha1", b"").expect("padded"),
            short
        );
        // What is not a block for this key is refused.
        assert_eq!(
            crypto_private_decrypt(&[], KEY_PKCS8, "pkcs1", "sha1", b""),
            Err(RSA_ERR_DATA_TOO_SMALL.to_string())
        );
        assert_eq!(
            crypto_private_decrypt(&[1u8; 129], KEY_PKCS8, "pkcs1", "sha1", b""),
            Err(RSA_ERR_DATA_GREATER_THAN_MOD_LEN.to_string())
        );
        assert_eq!(
            crypto_private_decrypt(&[0xffu8; 128], KEY_PKCS8, "pkcs1", "sha1", b""),
            Err(RSA_ERR_DATA_TOO_LARGE_FOR_MODULUS.to_string())
        );
        // A well-formed ciphertext still round-trips.
        let ct = crypto_public_encrypt(b"plain", KEY_SPKI, "pkcs1", "sha1", b"").expect("encrypt");
        assert_eq!(
            crypto_private_decrypt(&ct, KEY_PKCS8, "pkcs1", "sha1", b"").expect("decrypt"),
            b"plain"
        );
    }

    #[test]
    fn oaep_labels_and_hashes() {
        // node's ciphertext, read back with the label it was made with.
        let node_ct = unhex(NODE_OAEP_SHA256_LABEL);
        assert_eq!(
            crypto_private_decrypt(&node_ct, KEY_PKCS8, "oaep", "sha256", b"oam-label")
                .expect("decrypt"),
            b"oaep"
        );
        for (hash, label) in [
            ("sha256", &b"other"[..]),
            ("sha256", b""),
            ("sha1", b"oam-label"),
        ] {
            assert_eq!(
                crypto_private_decrypt(&node_ct, KEY_PKCS8, "oaep", hash, label),
                Err(RSA_ERR_OAEP_DECODING.to_string())
            );
        }
        // Every hash, with and without a label; sha512 does not fit 1024 bits
        // with any message (2 * 64 + 2 bytes of overhead).
        for hash in ["sha1", "sha224", "sha256", "sha384", "SHA-256", "RSA-SHA1"] {
            for label in [&b""[..], b"L"] {
                let ct =
                    crypto_public_encrypt(b"m", KEY_SPKI, "oaep", hash, label).expect("encrypt");
                assert_eq!(ct.len(), 128);
                assert_eq!(
                    crypto_private_decrypt(&ct, KEY_PKCS8, "oaep", hash, label).expect("decrypt"),
                    b"m"
                );
            }
        }
        assert_eq!(
            crypto_public_encrypt(b"m", KEY_SPKI, "oaep", "sha512", b""),
            Err(RSA_ERR_DATA_TOO_LARGE_FOR_KEY_SIZE.to_string())
        );
        assert_eq!(
            crypto_public_encrypt(b"m", KEY_SPKI, "oaep", "md4", b""),
            Err("unsupported OAEP hash: 'md4'".to_string())
        );
        // The longest message that fits under sha1, and one byte more.
        assert!(crypto_public_encrypt(&[0u8; 86], KEY_SPKI, "oaep", "sha1", b"").is_ok());
        assert_eq!(
            crypto_public_encrypt(&[0u8; 87], KEY_SPKI, "oaep", "sha1", b""),
            Err(RSA_ERR_DATA_TOO_LARGE_FOR_KEY_SIZE.to_string())
        );
    }

    #[test]
    fn pss_signature_from_node_verifies() {
        let sig = unhex(NODE_PSS_SHA256_SALT7);
        let verify = |msg: &[u8], sig: &[u8], salt: i64| {
            crypto_verify_rsa_pss("sha256", msg, KEY_SPKI, sig, salt).expect("verify")
        };
        assert!(verify(b"round trip", &sig, 7));
        assert!(verify(b"round trip", &sig, RSA_PSS_SALTLEN_AUTO));
        assert!(!verify(b"round trip", &sig, RSA_PSS_SALTLEN_DIGEST));
        assert!(!verify(b"round trip", &sig, 8));
        assert!(!verify(b"another message", &sig, RSA_PSS_SALTLEN_AUTO));
        // A signature of any other length does not verify.
        assert!(!verify(b"round trip", &sig[1..], RSA_PSS_SALTLEN_AUTO));
        assert!(!verify(b"round trip", &[], RSA_PSS_SALTLEN_AUTO));
        let mut long = sig.clone();
        long.push(0);
        assert!(!verify(b"round trip", &long, RSA_PSS_SALTLEN_AUTO));
        // The private key verifies with its public half.
        assert!(
            crypto_verify_rsa_pss("sha256", b"round trip", KEY_PKCS8, &sig, 7).expect("verify")
        );
    }

    // Keys of less usual shapes, made with node v22.22.2: NEAR has factors
    // of 511 and 513 bits (a 1024-bit modulus), and LARGE_D a private
    // exponent that is not below its modulus. The ciphertext is node's
    // publicEncrypt (OAEP, sha1) of 'fixture message' under NEAR.
    const NEAR_PKCS8: &str = "-----BEGIN PRIVATE KEY-----\n\
        MIICdAIBADANBgkqhkiG9w0BAQEFAASCAl4wggJaAgEAAoGBALIziAkoPj9hdxaL\n\
        G3+xw2vzk4iveR0TwVaHs1+g0uLL35MFpJ4RPKYyk0kqgCkHHbzPI2TEIcbCQRVk\n\
        VLYaBpiHPL0IMysgSVgDJeyLZEenLdUMwkcp/XnQHoeTD8983sIfoY9Q5SHRwyUq\n\
        XsDZqd4a5K6RVYsxE9JO/EH0v16DAgMBAAECgYAPJy71/ww4AyzcvQx5Zx95J60P\n\
        zkK2vxjNfQblkVKNbYz/6VbDstnc6hJjLNrZYkexkqLtrSM6zbGPb54nyVPDvLmb\n\
        Uf9VbaY8l39r3kiBqKfz2PsO8GHH5tvzXCZV52FUcsWcmEIY8aAL0L9P22p5Rign\n\
        1ZbX/i7d7U5aBpNBXQJAYy4/cAzGNa3F9toLWRzpO3+jGOrjoLOmuzyUUInkmgLJ\n\
        fCPoy6DN6/C3dhUxvnC9TwQ+HcZujUihPHhX6EPgnQJBAcv2yPMNcD2fs5BPvKTd\n\
        9/8Olh0kpq8NzWL3m3BSNIVpkpuKew/3GxQeknK4tuSK8uBQdwwld875MVMe5/4b\n\
        QZ8CQBHM0RHZGRG1OFxPUc4ShEFrbOgh7+x9kaT1AGFb/i0t5t9iynsRRyPbNfH5\n\
        Z84YCcHOrx9E575vlJB3wL8sv3kCQG4gIQ6oWgY3FVvo62ahyn2jtWc23j2tVU5r\n\
        sCidVfvXQgXBiyNQa4hOpYuYFBn19p6kRUyAMAY6/YdEMBFT30sCQF13AE8gFE+r\n\
        Tty6GoeW2XUbVrDhqABM8aWniZg7l/SfRiMmZpHHZhpVyprgXt1JWhgoaahCCDxn\n\
        QHWTEs8iRNo=\n\
        -----END PRIVATE KEY-----\n";
    const NEAR_PKCS1: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
        MIICWgIBAAKBgQCyM4gJKD4/YXcWixt/scNr85OIr3kdE8FWh7NfoNLiy9+TBaSe\n\
        ETymMpNJKoApBx28zyNkxCHGwkEVZFS2GgaYhzy9CDMrIElYAyXsi2RHpy3VDMJH\n\
        Kf150B6Hkw/PfN7CH6GPUOUh0cMlKl7A2aneGuSukVWLMRPSTvxB9L9egwIDAQAB\n\
        AoGADycu9f8MOAMs3L0MeWcfeSetD85Ctr8YzX0G5ZFSjW2M/+lWw7LZ3OoSYyza\n\
        2WJHsZKi7a0jOs2xj2+eJ8lTw7y5m1H/VW2mPJd/a95Igain89j7DvBhx+bb81wm\n\
        VedhVHLFnJhCGPGgC9C/T9tqeUYoJ9WW1/4u3e1OWgaTQV0CQGMuP3AMxjWtxfba\n\
        C1kc6Tt/oxjq46Czprs8lFCJ5JoCyXwj6Mugzevwt3YVMb5wvU8EPh3Gbo1IoTx4\n\
        V+hD4J0CQQHL9sjzDXA9n7OQT7yk3ff/DpYdJKavDc1i95twUjSFaZKbinsP9xsU\n\
        HpJyuLbkivLgUHcMJXfO+TFTHuf+G0GfAkARzNER2RkRtThcT1HOEoRBa2zoIe/s\n\
        fZGk9QBhW/4tLebfYsp7EUcj2zXx+WfOGAnBzq8fROe+b5SQd8C/LL95AkBuICEO\n\
        qFoGNxVb6Otmocp9o7VnNt49rVVOa7AonVX710IFwYsjUGuITqWLmBQZ9faepEVM\n\
        gDAGOv2HRDARU99LAkBddwBPIBRPq07cuhqHltl1G1aw4agATPGlp4mYO5f0n0Yj\n\
        JmaRx2YaVcqa4F7dSVoYKGmoQgg8Z0B1kxLPIkTa\n\
        -----END RSA PRIVATE KEY-----\n";
    const NEAR_SPKI: &str = "-----BEGIN PUBLIC KEY-----\n\
        MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQCyM4gJKD4/YXcWixt/scNr85OI\n\
        r3kdE8FWh7NfoNLiy9+TBaSeETymMpNJKoApBx28zyNkxCHGwkEVZFS2GgaYhzy9\n\
        CDMrIElYAyXsi2RHpy3VDMJHKf150B6Hkw/PfN7CH6GPUOUh0cMlKl7A2aneGuSu\n\
        kVWLMRPSTvxB9L9egwIDAQAB\n\
        -----END PUBLIC KEY-----\n";
    const NEAR_OAEP: &[&str] = &[
        "2f17655ab3b0c554a5152f3f2c1977f5ca9d306ed0edeaf32de6fb85b16a5888",
        "7c95cecb85f356bd975f075ba05252a3cde675ec871d2a2f2ce1047c86cd8323",
        "177c99ed73fe121bb2a2d70e4501b4d9e6b0335d9a2aea589a3b571d64028986",
        "73119cfb57e28c551b8dd53ab63a062696dbf16a16580b5f853896adcd2875b2",
    ];
    const NEAR_PRIVATE_ENCRYPT: &[&str] = &[
        "777afc15d5011fdf702936872dc96ea2ed66609ff6d2b7b50a0475f3896d8ba1",
        "14a35d4680bfc1a73adb43cda99f1ce0511a06ce7a08df9fb0afe360683551e1",
        "7b6a38aac4ff3f7789d0a5c15c70722a9c4b7501476fa96bdd9fbf02fbbda2d1",
        "6b2339043b43be08164f54e2197345f3684b7238726fa91da595575e28f215d3",
    ];
    const LARGE_D_PKCS8: &str = "-----BEGIN PRIVATE KEY-----\n\
        MIICeAIBADANBgkqhkiG9w0BAQEFAASCAmIwggJeAgEAAoGBAOm7LJlkslcLVU3y\n\
        J5Lu1FLbibLSsQlK0W2EBxsecoYfDKVDs09eXwXDoeG1vC44NCKwFpPX6f1KXkcK\n\
        YR9Z6B0+IySQcvw54Sp6MpHMziW/ex1ZC4ahIPODYWT+1vg6NudesrsZgjt9pGJ8\n\
        vO1DK8jCIsbasPJYjZtLzhUs2eQbAgMBAAECgYEA6uUmW2M9BMEbhkIfF34KnUvq\n\
        /y2FJAQwm0nNW3DX+1v86vdOb+ryyST+33fB0UqHPE+6x4EVmA8pSF3LZkZD88Vk\n\
        s5u8YF5eg0yYYEaUyVj1DGYJR8mx5u2tyQmzyY2ZJUH3I1V5xJ6SCozBrYM4viWK\n\
        95xH2IqQj9c+cIvjLVkCQQD0eOy88kE+wn2K6L4uRbfroPPjKls0s0K5SdTUrUOf\n\
        7M11ftTsZVyByQ8HJklDWnNOXb3HBMMPR+fyi5z+TGCFAkEA9MCXETKzxO4Ehsxd\n\
        aO0HshWKd74ukEoa25zQkM0uvR/3awQOIiScqApw51KjIvv/Hqm320nU+kX9pWfs\n\
        ykGkHwJAE/MaPoD2Wgy8+ezsOBJwY8YKoOr/7lkChORFfeV1s0M7sO7cqT3MQrsJ\n\
        gufY6qoi5q43oln9IO4ZO22xLf9FkQJBALHD7/UdzlQKTF9br7y17BD8dEXkbA2A\n\
        xWEc1J9XZ8sVnK6tk1ZZIS8r5jUdYD/Yp8OtoUw0o+vjSgcJJIMCiiECQQCD8FBg\n\
        Y6C+mnGPb4ANdeRAkltZ08hCcaqyn8xrf7Umo/cb7wMZREbIGLqwc6tA4IMEN1oj\n\
        7wYE87Hp+DbS8+2c\n\
        -----END PRIVATE KEY-----\n";
    const LARGE_D_SPKI: &str = "-----BEGIN PUBLIC KEY-----\n\
        MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDpuyyZZLJXC1VN8ieS7tRS24my\n\
        0rEJStFthAcbHnKGHwylQ7NPXl8Fw6HhtbwuODQisBaT1+n9Sl5HCmEfWegdPiMk\n\
        kHL8OeEqejKRzM4lv3sdWQuGoSDzg2Fk/tb4OjbnXrK7GYI7faRifLztQyvIwiLG\n\
        2rDyWI2bS84VLNnkGwIDAQAB\n\
        -----END PUBLIC KEY-----\n";

    #[test]
    fn bit_length_of_a_big_endian_integer() {
        assert_eq!(be_bit_length(&[]), 0);
        assert_eq!(be_bit_length(&[0, 0]), 0);
        assert_eq!(be_bit_length(&[1]), 1);
        assert_eq!(be_bit_length(&[0x80]), 8);
        assert_eq!(be_bit_length(&[0, 0x01, 0x00]), 9);
        assert_eq!(be_bit_length(&[0xff; 64]), 512);
        assert_eq!(be_bit_length(&unhex(JWK_N)), 1024);
    }

    // A key whose factors do not both fit half the modulus length reads,
    // from PKCS#8 and from PKCS#1, and gives node's results.
    #[test]
    fn key_with_factors_of_unequal_length_is_read() {
        for (pem, label) in [(NEAR_PKCS8, "PRIVATE KEY"), (NEAR_PKCS1, "RSA PRIVATE KEY")] {
            let key = parse_rsa_private_key(&pem_to_der(pem).expect("pem"), label).expect("key");
            assert_eq!(key.size(), 128);
            let mut widths: Vec<u32> = key.primes().iter().map(|p| p.bit_width_vartime()).collect();
            widths.sort_unstable();
            assert_eq!(widths, [511, 513]);
            assert_crt_parameters_hold(&rsa_private_jwk_parts(&key).expect("jwk parts"));
            assert_eq!(
                crypto_private_decrypt(&unhex(NEAR_OAEP), pem, "oaep", "sha1", b""),
                Ok(b"fixture message".to_vec())
            );
            // privateEncrypt is deterministic: node's bytes.
            assert_eq!(
                crypto_private_encrypt(b"fixture message", pem, "pkcs1"),
                Ok(unhex(NEAR_PRIVATE_ENCRYPT))
            );
            let sig = crypto_sign_rsa_pss("sha256", b"m", pem, -1).expect("pss sign");
            assert_eq!(
                crypto_verify_rsa_pss("sha256", b"m", NEAR_SPKI, &sig, -2),
                Ok(true)
            );
        }
    }

    // Handed to a public-key operation, a private key is its modulus and
    // public exponent: the same key the SPKI gives.
    #[test]
    fn private_key_stands_for_its_public_half() {
        for (private, public) in [
            (KEY_PKCS8, KEY_SPKI),
            (NEAR_PKCS8, NEAR_SPKI),
            (LARGE_D_PKCS8, LARGE_D_SPKI),
        ] {
            let from_private =
                parse_rsa_public_key(&pem_to_der(private).expect("pem"), "PRIVATE KEY")
                    .expect("public half");
            let from_spki = parse_rsa_public_key(&pem_to_der(public).expect("pem"), "PUBLIC KEY")
                .expect("public key");
            assert_eq!(from_private, from_spki);
        }
        let pkcs1 = parse_rsa_public_key(&pem_to_der(KEY_PKCS1).expect("pem"), "RSA PRIVATE KEY")
            .expect("public half of a PKCS#1 key");
        assert_eq!(pkcs1.n_bytes().into_vec(), unhex(JWK_N));
        assert_eq!(
            crypto_public_decrypt(&unhex(NEAR_PRIVATE_ENCRYPT), NEAR_PKCS8, "pkcs1"),
            Ok(b"fixture message".to_vec())
        );
    }

    // A private exponent that is not below the modulus is not read as a
    // private key (node reads it).
    #[test]
    fn private_exponent_not_below_the_modulus_is_refused() {
        let der = pem_to_der(LARGE_D_PKCS8).expect("pem");
        assert_eq!(
            parse_rsa_private_key(&der, "PRIVATE KEY").map(|_| ()),
            Err("RSA private key parse: invalid exponent".to_string())
        );
    }

    // What is not an RSA private key structure is refused before any
    // integer is read.
    #[test]
    fn private_key_structure_is_checked() {
        let pkcs8 = pem_to_der(KEY_PKCS8).expect("pem");
        let pkcs1 = pem_to_der(KEY_PKCS1).expect("pem");
        assert!(rsa_private_key_structure(&pkcs8, "PRIVATE KEY", "RSA private key").is_ok());
        assert!(rsa_private_key_structure(&pkcs1, "RSA PRIVATE KEY", "RSA private key").is_ok());
        // Each encoding under the other's label.
        assert!(parse_rsa_private_key(&pkcs8, "RSA PRIVATE KEY").is_err());
        assert!(parse_rsa_private_key(&pkcs1, "PRIVATE KEY").is_err());
        // A PKCS#8 key of another algorithm (the OID's last byte changed).
        let mut other = pkcs8.clone();
        let oid = [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
        let at = other
            .windows(oid.len())
            .position(|window| window == oid)
            .expect("rsaEncryption OID");
        other[at + oid.len() - 1] = 0x05;
        let err = parse_rsa_private_key(&other, "PRIVATE KEY").expect_err("other algorithm");
        assert!(err.starts_with("RSA private key parse: unknown/unsupported algorithm OID"));
        assert_eq!(
            parse_rsa_private_key(&pkcs8, "PUBLIC KEY").map(|_| ()),
            Err("unsupported PEM label for RSA private key: 'PUBLIC KEY'".to_string())
        );
        // A truncated structure.
        assert!(parse_rsa_private_key(&pkcs8[..pkcs8.len() - 1], "PRIVATE KEY").is_err());
    }
}

#[cfg(test)]
mod streaming_ciphers {
    use super::*;

    /// Deterministic bytes, so a failure reproduces.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    /// Run `data` through a block-mode cipher in the given chunk sizes
    /// (cycled); returns the per-call output lengths, final() included, and
    /// everything concatenated.
    fn drive(
        mode: &str,
        key: &[u8],
        iv: &[u8],
        encrypt: bool,
        data: &[u8],
        sizes: &[usize],
    ) -> (Vec<usize>, Vec<u8>) {
        let mut c =
            CipherInstance::new(CipherMode::from_name(mode).unwrap(), key, iv, encrypt).unwrap();
        let (mut lens, mut out, mut at, mut k) = (Vec::new(), Vec::new(), 0, 0);
        while at < data.len() {
            let n = sizes[k % sizes.len()].min(data.len() - at);
            k += 1;
            let part = c.update(data[at..at + n].to_vec()).unwrap();
            lens.push(part.len());
            out.extend_from_slice(&part);
            at += n;
        }
        let CipherEngine::Block(state) = c.engine else {
            panic!("drive() is for the block modes");
        };
        let last = state.finish().unwrap();
        lens.push(last.len());
        out.extend_from_slice(&last);
        (lens, out)
    }

    #[test]
    fn cbc_streaming_matches_the_one_shot_crate_at_every_chunking() {
        use cipher::block_padding::Pkcs7;
        let mut rng = Lcg(11);
        let (key, iv) = (rng.bytes(32), rng.bytes(16));
        for len in [0usize, 1, 15, 16, 17, 31, 32, 33, 100, 256] {
            let plain = rng.bytes(len);
            let expected = cbc::Encryptor::<aes::Aes256>::new_from_slices(&key, &iv)
                .unwrap()
                .encrypt_padded_vec_mut::<Pkcs7>(&plain);
            for sizes in [&[1usize][..], &[5, 16, 17, 31], &[16], &[32, 1], &[1000]] {
                let (_, ct) = drive("aes-256-cbc", &key, &iv, true, &plain, sizes);
                assert_eq!(ct, expected, "encrypt len={len} sizes={sizes:?}");
                let (lens, pt) = drive("aes-256-cbc", &key, &iv, false, &ct, sizes);
                assert_eq!(pt, plain, "decrypt len={len} sizes={sizes:?}");
                // Nothing but the held-back block is left for final().
                assert!(*lens.last().unwrap() < AES_BLOCK, "lens={lens:?}");
            }
        }
    }

    #[test]
    fn block_update_holds_at_most_one_block() {
        let mut rng = Lcg(13);
        let (key, iv) = (rng.bytes(16), rng.bytes(16));
        let ct = rng.bytes(64);
        // Padded decrypt, block-aligned chunks: one block trails behind.
        let mode = CipherMode::from_name("aes-128-cbc").unwrap();
        let mut dec = CipherInstance::new(mode, &key, &iv, false).unwrap();
        assert_eq!(dec.update(ct[..16].to_vec()).unwrap().len(), 0);
        assert_eq!(dec.update(ct[16..48].to_vec()).unwrap().len(), 32);
        // Off a boundary the whole blocks all come out.
        assert_eq!(dec.update(ct[48..53].to_vec()).unwrap().len(), 16);
        let CipherEngine::Block(state) = &dec.engine else {
            unreachable!()
        };
        assert_eq!(state.pending.len(), 5);
        // Encrypt never holds a whole block.
        let mut enc = CipherInstance::new(mode, &key, &iv, true).unwrap();
        assert_eq!(enc.update(ct[..16].to_vec()).unwrap().len(), 16);
        assert_eq!(enc.update(ct[..31].to_vec()).unwrap().len(), 16);
        assert_eq!(enc.update(ct[..1].to_vec()).unwrap().len(), 16);
    }

    #[test]
    fn block_final_error_shapes() {
        let (key, iv) = ([1u8; 16], [2u8; 16]);
        let new = |name: &str, encrypt: bool| {
            let iv: &[u8] = if name.ends_with("ecb") { &[] } else { &iv };
            let c = CipherInstance::new(CipherMode::from_name(name).unwrap(), &key, iv, encrypt);
            let CipherEngine::Block(state) = c.unwrap().engine else {
                unreachable!()
            };
            state
        };
        let mut enc = new("aes-128-cbc", true);
        enc.auto_padding = false;
        enc.update(&[0; 21]);
        assert_eq!(
            enc.finish().unwrap_err(),
            "data length 21 not a multiple of block size 16 (autoPadding is off)"
        );
        let mut dec = new("aes-128-cbc", false);
        dec.update(&[0; 17]);
        assert_eq!(
            dec.finish().unwrap_err(),
            "ciphertext length 17 not a multiple of block size 16"
        );
        let mut dec = new("aes-128-cbc", false);
        dec.update(&[0; 16]);
        assert_eq!(dec.finish().unwrap_err(), "cbc decrypt: invalid padding");
        assert_eq!(
            new("aes-128-cbc", false).finish().unwrap_err(),
            "cbc decrypt: invalid padding"
        );
        let mut dec = new("aes-128-ecb", false);
        dec.update(&[0; 16]);
        assert_eq!(dec.finish().unwrap_err(), "ecb decrypt: invalid padding");
        // Padding switched off after a block was held back: final() still
        // owes that block.
        let mut dec = new("aes-128-ecb", false);
        assert!(dec.update(&[0; 16]).is_empty());
        dec.auto_padding = false;
        assert_eq!(dec.finish().unwrap().len(), 16);
    }

    #[test]
    fn gcm_streaming_matches_the_aes_gcm_crate_at_every_chunking() {
        let mut rng = Lcg(17);
        for key_len in [16usize, 32] {
            let (key, iv) = (rng.bytes(key_len), rng.bytes(12));
            let name = if key_len == 16 {
                "aes-128-gcm"
            } else {
                "aes-256-gcm"
            };
            let mode = CipherMode::from_name(name).unwrap();
            for (aad_len, len) in [
                (0usize, 0usize),
                (0, 1),
                (5, 15),
                (16, 16),
                (17, 17),
                (33, 100),
                (0, 4096),
                (300, 257),
            ] {
                let (aad, plain) = (rng.bytes(aad_len), rng.bytes(len));
                let sealed = gcm_one_shot(&key, &iv, &aad, &plain, true).unwrap();
                let (expected_ct, expected_tag) = sealed.split_at(len);
                for sizes in [&[1usize][..], &[5, 16, 17, 31], &[16], &[7], &[5000]] {
                    let mut enc = CipherInstance::new(mode, &key, &iv, true).unwrap();
                    let mut dec = CipherInstance::new(mode, &key, &iv, false).unwrap();
                    let (CipherEngine::Gcm(e), CipherEngine::Gcm(d)) =
                        (&mut enc.engine, &mut dec.engine)
                    else {
                        unreachable!()
                    };
                    // setAAD in pieces appends.
                    for piece in aad.chunks(sizes[0]) {
                        e.add_aad(piece).unwrap();
                        d.add_aad(piece).unwrap();
                    }
                    let (mut ct, mut pt, mut at, mut k) = (Vec::new(), Vec::new(), 0, 0);
                    while at < len {
                        let n = sizes[k % sizes.len()].min(len - at);
                        k += 1;
                        let part = enc.update(plain[at..at + n].to_vec()).unwrap();
                        // A stream mode: every update returns its own bytes.
                        assert_eq!(part.len(), n);
                        pt.extend_from_slice(&dec.update(part.clone()).unwrap());
                        ct.extend_from_slice(&part);
                        at += n;
                    }
                    let ctx = format!("{name} aad={aad_len} len={len} sizes={sizes:?}");
                    assert_eq!(ct, expected_ct, "{ctx}");
                    assert_eq!(pt, plain, "{ctx}");
                    let (CipherEngine::Gcm(e), CipherEngine::Gcm(d)) = (enc.engine, dec.engine)
                    else {
                        unreachable!()
                    };
                    let tag = e.finish(None).unwrap().unwrap();
                    assert_eq!(tag, expected_tag, "{ctx}");
                    let mut bad = tag.clone();
                    bad[15] ^= 1;
                    assert_eq!(d.finish(Some(&tag)).unwrap(), None, "{ctx}");
                    // A fresh decrypt of the same input refuses a wrong, a
                    // short and a missing tag.
                    for (wrong, message) in [
                        (Some(&bad[..]), "gcm decrypt: authentication failed"),
                        (Some(&tag[..12]), "gcm decrypt: authentication failed"),
                        (
                            None,
                            "gcm decrypt: auth tag required (call setAuthTag before final)",
                        ),
                    ] {
                        let mut again = GcmState::new(&key, &iv, false).unwrap();
                        again.add_aad(&aad).unwrap();
                        again.update(ct.clone()).unwrap();
                        assert_eq!(again.finish(wrong).unwrap_err(), message, "{ctx}");
                    }
                    // And the one-shot path opens what the streaming one sealed.
                    let mut sealed_stream = ct.clone();
                    sealed_stream.extend_from_slice(&tag);
                    assert_eq!(
                        gcm_one_shot(&key, &iv, &aad, &sealed_stream, false).unwrap(),
                        plain,
                        "{ctx}"
                    );
                }
            }
        }
    }

    #[test]
    fn gcm_refuses_aad_after_data() {
        let mut gcm = GcmState::new(&[3; 32], &[4; 12], true).unwrap();
        gcm.add_aad(b"before").unwrap();
        gcm.update(vec![0; 3]).unwrap();
        assert_eq!(
            gcm.add_aad(b"after").unwrap_err(),
            "Invalid state for operation setAAD"
        );
    }

    #[test]
    fn ctr_keystream_position_survives_chunking() {
        let mut rng = Lcg(19);
        let (key, iv) = (rng.bytes(32), rng.bytes(16));
        let plain = rng.bytes(200);
        let mut whole = plain.clone();
        CtrCore::new(&key, &iv).unwrap().apply(&mut whole).unwrap();
        let mode = CipherMode::from_name("aes-256-ctr").unwrap();
        let mut c = CipherInstance::new(mode, &key, &iv, true).unwrap();
        let mut out = Vec::new();
        for chunk in plain.chunks(7) {
            let part = c.update(chunk.to_vec()).unwrap();
            assert_eq!(part.len(), chunk.len());
            out.extend_from_slice(&part);
        }
        assert_eq!(out, whole);
    }
}
