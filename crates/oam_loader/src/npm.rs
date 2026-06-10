//! Bare-specifier resolution against existing node_modules trees (M2: the
//! installer lands in M3; until then oam runs alongside npm/pnpm installs).
//!
//! Implements the Node ESM algorithm: walk node_modules upward from the
//! referrer; inside a package honor the `exports` map (exclusive when
//! present — exact subpaths, single-`*` wildcards with longest-prefix
//! precedence, condition objects in declaration order with conditions
//! ["node", "import", "default"], arrays as fallback chains, null as an
//! explicit block) and otherwise the legacy main/index rules.
//!
//! Documented divergence: for legacy entry resolution we honor the
//! bundler-standard "module" field before "main" — Node ignores it, but it
//! is the difference between running a large class of dual-published ESM
//! packages today versus gating them on CJS interop. Revisit when CJS
//! interop lands.
//!
//! Execution gates (precise diagnostics, never silent): resolved CJS
//! entries -> OAM-MOD0005 (interop is the next slice); node builtins
//! (prefixed or bare) -> OAM-MOD0006 (compat wave 1); exports-blocked
//! subpaths -> OAM-MOD0007 (Node's ERR_PACKAGE_PATH_NOT_EXPORTED).

use oam_diagnostics::{Diagnostic, Origin, Severity};
use serde_json::Value;
use std::path::{Path, PathBuf};

const CONDITIONS: [&str; 3] = ["node", "import", "default"];

/// Node builtin module names (bare or node:-prefixed) as of Node 26.
const NODE_BUILTINS: [&str; 41] = [
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "worker_threads",
    "zlib",
];

fn diag(code: &str, message: String) -> Diagnostic {
    Diagnostic::new(code, Severity::Error, Origin::Resolve, message)
}

pub(crate) fn is_node_builtin(specifier: &str) -> bool {
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);
    let name = name.split('/').next().unwrap_or(name);
    NODE_BUILTINS.contains(&name)
}

/// Resolve a bare specifier from `referrer` against node_modules.
pub(crate) fn resolve_bare(specifier: &str, referrer: &Path) -> Result<PathBuf, Diagnostic> {
    if specifier.starts_with("node:") || is_node_builtin(specifier) {
        return Err(diag(
            "OAM-MOD0006",
            format!("'{specifier}' is a Node builtin; the node: compat layer lands with M2 wave 1"),
        ));
    }

    let (package, subpath) = split_specifier(specifier).ok_or_else(|| {
        diag(
            "OAM-MOD0002",
            format!("invalid package specifier '{specifier}'"),
        )
    })?;

    let mut searched = 0usize;
    let mut dir = referrer.parent().map(Path::to_path_buf);
    while let Some(current) = dir {
        let package_dir = current.join("node_modules").join(&package);
        if package_dir.is_dir() {
            // Node stops at the first matching package directory: failures
            // inside it are real errors, not reasons to keep walking.
            return resolve_in_package(&package_dir, &package, &subpath, specifier);
        }
        searched += 1;
        dir = current.parent().map(Path::to_path_buf);
    }
    Err(diag(
        "OAM-MOD0002",
        format!(
            "cannot find package '{package}' (searched node_modules in {searched} director{} \
             above {}); is it installed?",
            if searched == 1 { "y" } else { "ies" },
            referrer.display()
        ),
    ))
}

/// "pkg" / "pkg/sub" / "@scope/pkg" / "@scope/pkg/sub" -> (name, "." or "./sub")
fn split_specifier(specifier: &str) -> Option<(String, String)> {
    let mut parts = specifier.splitn(if specifier.starts_with('@') { 3 } else { 2 }, '/');
    let name = if specifier.starts_with('@') {
        let scope = parts.next()?;
        let pkg = parts.next()?;
        if pkg.is_empty() {
            return None;
        }
        format!("{scope}/{pkg}")
    } else {
        parts.next()?.to_string()
    };
    if name.is_empty() {
        return None;
    }
    let rest = parts.next().unwrap_or("");
    let subpath = if rest.is_empty() {
        ".".to_string()
    } else {
        format!("./{rest}")
    };
    Some((name, subpath))
}

fn resolve_in_package(
    package_dir: &Path,
    package: &str,
    subpath: &str,
    specifier: &str,
) -> Result<PathBuf, Diagnostic> {
    let manifest_path = package_dir.join("package.json");
    let manifest: Value = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok())
        .unwrap_or(Value::Null);

    let resolved = if let Some(exports) = manifest.get("exports") {
        let target = exports_resolve(exports, subpath).map_err(|reason| match reason {
            ExportsError::NotExported => diag(
                "OAM-MOD0007",
                format!(
                    "subpath '{subpath}' is not exported by {package} \
                     (its exports map does not allow it)"
                ),
            ),
            ExportsError::InvalidTarget(target) => diag(
                "OAM-MOD0002",
                format!("package {package} has an invalid exports target '{target}'"),
            ),
        })?;
        let path = package_dir.join(target.trim_start_matches("./"));
        if !path.is_file() {
            return Err(diag(
                "OAM-MOD0002",
                format!(
                    "package {package} exports '{subpath}' -> {} which does not exist",
                    path.display()
                ),
            ));
        }
        path
    } else if subpath != "." {
        // No exports map: subpaths hit the filesystem with legacy probing.
        let raw = package_dir.join(subpath.trim_start_matches("./"));
        probe_legacy(&raw).ok_or_else(|| {
            diag(
                "OAM-MOD0002",
                format!("cannot resolve '{specifier}': no file at {}", raw.display()),
            )
        })?
    } else {
        // Legacy entry: module (bundler-standard, see module docs) > main > index.js.
        let entry = ["module", "main"]
            .iter()
            .find_map(|field| manifest.get(*field).and_then(Value::as_str))
            .unwrap_or("index.js");
        let raw = package_dir.join(entry);
        probe_legacy(&raw).ok_or_else(|| {
            diag(
                "OAM-MOD0002",
                format!(
                    "package {package} entry '{entry}' does not resolve to a file under {}",
                    package_dir.display()
                ),
            )
        })?
    };

    // Execution gate: ESM only until CJS interop lands.
    if is_cjs(&resolved, package_dir, &manifest) {
        return Err(diag(
            "OAM-MOD0005",
            format!(
                "package {package} resolved to a CommonJS entry ({}); CJS interop is the next \
                 M2 slice — ESM builds of dual-published packages work today",
                resolved.display()
            ),
        ));
    }
    Ok(resolved)
}

/// Legacy probing: exact, +.js, /index.js.
fn probe_legacy(raw: &Path) -> Option<PathBuf> {
    if raw.is_file() {
        return Some(raw.to_path_buf());
    }
    let with_js = PathBuf::from(format!("{}.js", raw.display()));
    if with_js.is_file() {
        return Some(with_js);
    }
    let index = raw.join("index.js");
    index.is_file().then_some(index)
}

/// .mjs is ESM, .cjs is CJS, .js asks the nearest package.json "type"
/// between the file and the package root (nested package.json files
/// override, per Node).
fn is_cjs(file: &Path, package_dir: &Path, root_manifest: &Value) -> bool {
    match file.extension().and_then(|e| e.to_str()) {
        Some("mjs") => return false,
        Some("cjs") => return true,
        _ => {}
    }
    let mut dir = file.parent();
    while let Some(current) = dir {
        let manifest_path = current.join("package.json");
        if manifest_path.is_file() {
            let manifest: Option<Value> = std::fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok());
            if let Some(manifest) = manifest
                && let Some(kind) = manifest.get("type").and_then(Value::as_str)
            {
                return kind != "module";
            }
            // package.json without "type": CJS default — unless this is the
            // root manifest we already inspected (fall through consistent).
            return root_manifest.get("type").and_then(Value::as_str) != Some("module")
                || current != package_dir;
        }
        if current == package_dir {
            break;
        }
        dir = current.parent();
    }
    root_manifest.get("type").and_then(Value::as_str) != Some("module")
}

enum ExportsError {
    NotExported,
    InvalidTarget(String),
}

/// PACKAGE_EXPORTS_RESOLVE, the subset that matters: exact subpaths,
/// single-* wildcards (longest-prefix wins), condition objects in
/// declaration order, arrays as fallbacks, null blocks.
fn exports_resolve(exports: &Value, subpath: &str) -> Result<String, ExportsError> {
    // Sugar: a top-level string/array/conditions-object is the "." entry.
    let as_map = match exports {
        Value::Object(map) if map.keys().any(|k| k.starts_with('.')) => Some(map),
        _ => None,
    };
    let Some(map) = as_map else {
        if subpath == "." {
            return resolve_target(exports, "");
        }
        return Err(ExportsError::NotExported);
    };

    if let Some(value) = map.get(subpath) {
        return resolve_target(value, "");
    }

    // Wildcards: one '*' per key; the longest static prefix wins.
    let mut best: Option<(usize, &Value, String)> = None;
    for (key, value) in map {
        let Some(star) = key.find('*') else { continue };
        let (prefix, suffix) = (&key[..star], &key[star + 1..]);
        if subpath.len() >= prefix.len() + suffix.len()
            && subpath.starts_with(prefix)
            && subpath.ends_with(suffix)
        {
            let matched = subpath[prefix.len()..subpath.len() - suffix.len()].to_string();
            if best.as_ref().is_none_or(|(len, _, _)| prefix.len() > *len) {
                best = Some((prefix.len(), value, matched));
            }
        }
    }
    match best {
        Some((_, value, matched)) => resolve_target(value, &matched),
        None => Err(ExportsError::NotExported),
    }
}

fn resolve_target(target: &Value, matched: &str) -> Result<String, ExportsError> {
    match target {
        Value::String(s) => {
            let substituted = s.replace('*', matched);
            if substituted.starts_with("./") {
                Ok(substituted)
            } else {
                Err(ExportsError::InvalidTarget(substituted))
            }
        }
        Value::Object(conditions) => {
            for (condition, value) in conditions {
                if condition == "default" || CONDITIONS.contains(&condition.as_str()) {
                    return resolve_target(value, matched);
                }
            }
            Err(ExportsError::NotExported)
        }
        Value::Array(options) => {
            for option in options {
                if let Ok(resolved) = resolve_target(option, matched) {
                    return Ok(resolved);
                }
            }
            Err(ExportsError::NotExported)
        }
        Value::Null => Err(ExportsError::NotExported),
        other => Err(ExportsError::InvalidTarget(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_specifiers() {
        assert_eq!(
            split_specifier("lodash"),
            Some(("lodash".into(), ".".into()))
        );
        assert_eq!(
            split_specifier("lodash/fp"),
            Some(("lodash".into(), "./fp".into()))
        );
        assert_eq!(
            split_specifier("@scope/pkg"),
            Some(("@scope/pkg".into(), ".".into()))
        );
        assert_eq!(
            split_specifier("@scope/pkg/deep/sub"),
            Some(("@scope/pkg".into(), "./deep/sub".into()))
        );
        assert_eq!(split_specifier("@scope"), None);
    }

    #[test]
    fn builtins_detected_bare_and_prefixed() {
        assert!(is_node_builtin("fs"));
        assert!(is_node_builtin("node:fs"));
        assert!(is_node_builtin("fs/promises"));
        assert!(!is_node_builtin("lodash"));
    }

    #[test]
    fn exports_sugar_and_conditions() {
        let exports = json!({ "import": "./esm.js", "require": "./cjs.cjs" });
        assert_eq!(exports_resolve(&exports, ".").ok().unwrap(), "./esm.js");
        let string_sugar = json!("./main.js");
        assert_eq!(
            exports_resolve(&string_sugar, ".").ok().unwrap(),
            "./main.js"
        );
        assert!(exports_resolve(&string_sugar, "./sub").is_err());
    }

    #[test]
    fn exports_subpaths_and_wildcards() {
        let exports = json!({
            ".": { "import": "./index.js" },
            "./extra": "./lib/extra.js",
            "./features/*": "./src/features/*.js",
            "./features/deep/*": "./src/deep/*.js",
            "./blocked": null,
        });
        assert_eq!(exports_resolve(&exports, ".").ok().unwrap(), "./index.js");
        assert_eq!(
            exports_resolve(&exports, "./extra").ok().unwrap(),
            "./lib/extra.js"
        );
        assert_eq!(
            exports_resolve(&exports, "./features/x").ok().unwrap(),
            "./src/features/x.js"
        );
        // Longest static prefix wins.
        assert_eq!(
            exports_resolve(&exports, "./features/deep/y").ok().unwrap(),
            "./src/deep/y.js"
        );
        assert!(matches!(
            exports_resolve(&exports, "./blocked"),
            Err(ExportsError::NotExported)
        ));
        assert!(matches!(
            exports_resolve(&exports, "./secret"),
            Err(ExportsError::NotExported)
        ));
    }

    #[test]
    fn exports_arrays_fall_back_and_bad_targets_error() {
        let exports = json!({ ".": [{ "browser": "./b.js" }, "./fallback.js"] });
        assert_eq!(
            exports_resolve(&exports, ".").ok().unwrap(),
            "./fallback.js"
        );
        let escape = json!({ ".": "../escape.js" });
        assert!(matches!(
            exports_resolve(&escape, "."),
            Err(ExportsError::InvalidTarget(_))
        ));
    }
}
