//! Gate: every URL oam's compiled sources can print is one somebody
//! published.
//!
//! Two URLs shipped broken for months -- `https://oamjs.org/docs/inspector`
//! on every `--inspect` run (the page never existed) and `oam.sh/e/<code>`
//! in ODIF's `docs` field (a domain that left the project and now
//! redirects elsewhere). Neither was caught, because a URL is the one thing
//! a compiler cannot check.
//!
//! Fetching them in the gate is not the answer: ci-local.sh has to pass
//! offline, and a network check turns somebody else's outage into a red
//! build. So this asserts against `docs/published-urls.txt` instead -- a
//! reviewer adding a line there is asserting they published the page, and
//! that assertion is the thing under review.
//!
//! Scope is what compiles into the binary and its snapshot: `crates/*/src`,
//! `js/` outside `js/vendor`, and `xtask/src`. Test trees are out --
//! `crates/oam_cli/tests/e2e.rs` alone carries hundreds of example URLs
//! that no user ever sees, and pulling them in would bury the signal.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Hosts the project speaks for. A full URL on one of these must be listed
/// in the `[urls]` section, because only we can publish the page behind it.
const PROJECT_HOSTS: &[&str] = &["oamjs.org", "www.oamjs.org"];

/// Hosts that must never appear again, allowlist or not.
///
/// `oam.sh` was the project's domain before decision D9 renamed it to
/// oamjs.org. It is not ours any more and 301s to `0am.sh`, so a diagnostic
/// citing it hands the reader to a stranger. Listing it in the allowlist
/// must not be able to bring it back, which is why this check runs before
/// the allowlist is consulted at all.
const RETIRED_HOSTS: &[&str] = &["oam.sh", "0am.sh"];

#[test]
fn every_printed_url_is_published() {
    let root = repo_root();
    let allow = Allowlist::parse(
        &std::fs::read_to_string(root.join("docs/published-urls.txt"))
            .expect("docs/published-urls.txt is readable"),
    );

    let mut seen_urls: BTreeSet<String> = BTreeSet::new();
    let mut used_host_entries: BTreeSet<String> = BTreeSet::new();
    let mut failures: Vec<String> = Vec::new();

    for file in scanned_files(&root) {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let shown = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .display()
            .to_string()
            .replace('\\', "/");

        for (lineno, line) in text.lines().enumerate() {
            let lineno = lineno + 1;

            // A project-host URL is checked wherever it appears, comments
            // included: `oam.sh/odif` lived in a module doc comment for its
            // whole life, and a URL in a comment is one copy-paste away from
            // being a URL in a `eprintln!`.
            for url in urls_in(line) {
                let host = host_of(&url);
                if RETIRED_HOSTS.iter().any(|h| is_host(host, h)) {
                    failures.push(format!(
                        "{shown}:{lineno}: {url} is on a retired domain. oamjs.org replaced oam.sh; \
                         oam.sh now redirects off the project entirely and cannot be allowlisted."
                    ));
                } else if PROJECT_HOSTS.iter().any(|h| is_host(host, h)) {
                    let normalized = strip_fragment(&url).to_string();
                    seen_urls.insert(normalized.clone());
                    if !allow.urls.contains(&normalized) {
                        failures.push(format!(
                            "{shown}:{lineno}: {normalized} is not in docs/published-urls.txt. \
                             Publish the page, then add it to the [urls] section with a note \
                             saying what prints it."
                        ));
                    }
                }
            }

            // Foreign hosts are checked with line comments stripped: in code
            // they are endpoints and fixtures worth listing once, in prose
            // ("'http://c\\r\\nd/e' has host 'cd'") they are examples of URL
            // parsing and listing them would say nothing.
            for url in urls_in(strip_line_comment(line)) {
                let host = host_of(&url);
                // A host assembled at runtime (`http://${host}/x`) has no
                // fixed spelling to check and is a request target, not a
                // link a reader follows.
                if host.is_empty() || host.contains('$') || host.contains('{') {
                    continue;
                }
                if RETIRED_HOSTS.iter().any(|h| is_host(host, h))
                    || PROJECT_HOSTS.iter().any(|h| is_host(host, h))
                {
                    continue; // handled above, over the un-stripped line
                }
                match allow.hosts.iter().find(|h| is_host(host, h)) {
                    // Record the ENTRY that matched, not the raw host, so a
                    // subdomain keeps its parent entry alive in the
                    // stale-entry pass below.
                    Some(entry) => {
                        used_host_entries.insert(entry.clone());
                    }
                    None => failures.push(format!(
                        "{shown}:{lineno}: host `{host}` (in {url}) is not in \
                         docs/published-urls.txt. Add it to the [hosts] section with a note \
                         saying why it is not a documentation link."
                    )),
                }
            }
        }
    }

    // Stale entries fail too. An allowlist that only ever grows stops being
    // a list of what the binary prints and becomes a list of what it used to.
    for url in &allow.urls {
        if !seen_urls.contains(url) {
            failures.push(format!(
                "docs/published-urls.txt lists {url} but nothing in the scanned sources \
                 prints it. Remove the entry."
            ));
        }
    }
    for host in &allow.hosts {
        if !used_host_entries.contains(host) {
            failures.push(format!(
                "docs/published-urls.txt lists host `{host}` but nothing in the scanned \
                 sources uses it. Remove the entry."
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "printed-URL gate failed:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn every_allowlisted_url_carries_a_note() {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("docs/published-urls.txt")).unwrap();
    let allow = Allowlist::parse(&text);
    // The note is what a reviewer reads to decide whether the page needs to
    // exist. An entry without one is a URL nobody has to defend.
    assert!(!allow.urls.is_empty() && !allow.hosts.is_empty());
    for (entry, note) in allow.notes {
        assert!(
            !note.trim().is_empty(),
            "docs/published-urls.txt: `{entry}` has no note. Say what prints it."
        );
    }
}

// ---------------------------------------------------------------------------
// Allowlist
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Allowlist {
    urls: BTreeSet<String>,
    hosts: BTreeSet<String>,
    /// entry -> the `#` note that follows it, for the note check.
    notes: Vec<(String, String)>,
}

impl Allowlist {
    fn parse(text: &str) -> Self {
        let mut out = Allowlist::default();
        let mut section = "";
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = match name {
                    "urls" => "urls",
                    "hosts" => "hosts",
                    other => panic!("docs/published-urls.txt: unknown section [{other}]"),
                };
                continue;
            }
            let (entry, note) = match line.split_once('#') {
                Some((entry, note)) => (entry.trim(), note.trim()),
                None => (line, ""),
            };
            out.notes.push((entry.to_string(), note.to_string()));
            match section {
                "urls" => {
                    out.urls.insert(entry.to_string());
                }
                "hosts" => {
                    out.hosts.insert(entry.to_string());
                }
                _ => panic!("docs/published-urls.txt: `{entry}` appears before any section header"),
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ has a parent")
        .to_path_buf()
}

fn scanned_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect(&root.join("js"), "js", &mut out);
    collect(&root.join("xtask/src"), "rs", &mut out);
    if let Ok(entries) = std::fs::read_dir(root.join("crates")) {
        for entry in entries.flatten() {
            collect(&entry.path().join("src"), "rs", &mut out);
        }
    }
    // js/vendor is Node's own source, verbatim and never edited here; the
    // upstream provenance URLs at the top of every file are the point of it.
    out.retain(|p| !p.components().any(|c| c.as_os_str() == "vendor"));
    out.sort();
    assert!(
        out.len() > 20,
        "scan found only {} files -- the roots moved and this gate is checking nothing",
        out.len()
    );
    out
}

fn collect(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// URL extraction
// ---------------------------------------------------------------------------

/// Cut a line at its first line comment.
///
/// Naive splitting on `//` would cut every URL in half at its own scheme
/// separator, so the marker only counts when the character before it is not
/// a colon. That is enough for both languages here: `//` opens a comment in
/// Rust and JS, and `://` is the only place a URL puts two slashes.
fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// Every absolute http(s) URL in a line, in order.
fn urls_in(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("http") {
        let candidate = &rest[at..];
        let after_scheme = if let Some(s) = candidate.strip_prefix("https://") {
            s
        } else if let Some(s) = candidate.strip_prefix("http://") {
            s
        } else {
            rest = &rest[at + 4..];
            continue;
        };
        let scheme_len = candidate.len() - after_scheme.len();
        let end = candidate[scheme_len..]
            .find(|c: char| {
                c.is_whitespace()
                    || matches!(c, '"' | '\'' | '`' | '<' | '>' | '\\' | ')' | ',' | ';')
            })
            .map(|i| scheme_len + i)
            .unwrap_or(candidate.len());
        let url = candidate[..end].trim_end_matches(['.', '*', ':']);
        if !url.is_empty() {
            out.push(url.to_string());
        }
        rest = &candidate[end..];
    }
    out
}

/// The host of an extracted URL: after the scheme and any userinfo, up to
/// the first `/`, `:`, `?` or `#`.
fn host_of(url: &str) -> &str {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    // Userinfo can itself contain a colon (`user:pass@host`), so the host
    // starts after the LAST `@` in the authority, not the first colon.
    let host_start = authority.rfind('@').map(|i| i + 1).unwrap_or(0);
    let host = &authority[host_start..];
    let host_end = host.find(':').unwrap_or(host.len());
    &host[..host_end]
}

/// `host` is `name`, or a subdomain of it. Substring matching would make
/// `foam.sh` a match for `oam.sh`.
fn is_host(host: &str, name: &str) -> bool {
    host.eq_ignore_ascii_case(name)
        || (host.len() > name.len()
            && host[host.len() - name.len()..].eq_ignore_ascii_case(name)
            && host.as_bytes()[host.len() - name.len() - 1] == b'.')
}

/// A fragment never reaches the server, so two URLs differing only in their
/// fragment are the same published page.
fn strip_fragment(url: &str) -> &str {
    url.split_once('#').map(|(base, _)| base).unwrap_or(url)
}

/// Unit tests for the scanner itself. No `#[cfg(test)]`: this file IS a test
/// target, and gating them would compile them out silently.
mod helper_tests {
    use super::*;

    #[test]
    fn line_comment_stripping_keeps_urls_intact() {
        assert_eq!(
            strip_line_comment(r#"eprintln!("see https://oamjs.org/docs/inspector");"#),
            r#"eprintln!("see https://oamjs.org/docs/inspector");"#
        );
        assert_eq!(strip_line_comment("// see http://example.com/"), "");
        assert_eq!(strip_line_comment("//! see http://example.com/"), "");
        assert_eq!(
            strip_line_comment("let a = 1; // http://example.com/"),
            "let a = 1; "
        );
        assert_eq!(strip_line_comment("plain"), "plain");
    }

    #[test]
    fn extracts_urls_out_of_real_source_shapes() {
        assert_eq!(
            urls_in(r#"        "https://oamjs.org/install.ps1""#),
            vec!["https://oamjs.org/install.ps1"]
        );
        assert_eq!(
            urls_in(r#"format!("{ERROR_REFERENCE_URL}#{code}") // https://oam.sh/e/x"#),
            vec!["https://oam.sh/e/x"]
        );
        assert_eq!(
            urls_in("a http://one.test/x and https://two.test/y."),
            vec!["http://one.test/x", "https://two.test/y"]
        );
        assert_eq!(urls_in("no url here"), Vec::<String>::new());
        // A bare `http` word must not be mistaken for a URL.
        assert_eq!(urls_in("the http protocol"), Vec::<String>::new());
    }

    #[test]
    fn host_parsing_survives_userinfo_ports_and_templates() {
        assert_eq!(host_of("https://oamjs.org/docs/errors"), "oamjs.org");
        assert_eq!(
            host_of("https://user:pass@example.com:8080/path?q=1#frag"),
            "example.com"
        );
        assert_eq!(host_of("http://127.0.0.1:${port}"), "127.0.0.1");
        assert_eq!(host_of("http://${host}${meta.uri}"), "${host}${meta.uri}");
        assert_eq!(host_of("http://x"), "x");
    }

    #[test]
    fn retired_host_matching_is_not_substring_matching() {
        assert!(is_host("oam.sh", "oam.sh"));
        assert!(is_host("www.oam.sh", "oam.sh"));
        assert!(!is_host("foam.sh", "oam.sh"));
        assert!(!is_host("oam.shop", "oam.sh"));
        assert!(is_host("oamjs.org", "oamjs.org"));
        assert!(!is_host("notoamjs.org", "oamjs.org"));
    }

    #[test]
    fn fragments_collapse_onto_the_page() {
        assert_eq!(
            strip_fragment("https://oamjs.org/docs/errors#OAM-RT0001"),
            "https://oamjs.org/docs/errors"
        );
        assert_eq!(
            strip_fragment("https://oamjs.org/docs/errors"),
            "https://oamjs.org/docs/errors"
        );
    }
}
