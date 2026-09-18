//! The Node the conformance oracle must be: exactly the version pinned in
//! `.node-version` at the repo root.
//!
//! oam's parity claim is against ONE Node. The vendored node-suite corpus is a
//! snapshot of that tag, and the node-differential cases, the builtin
//! export-parity ratchet and the "probed on vX" comments across the tree were
//! all measured against it -- so a differential run against any other `node`
//! compares oam with the wrong thing and says nothing about it. Before this
//! module each build leg used whatever `node` its PATH found: the Windows box
//! happened to carry the target, while the mac and linux legs ran v22.23.1 and
//! their receipts never once described the parity target.
//!
//! This is the Rust twin of `scripts/lib/node-pin.sh` (the parse and the
//! verdict are deliberately identical; `scripts/test-scripts.sh` drives the
//! shell side). The remote legs provision the pinned Node from nodejs.org;
//! here we only refuse to run against anything else.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::Path;

/// Repo-relative pin file. The name version managers already read (nodenv,
/// fnm, mise, asdf), so a developer's own tooling lands on the same Node.
pub(crate) const PIN_FILE: &str = ".node-version";

/// Only the literal value `1` allows a mismatch (the `OAM_ALLOW_DOWNGRADE`
/// convention), so a stray `0` or `false` cannot switch the gate off.
pub(crate) const ALLOW_MISMATCH_ENV: &str = "OAM_ALLOW_NODE_MISMATCH";

/// Parse the pin file's contents into `node --version` form (`v22.22.2`).
///
/// Accepts surrounding whitespace, CRLF and an optional leading `v`, and
/// nothing else: exactly one MAJOR.MINOR.PATCH. A range or an alias (`22`,
/// `lts/jod`) is refused rather than resolved, because the point is that every
/// host lands on the SAME build. Whitespace INSIDE the text is refused too, so a
/// second token on another line fails loudly instead of being ignored.
pub(crate) fn parse_pin(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let bare = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let parts: Vec<&str> = bare.split('.').collect();
    let well_formed = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    well_formed.then(|| format!("v{bare}"))
}

/// The pinned version for the repo at `repo`, in `v22.22.2` form.
pub(crate) fn pinned_node_version(repo: &Path) -> Result<String> {
    let path = repo.join(PIN_FILE);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "{} is missing or unreadable -- it pins the exact Node the conformance oracle \
             must be; restore it from git",
            path.display()
        )
    })?;
    match parse_pin(&text) {
        Some(version) => Ok(version),
        None => bail!(
            "{} must hold exactly one MAJOR.MINOR.PATCH version (e.g. 22.22.2), got {:?}",
            path.display(),
            text.chars().take(80).collect::<String>()
        ),
    }
}

/// What the `node` on PATH is, relative to the pin.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Pinned,
    Mismatch,
    Absent,
}

/// `found` is `node --version` verbatim (already trimmed), `None` when there
/// is no runnable node.
pub(crate) fn judge(pinned: &str, found: Option<&str>) -> Verdict {
    match found {
        None => Verdict::Absent,
        Some(v) if v == pinned => Verdict::Pinned,
        Some(_) => Verdict::Mismatch,
    }
}

/// Whether the escape hatch is set in this process's environment.
pub(crate) fn mismatch_allowed() -> bool {
    std::env::var(ALLOW_MISMATCH_ENV).is_ok_and(|v| v == "1")
}

/// Refuse a run whose oracle is not the pinned Node.
///
/// `Ok(None)` when it is; `Ok(Some(warning))` when it is not but `allow` is
/// set (the caller prints the warning and marks its receipts); `Err` otherwise.
pub(crate) fn enforce(pinned: &str, found: Option<&str>, allow: bool) -> Result<Option<String>> {
    let hatch = format!(
        "set {ALLOW_MISMATCH_ENV}=1 for an ad-hoc run against a different node (its receipts then \
         name the node that really ran)"
    );
    match (judge(pinned, found), allow) {
        (Verdict::Pinned, _) => Ok(None),
        (Verdict::Mismatch, false) => bail!(
            "the node on PATH is {}, but {PIN_FILE} pins {pinned}. The node-differential oracle \
             must be exactly the pinned Node -- every recorded expectation was measured against \
             it. Put Node {pinned} first on PATH (the remote legs provision it themselves, see \
             scripts/lib/node-pin.sh), or {hatch}.",
            found.unwrap_or_default()
        ),
        (Verdict::Absent, false) => bail!(
            "no runnable node on PATH, and the node-differential oracle must be exactly the Node \
             {PIN_FILE} pins ({pinned}). Install it and put it first on PATH, or {hatch}."
        ),
        (Verdict::Mismatch, true) => Ok(Some(format!(
            "node on PATH is {}, NOT the pinned {pinned} -- continuing because \
             {ALLOW_MISMATCH_ENV}=1. These results do not describe the parity target.",
            found.unwrap_or_default()
        ))),
        (Verdict::Absent, true) => Ok(Some(format!(
            "no node on PATH (pinned: {pinned}) -- continuing because {ALLOW_MISMATCH_ENV}=1; \
             every node-differential case will be skipped, which still fails the gate."
        ))),
    }
}

/// The node-suite corpus must be a snapshot of the pinned Node.
///
/// The node-suite never runs node -- its oracle is each vendored test's exit
/// code -- so what ties its receipts to the pin is the corpus itself. A
/// manifest naming a different version means a half-finished Node bump: the
/// pin moved and the corpus did not (or the reverse), and a scorecard stamped
/// with either version would misdescribe the run. No escape hatch: that is a
/// repo inconsistency, not a property of the machine running the suite.
pub(crate) fn check_corpus(pinned: &str, manifest_path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    corpus_verdict(pinned, manifest["nodeVersion"].as_str(), manifest_path)
}

fn corpus_verdict(pinned: &str, corpus: Option<&str>, manifest_path: &Path) -> Result<()> {
    match corpus {
        Some(v) if v == pinned => Ok(()),
        Some(v) => bail!(
            "{} says the vendored corpus is Node {v}, but {PIN_FILE} pins {pinned}. A Node bump \
             moves both together: re-vendor the corpus at {pinned} (and update its nodeVersion), \
             or put {PIN_FILE} back to {v}.",
            manifest_path.display()
        ),
        None => bail!(
            "{} has no string \"nodeVersion\" -- it must name the Node tag the corpus was \
             vendored from, and that must equal {PIN_FILE} ({pinned})",
            manifest_path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_spellings_version_managers_accept() {
        assert_eq!(parse_pin("22.22.2\n").as_deref(), Some("v22.22.2"));
        assert_eq!(parse_pin("v22.22.2").as_deref(), Some("v22.22.2"));
        assert_eq!(parse_pin("  22.22.2\r\n").as_deref(), Some("v22.22.2"));
    }

    #[test]
    fn parse_refuses_anything_but_one_exact_version() {
        for bad in [
            "",
            "\n",
            "22",
            "22.22",
            "22.22.2.1",
            "lts/jod",
            "22.x",
            "^22.22.2",
            "vv22.22.2",
            "22..2",
            "22.22.2\n22.23.1",
            "22.22.2 # comment",
            "22.22.-2",
        ] {
            assert_eq!(parse_pin(bad), None, "{bad:?} must be refused");
        }
    }

    #[test]
    fn the_committed_pin_parses_and_matches_the_vendored_corpus() {
        let repo = crate::conformance::repo_root().unwrap();
        let pinned = pinned_node_version(&repo).unwrap();
        check_corpus(&pinned, &repo.join("conformance/vendor/node/manifest.json")).unwrap();
    }

    #[test]
    fn judge_is_exact_string_equality() {
        assert_eq!(judge("v22.22.2", Some("v22.22.2")), Verdict::Pinned);
        assert_eq!(judge("v22.22.2", Some("v22.23.1")), Verdict::Mismatch);
        // A prefix is not a match: v22.22.20 is a different release.
        assert_eq!(judge("v22.22.2", Some("v22.22.20")), Verdict::Mismatch);
        assert_eq!(judge("v22.22.2", Some("22.22.2")), Verdict::Mismatch);
        assert_eq!(judge("v22.22.2", None), Verdict::Absent);
    }

    #[test]
    fn enforce_refuses_a_mismatch_and_names_the_hatch() {
        let err = enforce("v22.22.2", Some("v22.23.1"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("v22.23.1"), "{err}");
        assert!(err.contains("v22.22.2"), "{err}");
        assert!(err.contains(ALLOW_MISMATCH_ENV), "{err}");
    }

    #[test]
    fn enforce_refuses_an_absent_node() {
        let err = enforce("v22.22.2", None, false).unwrap_err().to_string();
        assert!(err.contains("no runnable node"), "{err}");
        assert!(err.contains(ALLOW_MISMATCH_ENV), "{err}");
    }

    #[test]
    fn enforce_passes_the_pin_silently_even_with_the_hatch_set() {
        assert_eq!(enforce("v22.22.2", Some("v22.22.2"), false).unwrap(), None);
        assert_eq!(enforce("v22.22.2", Some("v22.22.2"), true).unwrap(), None);
    }

    #[test]
    fn the_hatch_downgrades_both_refusals_to_warnings() {
        let warning = enforce("v22.22.2", Some("v22.23.1"), true)
            .unwrap()
            .expect("a mismatch must still warn");
        assert!(warning.contains("NOT the pinned v22.22.2"), "{warning}");
        let warning = enforce("v22.22.2", None, true)
            .unwrap()
            .expect("an absent node must still warn");
        assert!(warning.contains("no node on PATH"), "{warning}");
    }

    #[test]
    fn corpus_must_name_the_pinned_version() {
        let path = Path::new("conformance/vendor/node/manifest.json");
        corpus_verdict("v22.22.2", Some("v22.22.2"), path).unwrap();
        let err = corpus_verdict("v22.22.2", Some("v22.23.1"), path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("re-vendor"), "{err}");
        let err = corpus_verdict("v22.22.2", None, path)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nodeVersion"), "{err}");
    }
}
