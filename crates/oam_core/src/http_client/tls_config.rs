//! The rustls client configs the fetch transport handshakes with.
//!
//! reqwest 0.13.4 built exactly this from oam's builder (async_impl/client.rs
//! 686-842): every protocol version the provider offers, the platform
//! verifier (or the platform verifier plus the added roots when
//! NODE_EXTRA_CA_CERTS supplied some), SNI on, ALPN `h2, http/1.1` towards
//! the origin, and a copy with ALPN cleared for the handshake with an
//! `https://` proxy (connect.rs:383-390). Node's undici and https share one
//! root store with tls.connect, so the extra CAs apply here as they do there.
//!
//! One difference from reqwest, deliberate: the platform configs are built on
//! the FIRST https request, not at boot. Building the verifier reads the
//! system store (rustls-native-certs on Linux), and a failure used to stop
//! `CoreRuntime::new` -- so a machine without a CA bundle could not run a
//! script that never makes an https request. Now oam boots, and an https
//! request fails with `tls configuration error: ...`.

use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};

/// The two client configs a transport uses.
#[derive(Clone)]
pub struct TlsConfigs {
    /// Origin handshakes: ALPN `h2, http/1.1`, SNI on.
    pub dst: Arc<ClientConfig>,
    /// The handshake with an `https://` proxy: the same trust with ALPN
    /// cleared, so an http/1.1 CONNECT or absolute-form request is never
    /// negotiated onto h2.
    pub proxy: Arc<ClientConfig>,
}

impl TlsConfigs {
    /// The pair derived from one base config: `dst` gets SNI and the ALPN
    /// list, `proxy` is the same config with no ALPN.
    pub fn from_client_config(mut config: ClientConfig) -> TlsConfigs {
        config.enable_sni = true;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let mut proxy = config.clone();
        proxy.alpn_protocols.clear();
        TlsConfigs {
            dst: Arc::new(config),
            proxy: Arc::new(proxy),
        }
    }
}

static PLATFORM: OnceLock<Result<TlsConfigs, String>> = OnceLock::new();

/// The platform-verifier configs (plus NODE_EXTRA_CA_CERTS), built once per
/// process on first use. The build runs under `spawn_blocking` (it may read
/// the system store from disk); later calls clone two `Arc`s. Two first
/// requests racing may both build; the first result stored wins.
pub async fn platform() -> Result<TlsConfigs, String> {
    if let Some(built) = PLATFORM.get() {
        return built.clone();
    }
    let built = match tokio::task::spawn_blocking(build_platform).await {
        Ok(built) => built,
        // The build panicked or the runtime is shutting down: nothing to
        // cache, the next request tries again.
        Err(e) => return Err(e.to_string()),
    };
    PLATFORM.get_or_init(|| built).clone()
}

fn build_platform() -> Result<TlsConfigs, String> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .ok_or_else(|| "no process-wide rustls crypto provider is installed".to_string())?;
    let extra = crate::tls::extra_ca_certs();
    let verifier = if extra.certs.is_empty() {
        rustls_platform_verifier::Verifier::new(provider.clone())
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(
            extra.certs.iter().cloned(),
            provider.clone(),
        )
    }
    .map_err(|e| e.to_string())?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NodeNamedRefusals {
            inner: Arc::new(verifier),
        }))
        .with_no_client_auth();
    Ok(TlsConfigs::from_client_config(config))
}

/// The platform verifier, its refusals named as Node names them
/// (`crate::tls::refusal_in_node_terms`): `UNABLE_TO_VERIFY_LEAF_SIGNATURE`,
/// `DEPTH_ZERO_SELF_SIGNED_CERT`, `CERT_HAS_EXPIRED`,
/// `ERR_TLS_CERT_ALTNAME_INVALID`, ... A named refusal leaves the handshake
/// as `CertificateError::Other(NodeCertRefusal)`, which the transport reports
/// with that code (`SendError::to_outcome`); fetch's cause and http.request's
/// error then carry it, as tls.connect's do, where they said only that the
/// request failed (and http.request `socket hang up`). What is accepted is
/// the platform verifier's call alone.
#[derive(Debug)]
struct NodeNamedRefusals {
    inner: Arc<dyn ServerCertVerifier>,
}

impl ServerCertVerifier for NodeNamedRefusals {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .map_err(|error| {
                match crate::tls::refusal_in_node_terms(
                    end_entity,
                    intermediates,
                    server_name,
                    now,
                    &error,
                ) {
                    Some(failure) if failure.code.is_some() => {
                        rustls::Error::InvalidCertificate(CertificateError::Other(
                            rustls::OtherError(Arc::new(crate::tls::NodeCertRefusal(failure))),
                        ))
                    }
                    _ => error,
                }
            })
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[rustls::DistinguishedName]> {
        self.inner.root_hint_subjects()
    }
}
