//! oam's own HTTP client transport for the `fetch` op (#143).
//!
//! The op used to hand everything to reqwest, whose defaults are reqwest's and
//! not Node's: ten redirects instead of twenty, a Referer on every hop, a
//! credential leak on the second same-host hop after a cross-origin one, only
//! an exact lowercase single `gzip`/`deflate` decoded, a truncated gzip body
//! turned into an error that Node returns as data. Node v22.22.2's `fetch` is
//! undici 6.24.1, so the rules here are undici's, measured against that build.
//!
//! The pure, transport-free halves -- nothing in these dials, awaits or
//! touches the runtime:
//!
//! - [`decode`]: the content-encoding plan and a bounded-output streaming
//!   decoder (gzip, deflate, br, stacked), undici fetch/index.js:2129-2172.
//! - [`redirect`]: the "follow" redirect step, undici fetch/index.js:1210-1318.
//! - [`prepare`]: URL, method and header preparation, keeping today's error
//!   texts so `http.request`'s mapping of them stays unchanged.
//!
//! The transport:
//!
//! - [`tls_config`]: the process-wide rustls client configs.
//! - `connector` (crate-private): node's connect algorithm
//!   (`net_connect`), TLS, the environment proxy and CONNECT tunnel, and a
//!   lookup-hooked fetch's own addresses.
//! - [`transport`]: the pooled hyper-util client, a hooked fetch's one-off
//!   client, the request bodies, and the send-failure mapping.
//!
//! And the op on top of it:
//!
//! - [`send`]: the request JS sends, the redirect loop, the lookup-hook
//!   continuation, and the payload a fetch resolves with.
//! - [`body`]: the response body reader and the outbound request-body
//!   channel lifecycle.
//! - [`bridge`]: an HTTP/1.1 exchange over a byte stream JS pumps to and from
//!   a socket object (`http.request` over an agent's socket).
//!
//! The modules are `pub` so the URL-heavy tests live in
//! `crates/oam_core/tests/http_client_*.rs`, outside the published-URLs gate's
//! scan (an `http://[::1]` literal parses to the host `[` there).

pub mod body;
pub mod bridge;
mod connector;
pub mod decode;
pub mod prepare;
pub mod redirect;
pub mod send;
pub mod tls_config;
pub mod transport;

pub use transport::{HttpTransport, ProxySource, Route, SendError, TlsSource, TransportOptions};

/// A host a fetch is about to connect to, as [`NetCheck`] sees it.
///
/// `host` is the URL's host exactly as the loop will resolve and dial it:
/// `url::Url::host_str`, which is the WHATWG serialization -- a domain
/// lower-cased and IDNA-mapped to ASCII, percent-decoding applied, a trailing
/// dot KEPT (`localhost.` is its own name); an IPv4 address in dotted-quad
/// form whatever it was written as (`0x7f.1`, `2130706433`); an IPv6 address
/// compressed and bracketed (`[::1]`). Userinfo is never part of it. It is the
/// DESTINATION, never the environment proxy a request may be tunnelled
/// through.
///
/// `port` is the port it will be dialled on: the URL's own, or the scheme's
/// default (80 / 443) when the URL names none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetTarget<'a> {
    pub host: &'a str,
    pub port: u16,
}

/// A `--permission` net grant, applied by the fetch loop to every host it is
/// about to dial -- the initial URL and each redirect hop -- before it dials
/// it and before a `connect.lookup` hook is asked to resolve it.
///
/// The grant lives in the engine (its `Permissions`), and this crate stays
/// v8-free and permission-model-free, so the engine hands the loop its check
/// as a closure. `None` means the grant covers every host (no `--permission`,
/// or a bare `--allow-net`): the loop then does no work at all. A refusal
/// fails the fetch with `OpOutcome::AccessDenied` carrying the verdict.
pub type NetCheck =
    std::sync::Arc<dyn Fn(&NetTarget<'_>) -> Result<(), crate::AccessDenial> + Send + Sync>;

/// The error type the connector and request bodies carry.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The request body type of the legacy client: every body shape, boxed once.
pub type ReqBody = http_body_util::combinators::BoxBody<bytes::Bytes, BoxError>;
