//! TLS client sockets (node:tls).
//!
//! Client connections use tokio-rustls over a TCP stream, split into
//! independent read/write halves via `tokio::io::split()`. The same
//! remove-await-reinsert discipline as tcp.rs keeps locks short.
//!
//! Certificate verification is rustls's webpki verifier wrapped in
//! `NodeCertVerifier`, which adds the two things Node's OpenSSL-backed
//! client does that webpki does not: it accepts a peer certificate the user
//! trusts BY NAME (the `ca` option, or the NODE_EXTRA_CA_CERTS bundle) as
//! the server's own certificate, and it reports a refusal in Node's terms
//! (`DEPTH_ZERO_SELF_SIGNED_CERT`, `CERT_HAS_EXPIRED`,
//! `ERR_TLS_CERT_ALTNAME_INVALID`, ...) rather than rustls's. Every code,
//! message and precedence rule here was measured on node v22.22.2.
//!
//! Server-side TLS (node:https) is handled in http_server.rs via
//! `https_serve` -- it wraps each accepted TCP stream with a TLS
//! acceptor before handing it to hyper. The request/response lifecycle
//! is identical to plain HTTP (shared HttpState, same ops).

use crate::{OpOutcome, node_errno, node_error_code, node_error_message};
use rustls::CertificateError;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::{Error as PemError, PemObject};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::BufReader;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::GeneralName;
use x509_parser::parse_x509_certificate;
use x509_parser::time::ASN1Time;

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
    /// Handles closed while a read or write had a half checked out. The
    /// marker keeps that half from being reinserted when its await returns,
    /// and the LAST checked-out half to come back removes it -- so the set
    /// only ever holds handles with work outstanding. It used to keep every
    /// closed handle's id for the life of the process (#139).
    closed: HashSet<u64>,
    /// Halves currently out of the maps for an await, per handle. Bumped by
    /// `take_reader` / `take_writer`, released by `InFlight`'s drop.
    in_flight: HashMap<u64, u32>,
    /// Per-handle cancellation, mirroring tcp.rs's accept cancel. A parked
    /// `tls_read` holds a `ReadHalf` from `tokio::io::split` (a BiLock: the
    /// socket only closes once BOTH halves drop). Without this, `tls_close`
    /// cannot tear down a connection whose read is parked waiting for bytes
    /// that never come -- the read stays in-flight, the event loop never
    /// drains, and the process hangs at exit. `tls_close` fires the Notify so
    /// the parked read drops its half and the socket closes.
    cancel: HashMap<u64, Arc<tokio::sync::Notify>>,
}

impl TlsState {
    fn take_reader(&mut self, handle: u64) -> Option<TlsReader> {
        let reader = self.readers.remove(&handle)?;
        *self.in_flight.entry(handle).or_insert(0) += 1;
        Some(reader)
    }

    fn take_writer(&mut self, handle: u64) -> Option<TlsWriter> {
        let writer = self.writers.remove(&handle)?;
        *self.in_flight.entry(handle).or_insert(0) += 1;
        Some(writer)
    }

    /// One checked-out half is back (reinserted or dropped). The last one
    /// back after a `tls_close` clears the closed marker, so nothing about
    /// the handle outlives its final await.
    fn release(&mut self, handle: u64) {
        if let Some(n) = self.in_flight.get_mut(&handle) {
            *n -= 1;
            if *n == 0 {
                self.in_flight.remove(&handle);
                self.closed.remove(&handle);
            }
        }
    }

    /// (closed markers, cancel notifies, handles with a half in flight,
    /// readers, writers): all five are 0 once every handle is closed.
    #[cfg(test)]
    pub(crate) fn bookkeeping(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.closed.len(),
            self.cancel.len(),
            self.in_flight.len(),
            self.readers.len(),
            self.writers.len(),
        )
    }
}

pub type TlsRegistry = Arc<Mutex<TlsState>>;

/// A half checked out of the registry for an await. Dropping it releases
/// the handle's in-flight count on every exit path -- reinserted, dropped
/// after an error, or dropped because `tls_close` fired the cancel -- so the
/// closed marker cannot outlive the await that needed it.
struct InFlight {
    registry: TlsRegistry,
    handle: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release(self.handle);
    }
}

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

// ----------------------------------------------------------- NODE_EXTRA_CA_CERTS

/// What `NODE_EXTRA_CA_CERTS` named, read once per process.
#[derive(Debug, Default)]
pub struct ExtraCaCerts {
    pub certs: Vec<CertificateDer<'static>>,
    /// Node's one-line warning for a file that could not be loaded (missing,
    /// unreadable, or a PEM section that would not decode), verbatim.
    pub warning: Option<String>,
}

static EXTRA_CA_CERTS: OnceLock<ExtraCaCerts> = OnceLock::new();

/// The `NODE_EXTRA_CA_CERTS` bundle, loaded on first call and cached for the
/// process. Node reads the variable in its crypto initialisation -- before
/// any script runs, whether or not a TLS connection ever follows -- and
/// prints the load warning right there, as a bare `Warning: ...` line on
/// stderr: not through `process.emitWarning`, so `--no-warnings` does not
/// silence it and `process.on('warning')` never sees it (measured on
/// v22.22.2). `CoreRuntime::new` calls this so oam does the same at boot.
pub fn extra_ca_certs() -> &'static ExtraCaCerts {
    EXTRA_CA_CERTS.get_or_init(|| {
        let loaded = match std::env::var_os("NODE_EXTRA_CA_CERTS") {
            Some(path) if !path.is_empty() => load_extra_ca_file(&path),
            _ => ExtraCaCerts::default(),
        };
        if let Some(warning) = &loaded.warning {
            eprintln!("{warning}");
        }
        loaded
    })
}

/// Read one extra-CA bundle the way Node's `AddCertsFromFile` does: every
/// certificate up to the first section that will not decode is kept, and
/// that section (or a file that could not be opened) becomes the warning.
/// A file with no certificate in it is silently empty -- OpenSSL's
/// `PEM_R_NO_START_LINE` is its end-of-file, not an error.
pub fn load_extra_ca_file(path: &OsStr) -> ExtraCaCerts {
    let display = path.to_string_lossy();
    let warn = |err: String| {
        Some(format!(
            "Warning: Ignoring extra certs from `{display}`, load failed: {err}"
        ))
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            // A directory: OpenSSL's fopen fails with ERROR_ACCESS_DENIED
            // on Windows, which Node then prints as errno 5 (`error:80000005:
            // system library::Input/output error`, measured), and its fread
            // fails with EISDIR elsewhere. std reports the open differently,
            // so the number comes from here.
            let e = if std::path::Path::new(path).is_dir() {
                std::io::Error::from_raw_os_error(if cfg!(windows) { 5 } else { 21 })
            } else {
                e
            };
            return ExtraCaCerts {
                certs: Vec::new(),
                warning: warn(openssl_system_error(&e)),
            };
        }
    };
    let (certs, error) = pem_certs_until_error(&bytes);
    ExtraCaCerts {
        certs,
        warning: error.and_then(warn),
    }
}

/// Certificates in PEM order up to the first section OpenSSL would refuse,
/// with that refusal in OpenSSL's `error:<code>:PEM routines::<reason>`
/// spelling (the codes are `ERR_LIB_PEM` | `PEM_R_*`).
fn pem_certs_until_error(bytes: &[u8]) -> (Vec<CertificateDer<'static>>, Option<String>) {
    let mut certs = Vec::new();
    for item in CertificateDer::pem_slice_iter(bytes) {
        match item {
            Ok(cert) => certs.push(cert),
            Err(PemError::Base64Decode(_)) => {
                return (
                    certs,
                    Some("error:04800064:PEM routines::bad base64 decode".to_string()),
                );
            }
            Err(PemError::MissingSectionEnd { .. }) => {
                return (
                    certs,
                    Some("error:04800066:PEM routines::bad end line".to_string()),
                );
            }
            // A malformed BEGIN line, or nothing further: OpenSSL scans to
            // the end of the file and stops without complaint.
            Err(_) => break,
        }
    }
    (certs, None)
}

/// OpenSSL's rendering of a system-library error: `error:8000XXXX:system
/// library::<strerror>`, where XXXX is the OS error in hex. Node reports
/// `GetLastError()` on Windows and `errno` elsewhere -- `raw_os_error()` is
/// exactly that value -- and renders it with the C runtime's `strerror`,
/// even on Windows, where the number is not an errno at all (a directory is
/// `error:80000005:system library::Input/output error`: ERROR_ACCESS_DENIED
/// printed as EIO). Measured for ENOENT on v22.22.2.
fn openssl_system_error(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!(
            "error:{:08X}:system library::{}",
            0x8000_0000u32 | code as u32,
            crt_strerror(code)
        ),
        None => format!("error:80000000:system library::{error}"),
    }
}

/// The C runtime's `strerror` text for an error number.
fn crt_strerror(code: i32) -> String {
    #[cfg(windows)]
    {
        // The UCRT table (verbatim: `strerror(n)` for n in 0..=42). std's
        // description on Windows is the Win32 message, not this one.
        const TABLE: [&str; 43] = [
            "No error",
            "Operation not permitted",
            "No such file or directory",
            "No such process",
            "Interrupted function call",
            "Input/output error",
            "No such device or address",
            "Arg list too long",
            "Exec format error",
            "Bad file descriptor",
            "No child processes",
            "Resource temporarily unavailable",
            "Not enough space",
            "Permission denied",
            "Bad address",
            "Unknown error",
            "Resource device",
            "File exists",
            "Improper link",
            "No such device",
            "Not a directory",
            "Is a directory",
            "Invalid argument",
            "Too many open files in system",
            "Too many open files",
            "Inappropriate I/O control operation",
            "Unknown error",
            "File too large",
            "No space left on device",
            "Invalid seek",
            "Read-only file system",
            "Too many links",
            "Broken pipe",
            "Domain error",
            "Result too large",
            "Unknown error",
            "Resource deadlock avoided",
            "Unknown error",
            "Filename too long",
            "No locks available",
            "Function not implemented",
            "Directory not empty",
            "Illegal byte sequence",
        ];
        usize::try_from(code)
            .ok()
            .and_then(|i| TABLE.get(i))
            .unwrap_or(&"Unknown error")
            .to_string()
    }
    #[cfg(not(windows))]
    {
        // std renders a raw OS error as "<strerror> (os error N)".
        let text = std::io::Error::from_raw_os_error(code).to_string();
        match text.rfind(" (os error ") {
            Some(at) => text[..at].to_string(),
            None => text,
        }
    }
}

// ------------------------------------------------------ certificate verdicts

/// A refused certificate, in Node's terms: `code` is what `err.code` and
/// `socket.authorizationError` carry; `message` is `err.message`. `code` is
/// None for a rustls refusal Node has no name for -- the caller then falls
/// back to the raw I/O error, as before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyFailure {
    pub code: Option<&'static str>,
    pub message: String,
}

impl VerifyFailure {
    fn named(code: &'static str, message: &str) -> Self {
        Self {
            code: Some(code),
            message: message.to_string(),
        }
    }
}

/// Where the verifier leaves its verdict for `tls_connect` to read after
/// the handshake: None means the certificate was accepted.
type VerifySlot = Arc<Mutex<Option<VerifyFailure>>>;

/// rustls's webpki verification plus Node's departures from it.
#[derive(Debug)]
struct NodeCertVerifier {
    /// Chain verification against the trust anchors; None when there is no
    /// anchor at all (a `ca` made only of non-self-signed certificates),
    /// where every chain is an unknown issuer and only a trusted leaf can
    /// pass.
    inner: Option<Arc<rustls::client::WebPkiServerVerifier>>,
    /// The handshake signature algorithms (ring's), used directly so the
    /// server proves it holds the certificate's key with or without
    /// `inner`, in advisory mode too.
    supported: rustls::crypto::WebPkiSupportedAlgorithms,
    /// Certificates the user trusts by name: the `ca` option, or the
    /// NODE_EXTRA_CA_CERTS bundle when there is no `ca` (never a bundled
    /// Mozilla root). A peer certificate byte-equal to one of these is the
    /// server's own certificate and is accepted after the validity and
    /// hostname checks -- OpenSSL's last-resort rule for a leaf that is
    /// itself in the store, which is how Node connects to a self-signed dev
    /// certificate passed as `ca`. rustls alone refuses a CA:TRUE one as
    /// `CaUsedAsEndEntity`.
    trusted_leaves: Vec<CertificateDer<'static>>,
    /// The user's certificates that are NOT self-signed. OpenSSL uses a
    /// store certificate to build the chain but, without
    /// X509_V_FLAG_PARTIAL_CHAIN (Node never sets it), only a self-signed
    /// one anchors it: `ca: <intermediate>` on its own fails with
    /// `UNABLE_TO_GET_ISSUER_CERT` (measured), while `ca: [intermediate,
    /// root]` verifies a server that sends its leaf alone. So these are
    /// offered to webpki as intermediates, never as anchors.
    trusted_non_anchors: Vec<CertificateDer<'static>>,
    /// Known but not trusted: the NODE_EXTRA_CA_CERTS bundle when a `ca`
    /// option replaces it. Node still completes the peer's chain through
    /// them before refusing it, and the refusal names the chain it built
    /// (`SELF_SIGNED_CERT_IN_CHAIN`, not `UNABLE_TO_VERIFY_LEAF_SIGNATURE`),
    /// so they inform the error code and nothing else.
    untrusted_known: Vec<CertificateDer<'static>>,
    /// rejectUnauthorized:false -- verify and record, never fail the
    /// handshake. What fills `socket.authorized` / `authorizationError`.
    advisory: bool,
    outcome: VerifySlot,
}

impl ServerCertVerifier for NodeCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verdict = self.verdict(end_entity, intermediates, server_name, now);
        *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) = match &verdict {
            Ok(()) => None,
            Err((failure, _)) => Some(failure.clone()),
        };
        match verdict {
            Ok(()) => Ok(ServerCertVerified::assertion()),
            Err(_) if self.advisory => Ok(ServerCertVerified::assertion()),
            Err((_, error)) => Err(error),
        }
    }

    // The handshake signature checks stay webpki's, in advisory mode too:
    // the server still has to prove it holds the certificate's key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported.supported_schemes()
    }
}

impl NodeCertVerifier {
    /// Node's verdict on the presented chain, with the rustls error that
    /// aborts the handshake when it is a refusal. Precedence is OpenSSL's as
    /// Node surfaces it (the LAST error its verify callback sees wins, and
    /// the date check runs last): a certificate outside its validity period
    /// is `CERT_HAS_EXPIRED` / `CERT_NOT_YET_VALID` whatever else is wrong
    /// with it; an untrusted chain is reported before the hostname is ever
    /// looked at; the hostname check runs only on a trusted chain.
    fn verdict(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        now: UnixTime,
    ) -> Result<(), (VerifyFailure, rustls::Error)> {
        if let Ok((_, leaf)) = parse_x509_certificate(end_entity.as_ref())
            && let Ok(at) = ASN1Time::from_timestamp(now.as_secs() as i64)
        {
            let validity = leaf.validity();
            if at.timestamp() > validity.not_after.timestamp() {
                return Err((
                    VerifyFailure::named("CERT_HAS_EXPIRED", "certificate has expired"),
                    CertificateError::Expired.into(),
                ));
            }
            if at.timestamp() < validity.not_before.timestamp() {
                return Err((
                    VerifyFailure::named("CERT_NOT_YET_VALID", "certificate is not yet valid"),
                    CertificateError::NotValidYet.into(),
                ));
            }
        }

        if self
            .trusted_leaves
            .iter()
            .any(|trusted| trusted.as_ref() == end_entity.as_ref())
        {
            return check_server_identity(server_name, end_entity.as_ref())
                .map_err(|failure| (failure, CertificateError::NotValidForName.into()));
        }

        // What OpenSSL would build the chain from: the peer's intermediates
        // plus every certificate the process knows -- the user's non-anchor
        // certificates and, when a `ca` superseded them, the extra-CA bundle.
        let known: Vec<CertificateDer<'_>> = intermediates
            .iter()
            .cloned()
            .chain(self.trusted_non_anchors.iter().cloned())
            .chain(self.untrusted_known.iter().cloned())
            .collect();
        let classify = || classify_unknown_issuer(end_entity, &known, &self.trusted_non_anchors);
        let error = match &self.inner {
            Some(inner) => {
                match inner.verify_server_cert(end_entity, &known, server_name, &[], now) {
                    Ok(_) => return Ok(()),
                    Err(error) => error,
                }
            }
            None => return Err((classify(), CertificateError::UnknownIssuer.into())),
        };
        let failure = match &error {
            rustls::Error::InvalidCertificate(reason) => match reason {
                // webpki insists on a subjectAltName; Node falls back to the
                // subject CN when there is none, so its check decides.
                CertificateError::NotValidForName
                | CertificateError::NotValidForNameContext { .. } => {
                    return check_server_identity(server_name, end_entity.as_ref())
                        .map_err(|failure| (failure, error));
                }
                CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                    VerifyFailure::named("CERT_HAS_EXPIRED", "certificate has expired")
                }
                CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                    VerifyFailure::named("CERT_NOT_YET_VALID", "certificate is not yet valid")
                }
                CertificateError::UnknownIssuer => classify(),
                // webpki refuses a CA:TRUE leaf before it looks for an
                // issuer; OpenSSL reads the same chain as "nobody I trust
                // signed this".
                CertificateError::Other(other)
                    if other
                        .0
                        .downcast_ref::<webpki::Error>()
                        .is_some_and(|e| matches!(e, webpki::Error::CaUsedAsEndEntity)) =>
                {
                    classify()
                }
                CertificateError::Revoked => {
                    VerifyFailure::named("CERT_REVOKED", "certificate revoked")
                }
                CertificateError::BadSignature => {
                    VerifyFailure::named("CERT_SIGNATURE_FAILURE", "certificate signature failure")
                }
                CertificateError::InvalidPurpose
                | CertificateError::InvalidPurposeContext { .. } => {
                    VerifyFailure::named("INVALID_PURPOSE", "unsupported certificate purpose")
                }
                CertificateError::UnhandledCriticalExtension => VerifyFailure::named(
                    "UNHANDLED_CRITICAL_EXTENSION",
                    "unhandled critical extension",
                ),
                _ => VerifyFailure {
                    code: None,
                    message: error.to_string(),
                },
            },
            _ => VerifyFailure {
                code: None,
                message: error.to_string(),
            },
        };
        Err((failure, error))
    }
}

/// Whether a certificate is its own issuer (subject == issuer, byte for
/// byte) -- the shape OpenSSL will accept as a trust anchor.
fn is_self_signed(der: &[u8]) -> bool {
    parse_x509_certificate(der)
        .ok()
        .is_some_and(|(_, cert)| cert.subject().as_raw() == cert.issuer().as_raw())
}

/// OpenSSL's name for an issuer it could not find, read off the chain it
/// would have built from `pool` (the peer's intermediates plus what the
/// process knows). A self-signed
/// top is `SELF_SIGNED_CERT_IN_CHAIN`, or `DEPTH_ZERO_SELF_SIGNED_CERT` when
/// the leaf is that top; a top that is one of the user's own non-anchor
/// certificates is `UNABLE_TO_GET_ISSUER_CERT` (it is in the store, its
/// issuer is not); otherwise a lone leaf is `UNABLE_TO_VERIFY_LEAF_SIGNATURE`
/// and a longer chain `UNABLE_TO_GET_ISSUER_CERT_LOCALLY`. All five measured
/// on v22.22.2.
fn classify_unknown_issuer(
    end_entity: &CertificateDer<'_>,
    pool: &[CertificateDer<'_>],
    trusted_non_anchors: &[CertificateDer<'static>],
) -> VerifyFailure {
    let parsed: Vec<X509Certificate<'_>> = pool
        .iter()
        .filter_map(|der| {
            parse_x509_certificate(der.as_ref())
                .ok()
                .map(|(_, cert)| cert)
        })
        .collect();
    let Ok((_, leaf)) = parse_x509_certificate(end_entity.as_ref()) else {
        return VerifyFailure::named(
            "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
            "unable to verify the first certificate",
        );
    };
    let self_signed =
        |cert: &X509Certificate<'_>| cert.subject().as_raw() == cert.issuer().as_raw();
    let mut top: &X509Certificate<'_> = &leaf;
    let mut depth = 1usize;
    while !self_signed(top) && depth <= parsed.len() {
        let issuer = top.issuer().as_raw();
        let Some(next) = parsed.iter().find(|c| c.subject().as_raw() == issuer) else {
            break;
        };
        top = next;
        depth += 1;
    }
    let top_is_users = trusted_non_anchors.iter().any(|c| {
        parse_x509_certificate(c.as_ref())
            .ok()
            .is_some_and(|(_, user)| user.tbs_certificate.as_ref() == top.tbs_certificate.as_ref())
    });
    match (self_signed(top), depth) {
        (true, 1) => VerifyFailure::named("DEPTH_ZERO_SELF_SIGNED_CERT", "self-signed certificate"),
        (true, _) => VerifyFailure::named(
            "SELF_SIGNED_CERT_IN_CHAIN",
            "self-signed certificate in certificate chain",
        ),
        (false, _) if top_is_users => VerifyFailure::named(
            "UNABLE_TO_GET_ISSUER_CERT",
            "unable to get issuer certificate",
        ),
        (false, 1) => VerifyFailure::named(
            "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
            "unable to verify the first certificate",
        ),
        (false, _) => VerifyFailure::named(
            "UNABLE_TO_GET_ISSUER_CERT_LOCALLY",
            "unable to get local issuer certificate",
        ),
    }
}

// --------------------------------------------- tls.checkServerIdentity port

/// The identity fields Node's `checkServerIdentity` reads off a peer
/// certificate object: `subjectaltname` in OpenSSL's rendering, split the
/// way Node splits it, and the subject CN(s).
#[derive(Debug, Default, PartialEq, Eq)]
struct CertIdentity {
    /// `cert.subjectaltname`, absent when the certificate has no SAN.
    alt_names: Option<String>,
    dns_names: Vec<String>,
    /// Canonical text of every IP Address entry (Node's `canonicalizeIP`).
    ips: Vec<String>,
    /// `cert.subject.CN`: one entry per CN attribute, empty ones dropped
    /// (Node reads an empty CN as no CN).
    cn: Vec<String>,
}

impl CertIdentity {
    /// From the two fields as Node sees them -- the SAN string is parsed
    /// exactly as `checkServerIdentity` parses it (`', '`-separated,
    /// `DNS:` and `IP Address:` entries, everything else ignored).
    fn from_node_shape(alt_names: Option<&str>, cn: Vec<String>) -> Self {
        let mut dns_names = Vec::new();
        let mut ips = Vec::new();
        if let Some(alt) = alt_names.filter(|a| !a.is_empty()) {
            for name in alt.split(", ") {
                if let Some(dns) = name.strip_prefix("DNS:") {
                    dns_names.push(dns.to_string());
                } else if let Some(ip) = name.strip_prefix("IP Address:") {
                    ips.push(canonicalize_ip(ip));
                }
            }
        }
        Self {
            alt_names: alt_names.filter(|a| !a.is_empty()).map(str::to_string),
            dns_names,
            ips,
            cn,
        }
    }

    fn from_der(der: &[u8]) -> Self {
        let Ok((_, cert)) = parse_x509_certificate(der) else {
            return Self::default();
        };
        let alt_names = match cert.subject_alternative_name() {
            Ok(Some(ext)) => {
                let entries: Vec<String> = ext
                    .value
                    .general_names
                    .iter()
                    .map(render_general_name)
                    .collect();
                Some(entries.join(", "))
            }
            _ => None,
        };
        let cn: Vec<String> = cert
            .subject()
            .iter_common_name()
            .filter_map(|attr| attr.as_str().ok())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        Self::from_node_shape(alt_names.as_deref(), cn)
    }
}

/// One SAN entry as OpenSSL prints it (`X509V3_EXT_print`), which is what
/// Node's `cert.subjectaltname` is made of and what its mismatch message
/// echoes back verbatim.
fn render_general_name(name: &GeneralName<'_>) -> String {
    match name {
        GeneralName::DNSName(dns) => format!("DNS:{dns}"),
        GeneralName::IPAddress(bytes) => match bytes.len() {
            4 => format!(
                "IP Address:{}",
                std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])
            ),
            // OpenSSL writes an IPv6 SAN as eight uncompressed uppercase
            // hex groups: `IP Address:0:0:0:0:0:0:0:1`.
            16 => {
                let groups: Vec<String> = bytes
                    .chunks(2)
                    .map(|pair| format!("{:X}", u16::from_be_bytes([pair[0], pair[1]])))
                    .collect();
                format!("IP Address:{}", groups.join(":"))
            }
            _ => "IP Address:<invalid>".to_string(),
        },
        GeneralName::RFC822Name(mail) => format!("email:{mail}"),
        GeneralName::URI(uri) => format!("URI:{uri}"),
        GeneralName::DirectoryName(dir) => format!("DirName:{dir}"),
        GeneralName::RegisteredID(oid) => format!("Registered ID:{oid}"),
        GeneralName::OtherName(..) => "othername:<unsupported>".to_string(),
        GeneralName::X400Address(_) => "X400Name:<unsupported>".to_string(),
        GeneralName::EDIPartyName(_) => "EdiPartyName:<unsupported>".to_string(),
    }
}

/// Node's `canonicalizeIP`: the address re-rendered, `""` if it is not one.
fn canonicalize_ip(text: &str) -> String {
    text.parse::<std::net::IpAddr>()
        .map(|ip| ip.to_string())
        .unwrap_or_default()
}

/// Node's `unfqdn`: one trailing dot removed.
fn unfqdn(host: &str) -> &str {
    host.strip_suffix('.').unwrap_or(host)
}

/// Node's `splitHost`: labels of the un-dotted, lower-cased name.
fn split_host(host: &str) -> Vec<String> {
    unfqdn(host)
        .to_lowercase()
        .split('.')
        .map(str::to_string)
        .collect()
}

/// Node's `check(hostParts, pattern, wildcards)`, label for label.
fn host_matches(host_parts: &[String], pattern: &str, wildcards: bool) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let pattern_parts = split_host(pattern);
    if host_parts.len() != pattern_parts.len() {
        return false;
    }
    // Pattern has empty components, e.g. "bad..example.com".
    if pattern_parts.iter().any(|p| p.is_empty()) {
        return false;
    }
    // RFC 6125 allows IDNA U-labels (Unicode) in A-labels (ASCII) in a
    // wildcard pattern, but rejects wildcard in a U-label.
    let is_bad = |s: &str| s.chars().any(|c| !('\u{21}'..='\u{7f}').contains(&c));
    if pattern_parts.iter().any(|p| is_bad(p)) {
        return false;
    }
    // Check host parts from right to left first.
    for i in (1..host_parts.len()).rev() {
        if host_parts[i] != pattern_parts[i] {
            return false;
        }
    }
    let host_subdomain = &host_parts[0];
    let pattern_subdomain = &pattern_parts[0];
    let pattern_subdomain_parts: Vec<&str> = pattern_subdomain.split('*').collect();
    // Short-circuit when the subdomain does not contain a wildcard. RFC 6125
    // does not allow wildcard substitution for components containing IDNA
    // A-labels (Punycode) so match those exactly.
    if pattern_subdomain_parts.len() == 1 || pattern_subdomain.contains("xn--") {
        return host_subdomain == pattern_subdomain;
    }
    if !wildcards {
        return false;
    }
    // More than one wildcard is always wrong.
    if pattern_subdomain_parts.len() > 2 {
        return false;
    }
    // *.tld wildcards are not allowed.
    if pattern_parts.len() <= 2 {
        return false;
    }
    let (prefix, suffix) = (pattern_subdomain_parts[0], pattern_subdomain_parts[1]);
    if prefix.len() + suffix.len() > host_subdomain.len() {
        return false;
    }
    host_subdomain.starts_with(prefix) && host_subdomain.ends_with(suffix)
}

/// Node's `checkServerIdentity(hostname, cert)`: None when the name
/// matches, else the `reason` its `ERR_TLS_CERT_ALTNAME_INVALID` carries.
fn check_identity(hostname: &str, identity: &CertIdentity) -> Option<String> {
    let hostname = unfqdn(hostname);
    let (valid, reason) = if let Ok(ip) = hostname.parse::<std::net::IpAddr>() {
        let wanted = ip.to_string();
        (
            identity.ips.contains(&wanted),
            format!(
                "IP: {hostname} is not in the cert's list: {}",
                identity.ips.join(", ")
            ),
        )
    } else if !identity.dns_names.is_empty() || !identity.cn.is_empty() {
        let host_parts = split_host(hostname);
        if !identity.dns_names.is_empty() {
            (
                identity
                    .dns_names
                    .iter()
                    .any(|pattern| host_matches(&host_parts, pattern, true)),
                format!(
                    "Host: {hostname}. is not in the cert's altnames: {}",
                    identity.alt_names.as_deref().unwrap_or("")
                ),
            )
        } else {
            // Match against Common Name only if no supported identifiers exist.
            (
                identity
                    .cn
                    .iter()
                    .any(|pattern| host_matches(&host_parts, pattern, true)),
                format!(
                    "Host: {hostname}. is not cert's CN: {}",
                    identity.cn.join(",")
                ),
            )
        }
    } else {
        (false, "Cert does not contain a DNS name".to_string())
    };
    if valid { None } else { Some(reason) }
}

/// The hostname check on a certificate the chain check accepted, as the
/// `ERR_TLS_CERT_ALTNAME_INVALID` Node raises from `checkServerIdentity`.
fn check_server_identity(server_name: &ServerName<'_>, der: &[u8]) -> Result<(), VerifyFailure> {
    let hostname = match server_name {
        ServerName::DnsName(dns) => dns.as_ref().to_string(),
        ServerName::IpAddress(ip) => std::net::IpAddr::from(*ip).to_string(),
        _ => String::new(),
    };
    match check_identity(&hostname, &CertIdentity::from_der(der)) {
        None => Ok(()),
        Some(reason) => Err(VerifyFailure {
            code: Some("ERR_TLS_CERT_ALTNAME_INVALID"),
            message: format!("Hostname/IP does not match certificate's altnames: {reason}"),
        }),
    }
}

// ------------------------------------------------------------- client config

/// Build a rustls ClientConfig from optional PEM-encoded CA certs. Trust is
/// the `ca` option alone when given, else the Mozilla root store plus the
/// NODE_EXTRA_CA_CERTS bundle -- `ca` replaces the extras rather than
/// adding to them, as in Node (measured). The returned slot is where the
/// verifier leaves its verdict.
fn build_client_config(
    ca_pem: Option<&str>,
    client_cert_pem: Option<&str>,
    client_key_pem: Option<&str>,
    reject_unauthorized: bool,
) -> Result<(rustls::ClientConfig, VerifySlot), String> {
    let mut root_store = rustls::RootCertStore::empty();
    let extras = extra_ca_certs();

    // The user's certificates: only a self-signed one anchors a chain (see
    // `trusted_non_anchors`); every one is trusted by name as a leaf.
    let (trusted_leaves, untrusted_known) = if let Some(ca) = ca_pem {
        let certs = rustls_pemfile::certs(&mut BufReader::new(ca.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("ca cert parse: {e}"))?;
        for cert in certs.iter().filter(|c| is_self_signed(c.as_ref())) {
            root_store
                .add(cert.clone())
                .map_err(|e| format!("ca cert add: {e}"))?;
        }
        (certs, extras.certs.clone())
    } else {
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in extras.certs.iter().filter(|c| is_self_signed(c.as_ref())) {
            // One that will not serve as an anchor is still trusted by
            // name, through `trusted_leaves`.
            let _ = root_store.add(cert.clone());
        }
        (extras.certs.clone(), Vec::new())
    };
    let trusted_non_anchors: Vec<CertificateDer<'static>> = trusted_leaves
        .iter()
        .filter(|c| !is_self_signed(c.as_ref()))
        .cloned()
        .collect();

    let inner = if root_store.is_empty() {
        None
    } else {
        Some(
            rustls::client::WebPkiServerVerifier::builder(Arc::new(root_store))
                .build()
                .map_err(|e| format!("tls verifier: {e}"))?,
        )
    };
    let outcome: VerifySlot = Arc::new(Mutex::new(None));
    let verifier = Arc::new(NodeCertVerifier {
        inner,
        supported: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        trusted_leaves,
        trusted_non_anchors,
        untrusted_known,
        advisory: !reject_unauthorized,
        outcome: outcome.clone(),
    });
    let builder = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier);

    let config = if let (Some(cert_pem), Some(key_pem)) = (client_cert_pem, client_key_pem) {
        let certs = rustls_pemfile::certs(&mut BufReader::new(cert_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("client cert parse: {e}"))?;
        let key = rustls_pemfile::private_key(&mut BufReader::new(key_pem.as_bytes()))
            .map_err(|e| format!("client key parse: {e}"))?
            .ok_or("no private key found in client key PEM")?;
        builder
            .with_client_auth_cert(certs, key)
            .map_err(|e| format!("client auth: {e}"))?
    } else {
        builder.with_no_client_auth()
    };
    Ok((config, outcome))
}

/// tls.connect: TCP connect + TLS handshake.
/// Returns Json {handle, protocol, cipher, authorized, authorizationError,
/// alpnProtocol, localAddr?, remoteAddr?} -- the address pair in the same
/// shape as tcp_connect's, so a TLSSocket can report `address()`,
/// `localPort` and the resolved `remoteAddress` the way a net.Socket does.
/// A certificate the verifier refuses (rejectUnauthorized true) rejects with
/// Node's code and message and nothing else -- no syscall, no errno.
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

    // Node strips one trailing dot (`unfqdn`) before it checks the name.
    let sni = unfqdn(server_name.as_deref().unwrap_or(&host));
    let server_name = match ServerName::try_from(sni.to_string()) {
        Ok(n) => n,
        Err(e) => return OpOutcome::Failed(format!("invalid server name '{sni}': {e}")),
    };

    let (config, verdict) = match build_client_config(
        ca_pem.as_deref(),
        client_cert_pem.as_deref(),
        client_key_pem.as_deref(),
        reject_unauthorized,
    ) {
        Ok(built) => built,
        Err(e) => return OpOutcome::Failed(e),
    };

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tls_stream = match connector.connect(server_name, tcp).await {
        Ok(s) => s,
        Err(e) => {
            if reject_unauthorized
                && let Some(VerifyFailure {
                    code: Some(code),
                    message,
                }) = verdict.lock().unwrap_or_else(|e| e.into_inner()).take()
            {
                return OpOutcome::node_failed(code, message);
            }
            return tls_fail(e, "connect", &addr);
        }
    };
    let failure = verdict.lock().unwrap_or_else(|e| e.into_inner()).take();

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

    // Node: `authorizationError` is the verify error's code (its message
    // when it has none), null on an accepted certificate.
    let authorization_error = match &failure {
        None => serde_json::Value::Null,
        Some(VerifyFailure {
            code: Some(code), ..
        }) => serde_json::Value::from(*code),
        Some(VerifyFailure {
            code: None,
            message,
        }) => serde_json::Value::from(message.as_str()),
    };
    let mut payload = serde_json::json!({
        "handle": handle,
        "protocol": protocol,
        "cipher": cipher,
        "authorized": failure.is_none(),
        "authorizationError": authorization_error,
        "alpnProtocol": alpn,
    });
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
        let Some(reader) = guard.take_reader(handle) else {
            // Closed (or never existed): no cancel Notify for a read that
            // will not park -- one created here outlived the handle (#139).
            return OpOutcome::Failed(format!("tls: read handle {handle} is gone"));
        };
        // Get-or-insert the per-handle cancel Notify (same idiom as
        // tcp_accept) so a concurrent tls_close can wake a parked read.
        let notify = guard
            .cancel
            .entry(handle)
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone();
        (reader, notify)
    };
    let _in_flight = InFlight {
        registry: registry.clone(),
        handle,
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
        .unwrap_or_else(|e| e.into_inner())
        .take_writer(handle);
    let Some(writer) = writer else {
        return OpOutcome::Failed(format!("tls: write handle {handle} is gone"));
    };
    let _in_flight = InFlight {
        registry: registry.clone(),
        handle,
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
        .unwrap_or_else(|e| e.into_inner())
        .take_writer(handle);
    let Some(writer) = writer else {
        return OpOutcome::Done;
    };
    let _in_flight = InFlight {
        registry: registry.clone(),
        handle,
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
    // Only a half that is out for an await can resurrect the handle; mark
    // the close for it, and let its return clear the mark (#139).
    if guard.in_flight.get(&handle).is_some_and(|n| *n > 0) {
        guard.closed.insert(handle);
    }
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
        .map(|v| format!("{v:?}"))
        .unwrap_or_default();
    let cipher = server_conn
        .negotiated_cipher_suite()
        .map(|c| format!("{:?}", c.suite()))
        .unwrap_or_default();
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
        "alpnProtocol": alpn,
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// The e2e suite's self-signed CA:TRUE localhost certificate (no
    /// subjectAltName at all, CN=localhost only) -- the shape rustls alone
    /// refuses as a leaf, and the one Node matches by CN.
    const CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDCTCCAfGgAwIBAgIUJscRiMbEzxV45KtAxD+Lly4dJrQwDQYJKoZIhvcNAQEL\n\
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDYxNTEyMzAwN1oXDTI3MDYx\n\
NTEyMzAwN1owFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF\n\
AAOCAQ8AMIIBCgKCAQEAoQ5a/fh4J3VW0MPpngEpN+yRUdJtlmY6aBhV/984yEIm\n\
ng9/MGoK0ZRdB8YYGqx4awK1z82ECwtmmdVO/77WA4q6N0CJRzmAF6BN9RgzoyKV\n\
2w1ltowPFyB6SrVqcW1MHqA/9NX/gw/ckvcjcuazYeI857joWulUmR/iWIpSNuBJ\n\
c6odEIkfXG9W6/GyZwlutQXnKaa8eClLqCm+hDnkPBHx+doGWxezFVeOfFAdQM8w\n\
NXT7mj4QN3fiHFDQHI6UkSnVttu7lAAEHY978gjnVyixAPX2dY9mB/Ed4R5eSOpJ\n\
eTR7bXH6+QmUcDJSaDblM5vB3fb3zhitEGLo/APdQQIDAQABo1MwUTAdBgNVHQ4E\n\
FgQUPffw9cdyC1LQ2PLrzN7IZjkpKmMwHwYDVR0jBBgwFoAUPffw9cdyC1LQ2PLr\n\
zN7IZjkpKmMwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAWtdW\n\
V/jSdVB5cN4GOwYXTHhh3dkYDtAPvFPCXbYacelaQe8mlRWv2BBHAhOZdmoJ3ai/\n\
kNRw0D6pKqjcF4p17of9S07ZFCRaQGBAsDEd9jNY156AlEXu4Z8yp/kXE3fvznib\n\
WHrQjdlDcmC2H/Ao+S7f4BkmbvsabyDbUoo+0Drk4MDvqga2azrFDdljqXQxzrEH\n\
/mEwoi9pfukgFnFnhDE+WEqNsZQF9Yxa5QEX6d5tgbOcxS2NpKDug4xSgkpAQ0l6\n\
XKpI59mdGTahOy9zGuNfTqVTHvrFoSXudnNHUjkfHK7Mh/VrNz9ZGpwDt5fGFD4x\n\
E13+0jp6In545LYu+A==\n\
-----END CERTIFICATE-----";

    const KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQChDlr9+HgndVbQ\n\
w+meASk37JFR0m2WZjpoGFX/3zjIQiaeD38wagrRlF0HxhgarHhrArXPzYQLC2aZ\n\
1U7/vtYDiro3QIlHOYAXoE31GDOjIpXbDWW2jA8XIHpKtWpxbUweoD/01f+DD9yS\n\
9yNy5rNh4jznuOha6VSZH+JYilI24Elzqh0QiR9cb1br8bJnCW61Becpprx4KUuo\n\
Kb6EOeQ8EfH52gZbF7MVV458UB1AzzA1dPuaPhA3d+IcUNAcjpSRKdW227uUAAQd\n\
j3vyCOdXKLEA9fZ1j2YH8R3hHl5I6kl5NHttcfr5CZRwMlJoNuUzm8Hd9vfOGK0Q\n\
Yuj8A91BAgMBAAECgf9+I0AgqPlx7fSQjN/rX/1oT1+BNc2efXJBFM5GGA3gye50\n\
3K5AvMy8V/aEoCFAwtOM/BJpLgy8mbFByk6U/mGfZIdzvpfFsMMhvetQiiPnIK89\n\
YMDIt+kZs9YTrQIw0+lKEzgECZaUj1exwt2AoC7d+tK4qZlRmm0ngFFGBw9c6g4E\n\
bKpZPPb62HjVAPcPPNJzj0ULTCkFQ7CPhgyz7q6UQUQJ0kM/8DWbnI9qbOWkv0qN\n\
TafdX50piyHstcXGNFelOXmUMw1qQvbPo28qpzkxH7bDU8pShzsnySJL2HL5Wxbr\n\
PbzZ94WOOLXfD3OmT5oW9kHDpH/zUd8pKlbYkNkCgYEA1ygCE4ZSNm0B7U8iwgBK\n\
0Aszxpnf9f4aKKfb9CsmLaf4rxmAAqRaxT4Eki2yebqRX15Ctzlf1ryddKxNAAdh\n\
gCcc+KAdwoJOO3pkwac+r/jsmvWqXHHi/Jn9Bhj886n5NkDGfKiCBL3rUHonwOjN\n\
7y61ExJIy46kOM81Pm88B90CgYEAv6E6owvvAjFs1eoWD5oyUnO1HeAhKvBClDjZ\n\
dcoY965ak5RFM4Da/HcnXAho4+pJY+4O48PIi8nQeZugm8DvpOKfivxYTI3ISDmz\n\
CG0m7N9jJiYOPyt8dpn7Yl7R8OqFvfZAd/KkJ1wBMpsKy1MGU1pdny9mVaAVjxei\n\
fnxNprUCgYEAn2onT6wgUe8mlFwkFrX8uHT0Ydw1EqC5ZRIqaJln6kAghCxSqqJ4\n\
FtjCrkRpjsPrXkwLBpLeLc8GoyHe03ykgz13u8d3BV1i9bLT4KA4VE4NkSsglOpV\n\
EnBOByyQj0GLQuVvq4F3BGhrZ+96cPaNTwC+bWkIwrnnd6gffSkRw4kCgYEAsAEE\n\
mzZdunTs0nii9IeaipJNmnf93rM3Y23nhUEut2ZDOOLowEosV8+UrfnnZNYNvCOt\n\
N1LeAk5FFTx0QjntoVKoWH43F3DtsDCWmDmwk8UFCsfPNAPb2A7LjekrCAxO9E+V\n\
nNWWIbRmQTWXr3G9EJeh/5AIfMKAqqF5lJTUuTUCgYEAtzMfzgUekShhJoGov7uH\n\
MyykhATJv+3ZlR0BCuEjgb7Lu6tu/pbgD1SkhpQ3QbM+XF5DgNJWxQATcgPWP6wy\n\
C7rRXUYQtUTmtwTetACx3EEz7k2ixAxxdDCUPJIxGcVIPVKt6sTovr3yGLMuc4f7\n\
I5PYIZ3kyY8EsQqX4JpTtbY=\n\
-----END PRIVATE KEY-----";

    fn identity(alt_names: Option<&str>, cn: &[&str]) -> CertIdentity {
        CertIdentity::from_node_shape(alt_names, cn.iter().map(|s| s.to_string()).collect())
    }

    /// Every row was run through node v22.22.2's `tls.checkServerIdentity`
    /// with the same inputs; `None` is a match, `Some` its `reason`.
    #[test]
    fn check_identity_matches_node_row_for_row() {
        let both = Some("DNS:localhost, IP Address:127.0.0.1");
        let rows: [(&str, CertIdentity, Option<&str>); 30] = [
            ("localhost", identity(both, &[]), None),
            ("LOCALHOST", identity(both, &[]), None),
            ("localhost", identity(Some("DNS:LOCALHOST"), &[]), None),
            ("localhost.", identity(Some("DNS:localhost"), &[]), None),
            ("localhost", identity(Some("DNS:localhost."), &[]), None),
            (
                "example.com",
                identity(both, &[]),
                Some(
                    "Host: example.com. is not in the cert's altnames: DNS:localhost, IP Address:127.0.0.1",
                ),
            ),
            ("127.0.0.1", identity(both, &[]), None),
            (
                "127.0.0.1",
                identity(Some("DNS:otherhost.example"), &[]),
                Some("IP: 127.0.0.1 is not in the cert's list: "),
            ),
            (
                "::1",
                identity(Some("DNS:localhost, IP Address:0:0:0:0:0:0:0:1"), &[]),
                None,
            ),
            (
                "0:0:0:0:0:0:0:1",
                identity(Some("IP Address:0:0:0:0:0:0:0:1"), &[]),
                None,
            ),
            (
                "10.0.0.1",
                identity(
                    Some("IP Address:127.0.0.1, IP Address:0:0:0:0:0:0:0:1"),
                    &[],
                ),
                Some("IP: 10.0.0.1 is not in the cert's list: 127.0.0.1, ::1"),
            ),
            ("localhost", identity(None, &["localhost"]), None),
            ("LocalHost", identity(None, &["localhost"]), None),
            (
                "example.com",
                identity(None, &["localhost"]),
                Some("Host: example.com. is not cert's CN: localhost"),
            ),
            (
                "example.com",
                identity(None, &["a", "b"]),
                Some("Host: example.com. is not cert's CN: a,b"),
            ),
            (
                "localhost",
                identity(Some("IP Address:127.0.0.1"), &["localhost"]),
                None,
            ),
            (
                "localhost",
                identity(None, &[]),
                Some("Cert does not contain a DNS name"),
            ),
            (
                "127.0.0.1",
                identity(None, &["127.0.0.1"]),
                Some("IP: 127.0.0.1 is not in the cert's list: "),
            ),
            (
                "a.example.test",
                identity(Some("DNS:*.example.test"), &[]),
                None,
            ),
            (
                "b.a.example.test",
                identity(Some("DNS:*.example.test"), &[]),
                Some("Host: b.a.example.test. is not in the cert's altnames: DNS:*.example.test"),
            ),
            (
                "a.test",
                identity(Some("DNS:*.test"), &[]),
                Some("Host: a.test. is not in the cert's altnames: DNS:*.test"),
            ),
            (
                "xa.example.test",
                identity(Some("DNS:x*.example.test"), &[]),
                None,
            ),
            (
                "axb.example.test",
                identity(Some("DNS:a*b.example.test"), &[]),
                None,
            ),
            (
                "a.example.test",
                identity(Some("DNS:a*b.example.test"), &[]),
                Some("Host: a.example.test. is not in the cert's altnames: DNS:a*b.example.test"),
            ),
            (
                "xn--a.example.test",
                identity(Some("DNS:xn--*.example.test"), &[]),
                Some(
                    "Host: xn--a.example.test. is not in the cert's altnames: DNS:xn--*.example.test",
                ),
            ),
            (
                "b.example.test",
                identity(
                    Some("DNS:a.example.test, email:x@y, URI:http://z, DNS:b.example.test"),
                    &[],
                ),
                None,
            ),
            ("other", identity(Some("email:x@y"), &["other"]), None),
            (
                "",
                identity(Some("DNS:localhost"), &[]),
                Some("Host: . is not in the cert's altnames: DNS:localhost"),
            ),
            ("A.b.c", identity(Some("DNS:*.b.c"), &[]), None),
            (
                "\u{e9}.example.test",
                identity(Some("DNS:\u{e9}.example.test"), &[]),
                Some(
                    "Host: \u{e9}.example.test. is not in the cert's altnames: DNS:\u{e9}.example.test",
                ),
            ),
        ];
        for (host, identity, expected) in rows {
            assert_eq!(
                check_identity(host, &identity).as_deref(),
                expected,
                "host {host:?} against {identity:?}"
            );
        }
    }

    /// The e2e certificate carries no subjectAltName at all (CN=localhost
    /// only), so Node matches it by CN -- the fallback webpki lacks.
    #[test]
    fn identity_is_read_off_the_certificate_in_openssl_spelling() {
        let der = CertificateDer::from_pem_slice(CERT.as_bytes()).unwrap();
        let identity = CertIdentity::from_der(der.as_ref());
        assert_eq!(identity.alt_names, None);
        assert!(identity.dns_names.is_empty());
        assert!(identity.ips.is_empty());
        assert_eq!(identity.cn, ["localhost"]);
        assert_eq!(check_identity("localhost", &identity), None);
        assert_eq!(
            check_identity("example.com", &identity).as_deref(),
            Some("Host: example.com. is not cert's CN: localhost")
        );
        assert_eq!(
            check_identity("127.0.0.1", &identity).as_deref(),
            Some("IP: 127.0.0.1 is not in the cert's list: ")
        );
    }

    #[test]
    fn unknown_issuer_is_named_from_the_chain_shape() {
        let cert = CertificateDer::from_pem_slice(CERT.as_bytes()).unwrap();
        // A self-signed leaf alone, and the same leaf echoed as its own
        // chain (OpenSSL de-duplicates): depth zero both times.
        assert_eq!(
            classify_unknown_issuer(&cert, &[], &[]).code,
            Some("DEPTH_ZERO_SELF_SIGNED_CERT")
        );
        assert_eq!(
            classify_unknown_issuer(&cert, std::slice::from_ref(&cert), &[]).code,
            Some("DEPTH_ZERO_SELF_SIGNED_CERT")
        );
        assert_eq!(
            classify_unknown_issuer(&cert, &[], &[]).message,
            "self-signed certificate"
        );
    }

    #[test]
    fn extra_ca_bundle_loads_up_to_the_first_bad_section() {
        let bad = "-----BEGIN CERTIFICATE-----\nnot base64 !!!\n-----END CERTIFICATE-----\n";
        let (certs, error) = pem_certs_until_error(format!("{CERT}\n{bad}").as_bytes());
        assert_eq!(certs.len(), 1);
        assert_eq!(
            error.as_deref(),
            Some("error:04800064:PEM routines::bad base64 decode")
        );
        let (certs, error) = pem_certs_until_error(format!("{bad}{CERT}").as_bytes());
        assert!(certs.is_empty());
        assert!(error.is_some());
        // No certificate at all is not an error (OpenSSL's end-of-file).
        assert_eq!(
            pem_certs_until_error(b"just some text, no certificates\n"),
            (Vec::new(), None)
        );
        assert_eq!(pem_certs_until_error(b""), (Vec::new(), None));
        // A key in the bundle is skipped, as PEM_read_bio_X509 skips it.
        let (certs, error) = pem_certs_until_error(format!("{KEY}\n{CERT}").as_bytes());
        assert_eq!((certs.len(), error), (1, None));
    }

    #[test]
    fn extra_ca_file_warnings_are_node_s_lines() {
        let dir = std::env::temp_dir().join(format!("oam-extra-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("does-not-exist.pem");
        let loaded = load_extra_ca_file(missing.as_os_str());
        assert!(loaded.certs.is_empty());
        assert_eq!(
            loaded.warning.as_deref(),
            Some(
                format!(
                    "Warning: Ignoring extra certs from `{}`, load failed: error:80000002:system library::No such file or directory",
                    missing.display()
                )
                .as_str()
            )
        );
        let good = dir.join("ca.pem");
        std::fs::write(&good, CERT).unwrap();
        let loaded = load_extra_ca_file(good.as_os_str());
        assert_eq!((loaded.certs.len(), loaded.warning), (1, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn openssl_system_error_is_hex_code_plus_strerror() {
        let enoent = std::io::Error::from_raw_os_error(2);
        assert_eq!(
            openssl_system_error(&enoent),
            "error:80000002:system library::No such file or directory"
        );
        let e13 = std::io::Error::from_raw_os_error(13);
        assert_eq!(
            openssl_system_error(&e13),
            "error:8000000D:system library::Permission denied"
        );
    }

    /// Accepts N connections; each echoes its first read, waits for the
    /// client to finish (so a second client read can park), then closes.
    async fn echo_server(listener: tokio::net::TcpListener, connections: usize) {
        let acceptor = tokio_rustls::TlsAcceptor::from(build_server_config(CERT, KEY).unwrap());
        for _ in 0..connections {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf).await {
                    let _ = stream.write_all(&buf[..n]).await;
                }
                let _ = stream.read(&mut buf).await;
                let _ = stream.shutdown().await;
            });
        }
    }

    /// #139: every clean close leaves the registry empty -- no closed
    /// marker, no cancel Notify, no in-flight count -- whether the close
    /// came after EOF, while a read was parked, or before a late read.
    #[tokio::test]
    async fn closed_handles_leave_no_bookkeeping_behind() {
        let registry: TlsRegistry = Arc::new(Mutex::new(TlsState::default()));
        let ids = Arc::new(AtomicU64::new(1));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        const N: usize = 9;
        tokio::spawn(echo_server(listener, N));

        for i in 0..N {
            // The self-signed CA:TRUE certificate is trusted by name as the
            // server's own certificate -- what Node does with it as `ca`.
            let OpOutcome::Json(payload) = tls_connect(
                registry.clone(),
                ids.clone(),
                "127.0.0.1".into(),
                port,
                Some("localhost".into()),
                Some(CERT.into()),
                true,
                None,
                None,
            )
            .await
            else {
                panic!("connect failed");
            };
            let info: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(info["authorized"], true, "{payload}");
            assert!(info["authorizationError"].is_null(), "{payload}");
            let handle = info["handle"].as_u64().unwrap();

            assert!(matches!(
                tls_write(registry.clone(), handle, b"ping".to_vec()).await,
                OpOutcome::Done
            ));
            assert!(matches!(
                tls_read(registry.clone(), handle, 64).await,
                OpOutcome::Bytes(b) if b == b"ping"
            ));
            match i % 3 {
                0 => {
                    // Our FIN, the server's close_notify, EOF, then close.
                    assert!(matches!(
                        tls_shutdown(registry.clone(), handle).await,
                        OpOutcome::Done
                    ));
                    assert!(matches!(
                        tls_read(registry.clone(), handle, 64).await,
                        OpOutcome::Done
                    ));
                    tls_close(&registry, handle);
                }
                1 => {
                    // Close while a read is parked: the closed marker's
                    // one legitimate use, cleared by the woken read.
                    let parked = tokio::spawn(tls_read(registry.clone(), handle, 64));
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    tls_close(&registry, handle);
                    assert!(matches!(parked.await.unwrap(), OpOutcome::Done));
                }
                _ => {
                    // Close first; a read that lands afterwards must fail
                    // without minting a cancel Notify.
                    tls_close(&registry, handle);
                    assert!(matches!(
                        tls_read(registry.clone(), handle, 64).await,
                        OpOutcome::Failed(_)
                    ));
                }
            }
        }

        let bookkeeping = registry.lock().unwrap().bookkeeping();
        assert_eq!(
            bookkeeping,
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// The verifier's verdict drives `authorized` / `authorizationError`
    /// in advisory mode and rejects the handshake otherwise.
    #[tokio::test]
    async fn untrusted_self_signed_certificate_is_depth_zero_self_signed() {
        let registry: TlsRegistry = Arc::new(Mutex::new(TlsState::default()));
        let ids = Arc::new(AtomicU64::new(1));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(echo_server(listener, 3));

        let connect = |ca: Option<&str>, reject: bool, sni: &str| {
            tls_connect(
                registry.clone(),
                ids.clone(),
                "127.0.0.1".into(),
                port,
                Some(sni.into()),
                ca.map(String::from),
                reject,
                None,
                None,
            )
        };
        match connect(None, true, "localhost").await {
            OpOutcome::NodeFailed {
                code,
                message,
                syscall,
                path,
                errno,
            } => {
                assert_eq!(code, "DEPTH_ZERO_SELF_SIGNED_CERT");
                assert_eq!(message, "self-signed certificate");
                assert_eq!((syscall, path, errno), (None, None, None));
            }
            other => panic!("expected a node-coded rejection, got {other:?}"),
        }
        match connect(None, false, "localhost").await {
            OpOutcome::Json(payload) => {
                let info: serde_json::Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(info["authorized"], false, "{payload}");
                assert_eq!(
                    info["authorizationError"], "DEPTH_ZERO_SELF_SIGNED_CERT",
                    "{payload}"
                );
                tls_close(&registry, info["handle"].as_u64().unwrap());
            }
            other => panic!("advisory mode must connect, got {other:?}"),
        }
        // Trusted by name but presented for the wrong host: Node's
        // checkServerIdentity error, with its exact message (this
        // certificate has no SAN, so the CN is what it is matched against).
        match connect(Some(CERT), true, "example.com").await {
            OpOutcome::NodeFailed { code, message, .. } => {
                assert_eq!(code, "ERR_TLS_CERT_ALTNAME_INVALID");
                assert_eq!(
                    message,
                    "Hostname/IP does not match certificate's altnames: Host: example.com. is not cert's CN: localhost"
                );
            }
            other => panic!("expected a hostname rejection, got {other:?}"),
        }
    }
}
