//! The rustls client configs the fetch transport handshakes with.
//!
//! reqwest 0.13.4 built this from oam's builder (async_impl/client.rs
//! 686-842): every protocol version the provider offers, the platform
//! verifier (or the platform verifier plus the added roots when
//! NODE_EXTRA_CA_CERTS supplied some), SNI on, ALPN `h2, http/1.1` towards
//! the origin, and a copy with ALPN cleared for the handshake with an
//! `https://` proxy (connect.rs:383-390). Node's undici and https share one
//! root store with tls.connect, so the extra CAs apply here as they do there.
//!
//! Two differences from reqwest, both deliberate:
//!
//! - The platform configs are built on the FIRST https request, not at boot.
//!   Building the verifier reads the system store (rustls-native-certs on
//!   Linux), and a failure used to stop `CoreRuntime::new` -- so a machine
//!   without a CA bundle could not run a script that never makes an https
//!   request. Now oam boots, and an https request fails with `tls
//!   configuration error: ...`.
//! - On macOS, a chain Apple's TLS policy refuses gets a second verdict when
//!   the NODE_EXTRA_CA_CERTS bundle anchors it ([`PolicyRelief`]). Apple
//!   applies that policy to the bundle's chains too, and it holds a rule
//!   node's OpenSSL does not: a server certificate valid for more than 825
//!   days is refused even under a root the user added (measured on macOS 27
//!   with `security verify-cert -p ssl`: 826 days refused, 825 accepted). So a
//!   long-lived private certificate node trusts through NODE_EXTRA_CA_CERTS
//!   failed oam's fetch on macOS alone. The second verdict takes Apple's
//!   basic X.509 trust evaluation of the chain against the bundle alone --
//!   signatures, validity periods, CA constraints, none of the TLS rules --
//!   and node's TLS rules for the rest (`crate::tls::ExtraCaVerifier`). Both
//!   must accept. Every other platform verifier decides alone, as before.

use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};

use crate::tls::{ExtraCaVerdict, ExtraCaVerifier, NodeCertRefusal, VerifyFailure};

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
        return Err(no_protocols());
    };
    let slot = &PLATFORM[usize::from(range.code())];
    if let Some(built) = slot.get() {
        return built.clone();
    }
    let built = match tokio::task::spawn_blocking(move || {
        build_platform(versions, &crate::tls::extra_ca_certs().certs)
    })
    .await
    {
        Ok(built) => built,
        // The build panicked or the runtime is shutting down: nothing to
        // cache, the next request tries again.
        Err(e) => return Err(e.to_string()),
    };
    slot.get_or_init(|| built).clone()
}

/// The configs [`platform`] would build had NODE_EXTRA_CA_CERTS named
/// `extra`: for tests of the bundle's trust, which is otherwise read once per
/// process from the environment. Built on the calling thread, never cached.
pub fn platform_with_extra_roots(
    range: TlsRange,
    extra: &[CertificateDer<'static>],
) -> Result<TlsConfigs, String> {
    let Some(versions) = range.versions() else {
        return Err(no_protocols());
    };
    build_platform(versions, extra)
}

fn no_protocols() -> String {
    "no protocols available for the requested TLS version range".to_string()
}

fn build_platform(
    versions: &'static [&'static rustls::SupportedProtocolVersion],
    extra: &[CertificateDer<'static>],
) -> Result<TlsConfigs, String> {
    // The suites in Node's order (`tls::node_crypto_provider`), as undici's
    // OpenSSL offers them; named here rather than read from the process-wide
    // default, which `oam install`'s own client may have installed first.
    let provider = crate::tls::node_crypto_provider();
    let platform = if extra.is_empty() {
        rustls_platform_verifier::Verifier::new(provider.clone())
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(
            extra.iter().cloned(),
            provider.clone(),
        )
    }
    .map_err(|e| e.to_string())?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NodeNamedRefusals {
            platform: Arc::new(platform),
            relief: bundle_chain_trust(extra).and_then(|chain| PolicyRelief::new(extra, chain)),
        }))
        .with_no_client_auth();
    Ok(TlsConfigs::from_client_config(config))
}

/// The transport's verifier: the platform verifier (given the
/// NODE_EXTRA_CA_CERTS bundle as extra roots), a second verdict on a chain it
/// refused where one applies ([`PolicyRelief`]), and every refusal named as
/// Node names it: `UNABLE_TO_VERIFY_LEAF_SIGNATURE`,
/// `DEPTH_ZERO_SELF_SIGNED_CERT`, `CERT_HAS_EXPIRED`,
/// `ERR_TLS_CERT_ALTNAME_INVALID`, ... A named refusal leaves the handshake
/// as `CertificateError::Other(NodeCertRefusal)`, which the transport reports
/// with that code (`SendError::to_outcome`); fetch's cause and http.request's
/// error then carry it, as tls.connect's do, where they said only that the
/// request failed (and http.request `socket hang up`).
#[derive(Debug)]
struct NodeNamedRefusals {
    platform: Arc<dyn ServerCertVerifier>,
    /// The second verdict: on macOS with a bundle holding a self-signed
    /// certificate; None everywhere else.
    relief: Option<PolicyRelief>,
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
        let error = match self.platform.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => return Ok(verified),
            Err(error) => error,
        };
        // A revocation the platform found is final: node checks none, but a
        // certificate the operating system knows is revoked is not accepted
        // on node's say-so.
        let revoked = matches!(
            error,
            rustls::Error::InvalidCertificate(CertificateError::Revoked)
        );
        if let Some(relief) = self.relief.as_ref().filter(|_| !revoked)
            && let Some(verdict) = relief.verdict(end_entity, intermediates, server_name, now)
        {
            return verdict;
        }
        Err(
            match crate::tls::refusal_in_node_terms(
                end_entity,
                intermediates,
                server_name,
                now,
                &error,
            ) {
                Some(failure) if failure.code.is_some() => named(failure),
                _ => error,
            },
        )
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.platform.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.platform.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.platform.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.platform.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[rustls::DistinguishedName]> {
        self.platform.root_hint_subjects()
    }
}

/// Whether a chain is trusted by X.509 rules alone against the
/// NODE_EXTRA_CA_CERTS bundle: the certificates chain to one of the bundle's
/// self-signed certificates, with every signature, validity period and CA
/// constraint on the way checked, and no TLS rule applied.
trait ChainTrust: std::fmt::Debug + Send + Sync {
    fn trusts(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>])
    -> bool;
}

/// The chain trust [`PolicyRelief`] asks, on the one platform that needs it
/// (see the module docs): Apple's basic X.509 policy over the bundle.
#[cfg(target_os = "macos")]
fn bundle_chain_trust(extra: &[CertificateDer<'static>]) -> Option<Arc<dyn ChainTrust>> {
    Some(Arc::new(apple::BundleChain::new(extra)))
}

/// No second verdict off macOS: the platform verifier decides alone.
#[cfg(not(target_os = "macos"))]
fn bundle_chain_trust(_extra: &[CertificateDer<'static>]) -> Option<Arc<dyn ChainTrust>> {
    None
}

/// The second verdict on a chain the platform's TLS policy refused: accepted
/// when the chain is trusted by X.509 rules against the NODE_EXTRA_CA_CERTS
/// bundle alone ([`ChainTrust`]) AND node's TLS rules accept it
/// (`crate::tls::ExtraCaVerifier`: the leaf's validity period and purpose,
/// the chain, the host name); refused with node's code when the chain is so
/// trusted and node's rules refuse it; the platform's refusal otherwise.
/// The chain trust checks the path's dates and CA constraints, the root's
/// included; node's rules check the extended key usage of every certificate
/// in the path, the leaf's purpose and the host name, which a basic X.509
/// policy does not.
#[derive(Debug)]
struct PolicyRelief {
    chain: Arc<dyn ChainTrust>,
    rules: ExtraCaVerifier,
}

impl PolicyRelief {
    /// None when the bundle holds nothing that could anchor a chain.
    fn new(extra: &[CertificateDer<'static>], chain: Arc<dyn ChainTrust>) -> Option<PolicyRelief> {
        Some(PolicyRelief {
            chain,
            rules: ExtraCaVerifier::new(extra)?,
        })
    }

    /// Some(verdict) when the bundle anchors the chain; None leaves the
    /// platform's refusal standing.
    fn verdict(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        now: UnixTime,
    ) -> Option<Result<ServerCertVerified, rustls::Error>> {
        if !self.chain.trusts(end_entity, intermediates) {
            return None;
        }
        match self
            .rules
            .judge(end_entity, intermediates, server_name, now)
        {
            ExtraCaVerdict::Accepted => Some(Ok(ServerCertVerified::assertion())),
            ExtraCaVerdict::Refused(failure) if failure.code.is_some() => Some(Err(named(failure))),
            ExtraCaVerdict::Refused(_) | ExtraCaVerdict::NotAnchored => None,
        }
    }
}

#[cfg(target_os = "macos")]
mod apple {
    use rustls::pki_types::CertificateDer;
    use security_framework::certificate::SecCertificate;
    use security_framework::policy::SecPolicy;
    use security_framework::trust::SecTrust;

    /// Apple's basic X.509 trust evaluation (`SecPolicyCreateBasicX509`) of
    /// a chain against the NODE_EXTRA_CA_CERTS bundle's self-signed
    /// certificates as the only anchors, its other certificates offered as
    /// intermediates, as OpenSSL's store holds them. Measured on macOS 27
    /// with `security verify-cert -p basic`, it accepts a leaf valid for 100
    /// years (which the SSL policy refuses) and refuses a chain under an
    /// expired root, a path longer than the root's path-length limit, an
    /// intermediate whose key usage lacks certificate signing, and a leaf
    /// signed with a CA:FALSE certificate's key -- each of which node refuses
    /// too -- while it reads no extended key usage and no leaf key usage
    /// (node's rules check those). It runs on the handshake's thread, as the
    /// platform verifier does, with network fetches off, at the current
    /// time: rustls's `now`, in a handshake. The bundle is parsed once.
    #[derive(Debug)]
    pub(super) struct BundleChain {
        anchors: Vec<SecCertificate>,
        intermediates: Vec<SecCertificate>,
    }

    impl BundleChain {
        pub(super) fn new(bundle: &[CertificateDer<'static>]) -> BundleChain {
            let mut anchors = Vec::new();
            let mut intermediates = Vec::new();
            for cert in bundle {
                let Ok(parsed) = SecCertificate::from_der(cert.as_ref()) else {
                    continue;
                };
                if crate::tls::is_self_signed(cert.as_ref()) {
                    anchors.push(parsed);
                } else {
                    intermediates.push(parsed);
                }
            }
            BundleChain {
                anchors,
                intermediates,
            }
        }
    }

    impl super::ChainTrust for BundleChain {
        fn trusts(
            &self,
            end_entity: &CertificateDer<'_>,
            intermediates: &[CertificateDer<'_>],
        ) -> bool {
            if self.anchors.is_empty() {
                return false;
            }
            let Some(mut certificates) = std::iter::once(end_entity.as_ref())
                .chain(intermediates.iter().map(|c| c.as_ref()))
                .map(|der| SecCertificate::from_der(der).ok())
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            certificates.extend(self.intermediates.iter().cloned());
            let Ok(mut trust) =
                SecTrust::create_with_certificates(&certificates, &[SecPolicy::create_x509()])
            else {
                return false;
            };
            trust.set_anchor_certificates(&self.anchors).is_ok()
                && trust.set_trust_anchor_certificates_only(true).is_ok()
                && trust.set_network_fetch_allowed(false).is_ok()
                && trust.evaluate_with_error().is_ok()
        }
    }
}

/// A refusal carried out of the handshake with Node's code on it.
fn named(failure: VerifyFailure) -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::Other(rustls::OtherError(Arc::new(
        NodeCertRefusal(failure),
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject;

    /// The e2e suites' private CA (CA:TRUE, valid to 2126) and the leaf it
    /// signed (SAN DNS:localhost, IP:127.0.0.1), as tests/common carries
    /// them: a leaf valid for 100 years, which Apple's TLS policy refuses on
    /// that alone.
    const TLS_TEST_CA_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDHzCCAgegAwIBAgIUX5ir308lg8m4hQdNnUz0UdN33DwwDQYJKoZIhvcNAQEL\n\
BQAwFjEUMBIGA1UEAwwLb2FtIHRlc3QgQ0EwIBcNMjYwOTE0MTExMTI0WhgPMjEy\n\
NjA4MjExMTExMjRaMBYxFDASBgNVBAMMC29hbSB0ZXN0IENBMIIBIjANBgkqhkiG\n\
9w0BAQEFAAOCAQ8AMIIBCgKCAQEA5oXf7XNg5MHjC511VA64HF8kdBHebuI207US\n\
fCQg9EYTe3hzOBACwsn78SNXFfmDw5E7hlF2xTuZmD3OJx9a0Ax54EoF67Z4Bigw\n\
My6GF1oKNsmeCGn9nv62+7jm9UspForbmWE8/rC3bM37BbvS87FoogEdXQS5uNQz\n\
4AuGbduhr27IXlScHsub4paSIrW6etllby5Ja+81NpVmwuZ32QNk+s0bwcLq8YIq\n\
5zpemaKeTGDBbG3mIt3vYsfjg8zUTdCdkjOs8q0+BSB8OkhGpe888d5JyUxd1WiK\n\
qiTpfG3+2Pbr0eK7pzIzeT+HDfzUInFfr7lu6lBtfjQRWSZWGwIDAQABo2MwYTAd\n\
BgNVHQ4EFgQUKmakijzWE71HQeyNAwaTnq9/xyMwHwYDVR0jBBgwFoAUKmakijzW\n\
E71HQeyNAwaTnq9/xyMwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYw\n\
DQYJKoZIhvcNAQELBQADggEBACX/zgcVyya26/+5t6Be9duAAJs1X0VSKSzXP/Au\n\
A+ngqWqFBPDhIzorx84d+siuRKVLOZUjObba245P4oiaJwNSz3Ihix5V3FHGZTVM\n\
vHpVP8V7tzKpoEz89vfhueFOB0u2TVJe/099DAHrjaaza0zWa1zfxucrBAFQiQIA\n\
2GK95UN3sSv9/rl3QlxQx8ld5QlpIjjhQL7N1JWWKcuBqDHgbfN1qwB2CWSB+v3g\n\
YyTiYg/yyeFi173xPPS3CoiyyVyO+6ySfhwvopDJVkTdZafDpV5/d1o+AssKYX3R\n\
Jd1U3J1YgXh3HzZEI9Yeo2jZzDogNzNObNoYdXRUoDVPMXo=\n\
-----END CERTIFICATE-----";

    const TLS_TEST_LEAF_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDRzCCAi+gAwIBAgIUXMdiPT0RoKd1ynyNQq5kRcwrF9UwDQYJKoZIhvcNAQEL\n\
BQAwFjEUMBIGA1UEAwwLb2FtIHRlc3QgQ0EwIBcNMjYwOTE0MTExMTI1WhgPMjEy\n\
NjA4MjExMTExMjVaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcN\n\
AQEBBQADggEPADCCAQoCggEBALtUgW8legRgDaIObCQ75gb63jPvvGLkgrmfvL+z\n\
zuIpFr6McD3Em6aX0fje4x8SjVF10F1HTa8pLDy4G6T/UiBuATovjMsEIqk1MLW2\n\
F6/KfQLO35pVC6PeUCYW8UkqymVifxsPQuzdV+Hbp9VDaamHtCFhJN0sl0TAbc37\n\
xp4WZwI1HTSQ4q+ReLSslNQiK+bwJQeKdiL7u6jzXqkb0uTxOJ2bSS2BhpPbPiNR\n\
fZObJiFr6wtURUvy0AY9AmbNJwuWkuM0aJlOibaVIPPgVGDtZJCd8gQEdV4pKIMZ\n\
avTN3AbNeIMmn3nZehk5jvEHxL+tjTXG8no5f5X2KFlMwi0CAwEAAaOBjDCBiTAa\n\
BgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMC\n\
BaAwEwYDVR0lBAwwCgYIKwYBBQUHAwEwHQYDVR0OBBYEFJpXOwzKMtLLnbaIViTA\n\
QsTBV5+8MB8GA1UdIwQYMBaAFCpmpIo81hO9R0HsjQMGk56vf8cjMA0GCSqGSIb3\n\
DQEBCwUAA4IBAQCsP5gsrw1RHvEN9oBR1Pf+CXylfpH7It7ZMWDFW73rdhuC3Zxr\n\
22zgG04mRt2Gd4Ufq4FCjqELVoecWx5U/hv2v/4KmVqegJkcTnMOmQ3Bs391XXa9\n\
C+07yxnaDXE19agNm4ZACwmdf30LPaSqeVp3Y3aw8lH+5KeWrrVBpi7m8NMyHThC\n\
Yn0a/DcxRET01zHZb6AEve5eJT6Lm0YF/DF6r4+YfGehLX892VDoWgrNCz7DpDuC\n\
1ALfON7I9FSAJGh3iBvTbX9R7xVuKd8Za2f8Xwr/t7jK/zYxLAT9oyTH1FXIFAnP\n\
H5shelNOFfKjeO2TTJ9u7hMSzF9fWd4EOB7u\n\
-----END CERTIFICATE-----";

    /// A self-signed certificate with a clientAuth-only extended key usage
    /// (SAN DNS:localhost, valid 2025-2125): trusted by name when it is in
    /// the bundle, but not for a server -- node refuses it served as itself
    /// with `INVALID_PURPOSE` (measured on v22.22.2).
    const CLIENT_ONLY_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBjTCCATKgAwIBAgIUKGSj6ZQ9JVKut7sib9f+UEE4IggwCgYIKoZIzj0EAwIw\n\
FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAx\n\
MDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO\n\
PQMBBwNCAAQHbBVWlR6yKfddR38qJxLUolWId+i7vFSYemj8yQJpBExtHtWErwCd\n\
wKqOiOiKegpftLhoa1U+WbZ6WvgfJ+Lao2AwXjAaBgNVHREEEzARgglsb2NhbGhv\n\
c3SHBH8AAAEwDAYDVR0TAQH/BAIwADATBgNVHSUEDDAKBggrBgEFBQcDAjAdBgNV\n\
HQ4EFgQUHxxo6zKn2lMRIoV38oL4T+oFeYMwCgYIKoZIzj0EAwIDSQAwRgIhANMg\n\
gPgvbHGQaCm12cfNKnd/xmUTkF82n0T9GLV5yq7VAiEAwtlptz96LHw0VD7qdIV1\n\
v9rosiSQqKE13Nl/HtOJB/w=\n\
-----END CERTIFICATE-----\n";

    /// A chain trust with one answer for every chain.
    #[derive(Debug)]
    struct Chain(bool);

    impl ChainTrust for Chain {
        fn trusts(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>]) -> bool {
            self.0
        }
    }

    /// A platform verifier with one answer for every chain.
    #[derive(Debug)]
    struct Platform(Result<(), rustls::Error>);

    impl ServerCertVerifier for Platform {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            self.0.clone().map(|()| ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Err(rustls::Error::General("not used here".into()))
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Err(rustls::Error::General("not used here".into()))
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            Vec::new()
        }
    }

    /// A refusal the platform gives no reason for: how
    /// rustls-platform-verifier carries every Security framework verdict
    /// but four (apple.rs), the 825-day validity rule among them. The text is
    /// the platform's own and nothing reads it.
    fn opaque_refusal() -> rustls::Error {
        rustls::Error::InvalidCertificate(CertificateError::Other(rustls::OtherError(Arc::from(
            Box::<dyn std::error::Error + Send + Sync>::from("opaque platform refusal"),
        ))))
    }

    fn der(pem: &str) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
    }

    /// The transport's verifier over a platform with one answer, a bundle,
    /// and -- None for the platforms that have none -- a chain trust.
    fn verifier(
        platform: Result<(), rustls::Error>,
        bundle: &[CertificateDer<'static>],
        chain: Option<bool>,
    ) -> NodeNamedRefusals {
        NodeNamedRefusals {
            platform: Arc::new(Platform(platform)),
            relief: chain.and_then(|trusts| PolicyRelief::new(bundle, Arc::new(Chain(trusts)))),
        }
    }

    /// The verdict on `leaf`, sent alone, for `host`, in 2027.
    fn verify(
        verifier: &NodeNamedRefusals,
        leaf: &CertificateDer<'_>,
        host: &str,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));
        let name = ServerName::try_from(host.to_string()).unwrap();
        verifier.verify_server_cert(leaf, &[], &name, &[], now)
    }

    /// The node code a refusal carries, if it carries one.
    fn node_code(result: &Result<ServerCertVerified, rustls::Error>) -> Option<&'static str> {
        match result {
            Err(rustls::Error::InvalidCertificate(CertificateError::Other(other))) => other
                .0
                .downcast_ref::<NodeCertRefusal>()
                .and_then(|refusal| refusal.0.code),
            _ => None,
        }
    }

    /// The macOS case: Apple's TLS policy refuses a chain the bundle anchors
    /// for a rule node does not have (the 825-day rule), its X.509 rules
    /// accept the chain, and so do node's TLS rules.
    #[test]
    fn a_chain_the_platform_s_tls_policy_refused_is_accepted_when_both_rule_sets_accept() {
        let bundle = [der(TLS_TEST_CA_CERT)];
        let accepted = verify(
            &verifier(Err(opaque_refusal()), &bundle, Some(true)),
            &der(TLS_TEST_LEAF_CERT),
            "localhost",
        );
        assert!(accepted.is_ok(), "{accepted:?}");
    }

    /// X.509 rules that refuse the chain -- an expired root, a key that
    /// cannot sign certificates -- leave the platform's refusal standing,
    /// named off the chain as before, whatever node's TLS rules make of it.
    #[test]
    fn the_platform_s_refusal_stands_when_the_chain_trust_refuses() {
        let bundle = [der(TLS_TEST_CA_CERT)];
        let refused = verify(
            &verifier(Err(opaque_refusal()), &bundle, Some(false)),
            &der(TLS_TEST_LEAF_CERT),
            "localhost",
        );
        assert_eq!(node_code(&refused), Some("UNABLE_TO_VERIFY_LEAF_SIGNATURE"));
    }

    /// A chain X.509 rules trust that node's TLS rules refuse is refused with
    /// node's code: the host name, and a leaf that is not for a server.
    #[test]
    fn node_s_tls_rules_name_the_refusal_of_a_chain_the_bundle_anchors() {
        let refused = verify(
            &verifier(Err(opaque_refusal()), &[der(TLS_TEST_CA_CERT)], Some(true)),
            &der(TLS_TEST_LEAF_CERT),
            "example.com",
        );
        assert_eq!(node_code(&refused), Some("ERR_TLS_CERT_ALTNAME_INVALID"));
        let refused = verify(
            &verifier(Err(opaque_refusal()), &[der(CLIENT_ONLY_CERT)], Some(true)),
            &der(CLIENT_ONLY_CERT),
            "localhost",
        );
        assert_eq!(node_code(&refused), Some("INVALID_PURPOSE"));
    }

    /// What the platform accepts is accepted; a revocation it found is final.
    #[test]
    fn the_platform_decides_first_and_a_revocation_is_final() {
        let bundle = [der(TLS_TEST_CA_CERT)];
        assert!(
            verify(
                &verifier(Ok(()), &bundle, Some(false)),
                &der(TLS_TEST_LEAF_CERT),
                "example.com"
            )
            .is_ok()
        );
        let refused = verify(
            &verifier(
                Err(rustls::Error::InvalidCertificate(CertificateError::Revoked)),
                &bundle,
                Some(true),
            ),
            &der(TLS_TEST_LEAF_CERT),
            "localhost",
        );
        assert_eq!(node_code(&refused), Some("CERT_REVOKED"));
    }

    /// A private root that expired at the end of 2024 and a leaf it signed
    /// valid to 2100 (node: `CERT_HAS_EXPIRED`); a root with a path-length
    /// limit of 0 above an intermediate and a leaf under that (node:
    /// `PATH_LENGTH_EXCEEDED`); a self-signed CA:FALSE certificate and a leaf
    /// signed with its key (node refuses it). All measured on v22.22.2.
    #[cfg(target_os = "macos")]
    const EXPIRED_ROOT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBgTCCASagAwIBAgIUWF2VTzhAJJlMLdpSV9u9Pppm+04wCgYIKoZIzj0EAwIw\n\
HjEcMBoGA1UEAwwTb2FtIGV4cGlyZWQgdGVzdCBDQTAeFw0yMDAxMDEwMDAwMDBa\n\
Fw0yNTAxMDEwMDAwMDBaMB4xHDAaBgNVBAMME29hbSBleHBpcmVkIHRlc3QgQ0Ew\n\
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASAal5CJ1bSBXfG0EGAUmLTbN7Fcdp9\n\
na1SXOKYRBky3YVMRHG6GXFbqXF/e7zVnHwXCLVWuWBXly3nb0qeObclo0IwQDAP\n\
BgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIBBjAdBgNVHQ4EFgQUTm4G2Wx7\n\
ZbGUQYUigETlp+BA2S0wCgYIKoZIzj0EAwIDSQAwRgIhALkdyMQceJO3E0SDuNQt\n\
4xf2BXzJJHRl3Bz0pZCKTduoAiEAkMpt4UlaWjBNixM5LdZ5o0j4fxhmDE5hjim7\n\
Wnud2EQ=\n\
-----END CERTIFICATE-----\n";
    #[cfg(target_os = "macos")]
    const LEAF_UNDER_EXPIRED_ROOT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBxDCCAWmgAwIBAgIUMUmWE0/KkVqOvlh/SOvXIXzqDKEwCgYIKoZIzj0EAwIw\n\
HjEcMBoGA1UEAwwTb2FtIGV4cGlyZWQgdGVzdCBDQTAgFw0yMDAxMDEwMDAwMDBa\n\
GA8yMTAwMDEwMTAwMDAwMFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZI\n\
zj0CAQYIKoZIzj0DAQcDQgAE/2NMFFBcJuZ7A4H0QzcY/J7Oc++uU6MVku0N81cn\n\
JQgZ41GmX1anbS2kuJGvdL2ZC5KNPpvJiH5kOT1YRYRLW6OBjDCBiTAaBgNVHREE\n\
EzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMCB4AwEwYD\n\
VR0lBAwwCgYIKwYBBQUHAwEwHQYDVR0OBBYEFJHqhBLDb/EiiCyntQNf4biAF0Av\n\
MB8GA1UdIwQYMBaAFE5uBtlse2WxlEGFIoBE5afgQNktMAoGCCqGSM49BAMCA0kA\n\
MEYCIQDNF0gGonW3vSMLp8Nlf+wGCdP4gej/hMnc6BxYHKs+dgIhALOKyMdQVvFO\n\
goOkDcKE5PAMwINxDB054VgK+v7BoEtX\n\
-----END CERTIFICATE-----\n";
    #[cfg(target_os = "macos")]
    const PATH_LEN_0_ROOT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBazCCARGgAwIBAgIUbD1OemLS+q0lBXkXCaQT95taTOwwCgYIKoZIzj0EAwIw\n\
ETEPMA0GA1UEAwwGb2FtIHIyMCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAxMDAw\n\
MDAwWjARMQ8wDQYDVQQDDAZvYW0gcjIwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNC\n\
AARqtabS05h+ELjbRFLvRBsq9D4qJbhJ/XhVK5T6ul4h/auxrs2JvSLX6WI+8YdL\n\
juP8ldykgYvX+tT9xLZjUDUBo0UwQzASBgNVHRMBAf8ECDAGAQH/AgEAMA4GA1Ud\n\
DwEB/wQEAwIBBjAdBgNVHQ4EFgQU9/nxAOCPIsr+Xt8RXBTLqAgjYiYwCgYIKoZI\n\
zj0EAwIDSAAwRQIhAKHn4GN6AKzpyLgEomyL8y85i610f8duw1UPLj0GaIeiAiAR\n\
tdEfifnHXbDiTgNFpkIMZlGA8WHzQ9bkJU7+Ky959A==\n\
-----END CERTIFICATE-----\n";
    #[cfg(target_os = "macos")]
    const INTERMEDIATE_UNDER_PATH_LEN_0: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBfjCCASSgAwIBAgIJAJXErXP1kkwFMAoGCCqGSM49BAMCMBExDzANBgNVBAMM\n\
Bm9hbSByMjAgFw0yNTAxMDEwMDAwMDBaGA8yMTI1MDEwMTAwMDAwMFowETEPMA0G\n\
A1UEAwwGb2FtIGkyMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEwoIAJHFlYUrM\n\
H80WaODEMku1jeNq/KLCuRoYvktwaOEoDpbqlmOKhoXqd+MKkpJcqFJcpVyjEZWq\n\
+XzYtScB4KNjMGEwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYwHQYD\n\
VR0OBBYEFJ+jJQVz/vRCi66JGwN3UOfWkQInMB8GA1UdIwQYMBaAFPf58QDgjyLK\n\
/l7fEVwUy6gII2ImMAoGCCqGSM49BAMCA0gAMEUCIGziLmHU+iECR7KZ+tSPfZuR\n\
fUSFOYYNZ24Td+zZXVA6AiEAyreg3ZWVaWefcJNgSfbehEPi+jA1FelGJ/q5Zfx1\n\
CAk=\n\
-----END CERTIFICATE-----\n";
    #[cfg(target_os = "macos")]
    const LEAF_UNDER_PATH_LEN_0: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBrzCCAVSgAwIBAgIJALfpyp8ynOxqMAoGCCqGSM49BAMCMBExDzANBgNVBAMM\n\
Bm9hbSBpMjAgFw0yNTAxMDEwMDAwMDBaGA8yMTI1MDEwMTAwMDAwMFowFDESMBAG\n\
A1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEEuUyiEFI\n\
g042CRrba0ZPJMy9nNsLmPBEHvlkndaJcXX0d7XX0bGio8KSIYlFMQrekTXqA2sE\n\
TnhKDZHl7pWoo6OBjzCBjDAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCQYD\n\
VR0TBAIwADAOBgNVHQ8BAf8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwEwHQYD\n\
VR0OBBYEFFEtW1gEc+hX3SyZj8pj9tax1tnAMB8GA1UdIwQYMBaAFJ+jJQVz/vRC\n\
i66JGwN3UOfWkQInMAoGCCqGSM49BAMCA0kAMEYCIQDarQVTcjMHO1GfSeakt1zD\n\
utAKnd18gYOJez/qQERPuQIhAPiyw4ipCy1VwLMhLMN/Jie6hsKh5q3h1ej5F4R0\n\
5g26\n\
-----END CERTIFICATE-----\n";
    #[cfg(target_os = "macos")]
    const DEV_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBrDCCAVKgAwIBAgIUOhmQZYt9Bjx2GmWM6t1lMYG6XnAwCgYIKoZIzj0EAwIw\n\
HDEaMBgGA1UEAwwRb2FtIGRldiBsb2NhbGhvc3QwIBcNMjUwMTAxMDAwMDAwWhgP\n\
MjEyNTAxMDEwMDAwMDBaMBwxGjAYBgNVBAMMEW9hbSBkZXYgbG9jYWxob3N0MFkw\n\
EwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE5yzAM4KyR4WAoPnqlA5I4956IqSIK5C0\n\
qhDvJoTws1BXnIFCGySUiKt+ym7O0AjL2X6K4479ohfwYX0GsA0N0KNwMG4wGgYD\n\
VR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/\n\
BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBRZIWZkadOURbzJ\n\
hPc9wc1ty+lu1jAKBggqhkjOPQQDAgNIADBFAiB0kOXBGX8XphkW2VSsP9xRR5eF\n\
Io8nesrXS7xDFFyo+wIhAJQ+G5IO6arB0XGOI1gRmTKcVtl9bwv8lpza3cr/lKR1\n\
-----END CERTIFICATE-----\n";
    #[cfg(target_os = "macos")]
    const MINTED_BY_DEV_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBpjCCAUugAwIBAgIBAjAKBggqhkjOPQQDAjAcMRowGAYDVQQDDBFvYW0gZGV2\n\
IGxvY2FsaG9zdDAgFw0yNTAxMDEwMDAwMDBaGA8yMTI1MDEwMTAwMDAwMFowGjEY\n\
MBYGA1UEAwwPYXBpLmV4YW1wbGUuY29tMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcD\n\
QgAEN6fQkfE1EZKW+OcZIEFixB2kF/tvaaJdQ1bgEoLRj+4GHAWfqoCuTHIpxqS8\n\
2HKoRgnO5iP6ysRmpGposTw9m6N+MHwwGgYDVR0RBBMwEYIPYXBpLmV4YW1wbGUu\n\
Y29tMAkGA1UdEwQCMAAwEwYDVR0lBAwwCgYIKwYBBQUHAwEwHQYDVR0OBBYEFBsK\n\
Y54HVtaPXg7CRiDV3gV8/nHuMB8GA1UdIwQYMBaAFFkhZmRp05RFvMmE9z3BzW3L\n\
6W7WMAoGCCqGSM49BAMCA0kAMEYCIQDiF33CY6Hy4OF+1HzWd7NkQPUoO1ItP3EO\n\
dXwNhtk1PgIhANPMCIJpgEhY29bZrgejraEMOHIbetKCQzq/eknK2H++\n\
-----END CERTIFICATE-----\n";

    /// The real Apple chain trust, on the chains the second verdict must
    /// refuse as node does and one it must accept.
    #[cfg(target_os = "macos")]
    #[test]
    fn apple_s_basic_x509_evaluation_refuses_what_node_refuses() {
        let trusts = |bundle: &[&str], leaf: &str, sent: &[&str]| {
            let bundle: Vec<_> = bundle.iter().map(|pem| der(pem)).collect();
            let sent: Vec<_> = sent.iter().map(|pem| der(pem)).collect();
            apple::BundleChain::new(&bundle).trusts(&der(leaf), &sent)
        };
        assert!(trusts(&[TLS_TEST_CA_CERT], TLS_TEST_LEAF_CERT, &[]));
        assert!(trusts(&[DEV_CERT], DEV_CERT, &[]));
        assert!(!trusts(&[EXPIRED_ROOT], LEAF_UNDER_EXPIRED_ROOT, &[]));
        assert!(!trusts(
            &[PATH_LEN_0_ROOT],
            LEAF_UNDER_PATH_LEN_0,
            &[INTERMEDIATE_UNDER_PATH_LEN_0]
        ));
        assert!(!trusts(&[DEV_CERT], MINTED_BY_DEV_CERT, &[]));
        assert!(!trusts(&[TLS_TEST_CA_CERT], MINTED_BY_DEV_CERT, &[]));
        assert!(!trusts(&[], TLS_TEST_LEAF_CERT, &[]));
    }

    /// Off macOS there is no second verdict: the platform decides alone, as
    /// it always has, and a bundle with nothing self-signed gives none either.
    #[test]
    fn without_a_chain_trust_the_platform_decides_alone() {
        let bundle = [der(TLS_TEST_CA_CERT)];
        let refused = verify(
            &verifier(Err(opaque_refusal()), &bundle, None),
            &der(TLS_TEST_LEAF_CERT),
            "localhost",
        );
        assert_eq!(node_code(&refused), Some("UNABLE_TO_VERIFY_LEAF_SIGNATURE"));
        assert!(PolicyRelief::new(&[der(TLS_TEST_LEAF_CERT)], Arc::new(Chain(true))).is_none());
        assert_eq!(
            bundle_chain_trust(&bundle).is_some(),
            cfg!(target_os = "macos")
        );
    }
}
