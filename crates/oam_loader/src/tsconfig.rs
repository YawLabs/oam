//! tsconfig discovery + the compilerOptions subset the loader honors.
//!
//! Contract (plan §2.6): the resolver consults tsconfig EXACTLY as tsgo
//! does, so `oam run` and `oam check` never disagree about what an import
//! means. Surface: compilerOptions.paths, jsx, jsxImportSource, jsxFactory
//! and jsxFragmentFactory from the nearest tsconfig.json, with relative (or
//! absolute) `extends` chains -- a string, or the TypeScript 5.0 array --
//! merged per option: the nearest declaration wins, and among array entries
//! a later one wins over an earlier one. Bare-package extends
//! ("@tsconfig/node20") needs npm resolution and is skipped with a deferred
//! warning (`crate::take_warnings`). Matching follows TypeScript 7: exact
//! keys beat patterns, one `*` per pattern, longest matched prefix wins and
//! the first DECLARED pattern keeps a tie (serde_json's `preserve_order`
//! feature keeps the map in declaration order), substitutions tried in
//! order, resolved against the DECLARING tsconfig's directory (TS 7 removed
//! baseUrl entirely -- TS5102 -- and requires relative substitutions --
//! TS5090; both probed against tsgo).
//!
//! Files under a `node_modules` directory get NO tsconfig: a dependency's
//! own tsconfig.json is a build-time artifact of that package, and neither
//! Node nor tsc applies it to the package's published files.
//!
//! tsconfig.json is JSONC: comments and trailing commas are stripped
//! before serde sees it. A file that still does not parse is skipped with a
//! deferred warning, never silently.

use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use oam_diagnostics::{Diagnostic, Origin, Severity};

use crate::resolver::Resolver;

/// Dedupe set for the deferred warnings, keyed (code, tsconfig path).
/// tsconfig is consulted once per resolve, so an un-deduped warning would
/// fire N times for a project with N resolves. Process-wide, like the sink
/// it feeds, so a second `Resolver` on the same project does not repeat it.
fn warned() -> &'static Mutex<HashSet<(String, PathBuf)>> {
    use std::sync::OnceLock;
    static SET: OnceLock<Mutex<HashSet<(String, PathBuf)>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Queue a `Severity::Warning` about `tsconfig`, at most once per (code,
/// path) for the life of the process.
fn warn_once(tsconfig: &Path, code: &str, message: String) {
    let first = warned()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert((code.to_string(), tsconfig.to_path_buf()));
    if first {
        crate::warnings::push(Diagnostic::new(
            code,
            Severity::Warning,
            Origin::Resolve,
            message,
        ));
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PathsConfig {
    /// The declaring tsconfig's directory; substitutions resolve here.
    pub base_dir: PathBuf,
    /// (pattern, substitutions) in declaration order.
    pub patterns: Vec<(String, Vec<String>)>,
}

/// The subset of a tsconfig (extends-chain merged) the loader consumes.
/// Each field merges independently: the nearest declaration wins, a child
/// without one inherits the parent's (matching tsc's per-option override).
#[derive(Debug, Clone, Default)]
pub struct TsconfigInfo {
    pub(crate) paths: Option<PathsConfig>,
    /// `compilerOptions.jsx` as written ("react", "react-jsx", ...).
    pub(crate) jsx: Option<String>,
    pub(crate) jsx_import_source: Option<String>,
    pub(crate) jsx_factory: Option<String>,
    pub(crate) jsx_fragment_factory: Option<String>,
}

impl TsconfigInfo {
    /// Every option the loader reads is declared right here: the extends
    /// chain cannot add anything (a declaration replaces the inherited
    /// value entirely), so it need not be loaded.
    fn is_complete(&self) -> bool {
        self.paths.is_some()
            && self.jsx.is_some()
            && self.jsx_import_source.is_some()
            && self.jsx_factory.is_some()
            && self.jsx_fragment_factory.is_some()
    }

    /// `self` over `inherited`: per option, a declaration here wins and a
    /// missing one falls through to what the extends chain supplied.
    fn merged_over(self, inherited: Option<TsconfigInfo>) -> TsconfigInfo {
        let Some(inherited) = inherited else {
            return self;
        };
        TsconfigInfo {
            paths: self.paths.or(inherited.paths),
            jsx: self.jsx.or(inherited.jsx),
            jsx_import_source: self.jsx_import_source.or(inherited.jsx_import_source),
            jsx_factory: self.jsx_factory.or(inherited.jsx_factory),
            jsx_fragment_factory: self.jsx_fragment_factory.or(inherited.jsx_fragment_factory),
        }
    }
}

#[derive(Deserialize, Default)]
struct RawTsconfig {
    #[serde(default)]
    extends: Option<Extends>,
    #[serde(default, rename = "compilerOptions")]
    compiler_options: RawCompilerOptions,
}

/// `extends` is a string or, since TypeScript 5.0, an array of them.
#[derive(Deserialize)]
#[serde(untagged)]
enum Extends {
    One(String),
    Many(Vec<String>),
}

impl Extends {
    fn into_vec(self) -> Vec<String> {
        match self {
            Extends::One(one) => vec![one],
            Extends::Many(many) => many,
        }
    }
}

#[derive(Deserialize, Default)]
struct RawCompilerOptions {
    // NOTE: no baseUrl. TypeScript 7 REMOVED the option (TS5102, verified
    // against tsgo 7.0.0-dev) — paths always resolve against the declaring
    // tsconfig's directory. A config that still sets baseUrl gets tsgo's
    // own loud TS5102 through `oam check`; the loader must not quietly
    // honor what the checker rejects.
    #[serde(default)]
    paths: Option<serde_json::Map<String, serde_json::Value>>,
    /// The JSX mode: "react" (classic), "react-jsx" (automatic),
    /// "react-jsxdev" (automatic + development), "preserve" / "react-native"
    /// (compiled as automatic -- see `JsxMode`).
    #[serde(default)]
    jsx: Option<String>,
    /// Retargets the automatic JSX runtime (`preact` -> imports from
    /// `preact/jsx-runtime`). Consumed by `transpile_typescript`.
    #[serde(default, rename = "jsxImportSource")]
    jsx_import_source: Option<String>,
    /// Classic runtime: the element factory (`React.createElement` default).
    #[serde(default, rename = "jsxFactory")]
    jsx_factory: Option<String>,
    /// Classic runtime: the fragment component (`React.Fragment` default).
    #[serde(default, rename = "jsxFragmentFactory")]
    jsx_fragment_factory: Option<String>,
}

/// Which JSX runtime a file compiles against, from `compilerOptions.jsx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsxMode {
    /// tsconfig `react-jsx` (and the default when unset): `jsx()` / `jsxs()`
    /// imported from `<source>/jsx-runtime`.
    Automatic,
    /// tsconfig `react-jsxdev`: `jsxDEV()` from `<source>/jsx-dev-runtime`,
    /// with `__source` / `__self` debugging props.
    AutomaticDev,
    /// tsconfig `react`: `React.createElement(...)` calls (or the configured
    /// jsxFactory / jsxFragmentFactory), no runtime import.
    Classic,
}

impl JsxMode {
    fn from_tsconfig(value: Option<&str>) -> Self {
        match value {
            Some("react") => JsxMode::Classic,
            Some("react-jsxdev") => JsxMode::AutomaticDev,
            // "react-jsx", plus everything oam cannot honor at runtime:
            // "preserve" and "react-native" leave JSX in the output for a
            // later tool, and there is no later tool when the file is about
            // to execute -- so they compile with the automatic runtime
            // (docs/node-divergences.md section 12). Unknown values too;
            // `oam check` surfaces those as tsgo's own diagnostic.
            _ => JsxMode::Automatic,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            JsxMode::Automatic => "automatic",
            JsxMode::AutomaticDev => "automatic-dev",
            JsxMode::Classic => "classic",
        }
    }
}

/// How JSX in one file is compiled, resolved from the nearest tsconfig.
/// Consumed by `transpile_typescript`; part of `transpile_fingerprint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JsxSettings {
    pub mode: JsxMode,
    /// Automatic runtimes import from `<import_source>/jsx-runtime`
    /// (`react` when None).
    pub import_source: Option<String>,
    /// Classic runtime: the element factory (`React.createElement` when None).
    pub factory: Option<String>,
    /// Classic runtime: the fragment component (`React.Fragment` when None).
    pub fragment_factory: Option<String>,
}

impl JsxSettings {
    /// Stable text for `transpile_fingerprint`: every field, unambiguously
    /// (`{:?}` keeps None distinct from an empty string).
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "mode={};import_source={:?};factory={:?};fragment_factory={:?}",
            self.mode.as_str(),
            self.import_source,
            self.factory,
            self.fragment_factory
        )
    }
}

/// The JSX settings for the file at `referrer`: nearest tsconfig, extends
/// chain merged per option. The defaults (automatic runtime from `react`)
/// when there is no tsconfig or it says nothing about JSX.
pub(crate) fn jsx_settings_for(resolver: &Resolver, referrer: &Path) -> JsxSettings {
    let info = info_for_with(resolver, referrer).unwrap_or_default();
    JsxSettings {
        mode: JsxMode::from_tsconfig(info.jsx.as_deref()),
        import_source: info.jsx_import_source,
        factory: info.jsx_factory,
        fragment_factory: info.jsx_fragment_factory,
    }
}

/// Walk up from `referrer` to the nearest tsconfig.json and load its
/// effective paths. None = no tsconfig or no paths configured.
///
/// `resolver` owns the per-resolver tsconfig caches, so a user with a
/// long-lived `Resolver` (CLI tool, daemon) can call
/// `resolver.clear_caches()` to drop stale entries when the project layout
/// changes (e.g. after `oam install` writes a new `node_modules/`).
pub(crate) fn load_for_with(resolver: &Resolver, referrer: &Path) -> Option<PathsConfig> {
    info_for_with(resolver, referrer)?.paths
}

/// The cached core: nearest tsconfig.json upward from `referrer`, with its
/// extends chain merged into a `TsconfigInfo`.
///
/// Runtime semantics: tsconfig options are a PROJECT-source concern. A
/// dependency's own tsconfig.json (shipped in its tarball; 17 packages under
/// bench/sdk-fixtures carry one) must not rewrite the bare imports of its
/// published files or retarget their JSX -- neither Node nor tsc applies it
/// -- so anything under node_modules gets no tsconfig at all.
pub(crate) fn info_for_with(resolver: &Resolver, referrer: &Path) -> Option<TsconfigInfo> {
    if in_node_modules(referrer) {
        return None;
    }
    let tsconfig = find_tsconfig_with(resolver, referrer)?;
    let mut cache = resolver
        .tsconfigs()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = cache.get(&tsconfig) {
        return cached.clone();
    }
    let result = load_chain(&tsconfig, 0);
    cache.insert(tsconfig, result.clone());
    result
}

fn in_node_modules(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}

/// Nearest tsconfig.json at or above `referrer` (a file or a directory).
/// Pure discovery -- no node_modules boundary, no parsing -- backed by the
/// resolver's per-directory cache, so a project with no tsconfig does not
/// re-stat every ancestor on every resolve; `Resolver::clear_caches` drops
/// the entries.
///
/// Twin: `oam_ts::find_tsconfig` (crates/oam_ts/src/lib.rs) is the same walk
/// for `oam check`. oam_ts must not depend on this crate (it would pull oxc
/// and reqwest into the checker), so the two are kept in sync by hand --
/// change one, change the other.
pub(crate) fn find_tsconfig_with(resolver: &Resolver, referrer: &Path) -> Option<PathBuf> {
    let start = if referrer.is_dir() {
        referrer.to_path_buf()
    } else {
        referrer.parent()?.to_path_buf()
    };
    resolver.nearest_tsconfig_from_dir(start)
}

/// Test shim: tsconfig paths for `referrer` via the thread-local default
/// `Resolver`.
#[cfg(test)]
pub(crate) fn load_for(referrer: &Path) -> Option<PathsConfig> {
    load_for_with(crate::resolver::default_resolver(), referrer)
}

/// Load a tsconfig and merge its `extends` chain (child wins per-option;
/// paths resolve against the file that DECLARED them, per TS 7). None only
/// when the file itself is unusable (unreadable, not JSONC, extends cycle);
/// every such case queues a warning so the silence is never total.
fn load_chain(tsconfig: &Path, depth: u8) -> Option<TsconfigInfo> {
    if depth > 8 {
        // extends cycle / absurd chain: give up on this branch.
        warn_once(
            tsconfig,
            "OAM-MOD0009",
            format!(
                "tsconfig extends chain through {} is deeper than 8 levels (a cycle?); \
                 options inherited beyond it are unavailable",
                tsconfig.display()
            ),
        );
        return None;
    }
    let raw = match std::fs::read_to_string(tsconfig) {
        Ok(raw) => raw,
        Err(e) => {
            warn_once(
                tsconfig,
                "OAM-MOD0009",
                format!(
                    "cannot read tsconfig {}: {e}; the options it declares are unavailable",
                    tsconfig.display()
                ),
            );
            return None;
        }
    };
    // Windows editors love BOMs; tsgo accepts them, so we must too — a
    // BOM'd tsconfig silently disabling paths was a real parity bug.
    let raw = raw.trim_start_matches('\u{feff}');
    let parsed: RawTsconfig = match serde_json::from_str(&strip_jsonc(raw)) {
        Ok(parsed) => parsed,
        Err(e) => {
            warn_once(
                tsconfig,
                "OAM-MOD0009",
                format!(
                    "tsconfig {} is not valid JSONC ({e}); its compilerOptions \
                     (paths, jsx settings) are ignored",
                    tsconfig.display()
                ),
            );
            return None;
        }
    };
    let dir = tsconfig.parent()?.to_path_buf();
    let options = parsed.compiler_options;

    let own = TsconfigInfo {
        paths: options.paths.map(|paths| {
            let base_dir = dir.clone();
            let patterns = paths
                .into_iter()
                .map(|(key, value)| {
                    let subs = value
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    (key, subs)
                })
                .collect();
            PathsConfig { base_dir, patterns }
        }),
        jsx: options.jsx,
        jsx_import_source: options.jsx_import_source,
        jsx_factory: options.jsx_factory,
        jsx_fragment_factory: options.jsx_fragment_factory,
    };
    if own.is_complete() {
        return Some(own);
    }

    // Some option is missing here: the extends chain may carry it. Entries
    // of a TS 5.0 array merge left to right (a later entry wins over an
    // earlier one), and the declaring file wins over all of them.
    let mut inherited: Option<TsconfigInfo> = None;
    for extends in parsed.extends.map(Extends::into_vec).unwrap_or_default() {
        let is_path = extends.starts_with("./")
            || extends.starts_with("../")
            || Path::new(&extends).is_absolute();
        let parent = if is_path {
            // TS resolves './tsconfig.base' by trying the exact path, then with
            // '.json' APPENDED (never set_extension — dotted basenames are normal).
            let mut parent = dir.join(&extends);
            if !parent.is_file() {
                let mut with_json = parent.into_os_string();
                with_json.push(".json");
                parent = PathBuf::from(with_json);
            }
            load_chain(&parent, depth + 1)
        } else {
            // Bare-package extends needs npm resolution, which the loader does
            // not do for tsconfig yet. The declaring file's own options stay
            // in effect; only what it would have INHERITED is missing.
            warn_once(
                tsconfig,
                "OAM-MOD0008",
                format!(
                    "tsconfig extends {extends:?} names a package, which oam does not \
                     resolve yet; options inherited from {extends:?} are unavailable \
                     (declared in {})",
                    tsconfig.display()
                ),
            );
            None
        };
        if let Some(parent) = parent {
            inherited = Some(parent.merged_over(inherited));
        }
    }

    Some(own.merged_over(inherited))
}

/// Candidate raw paths for a bare specifier, per TypeScript's precedence.
/// Empty = no pattern matched.
pub(crate) fn match_specifier(config: &PathsConfig, specifier: &str) -> Vec<PathBuf> {
    // Exact key wins outright.
    if let Some((_, subs)) = config
        .patterns
        .iter()
        .find(|(key, _)| !key.contains('*') && key == specifier)
    {
        return subs.iter().map(|s| config.base_dir.join(s)).collect();
    }

    // Otherwise: single-* patterns, longest matched prefix wins; the strict
    // `>` keeps the first DECLARED pattern on a tie, as TypeScript's
    // findBestPatternMatch does.
    let mut best: Option<(usize, &Vec<String>, &str)> = None;
    for (key, subs) in &config.patterns {
        let Some(star) = key.find('*') else { continue };
        let (prefix, suffix) = (&key[..star], &key[star + 1..]);
        if specifier.len() >= prefix.len() + suffix.len()
            && specifier.starts_with(prefix)
            && specifier.ends_with(suffix)
        {
            let matched = &specifier[prefix.len()..specifier.len() - suffix.len()];
            if best.is_none_or(|(len, _, _)| prefix.len() > len) {
                best = Some((prefix.len(), subs, matched));
            }
        }
    }
    let Some((_, subs, matched)) = best else {
        return Vec::new();
    };
    subs.iter()
        .map(|s| config.base_dir.join(s.replace('*', matched)))
        .collect()
}

/// Strip JSONC down to JSON: // and /* */ comments become spaces (string
/// contents untouched), then trailing commas before } or ] are dropped.
pub(crate) fn strip_jsonc(input: &str) -> String {
    #[derive(PartialEq)]
    enum State {
        Normal,
        InString { escaped: bool },
        LineComment,
        BlockComment { star: bool },
    }
    let mut out = String::with_capacity(input.len());
    let mut state = State::Normal;
    for c in input.chars() {
        match &mut state {
            State::Normal => match c {
                '"' => {
                    state = State::InString { escaped: false };
                    out.push(c);
                }
                '/' => {
                    // Peek handled by a marker: emit nothing yet, decide on
                    // the NEXT char. Simplify by encoding the pending slash.
                    state = State::LineComment; // provisional; fixed below
                    out.push('\u{0}'); // placeholder, replaced or kept as '/'
                }
                _ => out.push(c),
            },
            State::InString { escaped } => {
                if *escaped {
                    *escaped = false;
                } else if c == '\\' {
                    *escaped = true;
                } else if c == '"' {
                    state = State::Normal;
                }
                out.push(c);
            }
            State::LineComment => {
                // Disambiguate the provisional slash: the placeholder is the
                // last char of `out` exactly when we just saw the first '/'.
                if out.ends_with('\u{0}') {
                    out.pop();
                    match c {
                        '/' => { /* real line comment: emit nothing */ }
                        '*' => {
                            state = State::BlockComment { star: false };
                        }
                        _ => {
                            // Lone slash (invalid JSON anyway): keep both.
                            out.push('/');
                            out.push(c);
                            state = State::Normal;
                        }
                    }
                } else if c == '\n' {
                    out.push('\n');
                    state = State::Normal;
                }
                // else: swallow comment chars
            }
            State::BlockComment { star } => {
                if *star && c == '/' {
                    out.push(' ');
                    state = State::Normal;
                } else {
                    *star = c == '*';
                }
            }
        }
    }

    // Second pass: drop trailing commas (string-aware).
    let mut cleaned = String::with_capacity(out.len());
    let mut in_string = false;
    let mut escaped = false;
    let bytes: Vec<char> = out.chars().collect();
    for (i, &c) in bytes.iter().enumerate() {
        if in_string {
            cleaned.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                cleaned.push(c);
            }
            ',' => {
                // bytes[i+1..] is empty when comma is last char; .iter().find() returns None, comma is preserved, and the malformed JSON surfaces as an OAM-MOD0009 warning from load_chain.
                let next_meaningful = bytes[i + 1..].iter().find(|c| !c.is_whitespace());
                if matches!(next_meaningful, Some('}') | Some(']')) {
                    continue; // trailing comma: drop
                }
                cleaned.push(c);
            }
            _ => cleaned.push(c),
        }
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::default_resolver;

    /// A fresh, empty temp dir unique to this test invocation.
    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CNT: AtomicU64 = AtomicU64::new(0);
        let id = CNT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("oam-tsc-{tag}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn strips_comments_and_trailing_commas() {
        let jsonc = r#"{
  // line comment
  "a": "value // not a comment",
  /* block
     comment */
  "b": [1, 2, 3,],
  "c": "/* also not a comment */",
}"#;
        let parsed: serde_json::Value = serde_json::from_str(&strip_jsonc(jsonc)).unwrap();
        assert_eq!(parsed["a"], "value // not a comment");
        assert_eq!(parsed["b"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["c"], "/* also not a comment */");
    }

    fn config(patterns: &[(&str, &[&str])]) -> PathsConfig {
        PathsConfig {
            base_dir: PathBuf::from("/proj"),
            patterns: patterns
                .iter()
                .map(|(k, subs)| (k.to_string(), subs.iter().map(|s| s.to_string()).collect()))
                .collect(),
        }
    }

    #[test]
    fn exact_key_beats_patterns() {
        let cfg = config(&[
            ("@lib/*", &["src/lib/*"]),
            ("@lib/special", &["src/special.ts"]),
        ]);
        let candidates = match_specifier(&cfg, "@lib/special");
        assert_eq!(
            candidates,
            vec![PathBuf::from("/proj").join("src/special.ts")]
        );
    }

    #[test]
    fn longest_prefix_wins_and_star_substitutes() {
        let cfg = config(&[("@app/*", &["src/*"]), ("@app/deep/*", &["src/deep/*"])]);
        let candidates = match_specifier(&cfg, "@app/deep/thing");
        assert_eq!(
            candidates,
            vec![PathBuf::from("/proj").join("src/deep/thing")]
        );
    }

    #[test]
    fn multiple_substitutions_stay_ordered() {
        let cfg = config(&[("#u/*", &["a/*", "b/*"])]);
        let candidates = match_specifier(&cfg, "#u/x");
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/proj").join("a/x"),
                PathBuf::from("/proj").join("b/x")
            ]
        );
    }

    #[test]
    fn no_match_is_empty() {
        let cfg = config(&[("@lib/*", &["src/lib/*"])]);
        assert!(match_specifier(&cfg, "lodash").is_empty());
    }

    /// Two patterns with the same prefix length ("foo/*.js" and "foo/*"
    /// both have prefix "foo/") tie on the longest-prefix rule; TypeScript
    /// keeps the first DECLARED one. That is only observable when the map
    /// preserves declaration order -- a sorted map put "foo/*" first and
    /// picked b/x.js for `foo/x.js` where tsc picks a/x.ts.
    #[test]
    fn paths_tie_keeps_first_declared_pattern() {
        let dir = temp_dir("tie");
        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "foo/*.js": ["./a/*.ts"], "foo/*": ["./b/*"] } } }"#,
        )
        .unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        let cfg = load_for(&dir.join("entry.ts")).expect("paths present");
        let keys: Vec<&str> = cfg.patterns.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["foo/*.js", "foo/*"], "declaration order kept");
        assert_eq!(
            match_specifier(&cfg, "foo/x.js"),
            vec![dir.join("./a/x.ts")],
            "first declared pattern wins the tie"
        );

        // Reversed declaration -> the other pattern wins the same tie.
        let dir2 = temp_dir("tie-rev");
        std::fs::write(
            dir2.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "foo/*": ["./b/*"], "foo/*.js": ["./a/*.ts"] } } }"#,
        )
        .unwrap();
        std::fs::write(dir2.join("entry.ts"), "").unwrap();
        let cfg2 = load_for(&dir2.join("entry.ts")).expect("paths present");
        assert_eq!(
            match_specifier(&cfg2, "foo/x.js"),
            vec![dir2.join("./b/x.js")]
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn bom_prefixed_tsconfig_still_supplies_paths() {
        // Windows editors write BOMs; tsgo accepts them. A BOM'd tsconfig
        // silently disabling paths was a real probe-found parity bug.
        let dir = temp_dir("bom");
        std::fs::write(
            dir.join("tsconfig.json"),
            "\u{feff}{ \"compilerOptions\": { \"paths\": { \"@b/*\": [\"./lib/*\"] } } }",
        )
        .unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        let cfg = load_for(&dir.join("entry.ts")).expect("BOM must not disable paths");
        assert!(!match_specifier(&cfg, "@b/util").is_empty());
    }

    #[test]
    fn extends_chain_supplies_paths() {
        let dir = temp_dir("chain");
        std::fs::write(
            dir.join("tsconfig.base.json"),
            r#"{ "compilerOptions": { "paths": { "@x/*": ["lib/*"] } } }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{ "extends": "./tsconfig.base", "compilerOptions": { "strict": true } }"#,
        )
        .unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        let cfg = load_for(&dir.join("entry.ts")).expect("paths via extends");
        // baseUrl defaults to the dir of the file DECLARING the paths.
        assert_eq!(cfg.base_dir, dir);
        assert_eq!(cfg.patterns.len(), 1);
        assert!(!match_specifier(&cfg, "@x/util").is_empty());
    }

    /// TypeScript 5.0 `"extends": [...]`: entries merge left to right with a
    /// later entry winning, and the declaring file wins over all of them.
    /// Before this shape was accepted the whole tsconfig failed to
    /// deserialize and every option was silently dropped.
    #[test]
    fn extends_array_merges_in_order_later_entry_wins() {
        let dir = temp_dir("extends-array");
        std::fs::write(
            dir.join("base1.json"),
            r#"{"compilerOptions":{"paths":{"@one/*":["./one/*"]},"jsxImportSource":"preact","jsxFactory":"h"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("base2.json"),
            r#"{"compilerOptions":{"jsxImportSource":"solid-js"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{"extends":["./base1.json","./base2.json"],"compilerOptions":{"jsx":"react"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("entry.tsx"), "").unwrap();
        let entry = dir.join("entry.tsx");
        let info = info_for_with(default_resolver(), &entry).expect("tsconfig loads");
        let keys: Vec<&str> = info
            .paths
            .as_ref()
            .expect("paths inherited from base1")
            .patterns
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, vec!["@one/*"]);
        assert_eq!(
            info.jsx_import_source.as_deref(),
            Some("solid-js"),
            "the later array entry wins over the earlier one"
        );
        assert_eq!(
            info.jsx_factory.as_deref(),
            Some("h"),
            "base1's unique option survives"
        );
        assert_eq!(
            info.jsx.as_deref(),
            Some("react"),
            "own declaration wins over both"
        );
        let settings = jsx_settings_for(default_resolver(), &entry);
        assert_eq!(settings.mode, JsxMode::Classic);
        assert_eq!(settings.import_source.as_deref(), Some("solid-js"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extends_cycle_depth_limit_returns_none() {
        // load_chain's depth > 8 guard gives up on extends cycles: a -> b ->
        // a -> b -> ... recurses until depth > 8, then that branch yields
        // None (with one deferred warning). Must NOT panic and must NOT
        // infinite-loop; no file in the cycle declares paths, so the merged
        // result carries none.
        let _serial = crate::warnings::test_serial();
        let dir = temp_dir("cycle");
        std::fs::write(dir.join("tsconfig.json"), r#"{ "extends": "./a.json" }"#).unwrap();
        std::fs::write(dir.join("a.json"), r#"{ "extends": "./b.json" }"#).unwrap();
        std::fs::write(dir.join("b.json"), r#"{ "extends": "./a.json" }"#).unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        let result = load_for(&dir.join("entry.ts"));
        assert!(
            result.is_none(),
            "extends cycle should give up quietly, got: {:?}",
            result
        );
        let cycle_warnings: Vec<_> = crate::take_warnings()
            .into_iter()
            .filter(|d| d.code == "OAM-MOD0009" && d.message.contains("deeper than 8 levels"))
            .collect();
        assert!(
            !cycle_warnings.is_empty(),
            "the cycle must surface as a deferred warning, not pure silence"
        );
    }

    #[test]
    fn match_specifier_multiple_stars_in_key() {
        // #14: match_specifier uses key.find('*'), which returns the FIRST
        // star. For "@lib/*/foo/*" with specifier "@lib/x/foo/y":
        //   prefix = "@lib/"
        //   suffix = "/foo/*"  (literal asterisk at the end)
        // The specifier "@lib/x/foo/y" starts with "@lib/" (true) but does
        // NOT end with "/foo/*" (the suffix has a literal '*' that the
        // specifier lacks). The pattern doesn't match, so the function
        // returns an empty Vec.
        //
        // This is silent garbage: a key with multiple stars is not supported
        // (TS 7 only allows one star per pattern), but match_specifier
        // doesn't error -- it just returns nothing useful. Callers can't
        // distinguish "no pattern matched" from "pattern was malformed".
        let cfg = config(&[("@lib/*/foo/*", &["src/lib/*/foo/*"])]);
        let candidates = match_specifier(&cfg, "@lib/x/foo/y");
        assert!(
            candidates.is_empty(),
            "multi-star key should not match (silent garbage), got: {:?}",
            candidates
        );
    }

    #[test]
    fn strip_jsonc_unterminated_block_comment_does_not_panic() {
        // #15: a block comment that never closes (no "*/" before EOF).
        // The block-comment state eats to EOF, leaving "{ \"a\": 1 " intact
        // (the space after "1" is preserved, the "/* unterminated" is
        // consumed). The trailing-comma pass is a no-op (no trailing
        // commas). The function must NOT panic; the result is malformed
        // JSON that serde_json::from_str rejects with Err, which load_chain
        // reports as an OAM-MOD0009 warning (see
        // `malformed_tsconfig_warns_once_and_yields_none`).
        let input = r#"{ "a": 1 /* unterminated"#;
        let result = strip_jsonc(input);
        // Document the exact output: the block comment consumed everything
        // from "/*" to EOF, leaving the prefix intact.
        assert_eq!(result, r#"{ "a": 1 "#);
        // The result must be parseable as an Err (not a panic).
        let parse_result: Result<serde_json::Value, _> = serde_json::from_str(&result);
        assert!(
            parse_result.is_err(),
            "unterminated block comment should produce invalid JSON, got: {:?}",
            result
        );
    }

    /// A tsconfig that does not parse used to vanish through `.ok()?`: paths
    /// and jsx settings silently dropped, and MOD0002 did not even carry
    /// the "paths were consulted" hint. Now it is one deferred warning per
    /// path, and the config is (still) unusable.
    #[test]
    fn malformed_tsconfig_warns_once_and_yields_none() {
        let _serial = crate::warnings::test_serial();
        let dir = temp_dir("malformed");
        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "@x/*": ["./x/*"] } "#,
        )
        .unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        let entry = dir.join("entry.ts");
        let resolver = Resolver::new();
        assert!(load_for_with(&resolver, &entry).is_none());
        // A second resolver on the same file re-parses (its own cache is
        // empty) but must not warn again: dedupe is per path, process-wide.
        let other = Resolver::new();
        assert!(load_for_with(&other, &entry).is_none());
        let mine: Vec<_> = crate::take_warnings()
            .into_iter()
            .filter(|d| d.message.contains(&dir.display().to_string()))
            .collect();
        assert_eq!(mine.len(), 1, "exactly one warning for the path: {mine:?}");
        assert_eq!(mine[0].code, "OAM-MOD0009");
        assert_eq!(mine[0].severity, Severity::Warning);
        assert!(
            mine[0].message.contains("not valid JSONC"),
            "message: {}",
            mine[0].message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bare-package extends is skipped, but the declaring file's own options
    /// stay in effect; the warning must say so (it used to claim the file's
    /// paths "will be unavailable" while they resolved fine) and must be an
    /// ODIF warning in the sink, never prose on stderr.
    #[test]
    fn bare_extends_warns_about_inherited_options_only() {
        let _serial = crate::warnings::test_serial();
        let dir = temp_dir("bare-extends");
        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{"extends":"@tsconfig/node20","compilerOptions":{"paths":{"@x/*":["./x/*"]}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        let entry = dir.join("entry.ts");
        let cfg = load_for(&entry).expect("own paths stay in effect");
        assert!(!match_specifier(&cfg, "@x/util").is_empty());
        let mine: Vec<_> = crate::take_warnings()
            .into_iter()
            .filter(|d| d.message.contains(&dir.display().to_string()))
            .collect();
        assert_eq!(mine.len(), 1, "got {mine:?}");
        assert_eq!(mine[0].code, "OAM-MOD0008");
        assert!(
            mine[0]
                .message
                .contains("options inherited from \"@tsconfig/node20\" are unavailable"),
            "message: {}",
            mine[0].message
        );
        assert!(
            !mine[0].message.contains("will be unavailable"),
            "the old, false wording must be gone: {}",
            mine[0].message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_jsonc_line_comment_with_backslash() {
        // #16: a line comment ends at "\n". A backslash before "\n" does NOT
        // escape the newline -- line comments are not string-aware for line
        // endings. The line comment swallows " backslash at end \" (the
        // backslash is not "\n"), then the "\n" terminates the comment and
        // IS pushed to the output (the newline is preserved).
        //
        // Regular string: "\\" is one backslash, "\n" is a newline.
        // Input:  "// backslash at end \<newline>{ \"a\": 1 }"
        // Output: "\n{ \"a\": 1 }"  (the leading newline is the comment
        // terminator that was pushed to output)
        let input = "// backslash at end \\\n{ \"a\": 1 }";
        let result = strip_jsonc(input);
        assert_eq!(result, "\n{ \"a\": 1 }");
    }

    #[test]
    fn load_for_walks_up_to_nearest_tsconfig() {
        // #17: when nested/tsconfig.json exists, load_for uses it and does
        // NOT walk up to root/tsconfig.json. The walk-up stops at the first
        // tsconfig.json it finds.
        let dir = temp_dir("nested");
        let root = dir.join("root");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "@root/*": ["root-src/*"] } } }"#,
        )
        .unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(
            nested.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "@nested/*": ["nested-src/*"] } } }"#,
        )
        .unwrap();
        std::fs::write(nested.join("entry.ts"), "").unwrap();
        let cfg = load_for(&nested.join("entry.ts")).expect("nested tsconfig must be found");
        // Must be the NESTED config, not the root one.
        let keys: Vec<&str> = cfg.patterns.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            keys.contains(&"@nested/*"),
            "expected @nested/* in patterns, got: {:?}",
            keys
        );
        assert!(
            !keys.contains(&"@root/*"),
            "should NOT have walked up to @root/*, got: {:?}",
            keys
        );
        // base_dir should be the nested dir (the declaring tsconfig's dir).
        assert_eq!(cfg.base_dir, nested);
        // Sanity: the nested path actually resolves.
        assert!(!match_specifier(&cfg, "@nested/thing").is_empty());
    }

    /// A dependency's own tsconfig.json must not touch its published files:
    /// node_modules/dep/tsconfig.json with `"*": ["./shim/*"]` used to make
    /// `require('peer')` from inside dep load dep/shim/peer.js instead of
    /// the real node_modules/peer (Node loads the real package). Mirrors the
    /// live probe that found it.
    #[test]
    fn node_modules_referrer_gets_no_tsconfig() {
        let dir = temp_dir("nm-boundary");
        let dep = dir.join("node_modules/dep");
        std::fs::create_dir_all(dep.join("shim")).unwrap();
        std::fs::write(
            dep.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "*": ["./shim/*"] } } }"#,
        )
        .unwrap();
        std::fs::write(dep.join("package.json"), r#"{"name":"dep"}"#).unwrap();
        std::fs::write(dep.join("shim/peer.js"), "module.exports = 'SHIM';").unwrap();
        std::fs::write(dep.join("index.js"), "").unwrap();
        let peer = dir.join("node_modules/peer");
        std::fs::create_dir_all(&peer).unwrap();
        std::fs::write(
            peer.join("package.json"),
            r#"{"name":"peer","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(peer.join("index.js"), "module.exports = 'REAL';").unwrap();
        // The project itself has a tsconfig too; it must still apply to
        // project files (the boundary is about the REFERRER, not the tree).
        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "@p/*": ["./src/*"] } } }"#,
        )
        .unwrap();
        std::fs::write(dir.join("entry.js"), "").unwrap();

        let resolver = Resolver::new();
        assert!(
            load_for_with(&resolver, &dep.join("index.js")).is_none(),
            "a file under node_modules gets no tsconfig at all"
        );
        assert!(
            info_for_with(&resolver, &dep.join("index.js")).is_none(),
            "nor jsx settings from the dependency's tsconfig"
        );
        assert!(
            load_for_with(&resolver, &dir.join("entry.js")).is_some(),
            "the project's own files keep their tsconfig"
        );
        let resolved = resolver
            .resolve_require("peer", &dep.join("index.js"))
            .expect("peer resolves through node_modules");
        assert!(
            resolved.ends_with(Path::new("node_modules").join("peer").join("index.js")),
            "must be the real package, not dep/shim/peer.js: {resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Discovery is cached per directory on the Resolver: a project with no
    /// tsconfig records the miss (no re-walk to the filesystem root on every
    /// resolve), and `clear_caches` is how a later-written tsconfig becomes
    /// visible.
    #[test]
    fn find_tsconfig_caches_misses_until_clear_caches() {
        let dir = temp_dir("discover");
        let deep = dir.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("entry.ts"), "").unwrap();
        let entry = deep.join("entry.ts");
        let resolver = Resolver::new();
        assert_eq!(resolver.find_tsconfig(&entry), None);

        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
        assert_eq!(
            resolver.find_tsconfig(&entry),
            None,
            "the miss is cached: no re-stat until the caller clears"
        );
        resolver.clear_caches();
        assert_eq!(
            resolver.find_tsconfig(&entry),
            Some(dir.join("tsconfig.json")),
            "after clear_caches the walk runs again and finds it"
        );
        // A directory referrer starts the walk AT that directory.
        assert_eq!(
            resolver.find_tsconfig(&dir),
            Some(dir.join("tsconfig.json"))
        );
        // Pure discovery: no node_modules boundary here (that is applied
        // when the OPTIONS are consumed, in info_for_with).
        let nm = dir.join("node_modules/dep");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("tsconfig.json"), "{}").unwrap();
        std::fs::write(nm.join("index.js"), "").unwrap();
        assert_eq!(
            resolver.find_tsconfig(&nm.join("index.js")),
            Some(nm.join("tsconfig.json"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jsx_mode_maps_every_tsconfig_value() {
        assert_eq!(JsxMode::from_tsconfig(Some("react")), JsxMode::Classic);
        assert_eq!(
            JsxMode::from_tsconfig(Some("react-jsx")),
            JsxMode::Automatic
        );
        assert_eq!(
            JsxMode::from_tsconfig(Some("react-jsxdev")),
            JsxMode::AutomaticDev
        );
        // Cannot be honored at runtime: compiled as automatic, documented.
        assert_eq!(JsxMode::from_tsconfig(Some("preserve")), JsxMode::Automatic);
        assert_eq!(
            JsxMode::from_tsconfig(Some("react-native")),
            JsxMode::Automatic
        );
        assert_eq!(JsxMode::from_tsconfig(None), JsxMode::Automatic);
    }

    /// jsxImportSource and paths merge INDEPENDENTLY across the extends
    /// chain. The interesting case is a child that declares only one of
    /// them: before this option existed, load_chain returned the moment it
    /// saw `paths` and never read the parent at all.
    #[test]
    fn jsx_import_source_and_paths_merge_independently() {
        let dir = temp_dir("jsx");
        std::fs::write(
            dir.join("base.json"),
            r#"{"compilerOptions":{"jsxImportSource":"preact","paths":{"@base/*":["./b/*"]}}}"#,
        )
        .unwrap();
        // Child declares paths ONLY -> inherits jsxImportSource, overrides paths.
        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{"extends":"./base.json","compilerOptions":{"paths":{"@child/*":["./c/*"]}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("entry.tsx"), "").unwrap();
        let entry = dir.join("entry.tsx");
        assert_eq!(
            jsx_settings_for(default_resolver(), &entry)
                .import_source
                .as_deref(),
            Some("preact")
        );
        let cfg = load_for(&entry).expect("paths present");
        let keys: Vec<&str> = cfg.patterns.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["@child/*"], "child paths replace parent's");

        // Child override wins over the extended base.
        let over = dir.join("over");
        std::fs::create_dir_all(&over).unwrap();
        std::fs::write(
            over.join("tsconfig.json"),
            r#"{"extends":"../base.json","compilerOptions":{"jsxImportSource":"solid-js"}}"#,
        )
        .unwrap();
        std::fs::write(over.join("e.tsx"), "").unwrap();
        assert_eq!(
            jsx_settings_for(default_resolver(), &over.join("e.tsx"))
                .import_source
                .as_deref(),
            Some("solid-js")
        );

        // No tsconfig at all -> the defaults, never a fabricated source.
        let bare = temp_dir("jsx-bare");
        std::fs::write(bare.join("e.tsx"), "").unwrap();
        let settings = jsx_settings_for(default_resolver(), &bare.join("e.tsx"));
        assert_eq!(settings.import_source, None);
        assert_eq!(settings.mode, JsxMode::Automatic);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&bare);
    }
}
