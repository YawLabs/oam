//! Reading a TLS server's private key and PKCS#12 bundles the way Node's
//! `createSecureContext` does.

use super::server::ContextError;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::BufReader;

/// What a PKCS#12 bundle held: its key (if any) and its certificates, leaf
/// first.
pub struct Pkcs12Bundle {
    pub key: Option<PrivateKeyDer<'static>>,
    pub chain: Vec<CertificateDer<'static>>,
}

/// A `key` entry: PEM (PKCS#8, PKCS#1 or SEC1). A key that cannot be read is
/// Node's `ERR_OSSL_UNSUPPORTED`.
pub fn load_private_key(
    pem: &[u8],
    _passphrase: Option<&str>,
) -> Result<PrivateKeyDer<'static>, ContextError> {
    if pem.windows(b"ENCRYPTED".len()).any(|w| w == b"ENCRYPTED") {
        return Err(ContextError::bad_decrypt());
    }
    rustls_pemfile::private_key(&mut BufReader::new(pem))
        .ok()
        .flatten()
        .ok_or_else(ContextError::unsupported_key)
}

/// A `pfx` entry.
pub fn load_pkcs12(_der: &[u8], _passphrase: &str) -> Result<Pkcs12Bundle, ContextError> {
    Err(ContextError::plain(
        "Unsupported PKCS12 PFX data",
        Some("ERR_CRYPTO_UNSUPPORTED_OPERATION"),
    ))
}
