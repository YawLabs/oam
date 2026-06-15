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

use crate::{OpOutcome, node_error_code, node_error_message};
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};

type ClientStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

#[derive(Default)]
pub struct TlsState {
    readers: HashMap<u64, ReadHalf<ClientStream>>,
    writers: HashMap<u64, WriteHalf<ClientStream>>,
    closed: HashSet<u64>,
}

pub type TlsRegistry = Arc<std::sync::Mutex<TlsState>>;

fn reinsert_reader(registry: &TlsRegistry, handle: u64, reader: ReadHalf<ClientStream>) -> bool {
    let mut guard = registry.lock().expect("tls registry lock");
    if guard.closed.contains(&handle) {
        drop(reader);
        false
    } else {
        guard.readers.insert(handle, reader);
        true
    }
}

fn reinsert_writer(registry: &TlsRegistry, handle: u64, writer: WriteHalf<ClientStream>) -> bool {
    let mut guard = registry.lock().expect("tls registry lock");
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
    OpOutcome::NodeFailed {
        code: code.to_string(),
        message: node_error_message(code, syscall, target, &error),
    }
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

/// tls.connect: TCP connect + TLS handshake.
/// Returns Json {handle, protocol, cipher, authorized, alpnProtocol}.
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
        .map(|v| format!("{v:?}"))
        .unwrap_or_default();
    let cipher = client_conn
        .negotiated_cipher_suite()
        .map(|c| format!("{:?}", c.suite()))
        .unwrap_or_default();
    let alpn = client_conn
        .alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .unwrap_or_default();

    let handle = ids.fetch_add(1, Ordering::Relaxed);
    let (reader, writer) = tokio::io::split(tls_stream);
    {
        let mut guard = registry.lock().expect("tls registry lock");
        guard.readers.insert(handle, reader);
        guard.writers.insert(handle, writer);
    }

    OpOutcome::Json(
        serde_json::json!({
            "handle": handle,
            "protocol": protocol,
            "cipher": cipher,
            "authorized": reject_unauthorized,
            "alpnProtocol": alpn,
        })
        .to_string(),
    )
}

pub async fn tls_read(registry: TlsRegistry, handle: u64, len: usize) -> OpOutcome {
    let reader = registry
        .lock()
        .expect("tls registry lock")
        .readers
        .remove(&handle);
    let Some(mut reader) = reader else {
        return OpOutcome::Failed(format!("tls: read handle {handle} is gone"));
    };

    let mut buf = vec![0u8; len.clamp(1, 8 * 1024 * 1024)];
    match reader.read(&mut buf).await {
        Ok(0) => {
            reinsert_reader(&registry, handle, reader);
            OpOutcome::Done
        }
        Ok(n) => {
            reinsert_reader(&registry, handle, reader);
            buf.truncate(n);
            OpOutcome::Bytes(buf)
        }
        Err(e) => tls_fail(e, "read", &handle.to_string()),
    }
}

pub async fn tls_write(registry: TlsRegistry, handle: u64, data: Vec<u8>) -> OpOutcome {
    let writer = registry
        .lock()
        .expect("tls registry lock")
        .writers
        .remove(&handle);
    let Some(mut writer) = writer else {
        return OpOutcome::Failed(format!("tls: write handle {handle} is gone"));
    };

    match writer.write_all(&data).await {
        Ok(()) => {
            reinsert_writer(&registry, handle, writer);
            OpOutcome::Done
        }
        Err(e) => tls_fail(e, "write", &handle.to_string()),
    }
}

pub async fn tls_shutdown(registry: TlsRegistry, handle: u64) -> OpOutcome {
    let writer = registry
        .lock()
        .expect("tls registry lock")
        .writers
        .remove(&handle);
    let Some(mut writer) = writer else {
        return OpOutcome::Done;
    };
    let _ = writer.shutdown().await;
    drop(writer);
    OpOutcome::Done
}

pub fn tls_close(registry: &TlsRegistry, handle: u64) {
    let mut guard = registry.lock().expect("tls registry lock");
    guard.readers.remove(&handle);
    guard.writers.remove(&handle);
    guard.closed.insert(handle);
}

/// Build a TLS server config from PEM-encoded cert chain + private key.
/// Used by https_serve in http_server.rs.
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
