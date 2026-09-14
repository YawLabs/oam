//! TLS client sockets (node:tls).
//!
//! Client connections use tokio-rustls over a TCP stream, split into
//! independent read/write halves via `tokio::io::split()`. The same
//! remove-await-reinsert discipline as tcp.rs keeps locks short.
//!
//! Server-side TLS (node:https) is handled in http_server.rs via
//! `https_serve` -- it wraps each accepted TCP stream with a TLS
//! acceptor before handing it to hyper. The request/response lifecycle
//! is identical to plain HTTP (shared HttpState, same ops).

use crate::{OpOutcome, node_errno, node_error_code, node_error_message};
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};

type ClientStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;
type ServerStream = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;

enum TlsReader {
    Client(ReadHalf<ClientStream>),
    Server(ReadHalf<ServerStream>),
}

enum TlsWriter {
    Client(WriteHalf<ClientStream>),
    Server(WriteHalf<ServerStream>),
}

#[derive(Default)]
pub struct TlsState {
    readers: HashMap<u64, TlsReader>,
    writers: HashMap<u64, TlsWriter>,
    closed: HashSet<u64>,
    /// Per-handle cancellation, mirroring tcp.rs's accept cancel. A parked
    /// `tls_read` holds a `ReadHalf` from `tokio::io::split` (a BiLock: the
    /// socket only closes once BOTH halves drop). Without this, `tls_close`
    /// cannot tear down a connection whose read is parked waiting for bytes
    /// that never come -- the read stays in-flight, the event loop never
    /// drains, and the process hangs at exit. `tls_close` fires the Notify so
    /// the parked read drops its half and the socket closes.
    cancel: HashMap<u64, Arc<tokio::sync::Notify>>,
}

pub type TlsRegistry = Arc<std::sync::Mutex<TlsState>>;

fn reinsert_reader(registry: &TlsRegistry, handle: u64, reader: TlsReader) -> bool {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.closed.contains(&handle) {
        drop(reader);
        false
    } else {
        guard.readers.insert(handle, reader);
        true
    }
}

fn reinsert_writer(registry: &TlsRegistry, handle: u64, writer: TlsWriter) -> bool {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.closed.contains(&handle) {
        drop(writer);
        false
    } else {
        guard.writers.insert(handle, writer);
        true
    }
}

fn tls_fail(error: std::io::Error, syscall: &str, target: &str) -> OpOutcome {
    let code = node_error_code(&error);
    // syscall + errno, but no `path`: a host:port is not a filesystem path,
    // and node does not put one on a net error.
    OpOutcome::node_failed_at(
        code,
        node_error_message(code, syscall, target, &error),
        syscall,
        None,
        node_errno(code, &error),
    )
}

/// Build a rustls ClientConfig from optional PEM-encoded CA certs.
/// Falls back to the Mozilla root store if no custom CA is provided.
fn build_client_config(
    ca_pem: Option<&str>,
    client_cert_pem: Option<&str>,
    client_key_pem: Option<&str>,
) -> Result<rustls::ClientConfig, String> {
    let mut root_store = rustls::RootCertStore::empty();

    if let Some(ca) = ca_pem {
        let certs = rustls_pemfile::certs(&mut BufReader::new(ca.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("ca cert parse: {e}"))?;
        for cert in certs {
            root_store
                .add(cert)
                .map_err(|e| format!("ca cert add: {e}"))?;
        }
    } else {
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    let builder = rustls::ClientConfig::builder().with_root_certificates(root_store);

    if let (Some(cert_pem), Some(key_pem)) = (client_cert_pem, client_key_pem) {
        let certs = rustls_pemfile::certs(&mut BufReader::new(cert_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("client cert parse: {e}"))?;
        let key = rustls_pemfile::private_key(&mut BufReader::new(key_pem.as_bytes()))
            .map_err(|e| format!("client key parse: {e}"))?
            .ok_or("no private key found in client key PEM")?;
        builder
            .with_client_auth_cert(certs, key)
            .map_err(|e| format!("client auth: {e}"))
    } else {
        Ok(builder.with_no_client_auth())
    }
}

// ------------------------------------------------------------ Node's names
// What rustls negotiated, spelled the way Node spells it (#138). Node reports
// OpenSSL's names -- "TLSv1.3" for the protocol, getCipher()'s OpenSSL `name`
// next to the IANA `standardName`, getEphemeralKeyInfo()'s { type, name,
// size } for the key-exchange group -- and code switches on them
// (`getProtocol() === 'TLSv1.3'`, cipher-name logs). rustls's Debug
// spellings ("TLSv1_3", "TLS13_AES_256_GCM_SHA384") never leave this module.

/// OpenSSL's name for a protocol version: what `socket.getProtocol()` and
/// `getCipher().version` report.
pub fn protocol_name(version: rustls::ProtocolVersion) -> String {
    use rustls::ProtocolVersion as V;
    match version {
        V::TLSv1_3 => "TLSv1.3".to_string(),
        V::TLSv1_2 => "TLSv1.2".to_string(),
        V::TLSv1_1 => "TLSv1.1".to_string(),
        V::TLSv1_0 => "TLSv1".to_string(),
        V::SSLv3 => "SSLv3".to_string(),
        other => format!("{other:?}"),
    }
}

/// (`name`, `standardName`) of a cipher suite as `socket.getCipher()` reports
/// them: OpenSSL's name and the IANA name. The table is every suite the ring
/// provider (with tls12) can negotiate; a TLS 1.3 suite has one name in both
/// columns, as in Node. Anything else -- unreachable today -- keeps rustls's
/// spelling rather than a guessed OpenSSL one.
pub fn cipher_names(suite: rustls::CipherSuite) -> (String, String) {
    use rustls::CipherSuite as S;
    let (name, standard) = match suite {
        S::TLS13_AES_128_GCM_SHA256 => ("TLS_AES_128_GCM_SHA256", "TLS_AES_128_GCM_SHA256"),
        S::TLS13_AES_256_GCM_SHA384 => ("TLS_AES_256_GCM_SHA384", "TLS_AES_256_GCM_SHA384"),
        S::TLS13_CHACHA20_POLY1305_SHA256 => (
            "TLS_CHACHA20_POLY1305_SHA256",
            "TLS_CHACHA20_POLY1305_SHA256",
        ),
        S::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 => (
            "ECDHE-ECDSA-AES128-GCM-SHA256",
            "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
        ),
        S::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 => (
            "ECDHE-ECDSA-AES256-GCM-SHA384",
            "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
        ),
        S::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 => (
            "ECDHE-ECDSA-CHACHA20-POLY1305",
            "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
        ),
        S::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => (
            "ECDHE-RSA-AES128-GCM-SHA256",
            "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
        ),
        S::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => (
            "ECDHE-RSA-AES256-GCM-SHA384",
            "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
        ),
        S::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => (
            "ECDHE-RSA-CHACHA20-POLY1305",
            "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
        ),
        other => {
            let name = format!("{other:?}");
            return (name.clone(), name);
        }
    };
    (name.to_string(), standard.to_string())
}

/// `socket.getEphemeralKeyInfo()`'s { type, name, size } for a key-exchange
/// group, with OpenSSL's curve names and bit sizes (X25519 is 253 bits).
pub fn ephemeral_key_info(group: rustls::NamedGroup) -> Option<serde_json::Value> {
    use rustls::NamedGroup as G;
    let (name, size) = match group {
        G::X25519 => ("X25519", 253),
        G::secp256r1 => ("prime256v1", 256),
        G::secp384r1 => ("secp384r1", 384),
        G::secp521r1 => ("secp521r1", 521),
        _ => return None,
    };
    Some(serde_json::json!({ "type": "ECDH", "name": name, "size": size }))
}

/// The peer's certificate chain as base64 DER, leaf first -- what the JS side
/// builds `getPeerCertificate()` and `getPeerX509Certificate()` from. None
/// when the peer sent no certificate (a server whose client sent none).
fn peer_certificates_b64(
    chain: Option<&[rustls::pki_types::CertificateDer<'static>]>,
) -> Option<Vec<String>> {
    use base64::Engine;
    let chain = chain?;
    if chain.is_empty() {
        return None;
    }
    Some(
        chain
            .iter()
            .map(|c| base64::engine::general_purpose::STANDARD.encode(c.as_ref()))
            .collect(),
    )
}

/// tls.connect: TCP connect + TLS handshake.
/// Returns Json {handle, protocol, cipher, cipherStandardName, authorized,
/// alpnProtocol, ephemeralKeyInfo?, peerCertificates?, localAddr?,
/// remoteAddr?} -- the names in Node's spelling (see above), the peer chain
/// as base64 DER leaf first, and the address pair in the same shape as
/// tcp_connect's, so a TLSSocket can report `address()`, `localPort` and the
/// resolved `remoteAddress` the way a net.Socket does.
#[allow(clippy::too_many_arguments)]
pub async fn tls_connect(
    registry: TlsRegistry,
    ids: Arc<std::sync::atomic::AtomicU64>,
    host: String,
    port: u16,
    server_name: Option<String>,
    ca_pem: Option<String>,
    reject_unauthorized: bool,
    client_cert_pem: Option<String>,
    client_key_pem: Option<String>,
) -> OpOutcome {
    let addr = format!("{host}:{port}");
    let tcp = match tokio::net::TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => return tls_fail(e, "connect", &addr),
    };

    let sni = server_name.as_deref().unwrap_or(&host);
    let server_name = match rustls::pki_types::ServerName::try_from(sni.to_string()) {
        Ok(n) => n,
        Err(e) => return OpOutcome::Failed(format!("invalid server name '{sni}': {e}")),
    };

    let config = if reject_unauthorized {
        match build_client_config(
            ca_pem.as_deref(),
            client_cert_pem.as_deref(),
            client_key_pem.as_deref(),
        ) {
            Ok(c) => c,
            Err(e) => return OpOutcome::Failed(e),
        }
    } else {
        // rejectUnauthorized: false -- accept any certificate
        let mut config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
            .with_no_client_auth();
        config.enable_sni = true;
        config
    };

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tls_stream = match connector.connect(server_name, tcp).await {
        Ok(s) => s,
        Err(e) => return tls_fail(e, "connect", &addr),
    };

    let (_, client_conn) = tls_stream.get_ref();
    let protocol = client_conn
        .protocol_version()
        .map(protocol_name)
        .unwrap_or_default();
    let (cipher, cipher_standard_name) = client_conn
        .negotiated_cipher_suite()
        .map(|c| cipher_names(c.suite()))
        .unwrap_or_default();
    let ephemeral_key_info = client_conn
        .negotiated_key_exchange_group()
        .and_then(|g| ephemeral_key_info(g.name()));
    let peer_certificates = peer_certificates_b64(client_conn.peer_certificates());
    let alpn = client_conn
        .alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .unwrap_or_default();

    let (tcp, _) = tls_stream.get_ref();
    let local_addr = tcp.local_addr().ok();
    let remote_addr = tcp.peer_addr().ok();

    let handle = ids.fetch_add(1, Ordering::Relaxed);
    let (reader, writer) = tokio::io::split(tls_stream);
    {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        guard.readers.insert(handle, TlsReader::Client(reader));
        guard.writers.insert(handle, TlsWriter::Client(writer));
    }

    let mut payload = serde_json::json!({
        "handle": handle,
        "protocol": protocol,
        "cipher": cipher,
        "cipherStandardName": cipher_standard_name,
        "authorized": reject_unauthorized,
        "alpnProtocol": alpn,
    });
    if let Some(info) = ephemeral_key_info {
        payload["ephemeralKeyInfo"] = info;
    }
    if let Some(chain) = peer_certificates {
        payload["peerCertificates"] = serde_json::Value::from(chain);
    }
    if let Some(la) = local_addr {
        payload["localAddr"] = crate::tcp::addr_to_json(la);
    }
    if let Some(ra) = remote_addr {
        payload["remoteAddr"] = crate::tcp::addr_to_json(ra);
    }
    OpOutcome::Json(payload.to_string())
}

pub async fn tls_read(registry: TlsRegistry, handle: u64, len: usize) -> OpOutcome {
    let (reader, notify) = {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        let reader = guard.readers.remove(&handle);
        // Get-or-insert the per-handle cancel Notify (same idiom as
        // tcp_accept) so a concurrent tls_close can wake a parked read.
        let notify = guard
            .cancel
            .entry(handle)
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone();
        (reader, notify)
    };
    let Some(reader) = reader else {
        return OpOutcome::Failed(format!("tls: read handle {handle} is gone"));
    };

    let mut buf = vec![0u8; len.clamp(1, 8 * 1024 * 1024)];
    match reader {
        TlsReader::Client(mut r) => {
            tokio::select! {
                res = r.read(&mut buf) => match res {
                    Ok(0) => {
                        reinsert_reader(&registry, handle, TlsReader::Client(r));
                        OpOutcome::Done
                    }
                    Ok(n) => {
                        reinsert_reader(&registry, handle, TlsReader::Client(r));
                        buf.truncate(n);
                        OpOutcome::Bytes(buf)
                    }
                    Err(e) => tls_fail(e, "read", &handle.to_string()),
                },
                // tls_close fired: drop the read half so the BiLock releases
                // and the socket closes; do NOT reinsert.
                _ = notify.notified() => {
                    drop(r);
                    OpOutcome::Done
                }
            }
        }
        TlsReader::Server(mut r) => {
            tokio::select! {
                res = r.read(&mut buf) => match res {
                    Ok(0) => {
                        reinsert_reader(&registry, handle, TlsReader::Server(r));
                        OpOutcome::Done
                    }
                    Ok(n) => {
                        reinsert_reader(&registry, handle, TlsReader::Server(r));
                        buf.truncate(n);
                        OpOutcome::Bytes(buf)
                    }
                    Err(e) => tls_fail(e, "read", &handle.to_string()),
                },
                _ = notify.notified() => {
                    drop(r);
                    OpOutcome::Done
                }
            }
        }
    }
}

pub async fn tls_write(registry: TlsRegistry, handle: u64, data: Vec<u8>) -> OpOutcome {
    let writer = registry
        .lock()
        .expect("tls registry lock")
        .writers
        .remove(&handle);
    let Some(writer) = writer else {
        return OpOutcome::Failed(format!("tls: write handle {handle} is gone"));
    };

    match writer {
        TlsWriter::Client(mut w) => match w.write_all(&data).await {
            Ok(()) => {
                reinsert_writer(&registry, handle, TlsWriter::Client(w));
                OpOutcome::Done
            }
            Err(e) => tls_fail(e, "write", &handle.to_string()),
        },
        TlsWriter::Server(mut w) => match w.write_all(&data).await {
            Ok(()) => {
                reinsert_writer(&registry, handle, TlsWriter::Server(w));
                OpOutcome::Done
            }
            Err(e) => tls_fail(e, "write", &handle.to_string()),
        },
    }
}

pub async fn tls_shutdown(registry: TlsRegistry, handle: u64) -> OpOutcome {
    let writer = registry
        .lock()
        .expect("tls registry lock")
        .writers
        .remove(&handle);
    let Some(writer) = writer else {
        return OpOutcome::Done;
    };
    match writer {
        TlsWriter::Client(mut w) => {
            let _ = w.shutdown().await;
        }
        TlsWriter::Server(mut w) => {
            let _ = w.shutdown().await;
        }
    }
    OpOutcome::Done
}

pub fn tls_close(registry: &TlsRegistry, handle: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard.readers.remove(&handle);
    guard.writers.remove(&handle);
    guard.closed.insert(handle);
    // Wake any parked tls_read so it drops its read half and the socket can
    // close -- otherwise the BiLock keeps the connection (and the event loop)
    // alive and the process hangs at exit.
    if let Some(notify) = guard.cancel.remove(&handle) {
        notify.notify_one();
    }
}

pub async fn tls_accept_wrap(
    tls_registry: TlsRegistry,
    tcp_registry: crate::tcp::TcpRegistry,
    ids: Arc<std::sync::atomic::AtomicU64>,
    tcp_handle: u64,
    cert_pem: String,
    key_pem: String,
) -> OpOutcome {
    let Some((reader, writer)) = tcp_registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_halves(tcp_handle)
    else {
        return OpOutcome::Failed(format!("tls accept: tcp handle {tcp_handle} is gone"));
    };

    let tcp_stream = match reader.reunite(writer) {
        Ok(s) => s,
        Err(e) => return OpOutcome::Failed(format!("tls accept: reunite failed: {e}")),
    };

    let tls_config = match build_server_config(&cert_pem, &key_pem) {
        Ok(c) => c,
        Err(e) => return OpOutcome::Failed(format!("tls accept config: {e}")),
    };

    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
    let tls_stream = match acceptor.accept(tcp_stream).await {
        Ok(s) => s,
        Err(e) => return tls_fail(e, "accept", &tcp_handle.to_string()),
    };

    let (_, server_conn) = tls_stream.get_ref();
    let protocol = server_conn
        .protocol_version()
        .map(protocol_name)
        .unwrap_or_default();
    let (cipher, cipher_standard_name) = server_conn
        .negotiated_cipher_suite()
        .map(|c| cipher_names(c.suite()))
        .unwrap_or_default();
    // The client's chain, if it sent one (this server requests none, so it
    // is absent today); no ephemeralKeyInfo -- Node reports null on a
    // server-side socket.
    let peer_certificates = peer_certificates_b64(server_conn.peer_certificates());
    let alpn = server_conn
        .alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .unwrap_or_default();

    let (tcp, _) = tls_stream.get_ref();
    let local_addr = tcp.local_addr().ok();
    let remote_addr = tcp.peer_addr().ok();

    let handle = ids.fetch_add(1, Ordering::Relaxed);
    let (reader, writer) = tokio::io::split(tls_stream);
    {
        let mut guard = tls_registry.lock().unwrap_or_else(|e| e.into_inner());
        guard.readers.insert(handle, TlsReader::Server(reader));
        guard.writers.insert(handle, TlsWriter::Server(writer));
    }

    let mut payload = serde_json::json!({
        "handle": handle,
        "protocol": protocol,
        "cipher": cipher,
        "cipherStandardName": cipher_standard_name,
        "alpnProtocol": alpn,
    });
    if let Some(chain) = peer_certificates {
        payload["peerCertificates"] = serde_json::Value::from(chain);
    }
    if let Some(la) = local_addr {
        payload["localAddr"] = crate::tcp::addr_to_json(la);
    }
    if let Some(ra) = remote_addr {
        payload["remoteAddr"] = crate::tcp::addr_to_json(ra);
    }
    OpOutcome::Json(payload.to_string())
}

/// Build a TLS server config from PEM-encoded cert chain + private key.
pub fn build_server_config(
    cert_pem: &str,
    key_pem: &str,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs = rustls_pemfile::certs(&mut BufReader::new(cert_pem.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("server cert parse: {e}"))?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_pem.as_bytes()))
        .map_err(|e| format!("server key parse: {e}"))?
        .ok_or("no private key found in server key PEM")?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("server tls config: {e}"))?;
    Ok(Arc::new(config))
}

/// Dangerous: skip all certificate verification (rejectUnauthorized: false).
#[derive(Debug)]
struct NoCertVerifier;

impl rustls::client::danger::ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod node_names {
    use super::*;
    use rustls::CipherSuite as S;
    use rustls::NamedGroup as G;
    use rustls::ProtocolVersion as V;

    fn names(suite: S) -> (String, String) {
        cipher_names(suite)
    }

    #[test]
    fn protocol_names_are_openssls() {
        assert_eq!(protocol_name(V::TLSv1_3), "TLSv1.3");
        assert_eq!(protocol_name(V::TLSv1_2), "TLSv1.2");
        assert_eq!(protocol_name(V::TLSv1_1), "TLSv1.1");
        assert_eq!(protocol_name(V::TLSv1_0), "TLSv1");
    }

    // Node's getCipher() after a TLS 1.3 handshake (probed on v22.22.2):
    // name and standardName are both the IANA spelling.
    #[test]
    fn tls13_suites_have_one_name() {
        for (suite, expected) in [
            (S::TLS13_AES_128_GCM_SHA256, "TLS_AES_128_GCM_SHA256"),
            (S::TLS13_AES_256_GCM_SHA384, "TLS_AES_256_GCM_SHA384"),
            (
                S::TLS13_CHACHA20_POLY1305_SHA256,
                "TLS_CHACHA20_POLY1305_SHA256",
            ),
        ] {
            assert_eq!(names(suite), (expected.to_string(), expected.to_string()));
        }
    }

    // A TLS 1.2 handshake (a Node server pinned with maxVersion) reports
    // OpenSSL's name next to the IANA standardName -- probed:
    // ECDHE-RSA-AES128-GCM-SHA256 / TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256.
    // oam's tls has no minVersion/maxVersion to pin 1.2 from JS, so the six
    // 1.2 suites are covered here rather than end to end.
    #[test]
    fn tls12_suites_carry_openssl_and_iana_names() {
        for (suite, name, standard) in [
            (
                S::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                "ECDHE-RSA-AES128-GCM-SHA256",
                "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
            ),
            (
                S::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
                "ECDHE-RSA-AES256-GCM-SHA384",
                "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
            ),
            (
                S::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
                "ECDHE-RSA-CHACHA20-POLY1305",
                "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
            ),
            (
                S::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
                "ECDHE-ECDSA-AES128-GCM-SHA256",
                "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
            ),
            (
                S::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
                "ECDHE-ECDSA-AES256-GCM-SHA384",
                "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
            ),
            (
                S::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
                "ECDHE-ECDSA-CHACHA20-POLY1305",
                "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
            ),
        ] {
            assert_eq!(names(suite), (name.to_string(), standard.to_string()));
        }
    }

    // Every suite the ring provider can negotiate is in the table: none falls
    // through to rustls's own spelling.
    #[test]
    fn every_ring_suite_is_tabled() {
        for suite in rustls::crypto::ring::ALL_CIPHER_SUITES {
            let (name, standard) = names(suite.suite());
            assert!(
                !name.starts_with("TLS13_") && !name.contains("_WITH_"),
                "{name}"
            );
            assert!(standard.starts_with("TLS_"), "{standard}");
        }
    }

    #[test]
    fn ephemeral_key_info_is_openssls() {
        assert_eq!(
            ephemeral_key_info(G::X25519),
            Some(serde_json::json!({ "type": "ECDH", "name": "X25519", "size": 253 }))
        );
        assert_eq!(
            ephemeral_key_info(G::secp256r1),
            Some(serde_json::json!({ "type": "ECDH", "name": "prime256v1", "size": 256 }))
        );
        assert_eq!(
            ephemeral_key_info(G::secp384r1),
            Some(serde_json::json!({ "type": "ECDH", "name": "secp384r1", "size": 384 }))
        );
        // Every group the ring provider offers has a Node name.
        for group in rustls::crypto::ring::ALL_KX_GROUPS {
            assert!(
                ephemeral_key_info(group.name()).is_some(),
                "{:?}",
                group.name()
            );
        }
    }

    #[test]
    fn peer_chain_is_base64_der_leaf_first() {
        let leaf = rustls::pki_types::CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x01]);
        let ca = rustls::pki_types::CertificateDer::from(vec![0x30, 0x00]);
        assert_eq!(peer_certificates_b64(None), None);
        assert_eq!(peer_certificates_b64(Some(&[])), None);
        assert_eq!(
            peer_certificates_b64(Some(&[leaf, ca])),
            Some(vec!["MAMCAQE=".to_string(), "MAA=".to_string()])
        );
    }
}
