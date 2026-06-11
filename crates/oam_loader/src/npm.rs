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
//! Documented divergence: for legacy IMPORT entry resolution we honor the
//! bundler-standard "module" field before "main" — Node ignores it, but a
//! large class of dual-published packages gets the better (ESM) build that
//! way. `require()` resolution uses "main" only, exactly like Node: CJS
//! callers expect the CJS build.
//!
//! Node builtins (prefixed or bare) resolve to virtual `node:NAME` paths
//! when wave 1 ships them (SUPPORTED_BUILTINS); recognized-but-unshipped
//! builtins gate on OAM-MOD0006. Exports-blocked subpaths ->
//! OAM-MOD0007 (Node's ERR_PACKAGE_PATH_NOT_EXPORTED). Resolved CJS files
//! are no longer gated (OAM-MOD0005 retired) — the engine routes them
//! through CJS interop.

use oam_diagnostics::{Diagnostic, Origin, Severity};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// How a resolution request reached us. Import = ESM `import` statements;
/// Require = `require()` calls inside CJS modules. They differ in exports
/// conditions, legacy entry fields, and extension probing — parameterized
/// here so the walk and the exports algorithm stay single-sourced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMode {
    Import,
    Require,
}

impl ResolveMode {
    fn conditions(self) -> &'static [&'static str] {
        match self {
            ResolveMode::Import => &["node", "import", "default"],
            ResolveMode::Require => &["node", "require", "default"],
        }
    }

    /// Legacy entry fields, in precedence order (see module docs for the
    /// "module"-field divergence on the import side).
    fn entry_fields(self) -> &'static [&'static str] {
        match self {
            ResolveMode::Import => &["module", "main"],
            ResolveMode::Require => &["main"],
        }
    }

    fn probe(self, raw: &Path) -> Option<PathBuf> {
        match self {
            ResolveMode::Import => probe_legacy(raw),
            ResolveMode::Require => probe_require(raw),
        }
    }
}

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

/// node: compat wave 1 — builtins that resolve to virtual node:NAME paths
/// the engine instantiates from the snapshot registry. Recognized names
/// outside this list gate on OAM-MOD0006 with a precise pointer.
const SUPPORTED_BUILTINS: [&str; 27] = [
    "assert",
    "async_hooks",
    "buffer",
    "console",
    "crypto",
    "events",
    "fs",
    "fs/promises",
    "http",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "process",
    "querystring",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "timers/promises",
    "tty",
    "url",
    "util",
    "zlib",
];

/// Subpath builtins Node recognizes by EXACT name. Anything else with a
/// slash ('process/browser', 'fs/foo', 'buffer/') is NOT a builtin — it
/// falls through to node_modules resolution like Node does (the userland
/// 'process' package ships process/browser, and packages require it).
const SUBPATH_BUILTINS: [&str; 12] = [
    "assert/strict",
    "dns/promises",
    "fs/promises",
    "inspector/promises",
    "path/posix",
    "path/win32",
    "readline/promises",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "timers/promises",
    "util/types",
];

fn diag(code: &str, message: String) -> Diagnostic {
    Diagnostic::new(code, Severity::Error, Origin::Resolve, message)
}

/// Exact-name builtin check (Node semantics): 'fs' and 'fs/promises' are
/// builtins; 'fs/foo' and 'process/browser' are NOT.
pub(crate) fn is_node_builtin(specifier: &str) -> bool {
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);
    NODE_BUILTINS.contains(&name) || SUBPATH_BUILTINS.contains(&name)
}

/// Resolve a bare specifier from `referrer` against node_modules.
pub(crate) fn resolve_bare(
    specifier: &str,
    referrer: &Path,
    mode: ResolveMode,
) -> Result<PathBuf, Diagnostic> {
    // oam: runtime modules (oam:test today). Same virtual-path mechanism
    // as node: builtins; the registry key is the FULL specifier.
    if let Some(rest) = specifier.strip_prefix("oam:") {
        if rest == "test" {
            return Ok(PathBuf::from("oam:test"));
        }
        return Err(diag(
            "OAM-MOD0006",
            format!("'{specifier}' is not a known oam: module (available: oam:test)"),
        ));
    }

    if specifier.starts_with("node:") || is_node_builtin(specifier) {
        let name = specifier.strip_prefix("node:").unwrap_or(specifier);
        if SUPPORTED_BUILTINS.contains(&name) {
            // Virtual path; the engine instantiates the builtin from the
            // snapshot registry (never touches the filesystem).
            return Ok(PathBuf::from(format!("node:{name}")));
        }
        if is_node_builtin(specifier) {
            return Err(diag(
                "OAM-MOD0006",
                format!(
                    "'{specifier}' is a Node builtin oam does not implement yet \
                     (wave 1 ships: {}); the rest land with later compat waves",
                    SUPPORTED_BUILTINS.join(", ")
                ),
            ));
        }
        // node:-prefixed but not a recognized builtin name — Node's
        // ERR_UNKNOWN_BUILTIN_MODULE, never a node_modules walk.
        return Err(diag(
            "OAM-MOD0006",
            format!("'{specifier}' is not a known node: builtin module"),
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
            return resolve_in_package(&package_dir, &package, &subpath, specifier, mode);
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

/// Resolve a `require()` specifier from the CJS module at `referrer`.
/// Covers the whole require surface: relative/absolute paths with Node's
/// CJS probing (exact, .js, .json, directory main/index), and bare
/// specifiers via the node_modules walk under require conditions. The
/// engine converts the Diagnostic's message into the thrown JS Error.
pub fn resolve_require(specifier: &str, referrer: &Path) -> Result<PathBuf, Diagnostic> {
    if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/') {
        let base = referrer.parent().unwrap_or_else(|| Path::new("."));
        let raw = base.join(specifier);
        return probe_require(&raw).ok_or_else(|| {
            diag(
                "OAM-MOD0001",
                format!(
                    "Cannot find module '{specifier}' required from {}",
                    referrer.display()
                ),
            )
        });
    }
    if Path::new(specifier).is_absolute() {
        let raw = PathBuf::from(specifier);
        return probe_require(&raw).ok_or_else(|| {
            diag(
                "OAM-MOD0001",
                format!(
                    "Cannot find module '{specifier}' required from {}",
                    referrer.display()
                ),
            )
        });
    }
    resolve_bare(specifier, referrer, ResolveMode::Require)
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
    mode: ResolveMode,
) -> Result<PathBuf, Diagnostic> {
    let manifest_path = package_dir.join("package.json");
    let manifest: Value = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok())
        .unwrap_or(Value::Null);

    let resolved = if let Some(exports) = manifest.get("exports") {
        let target =
            exports_resolve(exports, subpath, mode.conditions()).map_err(
                |reason| match reason {
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
                },
            )?;
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
        mode.probe(&raw).ok_or_else(|| {
            diag(
                "OAM-MOD0002",
                format!("cannot resolve '{specifier}': no file at {}", raw.display()),
            )
        })?
    } else {
        // Legacy entry (per mode — see module docs) falling back to index.js.
        let entry = mode
            .entry_fields()
            .iter()
            .find_map(|field| manifest.get(*field).and_then(Value::as_str))
            .unwrap_or("index.js");
        let raw = package_dir.join(entry);
        mode.probe(&raw).ok_or_else(|| {
            diag(
                "OAM-MOD0002",
                format!(
                    "package {package} entry '{entry}' does not resolve to a file under {}",
                    package_dir.display()
                ),
            )
        })?
    };
    Ok(resolved)
}

/// Legacy probing (import side): exact, +.js, /index.js.
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

/// Node's CJS LOAD_AS_FILE + LOAD_AS_DIRECTORY: exact, +.js, +.json, then
/// directory (package.json "main" probed as a file/index, index.js,
/// index.json). `.node` addons are N-API territory (later milestone).
fn probe_require(raw: &Path) -> Option<PathBuf> {
    fn as_file(raw: &Path) -> Option<PathBuf> {
        if raw.is_file() {
            return Some(raw.to_path_buf());
        }
        for ext in ["js", "json"] {
            let candidate = PathBuf::from(format!("{}.{ext}", raw.display()));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
    fn as_index(dir: &Path) -> Option<PathBuf> {
        for index in ["index.js", "index.json"] {
            let candidate = dir.join(index);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    if let Some(found) = as_file(raw) {
        return Some(found);
    }
    if raw.is_dir() {
        let main = std::fs::read_to_string(raw.join("package.json"))
            .ok()
            .and_then(|text| {
                serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')).ok()
            })
            .and_then(|manifest| {
                manifest
                    .get("main")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if let Some(main) = main {
            let target = raw.join(main);
            if let Some(found) = as_file(&target).or_else(|| as_index(&target)) {
                return Some(found);
            }
        }
        return as_index(raw);
    }
    None
}

/// Is the file at `path` an ES module or CommonJS?
///
/// Extensions are authoritative: .mjs/.mts/.ts/.tsx are ESM, .cjs/.cts are
/// CJS (TypeScript is always ESM in oam unless explicitly .cts). For
/// .js/.jsx the nearest package.json "type" decides, walking up and
/// stopping at a node_modules boundary, per Node.
///
/// Documented divergence on the MISSING-"type" default: inside
/// node_modules it is CJS (Node-exact — typeless packages ship CJS), but
/// for project files it is ESM. `npm init -y` writes no "type" field, and
/// punishing modern project code with require semantics for that is the
/// wrong default in 2026. Write .cjs (or "type": "commonjs") to opt
/// project files into CJS.
pub fn module_kind(path: &Path) -> ModuleKind {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mjs" | "mts" | "ts" | "tsx") => return ModuleKind::Esm,
        Some("cjs" | "cts") => return ModuleKind::Cjs,
        _ => {}
    }
    let in_node_modules = path.components().any(|c| c.as_os_str() == "node_modules");
    let untyped_default = if in_node_modules {
        ModuleKind::Cjs
    } else {
        ModuleKind::Esm
    };

    let mut dir = path.parent();
    while let Some(current) = dir {
        let manifest_path = current.join("package.json");
        if manifest_path.is_file() {
            let declared = std::fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|text| {
                    serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')).ok()
                })
                .and_then(|manifest| {
                    manifest
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            return match declared.as_deref() {
                Some("module") => ModuleKind::Esm,
                Some(_) => ModuleKind::Cjs,
                None => untyped_default,
            };
        }
        if current
            .file_name()
            .is_some_and(|name| name == "node_modules")
        {
            break;
        }
        dir = current.parent();
    }
    untyped_default
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Esm,
    Cjs,
}

enum ExportsError {
    NotExported,
    InvalidTarget(String),
}

/// PACKAGE_EXPORTS_RESOLVE, the subset that matters: exact subpaths,
/// single-* wildcards (longest-prefix wins), condition objects in
/// declaration order, arrays as fallbacks, null blocks.
fn exports_resolve(
    exports: &Value,
    subpath: &str,
    conditions: &[&str],
) -> Result<String, ExportsError> {
    // Sugar: a top-level string/array/conditions-object is the "." entry.
    let as_map = match exports {
        Value::Object(map) if map.keys().any(|k| k.starts_with('.')) => Some(map),
        _ => None,
    };
    let Some(map) = as_map else {
        if subpath == "." {
            return resolve_target(exports, "", conditions);
        }
        return Err(ExportsError::NotExported);
    };

    if let Some(value) = map.get(subpath) {
        return resolve_target(value, "", conditions);
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
        Some((_, value, matched)) => resolve_target(value, &matched, conditions),
        None => Err(ExportsError::NotExported),
    }
}

fn resolve_target(
    target: &Value,
    matched: &str,
    conditions: &[&str],
) -> Result<String, ExportsError> {
    match target {
        Value::String(s) => {
            let substituted = s.replace('*', matched);
            if substituted.starts_with("./") {
                Ok(substituted)
            } else {
                Err(ExportsError::InvalidTarget(substituted))
            }
        }
        Value::Object(branches) => {
            for (condition, value) in branches {
                if condition == "default" || conditions.contains(&condition.as_str()) {
                    return resolve_target(value, matched, conditions);
                }
            }
            Err(ExportsError::NotExported)
        }
        Value::Array(options) => {
            for option in options {
                if let Ok(resolved) = resolve_target(option, matched, conditions) {
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
        // Exact-name semantics: builtin-prefixed subpaths are NOT builtins;
        // they resolve through node_modules (the userland 'process' package
        // ships process/browser).
        assert!(!is_node_builtin("process/browser"));
        assert!(!is_node_builtin("fs/foo"));
        assert!(!is_node_builtin("buffer/"));
    }

    const IMPORT: &[&str] = &["node", "import", "default"];
    const REQUIRE: &[&str] = &["node", "require", "default"];

    #[test]
    fn exports_sugar_and_conditions() {
        let exports = json!({ "import": "./esm.js", "require": "./cjs.cjs" });
        assert_eq!(
            exports_resolve(&exports, ".", IMPORT).ok().unwrap(),
            "./esm.js"
        );
        // The same map under require conditions picks the CJS build.
        assert_eq!(
            exports_resolve(&exports, ".", REQUIRE).ok().unwrap(),
            "./cjs.cjs"
        );
        let string_sugar = json!("./main.js");
        assert_eq!(
            exports_resolve(&string_sugar, ".", IMPORT).ok().unwrap(),
            "./main.js"
        );
        assert!(exports_resolve(&string_sugar, "./sub", IMPORT).is_err());
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
        assert_eq!(
            exports_resolve(&exports, ".", IMPORT).ok().unwrap(),
            "./index.js"
        );
        assert_eq!(
            exports_resolve(&exports, "./extra", IMPORT).ok().unwrap(),
            "./lib/extra.js"
        );
        assert_eq!(
            exports_resolve(&exports, "./features/x", IMPORT)
                .ok()
                .unwrap(),
            "./src/features/x.js"
        );
        // Longest static prefix wins.
        assert_eq!(
            exports_resolve(&exports, "./features/deep/y", IMPORT)
                .ok()
                .unwrap(),
            "./src/deep/y.js"
        );
        assert!(matches!(
            exports_resolve(&exports, "./blocked", IMPORT),
            Err(ExportsError::NotExported)
        ));
        assert!(matches!(
            exports_resolve(&exports, "./secret", IMPORT),
            Err(ExportsError::NotExported)
        ));
    }

    #[test]
    fn exports_arrays_fall_back_and_bad_targets_error() {
        let exports = json!({ ".": [{ "browser": "./b.js" }, "./fallback.js"] });
        assert_eq!(
            exports_resolve(&exports, ".", IMPORT).ok().unwrap(),
            "./fallback.js"
        );
        let escape = json!({ ".": "../escape.js" });
        assert!(matches!(
            exports_resolve(&escape, ".", IMPORT),
            Err(ExportsError::InvalidTarget(_))
        ));
    }

    #[test]
    fn module_kind_extensions_are_authoritative() {
        assert_eq!(module_kind(Path::new("x.ts")), ModuleKind::Esm);
        assert_eq!(module_kind(Path::new("x.mts")), ModuleKind::Esm);
        assert_eq!(module_kind(Path::new("x.mjs")), ModuleKind::Esm);
        assert_eq!(module_kind(Path::new("x.cjs")), ModuleKind::Cjs);
        assert_eq!(module_kind(Path::new("x.cts")), ModuleKind::Cjs);
    }
}
