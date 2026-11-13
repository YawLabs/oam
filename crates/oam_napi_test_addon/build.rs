//! Deliberately empty. A build script that prints no `cargo:rerun-if-*` line
//! makes cargo re-run it -- and so rebuild this cdylib -- whenever ANY file of
//! the package changes, Cargo.toml included. Without it, a manifest edit cargo
//! does not compile differently (a `[lints]` table, a comment) left the .dll /
//! .so / .dylib older than Cargo.toml, and the e2e and engine tests' staleness
//! guard (which compares the addon's mtime against its sources) then failed
//! with no `cargo build` able to clear it.
fn main() {}
