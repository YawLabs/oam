//! Shared by the test targets that spawn oam and can be pointed at another
//! build of it (http_server_wire.rs, event_loop_heap.rs).
//!
//! Not a test target: cargo builds only the files directly under tests/ as
//! tests, and each of the two includes this with `mod wire_bin;`.

use std::path::PathBuf;

/// The oam binary these tests run: `OAM_WIRE_TEST_BIN` when it is set --
/// which runs a suite against another oam build, how each test was shown to
/// fail on the release before its fix -- otherwise the one cargo built for
/// this test target. A value that does not name a file fails the test
/// instead of quietly testing the cargo build in its place (the rule
/// `OAM_CONPTY_BIN` follows in conpty.rs); `var_os` keeps a path that is not
/// UTF-8 from reading as "unset".
pub fn oam_under_test() -> PathBuf {
    match std::env::var_os("OAM_WIRE_TEST_BIN").filter(|value| !value.is_empty()) {
        Some(bin) => {
            let bin = PathBuf::from(bin);
            assert!(
                bin.is_file(),
                "OAM_WIRE_TEST_BIN={} is not a file. Refusing to fall back to the \
                 cargo-built oam, which would pass for a binary nobody asked about.",
                bin.display()
            );
            bin
        }
        None => PathBuf::from(env!("CARGO_BIN_EXE_oam")),
    }
}
