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
const NODE_BUILTINS: [&str; 46] = [
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
    // Legacy internal stream module aliases: require()able builtins Node keeps
    // as aliases of stream.{Readable,Writable,Duplex,Transform,PassThrough}
    // (also reported by module.builtinModules). Mapped to the public exports
    // by the registry factories in node_compat.js.
    "_stream_readable",
    "_stream_writable",
    "_stream_duplex",
    "_stream_transform",
    "_stream_passthrough",
];

/// node: compat wave 1 + wave 2 stubs — builtins that resolve to virtual
/// node:NAME paths the engine instantiates from the snapshot registry.
/// Recognized names outside this list gate on OAM-MOD0006 with a precise
/// pointer.
const SUPPORTED_BUILTINS: [&str; 58] = [
    "assert",
    "assert/strict",
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
    "dns/promises",
    "domain",
    "events",
    "fs",
    "fs/promises",
    "http",
    "http2",
    "https",
    "inspector",
    "internal/errors",
    "module",
    "net",
    "os",
    "path",
    "path/posix",
    "path/win32",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "readline/promises",
    "repl",
    "stream",
    "stream/consumers",
    "stream/promises",
    "stream/web",
    "string_decoder",
    "test",
    "timers",
    "timers/promises",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "util/types",
    "v8",
    "vm",
    "worker_threads",
    "zlib",
    // Legacy internal stream module aliases (require()able builtins; resolve to
    // node:_stream_* virtual paths instantiated from the registry factories).
    "_stream_readable",
    "_stream_writable",
    "_stream_duplex",
    "_stream_transform",
    "_stream_passthrough",
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

/// Cache of package.json path -> parsed JSON value. Avoids re-reading and
/// re-parsing the same package.json on every resolve/module_kind call.
fn cached_manifest(path: &Path) -> Option<serde_json::Value> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<serde_json::Value>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = map.get(path) {
        return cached.clone();
    }
    let result = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok());
    map.insert(path.to_path_buf(), result.clone());
    result
}

/// Exact-name builtin check (Node semantics): 'fs' and 'fs/promises' are
/// builtins; 'fs/foo' and 'process/browser' are NOT.
pub(crate) fn is_node_builtin(specifier: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static BUILTINS: OnceLock<HashSet<&str>> = OnceLock::new();
    let set = BUILTINS.get_or_init(|| {
        let mut s = HashSet::with_capacity(NODE_BUILTINS.len() + SUBPATH_BUILTINS.len());
        s.extend(NODE_BUILTINS.iter().copied());
        s.extend(SUBPATH_BUILTINS.iter().copied());
        s
    });
    let name = specifier.strip_prefix("node:").unwrap_or(specifier);
    set.contains(name)
}

/// Walk up from `file` to the nearest directory containing a package.json.
/// Returns (package_dir, parsed_manifest) on hit. The walk anchors subpath
/// imports (#xxx) to the OWNING package, per Node's subpath-imports spec.
fn find_owning_package(file: &Path) -> Option<(PathBuf, Value)> {
    let mut dir = file.parent()?.to_path_buf();
    loop {
        let manifest_path = dir.join("package.json");
        if manifest_path.is_file() {
            let manifest = cached_manifest(&manifest_path).unwrap_or(Value::Null);
            return Some((dir, manifest));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Resolve a `#xxx` subpath-import specifier against the importing package's
/// `imports` field (per the Node packages spec). Targets may be relative
/// (`./...` within the package) or bare (resolved through node_modules from
/// the package root, exactly like a regular bare import would be).
fn resolve_subpath_import(
    specifier: &str,
    referrer: &Path,
    mode: ResolveMode,
) -> Result<PathBuf, Diagnostic> {
    let Some((pkg_dir, manifest)) = find_owning_package(referrer) else {
        return Err(diag(
            "OAM-MOD0002",
            format!(
                "subpath import '{specifier}' has no owning package.json above {}",
                referrer.display()
            ),
        ));
    };
    let Some(imports) = manifest.get("imports") else {
        return Err(diag(
            "OAM-MOD0007",
            format!(
                "subpath import '{specifier}' is not declared in {}/package.json (no 'imports' field)",
                pkg_dir.display()
            ),
        ));
    };
    let target =
        exports_resolve(imports, specifier, mode.conditions()).map_err(|reason| match reason {
            ExportsError::NotExported => diag(
                "OAM-MOD0007",
                format!(
                    "subpath import '{specifier}' is not declared in {}/package.json imports",
                    pkg_dir.display()
                ),
            ),
            ExportsError::InvalidTarget(target) => diag(
                "OAM-MOD0002",
                format!(
                    "{}/package.json imports has invalid target '{target}' for '{specifier}'",
                    pkg_dir.display()
                ),
            ),
        })?;
    // Per spec: a target starting with './' is package-relative; anything else
    // is a bare specifier resolved as a regular import from the package root.
    if let Some(rel) = target.strip_prefix("./") {
        let path = pkg_dir.join(rel);
        if !path.is_file() {
            return Err(diag(
                "OAM-MOD0002",
                format!(
                    "subpath import '{specifier}' -> {} which does not exist",
                    path.display()
                ),
            ));
        }
        return Ok(path);
    }
    // Bare target: re-enter resolve_bare from the owning package's root. A
    // pseudo file under pkg_dir gives the node_modules walk the right anchor
    // (resolve_bare's loop starts at referrer.parent()).
    resolve_bare(&target, &pkg_dir.join("__oam_subpath_anchor"), mode)
}

/// npm packages oam provides NATIVELY, shadowing any installed copy. They
/// resolve to the matching `oam:` virtual builtin before the node_modules
/// walk (the Bun/Deno approach). `undici` is shadowed because the real
/// package cannot load on oam (it pulls node:sqlite + a WASM HTTP stack) and
/// adds nothing over oam's web-standard fetch.
fn shadowed_builtin(specifier: &str) -> Option<&'static str> {
    match specifier {
        "undici" => Some("oam:undici"),
        _ => None,
    }
}

/// Resolve a bare specifier from `referrer` against node_modules.
pub(crate) fn resolve_bare(
    specifier: &str,
    referrer: &Path,
    mode: ResolveMode,
) -> Result<PathBuf, Diagnostic> {
    // Subpath imports (#xxx): resolved against the owning package's
    // package.json `imports` field (per Node's subpath-imports spec).
    // chalk and many other published packages use this to expose
    // package-private modules; the resolver MUST honor it before trying
    // node_modules (#xxx would never match a bare package name there).
    if specifier.starts_with('#') {
        return resolve_subpath_import(specifier, referrer, mode);
    }

    // Natively-shadowed packages (undici, ...) resolve to their oam: virtual
    // builtin, ahead of any installed node_modules copy.
    if let Some(virt) = shadowed_builtin(specifier) {
        return Ok(PathBuf::from(virt));
    }

    // oam: runtime modules. Same virtual-path mechanism as node: builtins;
    // the registry key is the FULL specifier.
    if let Some(rest) = specifier.strip_prefix("oam:") {
        match rest {
            "test" => return Ok(PathBuf::from("oam:test")),
            "permissions" => return Ok(PathBuf::from("oam:permissions")),
            "ai" => return Ok(PathBuf::from("oam:ai")),
            "mcp" => return Ok(PathBuf::from("oam:mcp")),
            _ => {}
        }
        return Err(diag(
            "OAM-MOD0006",
            format!(
                "'{specifier}' is not a known oam: module (available: oam:test, oam:permissions, oam:ai, oam:mcp)"
            ),
        ));
    }

    // `internal/...` under --expose-internals: a virtual path like any other
    // builtin, resolved from the snapshot's internal registry. Whether the
    // NAME actually exists is decided there, not here -- an unknown one
    // throws from the registry with the same shape Node gives.
    if crate::is_exposed_internal(specifier) {
        return Ok(PathBuf::from(format!("node:{specifier}")));
    }

    if specifier.starts_with("node:") || is_node_builtin(specifier) {
        let name = specifier.strip_prefix("node:").unwrap_or(specifier);
        if SUPPORTED_BUILTINS.contains(&name) {
            // Virtual path; the engine instantiates the builtin from the
            // snapshot registry (never touches the filesystem).
            return Ok(PathBuf::from(format!("node:{name}")));
        }
        if is_node_builtin(specifier) {
            let preview = SUPPORTED_BUILTINS[..6].join(", ");
            return Err(diag(
                "OAM-MOD0006",
                format!(
                    "'{specifier}' is a Node builtin oam does not implement yet \
                     (wave 1 ships: {preview}, ..., others -- see docs); \
                     the rest land with later compat waves",
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
///
/// Successful file resolutions are realpath'd (Node's default
/// no-preserve-symlinks semantics) — see `resolve_import`.
pub fn resolve_require(specifier: &str, referrer: &Path) -> Result<PathBuf, Diagnostic> {
    resolve_require_inner(specifier, referrer).map(crate::pathutil::finalize_resolved)
}

fn resolve_require_inner(specifier: &str, referrer: &Path) -> Result<PathBuf, Diagnostic> {
    // CJS treats bare '.' and '..' as relative directory requires (Node's
    // _resolveLookupPaths: any request starting with '.' is relative, so
    // '.' is the referrer's directory and '..' its parent, each resolved
    // as a directory via package.json main / index). ajv ships
    // `require("..")` in production code — this is not a corner case.
    if specifier == "." || specifier == ".." {
        let base = referrer.parent().unwrap_or_else(|| Path::new("."));
        let raw = if specifier == "." {
            base.to_path_buf()
        } else {
            base.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| base.join(".."))
        };
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
    // Bare specifier: tsconfig paths get first crack (mirrors lib.rs:158-176),
    // then the Node CJS node_modules walk.
    let mut consulted_paths = false;
    if let Some(config) = crate::tsconfig::load_for(referrer) {
        consulted_paths = true;
        for raw in crate::tsconfig::match_specifier(&config, specifier) {
            if let Some(found) = ResolveMode::Require.probe(&raw) {
                return Ok(found);
            }
        }
    }
    resolve_bare(specifier, referrer, ResolveMode::Require).map_err(|mut failure| {
        if consulted_paths && failure.code == "OAM-MOD0002" {
            failure
                .message
                .push_str(" (tsconfig paths were consulted; no pattern produced a file)");
        }
        failure
    })
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

/// True for paths ending in `.d.ts` / `.d.mts` / `.d.cts` / `.d.tsx` /
/// `.d.jsx` (case-insensitive). Declaration files are types-only and must
/// never be returned as a runtime entry, even if a package's `exports` /
/// `main` / `module` field happens to point at one. Mirrors the check in
/// `oam_loader::lib::is_declaration_file` for paths that come back from
/// `resolve_in_package` rather than from the directory-probe path.
fn is_declaration_target(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lc = name.to_ascii_lowercase();
    lc.ends_with(".d.ts")
        || lc.ends_with(".d.mts")
        || lc.ends_with(".d.cts")
        || lc.ends_with(".d.tsx")
        || lc.ends_with(".d.jsx")
}

fn resolve_in_package(
    package_dir: &Path,
    package: &str,
    subpath: &str,
    specifier: &str,
    mode: ResolveMode,
) -> Result<PathBuf, Diagnostic> {
    let manifest_path = package_dir.join("package.json");
    let manifest: Value = cached_manifest(&manifest_path).unwrap_or(Value::Null);

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
        // The import-side `module`-field preference (the bundler ESM build)
        // only wins if the file it points at actually CLASSIFIES as ESM. A
        // package like turndown ships `module: ./x.es.js` + `main: ./x.cjs.js`
        // with no `type: module`, so `x.es.js` (an ESM file) classifies as CJS
        // and running it as CJS dies on `export`. In that contradiction we
        // fall through to `main` (the CJS build), which loads via interop --
        // matching Node, which ignores `module` entirely.
        let mut chosen: Option<(&str, PathBuf)> = None;
        for field in mode.entry_fields() {
            let Some(entry) = manifest.get(*field).and_then(Value::as_str) else {
                continue;
            };
            let Some(found) = mode.probe(&package_dir.join(entry)) else {
                continue;
            };
            if *field == "module" && module_kind(&found) == ModuleKind::Cjs {
                // `module` promised ESM but the file is CJS -> skip to `main`.
                continue;
            }
            chosen = Some((entry, found));
            break;
        }
        match chosen {
            Some((_, found)) => found,
            None => {
                // No usable declared entry: fall back to index.js probing.
                let raw = package_dir.join("index.js");
                mode.probe(&raw).ok_or_else(|| {
                    diag(
                        "OAM-MOD0002",
                        format!(
                            "package {package} has no resolvable entry (main/module/index) under {}",
                            package_dir.display()
                        ),
                    )
                })?
            }
        }
    };
    // Final guard: even if a package's exports / main / module field points
    // at a declaration file (rare but real -- some types-only packages or
    // misconfigured dual builds do this), loading it as a runtime module is
    // always wrong. Reject with MOD0001 plus a hint so the user knows why
    // the bare-import didn't work.
    if is_declaration_target(&resolved) {
        return Err(diag(
            "OAM-MOD0001",
            format!(
                "package {package} resolves '{specifier}' to {} which is a TypeScript \
                 declaration file (types-only) -- cannot load as a runtime module",
                resolved.display()
            ),
        ));
    }
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
        // Node's LOAD_AS_FILE order: .js, .json, .node.
        for ext in ["js", "json", "node"] {
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
        let main = cached_manifest(&raw.join("package.json")).and_then(|manifest| {
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
        // .node addons ride the CJS/require machinery, per Node.
        Some("cjs" | "cts" | "node") => return ModuleKind::Cjs,
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
            let declared = cached_manifest(&manifest_path).and_then(|manifest| {
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
    // Map keys: exports uses './x' (and bare '.'), imports uses '#x'. Both
    // shapes route through this same function; the subpath shape (specifier
    // vs '.') matches the key shape.
    let as_map = match exports {
        Value::Object(map) if map.keys().any(|k| k.starts_with('.') || k.starts_with('#')) => {
            Some(map)
        }
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

    // Subpath imports (#xxx) reuse exports_resolve with #-keyed maps. Verify
    // the algorithm covers the shapes that chalk + family ship.
    #[test]
    fn subpath_imports_match_string_target_and_wildcards() {
        let imports = json!({
            "#ansi-styles": "./source/vendor/ansi-styles/index.js",
            "#deep/*": "./src/deep/*.js",
            "#cond": { "import": "./esm.js", "require": "./cjs.js" },
        });
        // Exact key.
        assert_eq!(
            exports_resolve(&imports, "#ansi-styles", IMPORT)
                .ok()
                .unwrap(),
            "./source/vendor/ansi-styles/index.js"
        );
        // Wildcard subpath.
        assert_eq!(
            exports_resolve(&imports, "#deep/feature", IMPORT)
                .ok()
                .unwrap(),
            "./src/deep/feature.js"
        );
        // Conditions branch (import vs require).
        assert_eq!(
            exports_resolve(&imports, "#cond", IMPORT).ok().unwrap(),
            "./esm.js"
        );
        assert_eq!(
            exports_resolve(&imports, "#cond", REQUIRE).ok().unwrap(),
            "./cjs.js"
        );
        // Undeclared specifier rejected.
        assert!(matches!(
            exports_resolve(&imports, "#missing", IMPORT),
            Err(ExportsError::NotExported)
        ));
    }

    #[test]
    fn resolve_subpath_import_walks_to_owning_package_json() {
        // chalk's package.json imports map points #ansi-styles at a vendored
        // copy. Re-resolving the specifier from any file in chalk's tree
        // (nested deep, walking up to find the nearest package.json) must
        // produce the vendored path on disk.
        use std::sync::atomic::{AtomicU64, Ordering};
        static CNT: AtomicU64 = AtomicU64::new(0);
        let id = CNT.fetch_add(1, Ordering::Relaxed);
        let pkg = std::env::temp_dir().join(format!("oam-subpath-{}-{}", std::process::id(), id));
        let _ = std::fs::remove_dir_all(&pkg);
        let vendor = pkg.join("source/vendor/ansi-styles");
        std::fs::create_dir_all(&vendor).unwrap();
        std::fs::write(vendor.join("index.js"), "// vendored").unwrap();
        std::fs::write(
            pkg.join("package.json"),
            serde_json::to_string(&json!({
                "name": "chalk",
                "type": "module",
                "imports": {
                    "#ansi-styles": "./source/vendor/ansi-styles/index.js"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        // Referrer is a file inside the package (the import sites are in
        // chalk's own modules, not in node_modules above it).
        let referrer = pkg.join("source/index.js");
        std::fs::create_dir_all(referrer.parent().unwrap()).unwrap();
        std::fs::write(&referrer, "import '#ansi-styles';").unwrap();
        let resolved = resolve_bare("#ansi-styles", &referrer, ResolveMode::Import).unwrap();
        assert!(
            resolved.ends_with("source/vendor/ansi-styles/index.js"),
            "got {resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&pkg);
    }

    #[test]
    fn resolve_subpath_import_rejects_when_no_imports_field() {
        // A #-specifier in a package whose package.json has no imports field
        // is an error (MOD0007), distinct from "no package.json at all".
        use std::sync::atomic::{AtomicU64, Ordering};
        static CNT: AtomicU64 = AtomicU64::new(0);
        let id = CNT.fetch_add(1, Ordering::Relaxed);
        let pkg =
            std::env::temp_dir().join(format!("oam-subpath-noimp-{}-{}", std::process::id(), id));
        let _ = std::fs::remove_dir_all(&pkg);
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"pkg","type":"module"}"#,
        )
        .unwrap();
        let referrer = pkg.join("entry.js");
        std::fs::write(&referrer, "").unwrap();
        let err = resolve_bare("#missing", &referrer, ResolveMode::Import).unwrap_err();
        assert_eq!(err.code, "OAM-MOD0007", "got {err:?}");
        let _ = std::fs::remove_dir_all(&pkg);
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

    #[test]
    fn resolve_bare_happy_path_walks_node_modules() {
        let dir = std::env::temp_dir().join(format!("oam-npm-6-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("node_modules/lodash")).unwrap();
        std::fs::write(
            dir.join("node_modules/lodash/package.json"),
            serde_json::to_string(&json!({
                "name": "lodash",
                "main": "index.js",
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("node_modules/lodash/index.js"), "").unwrap();
        let entry = dir.join("entry.ts");
        std::fs::write(&entry, "").unwrap();
        let resolved = resolve_bare("lodash", &entry, ResolveMode::Import).unwrap();
        assert!(
            resolved.ends_with("node_modules/lodash/index.js"),
            "got {resolved:?}"
        );
    }

    #[test]
    fn resolve_require_dot_and_dotdot_are_relative_directory_requires() {
        let dir = std::env::temp_dir().join(format!("oam-npm-dot-{}", std::process::id()));
        // pkg/package.json main -> index.js; pkg/lib/child.js does require('..')
        std::fs::create_dir_all(dir.join("pkg/lib")).unwrap();
        std::fs::write(
            dir.join("pkg/package.json"),
            serde_json::to_string(&json!({ "name": "pkg", "main": "index.js" })).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("pkg/index.js"), "").unwrap();
        std::fs::write(dir.join("pkg/lib/index.js"), "").unwrap();
        let child = dir.join("pkg/lib/child.js");
        std::fs::write(&child, "").unwrap();

        let parent = resolve_require("..", &child).expect("require('..') resolves parent dir");
        assert!(parent.ends_with("index.js"), "got {parent:?}");
        assert!(
            parent.parent().unwrap().ends_with("pkg"),
            "'..' must land on the package dir's main, got {parent:?}"
        );
        let here = resolve_require(".", &child).expect("require('.') resolves own dir");
        assert!(here.ends_with("index.js"), "got {here:?}");
        assert!(
            here.parent().unwrap().ends_with("lib"),
            "'.' must land on the referrer's dir index, got {here:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_realpaths_symlinked_packages_pnpm_layout() {
        // pnpm shape: node_modules/pkg-a -> .pnpm/pkg-a@1/node_modules/pkg-a,
        // pkg-b only exists as a sibling inside the .pnpm dir. Resolving
        // pkg-a must return its REAL path so pkg-a's own walk finds pkg-b.
        let dir = std::env::temp_dir().join(format!("oam-npm-pnpm-{}", std::process::id()));
        let store_a = dir.join(".pnpm/pkg-a@1/node_modules/pkg-a");
        let store_b = dir.join(".pnpm/pkg-a@1/node_modules/pkg-b");
        std::fs::create_dir_all(&store_a).unwrap();
        std::fs::create_dir_all(&store_b).unwrap();
        for (pkg, name) in [(&store_a, "pkg-a"), (&store_b, "pkg-b")] {
            std::fs::write(
                pkg.join("package.json"),
                serde_json::to_string(&json!({ "name": name, "main": "index.js" })).unwrap(),
            )
            .unwrap();
            std::fs::write(pkg.join("index.js"), "").unwrap();
        }
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        let link = dir.join("node_modules/pkg-a");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&store_a, &link).unwrap();
        let entry = dir.join("entry.js");
        std::fs::write(&entry, "").unwrap();

        let a = resolve_require("pkg-a", &entry).expect("pkg-a resolves via the link");
        let real_a = std::fs::canonicalize(store_a.join("index.js")).unwrap();
        assert_eq!(a, real_a, "resolution must return the realpath");
        // The transitive hop: from pkg-a's real location, pkg-b is a sibling.
        let b = resolve_require("pkg-b", &a).expect("pkg-b resolves from pkg-a's realpath");
        assert!(b.ends_with("pkg-b/index.js"), "got {b:?}");
    }

    #[test]
    fn resolve_bare_require_uses_main_only() {
        let dir = std::env::temp_dir().join(format!("oam-npm-7-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("node_modules/lodash")).unwrap();
        std::fs::write(
            dir.join("node_modules/lodash/package.json"),
            serde_json::to_string(&json!({
                "name": "lodash",
                "main": "main.js",
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("node_modules/lodash/main.js"), "").unwrap();
        let entry = dir.join("entry.cjs");
        std::fs::write(&entry, "").unwrap();
        let resolved = resolve_bare("lodash", &entry, ResolveMode::Require).unwrap();
        assert!(
            resolved.ends_with("node_modules/lodash/main.js"),
            "got {resolved:?}"
        );
    }

    // turndown shape: a dual package with `module: ./x.es.js` (ESM content,
    // but no `type: module` so it CLASSIFIES as CJS) + `main: ./x.cjs.js`.
    // The import-side `module` preference must NOT pick x.es.js (running an
    // ESM file as CJS dies on `export`); it falls through to the CJS main.
    #[test]
    fn import_skips_module_field_when_it_classifies_as_cjs() {
        let dir = std::env::temp_dir().join(format!("oam-npm-dual-cjs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("node_modules/turndownish");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            serde_json::to_string(&json!({
                "name": "turndownish",
                "main": "lib/t.cjs.js",
                "module": "lib/t.es.js",
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(pkg.join("lib")).unwrap();
        std::fs::write(pkg.join("lib/t.cjs.js"), "module.exports = {};").unwrap();
        std::fs::write(pkg.join("lib/t.es.js"), "export default {};").unwrap();
        let entry = dir.join("entry.ts");
        std::fs::write(&entry, "").unwrap();
        let resolved = resolve_bare("turndownish", &entry, ResolveMode::Import).unwrap();
        assert!(
            resolved.ends_with("lib/t.cjs.js"),
            "module field points at a CJS-classified .es.js -> must fall to CJS main; got {resolved:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The `module`-field ESM preference is preserved where it's VALID: a dual
    // package whose `module` points at a real ESM file (.mjs) still wins over
    // the CJS `main` on the import side (the bundler "better build" intent).
    #[test]
    fn import_prefers_module_field_when_it_is_real_esm() {
        let dir = std::env::temp_dir().join(format!("oam-npm-dual-esm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("node_modules/dualmjs");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            serde_json::to_string(&json!({
                "name": "dualmjs",
                "main": "index.js",
                "module": "index.mjs",
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(pkg.join("index.js"), "module.exports = {};").unwrap();
        std::fs::write(pkg.join("index.mjs"), "export default {};").unwrap();
        let entry = dir.join("entry.ts");
        std::fs::write(&entry, "").unwrap();
        let resolved = resolve_bare("dualmjs", &entry, ResolveMode::Import).unwrap();
        assert!(
            resolved.ends_with("index.mjs"),
            "module field points at real ESM (.mjs) -> must be preferred; got {resolved:?}"
        );
        // Require side ignores `module` entirely (Node parity): uses main.
        let cjs_entry = dir.join("entry.cjs");
        std::fs::write(&cjs_entry, "").unwrap();
        let req = resolve_bare("dualmjs", &cjs_entry, ResolveMode::Require).unwrap();
        assert!(req.ends_with("index.js"), "require uses main; got {req:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_prefixed_unknown_is_builtin_error() {
        let dir = std::env::temp_dir().join(format!("oam-npm-8a-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("entry.ts");
        std::fs::write(&entry, "").unwrap();
        let err = resolve_bare("node:foo-does-not-exist", &entry, ResolveMode::Import)
            .expect_err("unknown node: prefix must error, not walk node_modules");
        assert_eq!(err.code, "OAM-MOD0006");
        assert!(
            err.message.contains("is not a known node: builtin module"),
            "message was: {}",
            err.message
        );
    }

    #[test]
    fn node_sys_still_in_builtins_list() {
        // `sys` is in NODE_BUILTINS (the legacy alias for 'util' was removed
        // from Node in 2017, but the resolver still recognizes it). That puts
        // `node:sys` on the "recognized builtin, not yet shipped" branch
        // (OAM-MOD0006 with the wave-N preview message), NOT the bare-
        // specifier / "cannot find package" branch. If a future change drops
        // `sys` from NODE_BUILTINS, this assertion becomes OAM-MOD0002 with
        // a "tsconfig paths were consulted" suffix — flip it then.
        let dir = std::env::temp_dir().join(format!("oam-npm-8b-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("entry.ts");
        std::fs::write(&entry, "").unwrap();
        let err = resolve_bare("node:sys", &entry, ResolveMode::Import)
            .expect_err("node:sys should not resolve to a real file");
        assert_eq!(err.code, "OAM-MOD0006");
        assert!(
            err.message.contains("does not implement yet")
                || err.message.contains("is not a known node: builtin module"),
            "message was: {}",
            err.message
        );
    }

    #[test]
    fn module_kind_node_modules_boundary() {
        let dir = std::env::temp_dir().join(format!("oam-npm-9-{}", std::process::id()));
        let proj = dir.join("proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::create_dir_all(proj.join("node_modules/lodash")).unwrap();
        std::fs::write(
            proj.join("package.json"),
            serde_json::to_string(&json!({})).unwrap(),
        )
        .unwrap();
        let inside = proj.join("node_modules/lodash/index.js");
        let outside = proj.join("src/index.js");
        std::fs::write(&inside, "").unwrap();
        std::fs::write(&outside, "").unwrap();
        assert_eq!(module_kind(&inside), ModuleKind::Cjs);
        assert_eq!(module_kind(&outside), ModuleKind::Esm);
    }

    #[test]
    fn module_kind_type_commonjs_is_cjs() {
        let dir = std::env::temp_dir().join(format!("oam-npm-10-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            serde_json::to_string(&json!({ "type": "commonjs" })).unwrap(),
        )
        .unwrap();
        let index = dir.join("index.js");
        std::fs::write(&index, "").unwrap();
        assert_eq!(module_kind(&index), ModuleKind::Cjs);
    }

    #[test]
    fn resolve_require_bare_uses_tsconfig_paths() {
        // `resolve_require` consults tsconfig paths before the node_modules
        // walk (mirroring `resolve_import`). Note the require-side probe is
        // `probe_require` — exact, .js, .json, .node — so we plant a .js
        // target here, not .ts. (`resolve_import` would probe .ts as well.)
        let dir = std::env::temp_dir().join(format!("oam-npm-11-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "@lib/*": ["src/*"] } } }"#,
        )
        .unwrap();
        std::fs::write(dir.join("src/util.js"), "").unwrap();
        let entry = dir.join("entry.cjs");
        std::fs::write(&entry, "").unwrap();
        let resolved = resolve_require("@lib/util", &entry).unwrap();
        assert!(resolved.ends_with("src/util.js"), "got {resolved:?}");
    }

    #[test]
    fn exports_array_falls_through_when_no_condition_matches() {
        // Distinct from `exports_arrays_fall_back_and_bad_targets_error`
        // (which uses a string in the array). Here the FIRST slot is a
        // condition object whose condition ('browser') is not in the import
        // conditions, so the array walks past it to the string fallback.
        let exports = json!({ ".": [{ "browser": "./b.js" }, "./fallback.js"] });
        assert_eq!(
            exports_resolve(&exports, ".", IMPORT).ok().unwrap(),
            "./fallback.js"
        );
    }
}
