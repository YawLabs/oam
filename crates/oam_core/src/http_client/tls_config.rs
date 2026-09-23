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

/// The protocol versions one request handshakes with: node's live
/// `tls.DEFAULT_MIN_VERSION` / `DEFAULT_MAX_VERSION` as `node:tls` resolved
/// them, narrowed to what rustls offers (`crate::tls::protocol_versions`).
/// Node's undici and https.Agent connect through tls.connect, whose
/// SecureContext reads the defaults for every connection, so a
/// `tls.DEFAULT_MAX_VERSION = 'TLSv1.2'` or `--tls-max-v1.2` caps fetch and
/// an option-less https.get as it caps tls.connect (measured on v22.22.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsRange {
    /// TLS 1.2 and 1.3: node's own default, and the range every request took
    /// before the defaults were honoured.
    Both,
    Tls12,
    Tls13,
    /// Nothing rustls can offer (a ceiling below 1.2, or a floor above the
    /// ceiling): the handshake fails with node's
    /// `ERR_SSL_NO_PROTOCOLS_AVAILABLE`, as tls.connect's does.
    None,
}

static ONLY_TLS12: [&rustls::SupportedProtocolVersion; 1] = [&rustls::version::TLS12];
static ONLY_TLS13: [&rustls::SupportedProtocolVersion; 1] = [&rustls::version::TLS13];

impl TlsRange {
    /// From the effective version names, `None` (or "") for a side with no
    /// bound of its own.
    pub fn from_versions(min: Option<&str>, max: Option<&str>) -> TlsRange {
        match crate::tls::protocol_versions(min, max) {
            Ok(versions) => match (
                versions.contains(&&rustls::version::TLS12),
                versions.contains(&&rustls::version::TLS13),
            ) {
                (true, true) => TlsRange::Both,
                (true, false) => TlsRange::Tls12,
                (false, true) => TlsRange::Tls13,
                (false, false) => TlsRange::None,
            },
            Err(_) => TlsRange::None,
        }
    }

    /// The rustls versions, high to low; `None` for a range with nothing in it.
    fn versions(self) -> Option<&'static [&'static rustls::SupportedProtocolVersion]> {
        match self {
            TlsRange::Both => Some(rustls::ALL_VERSIONS),
            TlsRange::Tls12 => Some(&ONLY_TLS12),
            TlsRange::Tls13 => Some(&ONLY_TLS13),
            TlsRange::None => None,
        }
    }

    /// A byte the connector's shared state carries the range as.
    pub(crate) fn code(self) -> u8 {
        match self {
            TlsRange::Both => 0,
            TlsRange::Tls12 => 1,
            TlsRange::Tls13 => 2,
            TlsRange::None => 3,
        }
    }

    pub(crate) fn from_code(code: u8) -> TlsRange {
        match code {
            1 => TlsRange::Tls12,
            2 => TlsRange::Tls13,
            3 => TlsRange::None,
            _ => TlsRange::Both,
        }
    }
}

/// One slot per range that can handshake, each built once per process.
static PLATFORM: [OnceLock<Result<TlsConfigs, String>>; 3] =
    [OnceLock::new(), OnceLock::new(), OnceLock::new()];

/// The platform-verifier configs (plus NODE_EXTRA_CA_CERTS) for `range`,
/// built once per process on first use. The build runs under
/// `spawn_blocking` (it may read the system store from disk); later calls
/// clone two `Arc`s. Two first requests racing may both build; the first
/// result stored wins. `TlsRange::None` has no config: the connector refuses
/// the handshake before asking.
pub async fn platform(range: TlsRange) -> Result<TlsConfigs, String> {
    let Some(versions) = range.versions() else {
        return Err("no protocols available for the requested TLS version range".to_string());
    };
    let slot = &PLATFORM[usize::from(range.code())];
    if let Some(built) = slot.get() {
        return built.clone();
    }
    let built = match tokio::task::spawn_blocking(move || build_platform(versions)).await {
        Ok(built) => built,
        // The build panicked or the runtime is shutting down: nothing to
        // cache, the next request tries again.
        Err(e) => return Err(e.to_string()),
    };
    slot.get_or_init(|| built).clone()
}

fn build_platform(
    versions: &'static [&'static rustls::SupportedProtocolVersion],
) -> Result<TlsConfigs, String> {
    // The suites in Node's order (`tls::node_crypto_provider`), as undici's
    // OpenSSL offers them; named here rather than read from the process-wide
    // default, which `oam install`'s own client may have installed first.
    let provider = crate::tls::node_crypto_provider();
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
        .with_protocol_versions(versions)
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
