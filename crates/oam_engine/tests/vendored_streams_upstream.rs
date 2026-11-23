//! Provenance gate for the vendored Node streams port
//! (js/vendor/node-streams, docs/design/streams-port.md).
//!
//! Every vendored file is an 8-line banner plus the byte-identical upstream
//! body, and `UPSTREAM` records each body's size and sha256. Nothing checked
//! that table before, and the v22.23.5 parity bump shipped with
//! internal/streams/readable.js one upstream fix behind the pin: same size,
//! different bytes, unnoticed. This test re-hashes every body, so a file
//! edited in place, a row left stale, a banner naming another tag, or a
//! vendored file the table does not list fails `cargo test -p oam_engine`.
//!
//! The offline tests cannot see the v22.23.5 failure itself: a body left
//! behind on a bump, with its banner re-stamped and its row untouched, still
//! hashes to its row. `upstream_rows_match_the_pinned_commit` closes that --
//! it fetches every listed path at UPSTREAM's `Commit:` and compares the
//! sha256. It needs the network (curl), so it is `#[ignore]`d; the re-vendor
//! procedure in UPSTREAM runs it:
//! `cargo test -p oam_engine --test vendored_streams_upstream -- --ignored`.

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const BANNER_LINES: usize = 8;

fn vendor_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../js/vendor/node-streams")
}

struct Manifest {
    tag: String,
    commit: String,
    retrieved: String,
    rows: Vec<(String, usize, String)>,
}

fn header_value(text: &str, key: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| panic!("UPSTREAM: no `{key}` line"))
        .trim()
        .to_string()
}

fn manifest() -> Manifest {
    let text = std::fs::read_to_string(vendor_dir().join("UPSTREAM")).expect("read UPSTREAM");
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        if line.starts_with("-----|") {
            in_table = true;
            continue;
        }
        if !in_table || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(" | ").collect();
        assert_eq!(cols.len(), 3, "UPSTREAM: malformed table row {line:?}");
        let bytes = cols[1]
            .parse()
            .unwrap_or_else(|_| panic!("UPSTREAM: bad size in {line:?}"));
        rows.push((cols[0].to_string(), bytes, cols[2].to_string()));
    }
    assert!(!rows.is_empty(), "UPSTREAM: empty table");
    Manifest {
        tag: header_value(&text, "Upstream tag:"),
        commit: header_value(&text, "Commit:"),
        retrieved: header_value(&text, "Retrieved:"),
        rows,
    }
}

/// Splits a vendored file into (banner lines, pristine body bytes).
fn split_banner(path: &Path) -> (Vec<String>, Vec<u8>) {
    let raw = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut at = 0;
    let mut banner = Vec::new();
    for _ in 0..BANNER_LINES {
        let nl = raw[at..]
            .iter()
            .position(|&b| b == b'\n')
            .unwrap_or_else(|| panic!("{}: shorter than its banner", path.display()));
        banner.push(String::from_utf8_lossy(&raw[at..at + nl]).into_owned());
        at += nl + 1;
    }
    (banner, raw[at..].to_vec())
}

#[test]
fn every_vendored_body_matches_its_upstream_row() {
    let m = manifest();
    let mut failures = Vec::new();
    for (rel, bytes, sha) in &m.rows {
        let path = vendor_dir().join(rel);
        let (banner, body) = split_banner(&path);
        let want_first = format!("// Vendored from Node.js {} (commit {})", m.tag, m.commit);
        if banner[0] != want_first {
            failures.push(format!(
                "{rel}: banner line 1 is {:?}, want {want_first:?}",
                banner[0]
            ));
        }
        let want_source = format!(
            "// Source: https://raw.githubusercontent.com/nodejs/node/{}/lib/{rel}",
            m.tag
        );
        if banner[1] != want_source {
            failures.push(format!(
                "{rel}: banner line 2 is {:?}, want {want_source:?}",
                banner[1]
            ));
        }
        if !banner[2].starts_with(&format!("// Retrieved: {}.", m.retrieved)) {
            failures.push(format!(
                "{rel}: banner line 3 does not name {}",
                m.retrieved
            ));
        }
        if !banner[BANNER_LINES - 1].is_empty() {
            failures.push(format!("{rel}: banner is not followed by a blank line"));
        }
        if body.len() != *bytes {
            failures.push(format!(
                "{rel}: body is {} bytes, UPSTREAM says {bytes}",
                body.len()
            ));
        }
        let got = sha256_hex(&body);
        if &got != sha {
            failures.push(format!("{rel}: body sha256 {got}, UPSTREAM says {sha}"));
        }
    }
    assert!(
        failures.is_empty(),
        "vendored streams drift:\n  {}",
        failures.join("\n  ")
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
#[ignore = "network: fetches every vendored file at the pinned commit (re-vendor step)"]
fn upstream_rows_match_the_pinned_commit() {
    let m = manifest();
    assert_eq!(m.commit.len(), 40, "UPSTREAM: Commit is not a full sha");
    let mut failures = Vec::new();
    for (rel, bytes, sha) in &m.rows {
        let url = format!(
            "https://raw.githubusercontent.com/nodejs/node/{}/lib/{rel}",
            m.commit
        );
        let out = std::process::Command::new("curl")
            .args(["-sSfL", "--retry", "2", &url])
            .output()
            .unwrap_or_else(|e| panic!("spawn curl: {e}"));
        if !out.status.success() {
            failures.push(format!(
                "{rel}: fetch {url} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            continue;
        }
        let got = sha256_hex(&out.stdout);
        if out.stdout.len() != *bytes || &got != sha {
            failures.push(format!(
                "{rel}: upstream at {} is {} bytes sha256 {got}; UPSTREAM row says {bytes} {sha}",
                m.commit,
                out.stdout.len()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "UPSTREAM rows vs the pinned commit:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn every_vendored_file_is_in_the_table() {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).expect("read vendor dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, base, out);
            } else if path.extension().is_some_and(|e| e == "js") {
                let rel = path.strip_prefix(base).expect("under base");
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let base = vendor_dir();
    let mut on_disk = BTreeSet::new();
    walk(&base, &base, &mut on_disk);
    let listed: BTreeSet<String> = manifest().rows.into_iter().map(|r| r.0).collect();
    assert_eq!(
        on_disk, listed,
        "js/vendor/node-streams files vs UPSTREAM rows"
    );
}
