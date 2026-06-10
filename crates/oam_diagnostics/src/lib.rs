//! ODIF v1 — oam Diagnostic Interchange Format.
//!
//! One versioned JSONL envelope for everything oam reports: parse/strip
//! errors, type errors, runtime exceptions, unhandled rejections, test
//! results, install events. JSON is the source of truth; the human
//! pretty-printer is a renderer over the same stream and can never drift.
//!
//! Spec home (once docs ship): https://oam.sh/odif

use serde::{Deserialize, Serialize};

pub const ODIF_VERSION: &str = "1";

/// Where a diagnostic came from. Determines its code namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    /// OAM-PARSE*: oxc parse / strip / transform errors.
    Parse,
    /// OAM-TS*: type diagnostics from the tsgo sidecar (TS codes pass through).
    Typecheck,
    /// OAM-MOD*: module resolution / loading.
    Resolve,
    /// OAM-RT*: runtime exceptions, unhandled rejections.
    Runtime,
    /// OAM-TEST*: test runner results.
    Test,
    /// OAM-PKG*: installer / resolver events.
    Install,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// How safe a repair is to apply without a human in the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FixSafety {
    /// Mechanical, semantics-preserving. `oam fix --safe` applies these.
    SafeAuto,
    /// Plausible but needs human review before applying.
    Review,
    /// A hint, not an edit plan. Never auto-applied.
    UnsafeHint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// 1-based.
    pub line: u32,
    /// 1-based.
    pub col: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub file: String,
    pub start: Position,
    pub end: Position,
}

/// A single proposed edit inside a repair plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub span: Span,
    pub new_text: String,
}

/// A typed repair plan attached to a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repair {
    /// Stable repair id, e.g. "R-TS-ADD-EXTENSION".
    pub id: String,
    pub safety: FixSafety,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<Edit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The ODIF diagnostic event.
///
/// Serialized as one JSON object per line (JSONL). The `odif` field carries
/// the envelope version so consumers can gate on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Envelope version. Always [`ODIF_VERSION`] for this crate.
    pub odif: String,
    /// Stable, namespaced code: OAM-TS2345, OAM-RT1001, ...
    pub code: String,
    pub severity: Severity,
    pub origin: Origin,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<Span>,
    /// Related locations (e.g. "declared here").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<Span>,
    /// Dedup / flake-correlation hash, stable across runs for "the same" problem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Offline-resolvable docs URL: https://oam.sh/e/<code>
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repairs: Vec<Repair>,
}

impl Diagnostic {
    pub fn new(
        code: impl Into<String>,
        severity: Severity,
        origin: Origin,
        message: impl Into<String>,
    ) -> Self {
        let code = code.into();
        let docs = Some(format!("https://oam.sh/e/{code}"));
        Self {
            odif: ODIF_VERSION.to_string(),
            code,
            severity,
            origin,
            message: message.into(),
            spans: Vec::new(),
            related: Vec::new(),
            fingerprint: None,
            docs,
            repairs: Vec::new(),
        }
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.spans.push(span);
        self
    }

    /// One JSONL line, the machine-facing source of truth.
    pub fn to_jsonl(&self) -> String {
        serde_json::to_string(self).expect("ODIF diagnostics always serialize")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_jsonl() {
        let d = Diagnostic::new("OAM-RT0001", Severity::Error, Origin::Runtime, "kaboom")
            .with_span(Span {
                file: "src/a.ts".into(),
                start: Position { line: 2, col: 1 },
                end: Position { line: 2, col: 6 },
            });
        let line = d.to_jsonl();
        let back: Diagnostic = serde_json::from_str(&line).unwrap();
        assert_eq!(d, back);
        assert!(line.contains("\"odif\":\"1\""));
        assert!(line.contains("oam.sh/e/OAM-RT0001"));
    }
}
