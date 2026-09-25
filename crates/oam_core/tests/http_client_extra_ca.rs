//! The fetch transport's trust for the NODE_EXTRA_CA_CERTS bundle
//! (`http_client::tls_config`), over real loopback TLS with this host's own
//! platform verifier: a chain the bundle anchors is accepted on every
//! platform -- the 100-year test leaf included, which Apple's TLS policy
//! refuses on its validity period alone, where the macOS second verdict
//! (Apple's X.509 rules and node's TLS rules) accepts it -- and refused with
//! node's code when node refuses it; a chain the bundle does not anchor is
//! refused with node's code for a lone leaf nobody trusts. The bundle is
//! passed in directly (`platform_with_extra_roots`): the environment
//! variable is read once per process, which no test in a shared binary can
//! own. How the two verdicts combine is pinned host-independently by
//! tls_config's unit tests.

mod common;

use std::time::Duration;

use common::*;
use http_body_util::BodyExt;
use oam_core::OpOutcome;
use oam_core::http_client::tls_config::platform_with_extra_roots;
use oam_core::http_client::transport::empty_body;
use oam_core::http_client::{HttpTransport, ProxySource, SendError, TlsRange, TlsSource};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

const ATTEMPT: Duration = Duration::from_millis(250);

/// A private CA that signed nothing the tests serve: conformance case 178's
/// (`oam case 178 test CA`, valid 2026-2126), unrelated to `TLS_TEST_CA_CERT`.
const UNRELATED_CA: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDMTCCAhmgAwIBAgIUDvtPdO4ljOTrt9v6/+Ds4F6Q6HgwDQYJKoZIhvcNAQEL\n\
BQAwHzEdMBsGA1UEAwwUb2FtIGNhc2UgMTc4IHRlc3QgQ0EwIBcNMjYwOTIzMTgy\n\
MjE0WhgPMjEyNjA4MzAxODIyMTRaMB8xHTAbBgNVBAMMFG9hbSBjYXNlIDE3OCB0\n\
ZXN0IENBMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAz9cdoNsHNOsj\n\
GTcYfimeOeV9zQaJgGgongxaEDtri3QGl0uNjPUhgRtvIkVagRLH3Huohw/yv6p8\n\
WhsTOdrJ8/OQ+kBTP/KNIkjjsVkXoPZDCt/FDa74EoNJZLJxAlF90nVFBiZzvB2B\n\
nKHVZJjyhyETcAEbSynrHbK+SpMlrFxcNLAatbPPI08KnBRCK7Lj6rA+yt98o3kb\n\
ZMbZ95i/OeBh/9gWCY/0GTTtIlLuG/Fgih7xYmyBXfITklrt3dzDL8Q9usb7IV6M\n\
cqwgdb78c90nw20dMRZ+EG5NAV4tkCYrhli7vOuep4JBI49ZpKpPNHkFn2+rawY5\n\
W+0PvYd5IQIDAQABo2MwYTAdBgNVHQ4EFgQU1XqMZ0VpqntEb9tcwFu+AzFxG2Uw\n\
HwYDVR0jBBgwFoAU1XqMZ0VpqntEb9tcwFu+AzFxG2UwDwYDVR0TAQH/BAUwAwEB\n\
/zAOBgNVHQ8BAf8EBAMCAQYwDQYJKoZIhvcNAQELBQADggEBAF+iugXNCBQDJpTg\n\
Urq26/DoFTPtrK6u4DHcPx9XpNavTh+uLr3xDxfjmH9ozOTjPkfJURHhDkSmBath\n\
fnr6RD9YjiZcrKAVA+V77dGo23MfeVa/xJnYHpXy2iuc4zm09s1KxYTOenw3+MKz\n\
qvGMiAZqXsd4KWxCeplPEA+E/T1Ytm4mY+cLFrohxPwFJakXemL60HcB0zALKAsk\n\
9U3s++koVXZ+olzVBNc6cDGNyEruzfKFzbSU7pXxqxGN0X3zwHlYoNB6SqpZuiUZ\n\
78SnbUSI6SHu6okYRpS7Ezfk7MDWoGaXL1bET7WGX+Tc6CLLCYLIOvFiQ+7pkwgi\n\
lWA6cC8=\n\
-----END CERTIFICATE-----\n";

fn bundle(pem: &str) -> Vec<CertificateDer<'static>> {
    vec![CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()]
}

/// A transport whose TLS is what `TlsSource::Platform` would build with
/// `extra` as the NODE_EXTRA_CA_CERTS bundle.
fn transport_trusting(extra: &[CertificateDer<'static>]) -> HttpTransport {
    let configs = platform_with_extra_roots(TlsRange::Both, extra).expect("tls configs");
    transport_with_tls(TlsSource::Fixed(configs), ProxySource::None)
}

async fn get(
    transport: &HttpTransport,
    target: &str,
) -> Result<http::Response<hyper::body::Incoming>, SendError> {
    let route = transport.route(false, ATTEMPT, TlsRange::Both);
    let request = http::Request::builder()
        .method("GET")
        .uri(target)
        .body(empty_body())
        .unwrap();
    transport.send(&route, request).await
}

async fn body_text(response: http::Response<hyper::body::Incoming>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The bundle's root anchors the served leaf: accepted, on macOS too, where
/// Apple's TLS policy refuses this 100-year leaf on its validity period
/// alone (the 825-day rule) and the second verdict accepts it.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_the_extra_ca_bundle_anchors_is_accepted() {
    within(async {
        let origin = serve_h2_tls("extra ok").await;
        let transport = transport_trusting(&bundle(TLS_TEST_CA_CERT));
        let target = format!("https://localhost:{}/", origin.port);
        let response = get(&transport, &target).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(body_text(response).await, "extra ok");
    })
    .await;
}

/// A self-signed development certificate (P-256, CA:FALSE, key usage
/// digitalSignature, extended key usage serverAuth, SAN DNS:localhost and
/// IP:127.0.0.1, valid 2025-2125) and its key: what a developer points
/// NODE_EXTRA_CA_CERTS at for a local https server.
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

const DEV_KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQguoknnn0/o/pphUNn\n\
QCsCIr74b28GTTgv5VYN61AuKi6hRANCAATnLMAzgrJHhYCg+eqUDkjj3noipIgr\n\
kLSqEO8mhPCzUFecgUIbJJSIq37Kbs7QCMvZforjjv2iF/BhfQawDQ3Q\n\
-----END PRIVATE KEY-----\n";

/// The bundle holds the server's own self-signed certificate: trusted by
/// name after the validity and host-name checks, as OpenSSL trusts a
/// self-signed leaf that is itself in the store (node v22.22.2 accepts this
/// one through NODE_EXTRA_CA_CERTS, measured). On macOS that is the second
/// verdict's call: Apple's TLS policy refuses this 100-year certificate.
#[tokio::test(flavor = "multi_thread")]
async fn the_server_s_own_self_signed_certificate_in_the_bundle_is_trusted_by_name() {
    within(async {
        let origin = serve_h2_tls_with(
            tls_acceptor_with(DEV_CERT, DEV_KEY, &[b"h2"]),
            "own leaf ok",
        )
        .await;
        let transport = transport_trusting(&bundle(DEV_CERT));
        let target = format!("https://localhost:{}/", origin.port);
        let response = get(&transport, &target).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(body_text(response).await, "own leaf ok");
    })
    .await;
}

/// The bundle anchors the leaf but the host name is another's: refused on
/// the name, with node's code -- by the platform's name check, or on macOS,
/// where Apple may report the validity period instead, by node's TLS rules.
#[tokio::test(flavor = "multi_thread")]
async fn an_anchored_chain_under_the_wrong_name_is_refused_in_node_s_terms() {
    within(async {
        let origin = serve_h2_tls("never").await;
        let transport = transport_trusting(&bundle(TLS_TEST_CA_CERT));
        // The leaf names localhost and 127.0.0.1; a lookup-hooked route
        // dials 127.0.0.1 for a host the leaf does not name.
        let target = format!("https://localhost.test:{}/", origin.port);
        let route = transport.route(true, ATTEMPT, TlsRange::Both);
        route.set_addrs(
            &format!("localhost.test:{}", origin.port),
            vec!["127.0.0.1".parse().unwrap()],
        );
        let request = http::Request::builder()
            .method("GET")
            .uri(&target)
            .body(empty_body())
            .unwrap();
        let err = transport.send(&route, request).await.unwrap_err();
        match err.to_outcome(&url::Url::parse(&target).unwrap()) {
            OpOutcome::NodeFailed { code, message, .. } => {
                assert_eq!(code, "ERR_TLS_CERT_ALTNAME_INVALID");
                assert!(
                    message.starts_with("Hostname/IP does not match certificate's altnames"),
                    "{message}"
                );
            }
            other => panic!("{other:?} ({err})"),
        }
    })
    .await;
}

/// A bundle that anchors nothing of the chain leaves the platform's refusal
/// standing, named as node names a lone leaf nobody trusts.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_the_bundle_does_not_anchor_is_the_platform_s_refused_in_node_s_terms() {
    within(async {
        let origin = serve_h2_tls("never").await;
        let transport = transport_trusting(&bundle(UNRELATED_CA));
        let target = format!("https://localhost:{}/", origin.port);
        let err = get(&transport, &target).await.unwrap_err();
        match err.to_outcome(&url::Url::parse(&target).unwrap()) {
            OpOutcome::NodeFailed { code, message, .. } => {
                assert_eq!(code, "UNABLE_TO_VERIFY_LEAF_SIGNATURE");
                assert_eq!(message, "unable to verify the first certificate");
            }
            other => panic!("{other:?} ({err})"),
        }
    })
    .await;
}
