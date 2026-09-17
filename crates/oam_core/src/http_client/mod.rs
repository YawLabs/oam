//! oam's own HTTP client transport for the `fetch` op (#143).
//!
//! The op used to hand everything to reqwest, whose defaults are reqwest's and
//! not Node's: ten redirects instead of twenty, a Referer on every hop, a
//! credential leak on the second same-host hop after a cross-origin one, only
//! an exact lowercase single `gzip`/`deflate` decoded, a truncated gzip body
//! turned into an error that Node returns as data. Node v22.22.2's `fetch` is
//! undici 6.24.1, so the rules here are undici's, measured against that build.
//!
//! This module holds the pure, transport-free halves -- nothing in here dials,
//! awaits or touches the runtime:
//!
//! - [`redirect`]: the "follow" redirect step, undici fetch/index.js:1210-1318.
//! - [`prepare`]: URL, method and header preparation, keeping today's error
//!   texts so `http.request`'s mapping of them stays unchanged.
//!
//! The modules are `pub` so the URL-heavy tests live in
//! `crates/oam_core/tests/http_client_*.rs`, outside the published-URLs gate's
//! scan (an `http://[::1]` literal parses to the host `[` there).

pub mod prepare;
pub mod redirect;
