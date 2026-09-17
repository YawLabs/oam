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
//! The modules are `pub` so the URL-heavy tests live in
//! `crates/oam_core/tests/http_client_*.rs`, outside the published-URLs gate's
//! scan (an `http://[::1]` literal parses to the host `[` there).

mod connector;
pub mod decode;
pub mod prepare;
pub mod redirect;
pub mod tls_config;
pub mod transport;

pub use transport::{HttpTransport, ProxySource, Route, SendError, TlsSource, TransportOptions};

/// The error type the connector and request bodies carry.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The request body type of the legacy client: every body shape, boxed once.
pub type ReqBody = http_body_util::combinators::BoxBody<bytes::Bytes, BoxError>;
