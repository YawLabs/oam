//! Server-side TLS (node:tls servers, and the http2 secure server built on
//! them): the server's secure context -- its certificate and key, the CAs it
//! checks client certificates against, whether it asks for one, its ALPN list
//! and version range -- built once when the server is created, and the
//! handshake each accepted connection runs through it.
//!
//! Node's model, measured on v22.22.2 and followed here:
//!
//! - The context is built at `tls.createServer()`: a key that cannot be read
//!   and a key that does not match its certificate throw there, synchronously
//!   (`ERR_OSSL_UNSUPPORTED`, `ERR_OSSL_X509_KEY_VALUES_MISMATCH`, ...). A
//!   server with no key or no certificate at all is created, and fails every
//!   handshake instead.
//! - `requestCert` asks the client for a certificate. OpenSSL's verify
//!   callback accepts whatever chain comes back ("we'll reject in
//!   javascript"); the verdict is read after the handshake
//!   (`onServerSocketSecure`): `authorized`, or `authorizationError` and --
//!   with `rejectUnauthorized` -- a socket destroyed before
//!   'secureConnection', from inside the handshake's completion: what the
//!   server's last flight would have said is never written, so a TLS 1.2
//!   client never sees the server's Finished and a TLS 1.3 client gets no
//!   session ticket. The one refusal the handshake itself makes is a client
//!   that sends no certificate at all while `rejectUnauthorized` is on
//!   (SSL_VERIFY_FAIL_IF_NO_PEER_CERT).
//! - ALPN is chosen in the SERVER's order among the protocols the client
//!   offers; a client that offers some but none the server has is refused
//!   with `no_application_protocol`; a client that offers none, or a server
//!   that has none, negotiates nothing (`alpnProtocol === false`).
//! - A first record that is not TLS at all (a cleartext HTTP/1 or HTTP/2
//!   client on a TLS port) is refused on OpenSSL's rules for it
//!   (`first_record_refusal`): never an answer a cleartext client could read.

use super::{
    TlsReader, TlsRegistry, TlsWriter, VerifyFailure, cipher_names, classify_unknown_issuer,
    extra_ca_certs, is_self_signed, peer_certificates_b64, protocol_name, protocol_versions,
    refuse_no_protocols,
};
use crate::OpOutcome;
use rustls::CertificateError;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use std::io::BufReader;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use x509_parser::parse_x509_certificate;
use x509_parser::time::ASN1Time;

// ------------------------------------------------------------------ context

/// What `tls.createServer()` hands the native side, already validated and
/// normalised by the JS layer (Node's option checks throw there).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerContextSpec {
    /// The `cert` option: one entry per chain (a string may hold a whole
    /// chain, leaf first), PEM text.
    pub certs: Vec<String>,
    /// The `key` option: one entry per key, each with its own passphrase
    /// (Node's `{ pem, passphrase }` form) or none.
    pub keys: Vec<KeySpec>,
    /// The `pfx` option: PKCS#12 bundles as base64, each with its own
    /// passphrase or none.
    pub pfx: Vec<PfxSpec>,
    /// The `passphrase` option: for any key or bundle that has none of its
    /// own.
    pub passphrase: Option<String>,
    /// The `ca` option, PEM text; None means Node's default store (the
    /// bundled roots plus NODE_EXTRA_CA_CERTS).
    pub ca: Option<Vec<String>>,
    /// The effective `minVersion` / `maxVersion` ("" = Node's default).
    pub min_version: String,
    pub max_version: String,
    /// The server's `honorCipherOrder`: a suite is chosen by the server's
    /// order of preference (Node's default, `SSL_OP_CIPHER_SERVER_PREFERENCE`
    /// there) rather than the client's.
    pub honor_cipher_order: bool,
}

impl Default for ServerContextSpec {
    fn default() -> Self {
        Self {
            certs: Vec::new(),
            keys: Vec::new(),
            pfx: Vec::new(),
            passphrase: None,
            ca: None,
            min_version: String::new(),
            max_version: String::new(),
            honor_cipher_order: true,
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct KeySpec {
    pub pem: String,
    pub passphrase: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct PfxSpec {
    /// base64 of the DER bundle.
    pub buf: String,
    pub passphrase: Option<String>,
}

/// A context build that Node would have thrown at `createServer()`, in its
/// shape: the message, and -- for OpenSSL's errors -- the `code`, `library`
/// and `reason` properties. A failure Node reports without a code (a PKCS#12
/// MAC that does not verify) has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextError {
    pub message: String,
    pub code: Option<&'static str>,
    pub library: Option<&'static str>,
    pub reason: Option<&'static str>,
}

impl ContextError {
    /// One of OpenSSL's errors as Node renders it: `error:<hex>:<library>::
    /// <reason>`, with the three properties.
    pub(crate) fn openssl(
        hex: &str,
        library: &'static str,
        reason: &'static str,
        code: &'static str,
    ) -> Self {
        ContextError {
            message: format!("error:{hex}:{library}::{reason}"),
            code: Some(code),
            library: Some(library),
            reason: Some(reason),
        }
    }

    /// A plain Error with Node's message and, optionally, a code.
    pub(crate) fn plain(message: &str, code: Option<&'static str>) -> Self {
        ContextError {
            message: message.to_string(),
            code,
            library: None,
            reason: None,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut error = serde_json::json!({ "message": self.message });
        if let Some(code) = self.code {
            error["code"] = code.into();
        }
        if let Some(library) = self.library {
            error["library"] = library.into();
        }
        if let Some(reason) = self.reason {
            error["reason"] = reason.into();
        }
        error
    }

    /// `ERR_OSSL_UNSUPPORTED`: a key OpenSSL's decoders cannot read.
    pub(crate) fn unsupported_key() -> Self {
        Self::openssl(
            "1E08010C",
            "DECODER routines",
            "unsupported",
            "ERR_OSSL_UNSUPPORTED",
        )
    }

    /// `ERR_OSSL_EVP_UNSUPPORTED`: a key protected with a cipher OpenSSL 3
    /// keeps in its legacy provider (single DES, RC2, RC4, ...), which Node
    /// does not load.
    pub(crate) fn evp_unsupported() -> Self {
        Self::openssl(
            "0308010C",
            "digital envelope routines",
            "unsupported",
            "ERR_OSSL_EVP_UNSUPPORTED",
        )
    }

    /// `ERR_OSSL_BAD_DECRYPT`: an encrypted key with a wrong or missing
    /// passphrase.
    pub(crate) fn bad_decrypt() -> Self {
        Self::openssl(
            "1C800064",
            "Provider routines",
            "bad decrypt",
            "ERR_OSSL_BAD_DECRYPT",
        )
    }

    /// `ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH`: an encrypted legacy PEM key whose
    /// ciphertext is not a whole number of cipher blocks (a truncated one).
    pub(crate) fn wrong_final_block_length() -> Self {
        Self::openssl(
            "1C80006B",
            "Provider routines",
            "wrong final block length",
            "ERR_OSSL_WRONG_FINAL_BLOCK_LENGTH",
        )
    }

    /// `ERR_OSSL_PEM_NO_START_LINE`: a `cert` with no certificate in it.
    fn no_start_line() -> Self {
        Self::openssl(
            "0480006C",
            "PEM routines",
            "no start line",
            "ERR_OSSL_PEM_NO_START_LINE",
        )
    }

    /// `ERR_OSSL_X509_KEY_VALUES_MISMATCH`: a key that is not the
    /// certificate's.
    fn key_mismatch() -> Self {
        Self::openssl(
            "05800074",
            "x509 certificate routines",
            "key values mismatch",
            "ERR_OSSL_X509_KEY_VALUES_MISMATCH",
        )
    }
}

/// A server's secure context: what every handshake on it runs with. Node
/// reads `requestCert`, `rejectUnauthorized` and `ALPNProtocols` off the
/// server for each connection it accepts, so those are applied per accept
/// (`tls_accept`).
pub struct ServerContext {
    /// None when the version range leaves nothing rustls can offer: every
    /// connection is refused with the `protocol_version` alert, as Node's
    /// server refuses each one (`refuse_no_protocols`).
    plain: Option<Arc<rustls::ServerConfig>>,
    versions: Vec<&'static rustls::SupportedProtocolVersion>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    /// `honorCipherOrder`: carried so a per-connection config built from
    /// scratch (`config_for`) picks suites the way `plain` does.
    honor_cipher_order: bool,
    resolver: Arc<dyn rustls::server::ResolvesServerCert>,
    /// Node's verdict on a requested client certificate.
    judge: Arc<ClientCertJudge>,
    /// No certificate / key pair to serve: Node creates the server and fails
    /// each handshake.
    has_identity: bool,
    /// The context's certificate store: the `ca` option's certificates and a
    /// pfx's, or None for Node's default store. What
    /// `getPeerCertificate(true)` looks a client chain's last issuer up in.
    store: Option<Vec<CertificateDer<'static>>>,
}

impl ServerContext {
    /// The certificates of this context's store, as `chain::store_issuers`
    /// takes them.
    fn store(&self) -> Vec<&CertificateDer<'static>> {
        match &self.store {
            Some(store) => store.iter().collect(),
            None => super::chain::default_store().iter().collect(),
        }
    }
}

impl ServerContext {
    /// The configuration one connection is accepted with: the server's, or
    /// -- when it asks for a client certificate -- one carrying this
    /// connection's own verifier (so the verdict is this connection's),
    /// sharing the server's session cache and ticketer; and the server's
    /// ALPN list.
    fn config_for(
        &self,
        plain: &Arc<rustls::ServerConfig>,
        options: &AcceptOptions,
        verdict: &Arc<VerdictSlot>,
        tls12: bool,
    ) -> Arc<rustls::ServerConfig> {
        let mut config = if options.request_cert {
            if tls12 && options.reject_unauthorized {
                verdict.require_tls12.store(true, Ordering::Release);
            }
            let verifier = RequestClientCert {
                supported: self.provider.signature_verification_algorithms,
                judge: Arc::clone(&self.judge),
                mandatory: options.reject_unauthorized,
                verdict: Arc::clone(verdict),
            };
            let Ok(builder) =
                rustls::ServerConfig::builder_with_provider(Arc::clone(&self.provider))
                    .with_protocol_versions(&self.versions)
            else {
                return Arc::clone(plain);
            };
            let mut config = builder
                .with_client_cert_verifier(Arc::new(verifier))
                .with_cert_resolver(Arc::clone(&self.resolver));
            config.session_storage = Arc::clone(&plain.session_storage);
            config.ticketer = Arc::clone(&plain.ticketer);
            config.ignore_client_order = self.honor_cipher_order;
            config
        } else if options.alpn.is_empty() {
            return Arc::clone(plain);
        } else {
            rustls::ServerConfig::clone(plain)
        };
        config.alpn_protocols = options.alpn.clone();
        Arc::new(config)
    }
}

/// What a secure context serves as its own identity: each key with the chain
/// it goes out with, and the CA certificates any pfx brought.
pub(crate) struct Identities {
    pub(crate) certified: Vec<Arc<CertifiedKey>>,
    pub(crate) pfx_cas: Vec<CertificateDer<'static>>,
}

/// The `ca` option's certificates as Node's context reads them: what parses,
/// the rest ignored (a `ca` of garbage does not throw; measured).
pub(crate) fn ca_certificates(ca: &[String]) -> Vec<CertificateDer<'static>> {
    ca.iter()
        .flat_map(|pem| {
            rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Read a context's `cert`, `key` and `pfx` options as Node's
/// createSecureContext does -- the errors it throws there, a key that is not
/// its certificate's included -- and give each key the chain it is sent
/// with: its `cert` entry's certificates, or, for a certificate given alone,
/// the chain OpenSSL builds from the context's store (`chain.rs`).
pub(crate) fn load_identities(
    spec: &ServerContextSpec,
    provider: &Arc<rustls::crypto::CryptoProvider>,
) -> Result<Identities, ContextError> {
    let mut chains = spec
        .certs
        .iter()
        .map(|pem| parse_cert_chain(pem))
        .collect::<Result<Vec<_>, _>>()?;
    let mut keys: Vec<PrivateKeyDer<'static>> = Vec::new();
    for key in &spec.keys {
        let passphrase = key.passphrase.as_deref().or(spec.passphrase.as_deref());
        keys.push(super::keys::load_private_key(
            key.pem.as_bytes(),
            passphrase,
        )?);
    }

    // A pfx carries its own key, its certificate and any CA certificates:
    // its chain is the certificate that is the key's, then the others, and
    // (Node's LoadPKCS12) those others are trusted CAs of the context too.
    let mut pfx_cas: Vec<CertificateDer<'static>> = Vec::new();
    for pfx in &spec.pfx {
        let passphrase = pfx.passphrase.as_deref().or(spec.passphrase.as_deref());
        let der = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(pfx.buf.as_bytes())
                .map_err(|_| ContextError::plain("not enough data", None))?
        };
        let bundle = super::keys::load_pkcs12(&der, passphrase.unwrap_or(""))?;
        let Some(key) = bundle.key else {
            pfx_cas.extend(bundle.certs);
            continue;
        };
        let signing_key = provider
            .key_provider
            .load_private_key(key.clone_key())
            .map_err(|_| ContextError::unsupported_key())?;
        let leaf = bundle.certs.iter().position(|cert| {
            CertifiedKey::new(vec![cert.clone()], Arc::clone(&signing_key))
                .keys_match()
                .is_ok()
        });
        match leaf {
            Some(index) => {
                let mut certs = bundle.certs;
                let leaf = certs.remove(index);
                pfx_cas.extend(certs.iter().cloned());
                let mut chain = vec![leaf];
                chain.extend(certs);
                chains.push(chain);
            }
            None => pfx_cas.extend(bundle.certs),
        }
        keys.push(key);
    }

    // A certificate given without its chain goes out with the chain OpenSSL
    // builds for it from the context's store: the `ca` certificates (or the
    // bundled roots and NODE_EXTRA_CA_CERTS without one), and a pfx's CAs.
    if chains.iter().any(|chain| chain.len() == 1) {
        let ca = spec.ca.as_deref().map(ca_certificates);
        let mut store: Vec<&CertificateDer<'static>> = match &ca {
            Some(ca) => ca.iter().collect(),
            None => super::chain::default_store().iter().collect(),
        };
        store.extend(pfx_cas.iter());
        let now = super::chain::unix_now();
        for chain in chains.iter_mut().filter(|chain| chain.len() == 1) {
            *chain = super::chain::complete_chain(&chain[0], &store, now);
        }
    }

    // Each key serves the chain whose leaf it matches; a key that matches no
    // certificate is Node's mismatch error.
    let mut certified: Vec<Arc<CertifiedKey>> = Vec::new();
    for key in keys {
        let signing_key = provider
            .key_provider
            .load_private_key(key)
            .map_err(|_| ContextError::unsupported_key())?;
        if chains.is_empty() {
            // A key with no certificate: nothing to serve (Node fails each
            // handshake), but nothing to compare it with either.
            continue;
        }
        let matched = chains.iter().find_map(|chain| {
            let candidate = CertifiedKey::new(chain.clone(), Arc::clone(&signing_key));
            candidate.keys_match().is_ok().then_some(candidate)
        });
        match matched {
            Some(identity) => certified.push(Arc::new(identity)),
            None => return Err(ContextError::key_mismatch()),
        }
    }
    Ok(Identities { certified, pfx_cas })
}

/// The crypto provider every context is built with: ring, its suites in
/// Node's order (`super::node_crypto_provider`), which is what a server
/// honouring its own order picks by.
pub(crate) fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    super::node_crypto_provider()
}

/// Build a server's context, or the error Node throws at `createServer()`.
pub fn build_server_context(spec: &ServerContextSpec) -> Result<ServerContext, ContextError> {
    let provider = provider();
    let Identities {
        certified: identities,
        pfx_cas,
    } = load_identities(spec, &provider)?;
    let has_identity = !identities.is_empty();
    let store = spec.ca.as_deref().map(|ca| {
        let mut store = ca_certificates(ca);
        store.extend(pfx_cas.iter().cloned());
        store
    });
    let judge = Arc::new(ClientCertJudge::new(spec.ca.as_deref(), &pfx_cas));
    let resolver: Arc<dyn rustls::server::ResolvesServerCert> =
        Arc::new(ServedIdentities(identities));

    let versions = protocol_versions(
        Some(spec.min_version.as_str()).filter(|v| !v.is_empty()),
        Some(spec.max_version.as_str()).filter(|v| !v.is_empty()),
    )
    .unwrap_or_default();
    let plain = if versions.is_empty() {
        None
    } else {
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(&versions)
            .map_err(|e| ContextError::plain(&format!("tls server config: {e}"), None))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::clone(&resolver));
        // Node's server picks by its own list unless honorCipherOrder is
        // false (measured: a client offering AES-256 before AES-128 under
        // TLS 1.2 gets AES-128 from a default server, AES-256 from one
        // created with `honorCipherOrder: false`); rustls's default is the
        // client's order.
        config.ignore_client_order = spec.honor_cipher_order;
        Some(Arc::new(config))
    };
    Ok(ServerContext {
        plain,
        versions,
        provider,
        honor_cipher_order: spec.honor_cipher_order,
        resolver,
        judge,
        has_identity,
        store,
    })
}

/// A `cert` entry: every certificate in it, leaf first. Node throws
/// `ERR_OSSL_PEM_NO_START_LINE` for one with none.
fn parse_cert_chain(pem: &str) -> Result<Vec<CertificateDer<'static>>, ContextError> {
    let certs = rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ContextError::no_start_line())?;
    if certs.is_empty() {
        return Err(ContextError::no_start_line());
    }
    Ok(certs)
}

/// The server's certificates: the first whose key can sign with a scheme the
/// client offers (OpenSSL picks among a context's certificates the same way).
#[derive(Debug)]
struct ServedIdentities(Vec<Arc<CertifiedKey>>);

impl rustls::server::ResolvesServerCert for ServedIdentities {
    fn resolve(&self, hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let schemes = hello.signature_schemes();
        self.0
            .iter()
            .find(|identity| identity.key.choose_scheme(schemes).is_some())
            .or_else(|| self.0.first())
            .cloned()
    }
}

// ------------------------------------------------------ client certificates

/// Where a connection's verifier leaves Node's verdict on the client's
/// certificate, and what that verdict means for the rest of the handshake.
#[derive(Debug, Default)]
pub(crate) struct VerdictSlot {
    /// Unset until the client sent a certificate and it was judged.
    verdict: std::sync::Mutex<Option<Option<VerifyFailure>>>,
    /// `rejectUnauthorized`: a refused verdict means this connection sends
    /// nothing more (`ServerIo`).
    reject: bool,
    /// A TLS 1.2 handshake that requires a certificate. rustls refuses a
    /// client that sends none with `certificate_required`, an alert TLS 1.2
    /// does not have; OpenSSL sends `handshake_failure`. So rustls is told
    /// the certificate is optional and the refusal is made here, in its
    /// place: the flight that would finish the handshake is replaced by
    /// OpenSSL's alert.
    require_tls12: std::sync::atomic::AtomicBool,
    /// The client's Certificate message has been read (rustls consults
    /// `client_auth_mandatory` exactly then).
    certificate_read: std::sync::atomic::AtomicBool,
}

/// What `ServerIo` does with the server's next write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Open,
    /// A certificate refused under `rejectUnauthorized`: nothing more is
    /// written.
    Silent,
    /// No certificate on a TLS 1.2 handshake that requires one: OpenSSL's
    /// `handshake_failure` alert instead of the server's Finished.
    NoCertificate,
}

impl VerdictSlot {
    fn record(&self, verdict: Option<VerifyFailure>) {
        *self.verdict.lock().unwrap_or_else(|e| e.into_inner()) = Some(verdict);
    }

    /// The verdict, once a certificate was judged.
    fn get(&self) -> Option<Option<VerifyFailure>> {
        self.verdict
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn gate(&self) -> Gate {
        match self.get() {
            Some(Some(_)) if self.reject => Gate::Silent,
            None if self.require_tls12.load(Ordering::Acquire)
                && self.certificate_read.load(Ordering::Acquire) =>
            {
                Gate::NoCertificate
            }
            _ => Gate::Open,
        }
    }
}

/// The handshake half of `requestCert`, one per connection: ask for a
/// certificate, name the CAs the server trusts (Node adds each `ca`
/// certificate to the client CA list), require one only under
/// `rejectUnauthorized`, and judge the chain the client sends -- recording
/// the verdict instead of failing the handshake with it, as OpenSSL's
/// always-accepting verify callback does. The client still has to prove it
/// holds its certificate's key: the handshake signature is verified here.
#[derive(Debug)]
struct RequestClientCert {
    supported: rustls::crypto::WebPkiSupportedAlgorithms,
    judge: Arc<ClientCertJudge>,
    mandatory: bool,
    verdict: Arc<VerdictSlot>,
}

impl ClientCertVerifier for RequestClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // Consulted when the client's Certificate message arrives.
        self.verdict.certificate_read.store(true, Ordering::Release);
        self.mandatory && !self.verdict.require_tls12.load(Ordering::Acquire)
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &self.judge.hints
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let chain: Vec<CertificateDer<'_>> = std::iter::once(end_entity.clone())
            .chain(intermediates.iter().cloned())
            .collect();
        self.verdict.record(self.judge.verdict(Some(&chain), now));
        Ok(ClientCertVerified::assertion())
    }

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

/// Node's verdict on a client's certificate (`verifyError()` after the
/// handshake), from the server's trust: the `ca` option alone when given,
/// else the bundled roots plus NODE_EXTRA_CA_CERTS -- the same store rules
/// the client side uses (see `build_client_config`).
#[derive(Debug)]
struct ClientCertJudge {
    inner: Option<Arc<dyn ClientCertVerifier>>,
    trusted_leaves: Vec<CertificateDer<'static>>,
    trusted_non_anchors: Vec<CertificateDer<'static>>,
    hints: Vec<rustls::DistinguishedName>,
}

impl ClientCertJudge {
    /// `ca` (None: the default store), plus any CA certificates a pfx
    /// brought.
    fn new(ca: Option<&[String]>, pfx_cas: &[CertificateDer<'static>]) -> Self {
        let mut root_store = rustls::RootCertStore::empty();
        let mut hints = Vec::new();
        let mut trusted_leaves: Vec<CertificateDer<'static>> = match ca {
            Some(ca) => {
                let certs = ca_certificates(ca);
                for cert in &certs {
                    if let Ok((_, parsed)) = parse_x509_certificate(cert.as_ref()) {
                        hints.push(rustls::DistinguishedName::from(
                            parsed.subject().as_raw().to_vec(),
                        ));
                    }
                    if is_self_signed(cert.as_ref()) {
                        let _ = root_store.add(cert.clone());
                    }
                }
                certs
            }
            None => {
                root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let extras = extra_ca_certs();
                for cert in extras.certs.iter().filter(|c| is_self_signed(c.as_ref())) {
                    let _ = root_store.add(cert.clone());
                }
                extras.certs.clone()
            }
        };
        for cert in pfx_cas {
            if let Ok((_, parsed)) = parse_x509_certificate(cert.as_ref()) {
                hints.push(rustls::DistinguishedName::from(
                    parsed.subject().as_raw().to_vec(),
                ));
            }
            if is_self_signed(cert.as_ref()) {
                let _ = root_store.add(cert.clone());
            }
            trusted_leaves.push(cert.clone());
        }
        let trusted_non_anchors = trusted_leaves
            .iter()
            .filter(|c| !is_self_signed(c.as_ref()))
            .cloned()
            .collect();
        let inner = if root_store.is_empty() {
            None
        } else {
            // Built on the same provider as every config here, not the
            // process-wide default (which `oam install`'s own client may
            // have installed first).
            rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(root_store),
                provider(),
            )
            .build()
            .ok()
        };
        ClientCertJudge {
            inner,
            trusted_leaves,
            trusted_non_anchors,
            hints,
        }
    }

    /// None when the client's certificate verifies; else Node's code for
    /// what is wrong with it. A client that sent none is
    /// `UNABLE_TO_GET_ISSUER_CERT` (Node's VerifyError default, measured).
    fn verdict(
        &self,
        chain: Option<&[CertificateDer<'_>]>,
        now: UnixTime,
    ) -> Option<VerifyFailure> {
        let Some((end_entity, intermediates)) = chain.and_then(|c| c.split_first()) else {
            return Some(VerifyFailure::named(
                "UNABLE_TO_GET_ISSUER_CERT",
                "unable to get issuer certificate",
            ));
        };
        // OpenSSL's date check runs last and so wins (as on the client side).
        if let Ok((_, leaf)) = parse_x509_certificate(end_entity.as_ref())
            && let Ok(at) = ASN1Time::from_timestamp(now.as_secs() as i64)
        {
            let validity = leaf.validity();
            if at.timestamp() > validity.not_after.timestamp() {
                return Some(VerifyFailure::named(
                    "CERT_HAS_EXPIRED",
                    "certificate has expired",
                ));
            }
            if at.timestamp() < validity.not_before.timestamp() {
                return Some(VerifyFailure::named(
                    "CERT_NOT_YET_VALID",
                    "certificate is not yet valid",
                ));
            }
        }
        if self
            .trusted_leaves
            .iter()
            .any(|trusted| trusted.as_ref() == end_entity.as_ref())
        {
            return None;
        }
        let known: Vec<CertificateDer<'_>> = intermediates
            .iter()
            .cloned()
            .chain(self.trusted_non_anchors.iter().cloned())
            .collect();
        let classify = || classify_unknown_issuer(end_entity, &known, &self.trusted_non_anchors);
        let Some(inner) = &self.inner else {
            return Some(classify());
        };
        let error = match inner.verify_client_cert(end_entity, &known, now) {
            Ok(_) => return None,
            Err(error) => error,
        };
        Some(match &error {
            rustls::Error::InvalidCertificate(reason) => match reason {
                CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                    VerifyFailure::named("CERT_HAS_EXPIRED", "certificate has expired")
                }
                CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                    VerifyFailure::named("CERT_NOT_YET_VALID", "certificate is not yet valid")
                }
                CertificateError::UnknownIssuer => classify(),
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
                _ => VerifyFailure::named("CERT_REJECTED", "certificate rejected"),
            },
            _ => VerifyFailure::named("CERT_REJECTED", "certificate rejected"),
        })
    }
}

// ---------------------------------------------------------------- handshake

/// Node's code for a server handshake that failed, from the rustls error
/// behind it; the message stays rustls's own (Node's is an OpenSSL blob that
/// names its build path), as on the client side.
fn handshake_failure(error: &std::io::Error, has_identity: bool) -> OpOutcome {
    let rustls_error = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>());
    let code = match rustls_error {
        Some(rustls::Error::NoApplicationProtocol) => Some("ERR_SSL_NO_APPLICATION_PROTOCOL"),
        Some(rustls::Error::NoCertificatesPresented) => {
            Some("ERR_SSL_PEER_DID_NOT_RETURN_A_CERTIFICATE")
        }
        Some(rustls::Error::AlertReceived(alert)) => alert_code(*alert),
        Some(rustls::Error::InvalidMessage(_)) => Some("ERR_SSL_WRONG_VERSION_NUMBER"),
        Some(rustls::Error::PeerIncompatible(_)) if !has_identity => {
            Some("ERR_SSL_NO_SUITABLE_SIGNATURE_ALGORITHM")
        }
        Some(rustls::Error::General(_)) if !has_identity => {
            Some("ERR_SSL_NO_SUITABLE_SIGNATURE_ALGORITHM")
        }
        _ => None,
    };
    if let Some(code) = code {
        return OpOutcome::node_failed(code, error.to_string());
    }
    if rustls_error.is_none()
        && matches!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        )
    {
        return socket_hang_up();
    }
    super::tls_fail(
        std::io::Error::new(error.kind(), error.to_string()),
        "accept",
        "",
    )
}

/// Node's `ConnResetException('socket hang up')`: a client that went away
/// before the handshake finished.
fn socket_hang_up() -> OpOutcome {
    OpOutcome::node_failed("ECONNRESET", "socket hang up")
}

/// OpenSSL's reason code for a fatal alert the peer sent, as Node names it
/// (`ERR_SSL_<reason>`): SSLv3-era alerts keep their `SSLV3_ALERT_` names,
/// TLS 1.0-era ones `TLSV1_ALERT_`, TLS 1.3 additions `TLSV13_ALERT_`.
pub(crate) fn alert_code(alert: rustls::AlertDescription) -> Option<&'static str> {
    use rustls::AlertDescription as A;
    Some(match alert {
        A::UnexpectedMessage => "ERR_SSL_SSLV3_ALERT_UNEXPECTED_MESSAGE",
        A::BadRecordMac => "ERR_SSL_SSLV3_ALERT_BAD_RECORD_MAC",
        A::DecompressionFailure => "ERR_SSL_SSLV3_ALERT_DECOMPRESSION_FAILURE",
        A::HandshakeFailure => "ERR_SSL_SSLV3_ALERT_HANDSHAKE_FAILURE",
        A::NoCertificate => "ERR_SSL_SSLV3_ALERT_NO_CERTIFICATE",
        A::BadCertificate => "ERR_SSL_SSLV3_ALERT_BAD_CERTIFICATE",
        A::UnsupportedCertificate => "ERR_SSL_SSLV3_ALERT_UNSUPPORTED_CERTIFICATE",
        A::CertificateRevoked => "ERR_SSL_SSLV3_ALERT_CERTIFICATE_REVOKED",
        A::CertificateExpired => "ERR_SSL_SSLV3_ALERT_CERTIFICATE_EXPIRED",
        A::CertificateUnknown => "ERR_SSL_SSLV3_ALERT_CERTIFICATE_UNKNOWN",
        A::IllegalParameter => "ERR_SSL_SSLV3_ALERT_ILLEGAL_PARAMETER",
        A::DecryptionFailed => "ERR_SSL_TLSV1_ALERT_DECRYPTION_FAILED",
        A::RecordOverflow => "ERR_SSL_TLSV1_ALERT_RECORD_OVERFLOW",
        A::UnknownCA => "ERR_SSL_TLSV1_ALERT_UNKNOWN_CA",
        A::AccessDenied => "ERR_SSL_TLSV1_ALERT_ACCESS_DENIED",
        A::DecodeError => "ERR_SSL_TLSV1_ALERT_DECODE_ERROR",
        A::DecryptError => "ERR_SSL_TLSV1_ALERT_DECRYPT_ERROR",
        A::ExportRestriction => "ERR_SSL_TLSV1_ALERT_EXPORT_RESTRICTION",
        A::ProtocolVersion => "ERR_SSL_TLSV1_ALERT_PROTOCOL_VERSION",
        A::InsufficientSecurity => "ERR_SSL_TLSV1_ALERT_INSUFFICIENT_SECURITY",
        A::InternalError => "ERR_SSL_TLSV1_ALERT_INTERNAL_ERROR",
        A::InappropriateFallback => "ERR_SSL_TLSV1_ALERT_INAPPROPRIATE_FALLBACK",
        A::UserCanceled => "ERR_SSL_TLSV1_ALERT_USER_CANCELLED",
        A::NoRenegotiation => "ERR_SSL_TLSV1_ALERT_NO_RENEGOTIATION",
        A::MissingExtension => "ERR_SSL_TLSV13_ALERT_MISSING_EXTENSION",
        A::UnsupportedExtension => "ERR_SSL_TLSV1_ALERT_UNSUPPORTED_EXTENSION",
        A::UnrecognisedName => "ERR_SSL_TLSV1_UNRECOGNIZED_NAME",
        A::BadCertificateStatusResponse => "ERR_SSL_TLSV1_BAD_CERTIFICATE_STATUS_RESPONSE",
        A::UnknownPSKIdentity => "ERR_SSL_TLSV1_ALERT_UNKNOWN_PSK_IDENTITY",
        A::CertificateRequired => "ERR_SSL_TLSV13_ALERT_CERTIFICATE_REQUIRED",
        A::NoApplicationProtocol => "ERR_SSL_TLSV1_ALERT_NO_APPLICATION_PROTOCOL",
        _ => return None,
    })
}

/// The largest record OpenSSL's server reads (its default read buffer less
/// the header); a first record declaring more is refused before anything
/// else is looked at (measured: 16711 passes, 16712 does not).
const OPENSSL_MAX_RECORD: usize = 16711;

/// What OpenSSL's server makes of a connection's first five bytes, before
/// any of it is parsed as TLS (measured on v22.22.2): None when it may be
/// TLS; else the error it fails with, and whether it answers with a
/// `record_overflow` alert first. A record that declares more than the
/// server reads is refused with the alert (a cleartext `POST ` or `HEAD `
/// request reads as one); a record whose version's major byte is not 3 is
/// "not TLS" and gets no answer -- named an HTTP request when it starts like
/// one. An SSLv2-style hello is left to the handshake.
pub(crate) fn first_record_refusal(first: &[u8]) -> Option<(&'static str, bool)> {
    if first.len() < 5 {
        return None;
    }
    let sslv2_hello = first[0] & 0x80 != 0 && first[2] == 1;
    if sslv2_hello {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([first[3], first[4]]));
    if length > OPENSSL_MAX_RECORD {
        return Some(("ERR_SSL_PACKET_LENGTH_TOO_LONG", true));
    }
    if first[1] == 3 {
        return None;
    }
    let starts = |prefix: &[u8]| first.starts_with(prefix);
    Some((
        if starts(b"GET ") || starts(b"POST ") || starts(b"HEAD ") || starts(b"PUT ") {
            "ERR_SSL_HTTP_REQUEST"
        } else if starts(b"CONNE") {
            "ERR_SSL_HTTPS_PROXY_REQUEST"
        } else {
            "ERR_SSL_WRONG_VERSION_NUMBER"
        },
        false,
    ))
}

/// OpenSSL's message for each of those.
fn first_record_message(code: &str) -> &'static str {
    match code {
        "ERR_SSL_HTTP_REQUEST" => "http request",
        "ERR_SSL_HTTPS_PROXY_REQUEST" => "https proxy request",
        "ERR_SSL_PACKET_LENGTH_TOO_LONG" => "packet length too long",
        _ => "wrong version number",
    }
}

/// Whether a ClientHello's ALPN offer leaves the server something to
/// negotiate: always, unless the server has protocols, the client offered
/// some, and none of them is the server's -- the one case OpenSSL refuses
/// (`no_application_protocol`) rather than negotiating nothing.
fn alpn_agrees<'a>(ours: &[Vec<u8>], theirs: Option<impl Iterator<Item = &'a [u8]>>) -> bool {
    let Some(theirs) = theirs else {
        return true;
    };
    let theirs: Vec<&[u8]> = theirs.collect();
    ours.is_empty()
        || theirs.is_empty()
        || ours.iter().any(|name| theirs.contains(&name.as_slice()))
}

/// The `no_application_protocol` fatal alert, in the clear: alert (21), TLS
/// 1.2 record version, length 2, fatal (2), no_application_protocol (120).
const NO_APPLICATION_PROTOCOL_ALERT: [u8; 7] = [0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x78];

/// Answer with one alert and close (gracefully, so it is read).
async fn close_with_alert(mut stream: tokio::net::TcpStream, alert: [u8; 7]) {
    let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, &alert).await;
    close_after_alert(stream).await;
}

/// The `record_overflow` fatal alert, one record: alert (21), TLS 1.2
/// record version, length 2, fatal (2), record_overflow (22).
const RECORD_OVERFLOW_ALERT: [u8; 7] = [0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x16];

/// What came before the handshake: at least the five bytes of a record
/// header, fewer only at EOF. Read with peek, so the handshake still sees
/// them.
async fn peek_record_header(stream: &tokio::net::TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = [0u8; 5];
    loop {
        let n = stream.peek(&mut buf).await?;
        if n == 0 || n >= buf.len() {
            return Ok(buf[..n].to_vec());
        }
        // A partial header: peek returns at once while those bytes are
        // unread, so wait for the socket to have more (or a short beat when
        // the readiness does not change) instead of spinning.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Close a connection after a failed handshake: the alert rustls wrote is
/// out, so half-close, and wait (bounded) for the client to read it and
/// close its side before the socket goes -- closing with the client's bytes
/// unread would reset the connection, and a reset can overtake the alert.
async fn close_after_alert(mut stream: tokio::net::TcpStream) {
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
    let mut sink = [0u8; 4096];
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
    })
    .await;
}

/// Close a connection whose first record was refused the way OpenSSL's
/// server does: read what the client sent (so the close is a FIN, not a
/// reset that could throw the client's own buffered data away), send the
/// alert if there is one and nothing else, close.
async fn close_refused(mut stream: tokio::net::TcpStream, alert: bool) {
    let mut sink = [0u8; 4096];
    loop {
        match stream.try_read(&mut sink) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    if alert {
        let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, &RECORD_OVERFLOW_ALERT).await;
    }
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
    // Give the client a moment to read the FIN and close its side before
    // this side's close; bounded for a client that never does.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
    })
    .await;
}

/// The TCP stream under an accepted TLS connection. It carries the
/// connection's client-certificate verdict: once a certificate is refused
/// under `rejectUnauthorized`, nothing more is written -- the flight rustls
/// queued after judging it (a TLS 1.2 server's Finished, a TLS 1.3 server's
/// session tickets) is dropped with the connection, as Node's server
/// destroys the socket before writing it.
pub struct ServerIo {
    tcp: tokio::net::TcpStream,
    verdict: Option<Arc<VerdictSlot>>,
    /// How much of the `handshake_failure` alert is out (`Gate::NoCertificate`).
    alert_written: usize,
}

/// The `handshake_failure` fatal alert, in the clear (the server has not
/// switched to its keys): alert (21), TLS 1.2, length 2, fatal (2),
/// handshake_failure (40).
const HANDSHAKE_FAILURE_ALERT: [u8; 7] = [0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28];

impl ServerIo {
    pub fn tcp(&self) -> &tokio::net::TcpStream {
        &self.tcp
    }

    fn gate(&self) -> Gate {
        self.verdict.as_ref().map_or(Gate::Open, |v| v.gate())
    }
}

fn gate_closed() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        "client certificate refused",
    )
}

impl tokio::io::AsyncRead for ServerIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.tcp).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for ServerIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.gate() {
            Gate::Open => std::pin::Pin::new(&mut self.tcp).poll_write(cx, buf),
            // Swallowed: the handshake completes on rustls's side without
            // its last flight reaching the client, and the connection is
            // then dropped (`close_refused_connection`).
            Gate::Silent => std::task::Poll::Ready(Ok(buf.len())),
            Gate::NoCertificate => {
                while self.alert_written < HANDSHAKE_FAILURE_ALERT.len() {
                    let from = self.alert_written;
                    let this = &mut *self;
                    match std::pin::Pin::new(&mut this.tcp)
                        .poll_write(cx, &HANDSHAKE_FAILURE_ALERT[from..])
                    {
                        std::task::Poll::Ready(Ok(n)) if n > 0 => this.alert_written += n,
                        std::task::Poll::Pending => return std::task::Poll::Pending,
                        _ => break,
                    }
                }
                std::task::Poll::Ready(Err(gate_closed()))
            }
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.tcp).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.tcp).poll_shutdown(cx)
    }
}

/// Drop a connection whose client certificate was refused, as Node's
/// server drops it (`socket.destroy()` straight from the handshake). Node
/// has read everything the client had sent by then (its TLS layer takes
/// all the socket holds), so what the client sent right behind its
/// handshake -- a request, an HTTP/2 preface -- is no reason for a reset:
/// the connection ends (measured on v22.22.2: a TLS 1.3 client that writes
/// a request on 'secureConnect' sees 'end', no error). Only bytes still
/// unread in the socket make the close a reset, as closing over unread
/// data does in Node.
fn close_refused_connection(stream: tokio_rustls::server::TlsStream<ServerIo>) {
    let (io, _) = stream.get_ref();
    let mut probe = [0u8; 1];
    let pending = matches!(io.tcp.try_read(&mut probe), Ok(n) if n > 0);
    if pending {
        let _ = socket2::SockRef::from(&io.tcp).set_linger(Some(std::time::Duration::ZERO));
    }
    drop(stream);
}

/// What a server reads off itself for each connection it accepts (Node's
/// `tlsConnectionListener`).
#[derive(Clone, Debug, Default)]
pub struct AcceptOptions {
    /// `server.requestCert`.
    pub request_cert: bool,
    /// `server.rejectUnauthorized`.
    pub reject_unauthorized: bool,
    /// `server.ALPNProtocols` as names, in the server's order of preference.
    pub alpn: Vec<Vec<u8>>,
    /// Node's `handshakeTimeout`.
    pub handshake_timeout: Duration,
}

/// What a server handshake settled, as Node's server-side TLSSocket reports
/// it: the version and cipher, the negotiated ALPN protocol and SNI name,
/// and -- when the server asked for a client certificate -- Node's verdict
/// on it and the chain the client sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandshakeInfo {
    pub protocol: String,
    pub cipher: String,
    pub cipher_standard_name: String,
    /// `alpnProtocol` (None: nothing negotiated, Node's `false`).
    pub alpn: Option<String>,
    /// `servername` (None: the client sent no SNI, Node's `false`).
    pub servername: Option<String>,
    /// `authorized`: only ever true when a certificate was requested.
    pub authorized: bool,
    /// `authorizationError`: Node's code for a requested certificate that
    /// did not verify (or was not sent).
    pub authorization_error: Option<&'static str>,
    /// The client's chain, leaf first, base64 DER (what
    /// `getPeerCertificate()` is built from).
    pub peer_certificates: Option<Vec<String>>,
    /// The issuers of that chain's last certificate that the server's store
    /// holds, base64 DER -- where Node's `getPeerCertificate(true)` goes on
    /// (`GetLastIssuedCert`).
    pub store_issuers: Option<Vec<String>>,
}

impl HandshakeInfo {
    /// The fields as the JS side reads them.
    pub fn to_json(&self) -> serde_json::Value {
        let mut info = serde_json::json!({
            "protocol": self.protocol,
            "cipher": self.cipher,
            "cipherStandardName": self.cipher_standard_name,
            "alpnProtocol": self.alpn,
            "servername": self.servername,
            "authorized": self.authorized,
            "authorizationError": self.authorization_error,
        });
        if let Some(chain) = &self.peer_certificates {
            info["peerCertificates"] = serde_json::Value::from(chain.clone());
        }
        if let Some(issuers) = &self.store_issuers {
            info["storeIssuers"] = serde_json::Value::from(issuers.clone());
        }
        info
    }
}

/// Run a server handshake on an accepted TCP connection with the server's
/// context: bounded by `handshake_timeout` (Node's `handshakeTimeout`,
/// `ERR_TLS_HANDSHAKE_TIMEOUT` when it runs out), then Node's verdict on a
/// requested client certificate. A failed handshake is Node's error for it
/// (what a server's 'tlsClientError' carries); the connection has then been
/// answered and is being closed the way OpenSSL's server closes it.
///
/// Shared by the node:tls server (`tls_accept`, which hands the connection
/// to JS) and the https server (`https_serve`, which serves it natively).
pub async fn accept_stream(
    tcp_stream: tokio::net::TcpStream,
    context: &ServerContext,
    options: &AcceptOptions,
) -> Result<(tokio_rustls::server::TlsStream<ServerIo>, HandshakeInfo), OpOutcome> {
    // A server range with nothing to offer fails this connection with Node's
    // per-connection code; the socket is answered with the alert the client
    // expects, off this task.
    let Some(plain) = &context.plain else {
        tokio::spawn(refuse_no_protocols(tcp_stream));
        return Err(OpOutcome::node_failed(
            "ERR_SSL_NO_PROTOCOLS_AVAILABLE",
            "no protocols available for the requested TLS version range".to_string(),
        ));
    };
    let verdict = Arc::new(VerdictSlot {
        reject: options.request_cert && options.reject_unauthorized,
        ..VerdictSlot::default()
    });

    let handshake = async {
        match peek_record_header(&tcp_stream).await {
            Ok(first) if first.is_empty() => return Err(socket_hang_up()),
            Ok(first) => {
                if let Some((code, alert)) = first_record_refusal(&first) {
                    tokio::spawn(close_refused(tcp_stream, alert));
                    return Err(OpOutcome::node_failed(code, first_record_message(code)));
                }
            }
            Err(_) => return Err(socket_hang_up()),
        }
        let io = ServerIo {
            tcp: tcp_stream,
            verdict: options.request_cert.then(|| Arc::clone(&verdict)),
            alert_written: 0,
        };
        // The ClientHello first: OpenSSL settles ALPN before its first
        // flight, and the version the handshake will run at decides how a
        // missing client certificate is refused.
        let mut lazy =
            tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), io);
        let start = match (&mut lazy).await {
            Ok(start) => start,
            Err(e) => {
                if let Some(io) = lazy.take_io() {
                    tokio::spawn(close_after_alert(io.tcp));
                }
                return Err(handshake_failure(&e, context.has_identity));
            }
        };
        let hello = start.client_hello();
        if !alpn_agrees(&options.alpn, hello.alpn()) {
            // OpenSSL answers the ClientHello with this alert alone, in the
            // clear.
            tokio::spawn(close_with_alert(
                start.io.tcp,
                NO_APPLICATION_PROTOCOL_ALERT,
            ));
            return Err(OpOutcome::node_failed(
                "ERR_SSL_NO_APPLICATION_PROTOCOL",
                "no application protocol",
            ));
        }
        // A client that can do TLS 1.3 offers its suites (0x1301-0x1305).
        let tls13 = context.versions.contains(&&rustls::version::TLS13)
            && hello
                .cipher_suites()
                .iter()
                .any(|suite| (0x1301..=0x1305).contains(&u16::from(*suite)));
        let config = context.config_for(plain, options, &verdict, !tls13);
        match start.into_stream(config).into_fallible().await {
            Ok(stream) => Ok(stream),
            Err((e, io)) => match verdict.gate() {
                // Refused under rejectUnauthorized: dropped unanswered, and
                // its close is the 'socket hang up' Node reports.
                Gate::Silent => Err(socket_hang_up()),
                Gate::NoCertificate => {
                    tokio::spawn(close_after_alert(io.tcp));
                    Err(OpOutcome::node_failed(
                        "ERR_SSL_PEER_DID_NOT_RETURN_A_CERTIFICATE",
                        "peer did not return a certificate",
                    ))
                }
                Gate::Open => {
                    // The alert rustls wrote has to reach the client: close
                    // with a FIN after it, not a reset that could discard it.
                    tokio::spawn(close_after_alert(io.tcp));
                    Err(handshake_failure(&e, context.has_identity))
                }
            },
        }
    };
    let tls_stream = match tokio::time::timeout(options.handshake_timeout, handshake).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(failed)) => return Err(failed),
        Err(_) => {
            return Err(OpOutcome::node_failed(
                "ERR_TLS_HANDSHAKE_TIMEOUT",
                "TLS handshake timeout",
            ));
        }
    };
    // A refusal whose queued flight was already out (nothing left to write
    // after the verdict): the connection still goes no further.
    if verdict.gate() != Gate::Open {
        close_refused_connection(tls_stream);
        return Err(socket_hang_up());
    }

    let (_, server_conn) = tls_stream.get_ref();
    let protocol = server_conn
        .protocol_version()
        .map(protocol_name)
        .unwrap_or_default();
    let (cipher, cipher_standard_name) = server_conn
        .negotiated_cipher_suite()
        .map(|c| cipher_names(c.suite()))
        .unwrap_or_default();
    let peer_chain = server_conn.peer_certificates();
    let peer_certificates = peer_certificates_b64(peer_chain);
    let store_issuers = peer_chain.and_then(|chain| {
        let issuers =
            super::chain::store_issuers(chain, &context.store(), super::chain::unix_now());
        peer_certificates_b64(Some(&issuers))
    });
    let alpn = server_conn
        .alpn_protocol()
        .map(|p| p.iter().map(|&b| char::from(b)).collect::<String>());
    let servername = server_conn.server_name().map(str::to_string);
    // The verdict: the verifier's on the chain the client sent in this
    // handshake; else (a resumed session, whose chain comes from the session
    // it resumes, or no certificate at all) Node's on the chain the
    // connection has.
    let (authorized, authorization_error) = if options.request_cert {
        let judged = verdict
            .get()
            .unwrap_or_else(|| context.judge.verdict(peer_chain, UnixTime::now()));
        match judged {
            None => (true, None),
            Some(failure) => (false, failure.code),
        }
    } else {
        (false, None)
    };
    Ok((
        tls_stream,
        HandshakeInfo {
            protocol,
            cipher,
            cipher_standard_name,
            alpn,
            servername,
            authorized,
            authorization_error,
            peer_certificates,
            store_issuers,
        },
    ))
}

/// Accept one TLS connection on an accepted TCP socket of a node:tls server
/// (`accept_stream`) and register it for JS. Resolves Json {handle,
/// protocol, cipher, cipherStandardName, alpnProtocol, servername,
/// authorized, authorizationError, peerCertificates?, localAddr?,
/// remoteAddr?}; a failed handshake rejects with Node's code for it.
pub async fn tls_accept(
    tls_registry: TlsRegistry,
    tcp_registry: crate::tcp::TcpRegistry,
    ids: Arc<std::sync::atomic::AtomicU64>,
    tcp_handle: u64,
    context: Arc<ServerContext>,
    options: AcceptOptions,
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
    let (tls_stream, info) = match accept_stream(tcp_stream, &context, &options).await {
        Ok(accepted) => accepted,
        Err(failed) => return failed,
    };
    let (io, _) = tls_stream.get_ref();
    let local_addr = io.tcp.local_addr().ok();
    let remote_addr = io.tcp.peer_addr().ok();

    let handle = ids.fetch_add(1, Ordering::Relaxed);
    let (reader, writer) = tokio::io::split(tls_stream);
    {
        let mut guard = tls_registry.lock().unwrap_or_else(|e| e.into_inner());
        guard.readers.insert(handle, TlsReader::Server(reader));
        guard.writers.insert(handle, TlsWriter::Server(writer));
    }

    let mut payload = info.to_json();
    payload["handle"] = serde_json::Value::from(handle);
    if let Some(la) = local_addr {
        payload["localAddr"] = crate::tcp::addr_to_json(la);
    }
    if let Some(ra) = remote_addr {
        payload["remoteAddr"] = crate::tcp::addr_to_json(ra);
    }
    OpOutcome::Json(payload.to_string())
}

/// Take an accepted server connection out of the registry whole, for a
/// protocol served natively on it (the http2 secure server). Only a
/// connection at rest can be taken -- both halves in the registry, which is
/// the case until JS first reads or writes it; None otherwise, and the
/// registry is left as it was.
pub fn take_server_stream(
    registry: &TlsRegistry,
    handle: u64,
) -> Option<tokio_rustls::server::TlsStream<ServerIo>> {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if !guard.readers.contains_key(&handle) || !guard.writers.contains_key(&handle) {
        return None;
    }
    let reader = guard.readers.remove(&handle)?;
    let Some(writer) = guard.writers.remove(&handle) else {
        guard.readers.insert(handle, reader);
        return None;
    };
    match (reader, writer) {
        (TlsReader::Server(reader), TlsWriter::Server(writer)) => {
            guard.cancel.remove(&handle);
            Some(reader.unsplit(writer))
        }
        (reader, writer) => {
            guard.readers.insert(handle, reader);
            guard.writers.insert(handle, writer);
            None
        }
    }
}

/// Register a context; its id is what `tls_accept` is called with.
pub fn register_context(
    registry: &TlsRegistry,
    ids: &std::sync::atomic::AtomicU64,
    context: ServerContext,
) -> u64 {
    let id = ids.fetch_add(1, Ordering::Relaxed);
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contexts
        .insert(id, Arc::new(context));
    id
}

/// The context `id` names, if it is still registered.
pub fn context(registry: &TlsRegistry, id: u64) -> Option<Arc<ServerContext>> {
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contexts
        .get(&id)
        .cloned()
}

/// Forget a context (its server closed). Connections already accepted keep
/// the configuration they were accepted with.
pub fn free_context(registry: &TlsRegistry, id: u64) {
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contexts
        .remove(&id);
}

#[cfg(test)]
mod tests {
    use super::*;

    // conformance case 141's fixtures: a P-256 CA (2025-2125), the localhost
    // leaf and a client leaf it signed, and a self-signed client certificate.
    const CA: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBmjCCAUGgAwIBAgIUHjF3aO/Nr2SNMEQNV9GNuumIljswCgYIKoZIzj0EAwIw\n\
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx\n\
MjUwMTAxMDAwMDAwWjAaMRgwFgYDVQQDDA9vYW0gaDJzIHRlc3QgQ0EwWTATBgcq\n\
hkjOPQIBBggqhkjOPQMBBwNCAAR6EfahtynuI8VLuixWn6GiZ3BYWFdJEqP1FfLE\n\
lCBVF/69Rm6fDrzSVP/GWO7qsNhAZmyIVWyRQJcQiBv55omto2MwYTAdBgNVHQ4E\n\
FgQUOlIo6O4tIFNjD7vXJV51FU2DLQcwHwYDVR0jBBgwFoAUOlIo6O4tIFNjD7vX\n\
JV51FU2DLQcwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYwCgYIKoZI\n\
zj0EAwIDRwAwRAIgFFCfCAiuzT1cHBF7zAQEVxSrWsoco8cOD49S6whO4vsCIC/T\n\
xtSxdoSsByDfaJz7qxOrhJzSD5lDwUdNMe3EoP9l\n\
-----END CERTIFICATE-----\n";

    const CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBvjCCAWWgAwIBAgIUOy7BLDqzc+0IZz2NWG95hnXgrd4wCgYIKoZIzj0EAwIw\n\
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx\n\
MjUwMTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIB\n\
BggqhkjOPQMBBwNCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0bj0ngz5dpnyIRlajs\n\
UptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1Vo4GMMIGJMBoGA1UdEQQTMBGC\n\
CWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDATBgNVHSUE\n\
DDAKBggrBgEFBQcDATAdBgNVHQ4EFgQUmxnUU2rP4FwgoXrkCkeRxNgCVycwHwYD\n\
VR0jBBgwFoAUOlIo6O4tIFNjD7vXJV51FU2DLQcwCgYIKoZIzj0EAwIDRwAwRAIg\n\
ItB5f9aIsf9D8cXBvJvvr5ahB57RK7DgAsIVf5uJ0zcCIBPOR2Z+ycbeeByMKH2v\n\
shKfeR1QdaoQHwJKJln0q1fo\n\
-----END CERTIFICATE-----\n";

    const KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgQLidYpqFITu5wno8\n\
Fw5b5Ahrg5eTwH0UqA7RU57egNKhRANCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0b\n\
j0ngz5dpnyIRlajsUptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1V\n\
-----END PRIVATE KEY-----\n";

    const CLIENT_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBoTCCAUigAwIBAgIUOy7BLDqzc+0IZz2NWG95hnXgrd8wCgYIKoZIzj0EAwIw\n\
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx\n\
MjUwMTAxMDAwMDAwWjAVMRMwEQYDVQQDDApvYW0gY2xpZW50MFkwEwYHKoZIzj0C\n\
AQYIKoZIzj0DAQcDQgAEulhTChDco8oZzXpPqo3iqtybv/nUXKwS67GiGZ23ra4b\n\
5Ta8McX1MVv2p0WA1/JYyncszN9kbKwE1oeV0Q0lTKNvMG0wCQYDVR0TBAIwADAL\n\
BgNVHQ8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwIwHQYDVR0OBBYEFCk7s+uR\n\
ZEuahksP0Vn6QPqJ+TmIMB8GA1UdIwQYMBaAFDpSKOjuLSBTYw+71yVedRVNgy0H\n\
MAoGCCqGSM49BAMCA0cAMEQCIFOOnRBbxbAbIOcU15I7xnKlD5QXj7P2ZHQbxax0\n\
goFxAiBEgrUhNh9pzkHEQGCdqJNAtqjNUURu8GVWs9re4QYI7A==\n\
-----END CERTIFICATE-----\n";

    const CLIENT_KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgT9C7qt8YZkF23ize\n\
WR5qGRqTlTsUdSjwsf/UmBhdckihRANCAAS6WFMKENyjyhnNek+qjeKq3Ju/+dRc\n\
rBLrsaIZnbetrhvlNrwxxfUxW/anRYDX8ljKdyzM32RsrATWh5XRDSVM\n\
-----END PRIVATE KEY-----\n";

    const ROGUE_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBmTCCAUCgAwIBAgIUGlc1ru8HwNb+Q/Mu7dMCNPfAocswCgYIKoZIzj0EAwIw\n\
FzEVMBMGA1UEAwwMcm9ndWUgY2xpZW50MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUw\n\
MTAxMDAwMDAwWjAXMRUwEwYDVQQDDAxyb2d1ZSBjbGllbnQwWTATBgcqhkjOPQIB\n\
BggqhkjOPQMBBwNCAAQ9RwUp47J8lOK3t92HA7gbn6/in8YmMNmd1xdfCfmgKTMz\n\
lDyFz1VM0dwI6KtooNv0ubR8/KmhnJS0yFCugC2Lo2gwZjAdBgNVHQ4EFgQU/7mR\n\
Dyd3M2+3xDFCFAhtl1mUL9kwHwYDVR0jBBgwFoAU/7mRDyd3M2+3xDFCFAhtl1mU\n\
L9kwDwYDVR0TAQH/BAUwAwEB/zATBgNVHSUEDDAKBggrBgEFBQcDAjAKBggqhkjO\n\
PQQDAgNHADBEAiAccfsRWDhnWobD+9J8R2fydTxf4E/ePbkue9NfHCHqcQIgRVGb\n\
asDZ6pyGf669FP4nlBCxQetAmb5ZvRlSGk6/Cus=\n\
-----END CERTIFICATE-----\n";

    fn spec(certs: &[&str], keys: &[&str]) -> ServerContextSpec {
        ServerContextSpec {
            certs: certs.iter().map(|c| c.to_string()).collect(),
            keys: keys
                .iter()
                .map(|k| KeySpec {
                    pem: k.to_string(),
                    passphrase: None,
                })
                .collect(),
            ..ServerContextSpec::default()
        }
    }

    fn der(pem: &str) -> Vec<CertificateDer<'static>> {
        rustls_pemfile::certs(&mut BufReader::new(pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn code(verdict: Option<VerifyFailure>) -> Option<&'static str> {
        verdict.map(|failure| failure.code.unwrap_or("<none>"))
    }

    // What createServer throws, measured on v22.22.2.
    #[test]
    fn context_errors_are_nodes() {
        assert!(build_server_context(&spec(&[CERT], &[KEY])).is_ok());
        let error = |s: ServerContextSpec| build_server_context(&s).err().unwrap();
        assert_eq!(
            error(spec(&[CERT], &["not a key"])),
            ContextError::openssl(
                "1E08010C",
                "DECODER routines",
                "unsupported",
                "ERR_OSSL_UNSUPPORTED"
            )
        );
        assert_eq!(
            error(spec(&[CERT], &[CLIENT_KEY])).code,
            Some("ERR_OSSL_X509_KEY_VALUES_MISMATCH")
        );
        assert_eq!(
            error(spec(&["not a certificate"], &[KEY])).message,
            "error:0480006C:PEM routines::no start line"
        );
        // A key alone, or a certificate alone, is a server that fails each
        // handshake -- not an error at createServer().
        let lone_key = build_server_context(&spec(&[], &[KEY])).unwrap();
        assert!(!lone_key.has_identity);
        let lone_cert = build_server_context(&spec(&[CERT], &[])).unwrap();
        assert!(!lone_cert.has_identity);
    }

    // Node's verdicts on a client certificate, with and without `ca`.
    #[test]
    fn client_certificate_verdicts_are_nodes() {
        let now = UnixTime::now();
        let with_ca = ClientCertJudge::new(Some(&[CA.to_string()]), &[]);
        assert_eq!(
            code(with_ca.verdict(None, now)),
            Some("UNABLE_TO_GET_ISSUER_CERT")
        );
        assert_eq!(code(with_ca.verdict(Some(&der(CLIENT_CERT)), now)), None);
        assert_eq!(
            code(with_ca.verdict(Some(&der(ROGUE_CERT)), now)),
            Some("DEPTH_ZERO_SELF_SIGNED_CERT")
        );
        // The server's own leaf is for serverAuth only.
        assert_eq!(
            code(with_ca.verdict(Some(&der(CERT)), now)),
            Some("INVALID_PURPOSE")
        );
        // Trusted by name: a self-signed certificate that is itself in `ca`.
        let rogue_trusted = ClientCertJudge::new(Some(&[ROGUE_CERT.to_string()]), &[]);
        assert_eq!(
            code(rogue_trusted.verdict(Some(&der(ROGUE_CERT)), now)),
            None
        );
        // Without `ca`, the bundled roots: nobody signed the test CA.
        let default_store = ClientCertJudge::new(None, &[]);
        assert_eq!(
            code(default_store.verdict(Some(&der(CLIENT_CERT)), now)),
            Some("UNABLE_TO_VERIFY_LEAF_SIGNATURE")
        );
        // The hints are the ca subjects.
        assert_eq!(with_ca.hints.len(), 1);
        assert!(default_store.hints.is_empty());
    }

    // OpenSSL's reading of a first record, measured byte for byte.
    #[test]
    fn first_records_that_are_not_tls() {
        let refusal = |bytes: &[u8]| first_record_refusal(bytes);
        assert_eq!(
            refusal(
                b"GET / HTTP/1.1
"
            ),
            Some(("ERR_SSL_HTTP_REQUEST", false))
        );
        assert_eq!(
            refusal(
                b"PUT / HTTP/1.1
"
            ),
            Some(("ERR_SSL_HTTP_REQUEST", false))
        );
        // "POST " and "HEAD " declare records longer than OpenSSL reads.
        assert_eq!(
            refusal(
                b"POST / HTTP/1.1
"
            ),
            Some(("ERR_SSL_PACKET_LENGTH_TOO_LONG", true))
        );
        assert_eq!(
            refusal(
                b"HEAD / HTTP/1.1
"
            ),
            Some(("ERR_SSL_PACKET_LENGTH_TOO_LONG", true))
        );
        assert_eq!(
            refusal(
                b"CONNECT a:443 HTTP/1.1
"
            ),
            Some(("ERR_SSL_PACKET_LENGTH_TOO_LONG", true))
        );
        assert_eq!(
            refusal(
                b"PRI * HTTP/2.0

SM

"
            ),
            Some(("ERR_SSL_WRONG_VERSION_NUMBER", false))
        );
        // The length boundary: 16711 is read, 16712 is not.
        assert_eq!(
            refusal(&[0x50, 0x52, 0x49, 0x41, 0x47]),
            Some(("ERR_SSL_WRONG_VERSION_NUMBER", false))
        );
        assert_eq!(
            refusal(&[0x50, 0x52, 0x49, 0x41, 0x48]),
            Some(("ERR_SSL_PACKET_LENGTH_TOO_LONG", true))
        );
        assert_eq!(
            refusal(&[0x16, 0x03, 0x03, 0x41, 0x48]),
            Some(("ERR_SSL_PACKET_LENGTH_TOO_LONG", true))
        );
        // A TLS record header, and an SSLv2-style hello, go to the handshake.
        assert_eq!(refusal(&[0x16, 0x03, 0x01, 0x00, 0xd6]), None);
        assert_eq!(refusal(&[0x80, 0x2e, 0x01, 0x03, 0x01]), None);
        assert_eq!(refusal(b"GET"), None);
    }

    #[test]
    fn alpn_is_refused_only_when_both_sides_named_protocols_and_none_meet() {
        let ours = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let offer = |names: &'static [&'static [u8]]| Some(names.iter().copied());
        assert!(alpn_agrees(&ours, offer(&[b"http/1.1"])));
        assert!(!alpn_agrees(&ours, offer(&[b"foo"])));
        assert!(alpn_agrees(&ours, None::<std::vec::IntoIter<&[u8]>>));
        assert!(alpn_agrees(&ours, offer(&[])));
        assert!(alpn_agrees(&[], offer(&[b"foo"])));
    }

    #[test]
    fn alerts_have_openssls_names() {
        use rustls::AlertDescription as A;
        assert_eq!(
            alert_code(A::NoApplicationProtocol),
            Some("ERR_SSL_TLSV1_ALERT_NO_APPLICATION_PROTOCOL")
        );
        assert_eq!(
            alert_code(A::CertificateRequired),
            Some("ERR_SSL_TLSV13_ALERT_CERTIFICATE_REQUIRED")
        );
        assert_eq!(
            alert_code(A::BadCertificate),
            Some("ERR_SSL_SSLV3_ALERT_BAD_CERTIFICATE")
        );
        assert_eq!(
            alert_code(A::UnknownCA),
            Some("ERR_SSL_TLSV1_ALERT_UNKNOWN_CA")
        );
    }

    /// `honorCipherOrder` (Node's default) makes a server pick by its own
    /// list -- Node's, AES-128 before AES-256 under TLS 1.2 -- and turning it
    /// off makes it pick by the client's. Measured against Node's server with
    /// a client whose `ciphers` puts AES-256 first: AES-128 from a default
    /// server, AES-256 from one created with `honorCipherOrder: false`. The
    /// client here offers rustls's own order for the same effect, and the
    /// per-connection config a certificate request builds from scratch
    /// carries the same choice as the shared one.
    #[tokio::test]
    async fn honor_cipher_order_picks_by_the_servers_list_unless_turned_off() {
        use rustls::CipherSuite as C;
        use rustls::crypto::ring::cipher_suite as ring;

        #[derive(Debug)]
        struct Trusting;
        impl rustls::client::danger::ServerCertVerifier for Trusting {
            fn verify_server_cert(
                &self,
                _: &CertificateDer<'_>,
                _: &[CertificateDer<'_>],
                _: &rustls::pki_types::ServerName<'_>,
                _: &[u8],
                _: UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                _: &[u8],
                _: &CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, rustls::Error> {
                Ok(HandshakeSignatureValid::assertion())
            }
            fn verify_tls13_signature(
                &self,
                _: &[u8],
                _: &CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, rustls::Error> {
                Ok(HandshakeSignatureValid::assertion())
            }
            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                provider()
                    .signature_verification_algorithms
                    .supported_schemes()
            }
        }

        for (honor, expect_aes128) in [(true, true), (false, false)] {
            let context = build_server_context(&ServerContextSpec {
                honor_cipher_order: honor,
                max_version: "TLSv1.2".into(),
                ..spec(&[CERT], &[KEY])
            })
            .unwrap();
            let plain = Arc::clone(context.plain.as_ref().unwrap());
            assert_eq!(plain.ignore_client_order, honor);
            let per_connection = context.config_for(
                &plain,
                &AcceptOptions {
                    request_cert: true,
                    reject_unauthorized: false,
                    alpn: Vec::new(),
                    handshake_timeout: Duration::from_secs(5),
                },
                &Arc::new(VerdictSlot::default()),
                true,
            );
            assert_eq!(per_connection.ignore_client_order, honor);

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(plain);
            let served = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let stream = acceptor.accept(tcp).await.unwrap();
                stream
                    .get_ref()
                    .1
                    .negotiated_cipher_suite()
                    .unwrap()
                    .suite()
            });
            // AES-256 before AES-128 for either key type: rustls's own order,
            // the one a client's `ciphers` list reverses in the measurement.
            let reversed = rustls::crypto::CryptoProvider {
                cipher_suites: vec![
                    ring::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
                    ring::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
                    ring::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                    ring::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
                ],
                ..rustls::crypto::ring::default_provider()
            };
            let client = rustls::ClientConfig::builder_with_provider(Arc::new(reversed))
                .with_protocol_versions(&[&rustls::version::TLS12])
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Trusting))
                .with_no_client_auth();
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let stream = tokio_rustls::TlsConnector::from(Arc::new(client))
                .connect(
                    rustls::pki_types::ServerName::try_from("localhost").unwrap(),
                    tcp,
                )
                .await
                .unwrap();
            let negotiated = stream
                .get_ref()
                .1
                .negotiated_cipher_suite()
                .unwrap()
                .suite();
            let aes128 = matches!(
                negotiated,
                C::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
                    | C::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
            );
            let aes256 = matches!(
                negotiated,
                C::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
                    | C::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
            );
            assert!(aes128 || aes256, "negotiated {negotiated:?}");
            assert_eq!(
                aes128, expect_aes128,
                "honorCipherOrder {honor}: negotiated {negotiated:?}"
            );
            assert_eq!(served.await.unwrap(), negotiated);
            drop(stream);
        }
    }
}
