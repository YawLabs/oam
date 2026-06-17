//! tsconfig discovery + the `paths` mapping subset the resolver honors.
//!
//! Contract (plan §2.6): the resolver consults tsconfig EXACTLY as tsgo
//! does, so `oam run` and `oam check` never disagree about what an import
//! means. v1 surface: compilerOptions.paths from the nearest
//! tsconfig.json, with relative `extends` chains merged (child wins).
//! Bare-package extends ("@tsconfig/node20") needs npm resolution and is
//! skipped until M2. Matching follows TypeScript 7: exact keys beat
//! patterns, one `*` per pattern, longest matched prefix wins,
//! substitutions tried in order, resolved against the DECLARING
//! tsconfig's directory (TS 7 removed baseUrl entirely — TS5102 — and
//! requires relative substitutions — TS5090; both probed against tsgo).
//!
//! tsconfig.json is JSONC: comments and trailing commas are stripped
//! before serde sees it.

use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::resolver::Resolver;

/// Dedupe the bare-package-extends warning: `load_for` is called per-resolve,
/// so an un-deduped eprintln would fire N times for a project with N resolves.
/// `Mutex<HashSet>` gives at-most-once-per-path warn behavior across threads.
/// Insert returns true the first time a given tsconfig path warns -- gate the
/// eprintln on that.
fn warned_bare_extends() -> &'static Mutex<HashSet<PathBuf>> {
    use std::sync::OnceLock;
    static SET: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

#[derive(Debug, Clone)]
pub(crate) struct PathsConfig {
    /// The declaring tsconfig's directory; substitutions resolve here.
    pub base_dir: PathBuf,
    /// (pattern, substitutions) in declaration order.
    pub patterns: Vec<(String, Vec<String>)>,
}

#[derive(Deserialize, Default)]
struct RawTsconfig {
    #[serde(default)]
    extends: Option<String>,
    #[serde(default, rename = "compilerOptions")]
    compiler_options: RawCompilerOptions,
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
}

/// Walk up from `referrer` to the nearest tsconfig.json and load its
/// effective paths. None = no tsconfig or no paths configured.
///
/// `resolver` owns the per-resolver tsconfig cache, so a user with a
/// long-lived `Resolver` (CLI tool, daemon) can call
/// `resolver.clear_caches()` to drop stale entries when the project layout
/// changes (e.g. after `oam install` writes a new `node_modules/`).
pub(crate) fn load_for_with(resolver: &Resolver, referrer: &Path) -> Option<PathsConfig> {
    let mut dir = if referrer.is_dir() {
        referrer.to_path_buf()
    } else {
        referrer.parent()?.to_path_buf()
    };
    loop {
        let candidate = dir.join("tsconfig.json");
        if candidate.is_file() {
            let mut cache = resolver
                .tsconfigs()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = cache.get(&candidate) {
                return cached.clone();
            }
            let result = load_chain(&candidate, 0);
            cache.insert(candidate, result.clone());
            return result;
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Shim: load tsconfig paths for `referrer` using the thread-local default
/// `Resolver`. Existing callers (and tests) keep working unchanged.
pub(crate) fn load_for(referrer: &Path) -> Option<PathsConfig> {
    load_for_with(crate::resolver::default_resolver(), referrer)
}

/// Load a tsconfig and merge its relative `extends` chain (child wins;
/// paths resolve against the file that DECLARED them, per TS 7).
fn load_chain(tsconfig: &Path, depth: u8) -> Option<PathsConfig> {
    if depth > 8 {
        return None; // extends cycle / absurd chain: give up quietly
    }
    let raw = std::fs::read_to_string(tsconfig).ok()?;
    // Windows editors love BOMs; tsgo accepts them, so we must too — a
    // BOM'd tsconfig silently disabling paths was a real parity bug.
    let raw = raw.trim_start_matches('\u{feff}');
    let parsed: RawTsconfig = serde_json::from_str(&strip_jsonc(raw)).ok()?;
    let dir = tsconfig.parent()?.to_path_buf();

    let own = parsed.compiler_options.paths.map(|paths| {
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
    });
    if let Some(config) = own {
        return Some(config); // child paths replace parent paths entirely (TS semantics)
    }

    // No paths here: a relative extends may carry them.
    let extends = parsed.extends?;
    if !(extends.starts_with("./") || extends.starts_with("../")) {
        let mut set = warned_bare_extends()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if set.insert(tsconfig.to_path_buf()) {
            eprintln!(
                "OAM-MOD: bare-package tsconfig extends ({:?}) is not yet resolved; \
                 tsconfig paths from {:?} will be unavailable",
                extends, tsconfig
            );
        }
        return None; // bare-package extends: needs npm resolution (M2)
    }
    // TS resolves './tsconfig.base' by trying the exact path, then with
    // '.json' APPENDED (never set_extension — dotted basenames are normal).
    let mut parent = dir.join(&extends);
    if !parent.is_file() {
        let mut with_json = parent.into_os_string();
        with_json.push(".json");
        parent = PathBuf::from(with_json);
    }
    load_chain(&parent, depth + 1)
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

    // Otherwise: single-* patterns, longest matched prefix wins.
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
                // bytes[i+1..] is empty when comma is last char; .iter().find() returns None, comma is preserved, downstream serde_json::from_str(...).ok()? handles the malformed JSON cleanly.
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

    #[test]
    fn bom_prefixed_tsconfig_still_supplies_paths() {
        // Windows editors write BOMs; tsgo accepts them. A BOM'd tsconfig
        // silently disabling paths was a real probe-found parity bug.
        let dir = std::env::temp_dir().join(format!("oam-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        let dir = std::env::temp_dir().join(format!("oam-tsc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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

    #[test]
    fn extends_cycle_depth_limit_returns_none() {
        // #13: load_chain's depth > 8 guard (tsconfig.rs:104-110) gives up
        // silently on extends cycles. a -> b -> a -> b -> ... recurses until
        // depth > 8, then returns None. Must NOT panic and must NOT
        // infinite-loop.
        //
        // load_for walks up looking for tsconfig.json, so we need a
        // tsconfig.json that extends a.json to enter the extends chain.
        let dir = std::env::temp_dir().join(format!("oam-tsc-cycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tsconfig.json"), r#"{ "extends": "./a.json" }"#).unwrap();
        std::fs::write(dir.join("a.json"), r#"{ "extends": "./b.json" }"#).unwrap();
        std::fs::write(dir.join("b.json"), r#"{ "extends": "./a.json" }"#).unwrap();
        std::fs::write(dir.join("entry.ts"), "").unwrap();
        // Must NOT panic and must NOT infinite-loop.
        let result = load_for(&dir.join("entry.ts"));
        assert!(
            result.is_none(),
            "extends cycle should give up quietly, got: {:?}",
            result
        );
    }

    #[test]
    fn match_specifier_multiple_stars_in_key() {
        // #14: match_specifier uses key.find('*') (tsconfig.rs:177), which
        // returns the FIRST star. For "@lib/*/foo/*" with specifier
        // "@lib/x/foo/y":
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
        // The block-comment state eats to EOF (tsconfig.rs:257-264), leaving
        // "{ \"a\": 1 " intact (the space after "1" is preserved, the
        // "/* unterminated" is consumed). The trailing-comma pass is a no-op
        // (no trailing commas). The function must NOT panic; the result is
        // malformed JSON that serde_json::from_str rejects with Err.
        // load_chain handles the Err via .ok()? (tsconfig.rs:111), so an
        // unterminated comment in a tsconfig.json silently disables paths.
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

    #[test]
    fn strip_jsonc_line_comment_with_backslash() {
        // #16: a line comment ends at "\n" (tsconfig.rs:251-253). A backslash
        // before "\n" does NOT escape the newline -- line comments are not
        // string-aware for line endings. The line comment swallows
        // " backslash at end \" (the backslash is not "\n"), then the "\n"
        // terminates the comment and IS pushed to the output (see
        // tsconfig.rs:251-253: the newline is preserved).
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
        // NOT walk up to root/tsconfig.json. The walk-up at tsconfig.rs:75-90
        // stops at the first tsconfig.json it finds.
        let dir = std::env::temp_dir().join(format!("oam-tsc-nested-{}", std::process::id()));
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
}
