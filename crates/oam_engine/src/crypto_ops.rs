//! node:crypto natives: streaming digests, HMAC, OS randomness, and
//! constant-time comparison. Pure-CPU sync work — these run on the isolate
//! thread directly, no op channel.
//!
//! State model: in-flight hashers live in an isolate-slot registry keyed
//! by handle (zero-capture callbacks can't hold Rust state). digest()
//! consumes the entry; copy() clones it (every RustCrypto hasher is
//! Clone, which is what makes Node's hash.copy() cheap here).

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
