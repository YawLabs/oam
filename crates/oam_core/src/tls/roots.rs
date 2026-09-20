//! The CA certificate lists node:tls hands out: `tls.rootCertificates` and
//! `tls.getCACertificates(type)`, as PEM strings.
//!
//! - `bundled` (and `rootCertificates`): the Mozilla root store oam's TLS
//!   client trusts by default, as whole certificates. The client verifies
//!   against webpki-roots' trust anchors; webpki-root-certs is the same
//!   CCADB release as full certificates, which is what code needs to extend
//!   the store (`ca: [...tls.rootCertificates, corporateCA]`) -- an anchor
//!   is only a name and a key. `bundled_matches_the_client_trust_store`
//!   holds the two crates to the same set. Node bundles its own copy of
//!   the Mozilla store, so the two lists differ by the store's release (144
//!   certificates in v22.22.2).
//! - `extra`: the NODE_EXTRA_CA_CERTS bundle.
//! - `system`: the operating system's store, read by rustls-native-certs
//!   (the current user's Root store on Windows, the keychains' trust
//!   settings on macOS, the OpenSSL-layout bundle elsewhere). Node reads
//!   its own set of Windows stores, so the two lists need not match.
//!
//! Node prints a bundled root without a trailing newline and every other
//! certificate with one (measured on v22.22.2); the base64 is wrapped at 64
//! columns, as PEM_write_bio_X509 wraps it.

use rustls::pki_types::CertificateDer;
use std::sync::OnceLock;

/// The bundled roots, whole certificates.
pub fn bundled() -> &'static [CertificateDer<'static>] {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
}

/// One certificate as PEM text.
fn pem(der: &[u8], trailing_newline: bool) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 64 + 64);
    out.push_str("-----BEGIN CERTIFICATE-----\n");
    for line in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----");
    if trailing_newline {
        out.push('\n');
    }
    out
}

/// `tls.getCACertificates(kind)`'s list for one of Node's kinds -- `bundled`,
/// `extra` or `system` -- or None for any other (the JS layer composes
/// `default` and refuses the rest).
pub fn ca_certificates(kind: &str) -> Option<Vec<String>> {
    Some(match kind {
        "bundled" => {
            static BUNDLED: OnceLock<Vec<String>> = OnceLock::new();
            BUNDLED
                .get_or_init(|| bundled().iter().map(|c| pem(c, false)).collect())
                .clone()
        }
        "extra" => super::extra_ca_certs()
            .certs
            .iter()
            .map(|c| pem(c, true))
            .collect(),
        "system" => {
            static SYSTEM: OnceLock<Vec<String>> = OnceLock::new();
            SYSTEM
                .get_or_init(|| {
                    rustls_native_certs::load_native_certs()
                        .certs
                        .iter()
                        .map(|c| pem(c, true))
                        .collect()
                })
                .clone()
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject;

    /// `rootCertificates` must be exactly what the client trusts when no
    /// `ca` is given, or `ca: [...tls.rootCertificates, extra]` would trust
    /// a different set than the default does. A bump of webpki-roots
    /// without webpki-root-certs (or the reverse) fails here.
    #[test]
    fn bundled_matches_the_client_trust_store() {
        let mut from_certs: Vec<(Vec<u8>, Vec<u8>)> = bundled()
            .iter()
            .map(|cert| {
                let anchor = webpki::anchor_from_trusted_cert(cert).unwrap();
                (
                    anchor.subject.as_ref().to_vec(),
                    anchor.subject_public_key_info.as_ref().to_vec(),
                )
            })
            .collect();
        let mut trusted: Vec<(Vec<u8>, Vec<u8>)> = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .map(|anchor| {
                (
                    anchor.subject.as_ref().to_vec(),
                    anchor.subject_public_key_info.as_ref().to_vec(),
                )
            })
            .collect();
        from_certs.sort();
        trusted.sort();
        assert!(!trusted.is_empty());
        assert_eq!(from_certs, trusted);
    }

    #[test]
    fn pems_read_back_as_the_certificates() {
        let bundled_pems = ca_certificates("bundled").unwrap();
        assert_eq!(bundled_pems.len(), bundled().len());
        for (text, der) in bundled_pems.iter().zip(bundled()) {
            assert!(text.starts_with("-----BEGIN CERTIFICATE-----\n"));
            assert!(text.ends_with("\n-----END CERTIFICATE-----"));
            assert!(
                text.lines()
                    .all(|line| line.len() <= 64 || line.starts_with("-----"))
            );
            assert_eq!(
                &CertificateDer::from_pem_slice(text.as_bytes()).unwrap(),
                der
            );
        }
        assert!(pem(b"\x30\x00", true).ends_with("-----END CERTIFICATE-----\n"));
        assert_eq!(ca_certificates("default"), None);
    }
}
