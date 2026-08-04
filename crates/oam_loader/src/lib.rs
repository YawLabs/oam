//! oam_loader: the module pipeline.
//!
//! M1 slice 1: TypeScript strip/transform via oxc. Source comes in as
//! .ts/.mts/.cts, comes out as plain JavaScript with types removed and
//! non-erasable syntax (enums, namespaces, parameter properties) lowered —
//! strictly more than Node's strip-only support. Parse and transform
//! failures surface as ODIF diagnostics (origin: parse), never as prose.
//!
//! Still ahead in this crate: the ESM module graph, npm resolution, CJS
//! interop, tsconfig paths, .tsx (needs the module loader for the JSX
//! automatic runtime), content-addressed transform caches.

// Diagnostic-as-error is ~200 bytes on cold resolution/parse failure paths;
// boxing would tax every caller's API for nothing. Crate-wide stance,
// shared with oam_ts.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};

use oam_diagnostics::{Diagnostic, Origin, Position, Severity, Span};
use oxc_allocator::Allocator;

pub mod install;
mod npm;
mod pathutil;
pub mod precompile;
mod resolver;
pub mod trust;
mod tsconfig;
pub use npm::{ModuleKind, module_kind, resolve_require};

/// Whether `specifier` names a Node builtin (bare like `fs` or prefixed
/// like `node:fs`) or an `oam:` runtime module. Engine-side surfaces
/// (`require.resolve.paths` returns null for builtins) need the check
/// without re-owning the builtin list.
pub fn is_builtin_specifier(specifier: &str) -> bool {
    specifier.starts_with("node:")
        || specifier.starts_with("oam:")
        || npm::is_node_builtin(specifier)
}
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};
pub use resolver::{Resolver, default_resolver};

/// Transpilation failure: one or more ODIF diagnostics (origin: parse).
#[derive(Debug, thiserror::Error)]
#[error("{} parse/transform error(s) in {file}", diagnostics.len())]
pub struct TranspileError {
    pub file: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// How a file on disk should be prepared for the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    JavaScript,
    TypeScript,
    /// .tsx/.jsx — not yet supported (JSX automatic runtime needs the module loader).
    Jsx,
}

pub fn classify(path: &Path) -> SourceKind {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "ts" | "mts" | "cts" => SourceKind::TypeScript,
        "tsx" | "jsx" => SourceKind::Jsx,
        _ => SourceKind::JavaScript,
    }
}

/// Transpile TypeScript source to plain JavaScript (types stripped,
/// non-erasable syntax lowered, modern JS syntax preserved as-is).
pub fn transpile_typescript(path: &Path, source: &str) -> Result<String, TranspileError> {
    let file = path.to_string_lossy().into_owned();
    let allocator = Allocator::default();
    // Always MODULE, never Script. SourceType::from_path infers script-vs-
    // module from the extension, and oxc's JSX transform picks the shape of
    // its injected runtime import from that: a Script gets
    // `var _x = require("react/jsx-runtime")`, which is undefined in oam's
    // ESM context, so a .tsx file with no import/export of its own died with
    // "require is not defined". oam only ever feeds this function ESM
    // (CommonJS entries route through the interop path instead), so pinning
    // module here is correct as well as necessary.
    let source_type = SourceType::from_path(path)
        .unwrap_or_else(|_| SourceType::ts())
        .with_module(true);

    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.errors.is_empty() {
        return Err(TranspileError {
            diagnostics: to_odif(&file, source, &parsed.errors, "OAM-PARSE0001"),
            file,
        });
    }
    let mut program = parsed.program;

    // with_enum_eval: the enum transform needs pre-computed member values in
    // Scoping or string-valued members produce wrong reverse mappings (oxc#21667).
    let scoping = SemanticBuilder::new()
        .with_enum_eval(true)
        .build(&program)
        .semantic
        .into_scoping();

    let mut options = TransformOptions::default();
    // tsconfig's compilerOptions.jsxImportSource retargets the automatic JSX
    // runtime (Preact, Solid: `preact` -> imports from `preact/jsx-runtime`).
    // A per-file `@jsxImportSource` pragma is parsed by oxc itself and wins
    // over this, matching tsc's precedence.
    options.jsx.import_source = tsconfig::jsx_import_source_for(path);
    let transformed =
        Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
    if !transformed.errors.is_empty() {
        return Err(TranspileError {
            diagnostics: to_odif(&file, source, &transformed.errors, "OAM-PARSE0002"),
            file,
        });
    }

    Ok(Codegen::new().build(&program).code)
}

fn to_odif(
    file: &str,
    source: &str,
    errors: &[oxc_diagnostics::OxcDiagnostic],
    code: &str,
) -> Vec<Diagnostic> {
    errors
        .iter()
        .map(|e| {
            let mut d = Diagnostic::new(code, Severity::Error, Origin::Parse, e.message.as_ref());
            if let Some(label) = e.labels.as_ref().and_then(|l| l.first()) {
                let start = offset_to_position(source, label.offset());
                let end = offset_to_position(source, label.offset() + label.len());
                d = d.with_span(Span {
                    file: file.to_string(),
                    start,
                    end,
                });
            }
            d
        })
        .collect()
}

/// Resolve an import specifier as written in the module at `referrer`.
///
/// This is a thin shim over `Resolver::resolve_import` routed through the
/// current thread's default `Resolver`. Long-lived consumers (CLI tools,
/// daemons) should construct their own `Resolver` and call
/// `resolver.resolve_import(...)` on it, so they can drop stale cache entries
/// via `clear_caches()` when the project layout changes.
pub fn resolve_import(specifier: &str, referrer: &Path) -> Result<PathBuf, Diagnostic> {
    default_resolver().resolve_import(specifier, referrer)
}

/// Check if a candidate path is a file, consulting the negative cache first.
/// On miss from `is_file()`, insert into the negative set. On hit, remove
/// from the negative set (in case a file was created since last check).
fn is_file_cached(resolver: &Resolver, path: &Path) -> bool {
    resolver.is_file_cached(path)
}

/// True for TypeScript / JS declaration files. These are types-only and
/// must never be treated as runtime entry points -- loading one as JS is
/// always wrong. Suffix-based so the match survives platform separators
/// (path components are stripped via `file_name()` first). The compare is
/// case-insensitive: Windows filesystems are case-insensitive at the OS
/// level (a literal `FOO.D.TS` on disk would otherwise slip through and
/// load as runtime), so we lowercase the basename before matching.
fn is_declaration_file(p: &Path) -> bool {
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

/// Probe a raw path for the actual module file. Candidate order: exact,
/// TS-source fallback for JS extensions, then APPENDED extensions +
/// directory index. Appending (not with_extension) keeps dotted basenames
/// intact: './my.module' probes 'my.module.ts', never clobbers the
/// '.module' segment. Returns (found-absolute, every candidate tried).
pub(crate) fn probe_candidates(resolver: &Resolver, raw: &Path) -> (Option<PathBuf>, Vec<PathBuf>) {
    fn append_ext(p: &Path, ext: &str) -> PathBuf {
        let mut s = p.as_os_str().to_os_string();
        s.push(".");
        s.push(ext);
        PathBuf::from(s)
    }

    let mut candidates: Vec<PathBuf> = vec![raw.to_path_buf()];
    match raw.extension().and_then(|e| e.to_str()) {
        Some("js") => candidates.push(raw.with_extension("ts")),
        Some("mjs") => candidates.push(raw.with_extension("mts")),
        Some("cjs") => candidates.push(raw.with_extension("cts")),
        Some("ts") | Some("mts") | Some("cts") | Some("tsx") | Some("jsx") | Some("json") => {}
        _ => {
            // No extension, or a dotted basename ('./my.module'): probe by
            // appending, then directory index.
            for ext in ["ts", "mts", "js", "mjs"] {
                candidates.push(append_ext(raw, ext));
            }
            for index in ["index.ts", "index.js"] {
                candidates.push(raw.join(index));
            }
        }
    }
    // Declaration files are types-only; they must never resolve as a runtime
    // entry point. './types' -> 'types.d.ts' would otherwise match and the
    // engine would try to load the .d.ts as a JS module.
    candidates.retain(|p| !is_declaration_file(p));

    let found = candidates
        .iter()
        .find(|c| is_file_cached(resolver, c))
        .map(|c| std::path::absolute(c).unwrap_or_else(|_| c.clone()));
    (found, candidates)
}

/// Byte offset -> 1-based line/col. Columns are counted in CHARACTERS (what
/// editors and LSP consumers of ODIF expect), not bytes; offsets are clamped
/// to the nearest char boundary so a mid-codepoint label can never panic.
fn offset_to_position(source: &str, offset: usize) -> Position {
    let mut offset = offset.min(source.len());
    while offset > 0 && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &source[..offset];
    let line = before.bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col = source[line_start..offset].chars().count() as u32 + 1;
    Position { line, col }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ts(name: &str, source: &str) -> Result<String, TranspileError> {
        transpile_typescript(&PathBuf::from(name), source)
    }

    #[test]
    fn strips_type_annotations() {
        let out = ts("a.ts", "const x: number = 1;\nconsole.log(x);").unwrap();
        assert!(out.contains("const x = 1"), "got: {out}");
        assert!(!out.contains("number"), "got: {out}");
    }

    #[test]
    fn lowers_enums_beyond_nodes_strip_support() {
        let out = ts("e.ts", "enum Color { Red, Green }\nconsole.log(Color.Red);").unwrap();
        assert!(!out.contains("enum "), "got: {out}");
        assert!(out.contains("Color"), "got: {out}");
    }

    #[test]
    fn preserves_modern_syntax_without_downleveling() {
        let out = ts(
            "m.ts",
            "let a: string | null = null;\na ??= 'x';\nconsole.log(a?.length);",
        )
        .unwrap();
        assert!(out.contains("??="), "got: {out}");
        assert!(out.contains("?."), "got: {out}");
    }

    #[test]
    fn parse_errors_become_odif_with_spans() {
        let err = ts("bad.ts", "const x: = 1;").unwrap_err();
        let d = &err.diagnostics[0];
        assert_eq!(d.code, "OAM-PARSE0001");
        assert_eq!(d.origin, oam_diagnostics::Origin::Parse);
        assert!(!d.spans.is_empty(), "expected a span");
        assert_eq!(d.spans[0].start.line, 1);
        let jsonl = d.to_jsonl();
        assert!(jsonl.contains("\"odif\":\"1\""));
    }

    #[test]
    fn classifies_extensions() {
        assert_eq!(classify(Path::new("a.ts")), SourceKind::TypeScript);
        assert_eq!(classify(Path::new("a.mts")), SourceKind::TypeScript);
        assert_eq!(classify(Path::new("a.tsx")), SourceKind::Jsx);
        assert_eq!(classify(Path::new("a.js")), SourceKind::JavaScript);
        assert_eq!(classify(Path::new("a.mjs")), SourceKind::JavaScript);
    }

    #[test]
    fn offset_to_position_is_one_based() {
        let src = "ab\ncd";
        assert_eq!(offset_to_position(src, 0), Position { line: 1, col: 1 });
        assert_eq!(offset_to_position(src, 3), Position { line: 2, col: 1 });
        assert_eq!(offset_to_position(src, 4), Position { line: 2, col: 2 });
    }

    #[test]
    fn resolver_skips_declaration_files() {
        // './types' must not resolve to 'types.d.ts' -- declaration files
        // are types-only and would otherwise be loaded as JS modules.
        let dir = std::env::temp_dir().join(format!("oam-dts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("types.d.ts"), "export declare const x: number;\n").unwrap();
        std::fs::write(dir.join("entry.ts"), "export const a = 1;\n").unwrap();
        let entry_ts = dir.join("entry.ts");
        let err = resolve_import("./types", &entry_ts)
            .expect_err("resolving './types' must not pick up types.d.ts");
        assert_eq!(err.code, "OAM-MOD0001");
    }

    #[test]
    fn resolver_clear_caches_lets_newly_added_files_resolve() {
        // The whole point of owning the cache on `Resolver` is that a
        // long-lived process (CLI, daemon) can drop stale entries when the
        // project layout changes. Simulate `oam install` adding a file in
        // the same process as a subsequent `oam run` resolve.
        let dir = std::env::temp_dir().join(format!("oam-cache-clear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("entry.ts"), "export {};\n").unwrap();
        let entry_ts = dir.join("entry.ts");
        let resolver = Resolver::new();

        // 1) First resolve: the file does not exist. The negative cache
        //    records the probe so the next call short-circuits.
        let err = resolver
            .resolve_import("./missing", &entry_ts)
            .expect_err("expected negative result before the file exists");
        assert_eq!(err.code, "OAM-MOD0001");

        // 2) The file appears (think: `oam install` wrote node_modules/foo).
        //    The negative cache still says "not found" -- this is the stale
        //    state the cache can serve.
        std::fs::write(dir.join("missing.ts"), "export const x = 1;\n").unwrap();
        let stale = resolver.resolve_import("./missing", &entry_ts);
        assert!(
            stale.is_err(),
            "without clear_caches the negative entry still wins: {stale:?}"
        );

        // 3) Drop the stale entries. The next resolve re-stats the
        //    filesystem and finds the newly-written file.
        resolver.clear_caches();
        let fresh = resolver
            .resolve_import("./missing", &entry_ts)
            .expect("after clear_caches the file must resolve");
        assert!(fresh.ends_with("missing.ts"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_relative_import_keeps_referrer_prefix() {
        // A specifier starting with '/' must NOT walk up from the referrer's
        // parent (the relative-import rule). It anchors at the referrer's
        // root so behavior matches POSIX semantics: '/x' from anywhere
        // inside the same logical filesystem is the same '/x'.
        //
        // We can't make a real '/x' file exist on the test machine, so the
        // assertion reads the candidate list out of the OAM-MOD0001 error.
        // The candidates are exactly `[ /x, /x.ts, /x.mts, /x.js, /x.mjs,
        // /x/index.ts, /x/index.js ]` — '/x', NOT '/proj/x'. That's the
        // invariant: the leading '/' is preserved by the root-relative
        // branch and not rewritten as a parent walk.
        let referrer = PathBuf::from("/proj/entry.ts");
        let err = resolve_import("/x", &referrer).expect_err(
            "'/x' cannot exist on the test machine, but the candidates list is the contract",
        );
        assert_eq!(err.code, "OAM-MOD0001");
        let candidates: Vec<&str> = err
            .message
            .split("(tried ")
            .nth(1)
            .unwrap_or("")
            .trim_end_matches(')')
            .split(", ")
            .collect();
        // Portable structural invariant: the first candidate is the raw
        // root-relative path with the specifier suffix only — no 'proj'
        // segment, no 'entry' parent walk. Whatever the platform's
        // separator display, '/proj/entry.ts' importing '/x' must probe
        // root-anchored 'x', not 'proj/x' or 'proj/entry.ts/x'.
        assert!(
            !candidates[0].contains("proj") && !candidates[0].contains("entry"),
            "first candidate must be root-anchored '/x', not derived from the referrer's tree; got: {:?}",
            candidates
        );
        assert!(
            candidates[0].ends_with("x") && candidates[0].len() <= 3,
            "first candidate is just 'x' (with at most a leading separator); got: {:?}",
            candidates
        );
        // Directory index is also probed at the root anchor.
        let index_candidate = candidates
            .iter()
            .find(|c| c.contains("index.ts"))
            .expect("directory index must be probed");
        assert!(
            !index_candidate.contains("proj") && !index_candidate.contains("entry"),
            "index probe must be root-anchored, not under the referrer: {index_candidate:?}"
        );
    }

    #[test]
    fn dotted_basename_probes_by_appending_extension() {
        // './my.module' is a dotted basename: the '.module' segment is part
        // of the name, not an extension. The probe must APPEND '.ts' (not
        // `with_extension('ts')`, which would clobber '.module' to give
        // 'my.ts'). Locks in the tsgo-parity rule for dot-rich basenames.
        let dir = std::env::temp_dir().join(format!("oam-dotted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("entry.ts"), "export {};\n").unwrap();
        std::fs::write(dir.join("my.module.ts"), "export const x = 1;\n").unwrap();
        let entry_ts = dir.join("entry.ts");
        let resolved =
            resolve_import("./my.module", &entry_ts).expect("dotted basename must resolve");
        assert!(resolved.ends_with("my.module.ts"), "got: {resolved:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn js_extension_falls_back_to_ts_source() {
        // './x.js' must resolve to './x.ts' on disk -- the tsgo rewrite
        // convention. This is the inverse of the dotted-basename test: here
        // the extension IS the suffix to swap, so `with_extension('ts')`
        // is the right tool. Locks in the symmetric half of the rule.
        let dir = std::env::temp_dir().join(format!("oam-js2ts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("entry.ts"), "export {};\n").unwrap();
        std::fs::write(dir.join("x.ts"), "export const x = 1;\n").unwrap();
        let entry_ts = dir.join("entry.ts");
        let resolved = resolve_import("./x.js", &entry_ts).expect("./x.js must hit x.ts");
        assert!(resolved.ends_with("x.ts"), "got: {resolved:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_specifier_shapes_emit_oam_mod0004() {
        // Four shapes that are invalid as ESM specifiers regardless of
        // filesystem state: empty, lone '.', lone '..', backslash. They get
        // their own diagnostic (OAM-MOD0004) -- npm resolution will never
        // fix them, so we don't waste a MOD0002 round-trip on them.
        let referrer = PathBuf::from("entry.ts");
        for spec in ["", ".", "..", "foo\\bar"] {
            match resolve_import(spec, &referrer) {
                Ok(p) => panic!("{spec:?} should be Err, got Ok: {p:?}"),
                Err(diagnostic) => {
                    assert_eq!(diagnostic.code, "OAM-MOD0004", "specifier: {spec:?}");
                }
            }
        }
    }

    #[test]
    fn transform_stage_errors_carry_oam_parse0002() {
        // Locks in the OAM-PARSE0002 code path: parse-clearly, transform-no.
        // '<svg:rect />' parses fine in .tsx, but the JSX transformer's
        // namespace-tag check (throwIfNamespace=true by default in oxc)
        // rejects it -- surfacing as a transform-stage error in
        // `Transformer::build_with_scoping` rather than a parse error in
        // `Parser::parse`. The code on line 89 maps that to OAM-PARSE0002.
        //
        // If a future oxc release changes the default to
        // `throwIfNamespace: false`, this test will start failing -- which
        // is the right signal: we need a new transform-only failure
        // candidate (or to accept that OAM-PARSE0002 is unreachable and
        // remove the code).
        let src = r#"const x = <svg:rect />;"#;
        let err = ts("e.tsx", src).expect_err("namespaced JSX should fail at transform stage");
        let d = &err.diagnostics[0];
        assert_eq!(d.code, "OAM-PARSE0002", "got: {d:?}");
        assert_eq!(d.origin, oam_diagnostics::Origin::Parse);
        assert!(
            !d.spans.is_empty(),
            "expected a span on the transform error"
        );
    }

    #[test]
    fn declaration_file_filter_is_uniform() {
        // The filter on probe_candidates::is_declaration_file runs
        // UNCONDITIONALLY, even when the import specifier names the
        // declaration file explicitly. `./types.d.ts` is types-only and
        // must never load as a runtime module -- "importing it by name"
        // doesn't make it a runtime artifact.
        let dir = std::env::temp_dir().join(format!("oam-dts-explicit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("types.d.ts"), "export declare const x: number;\n").unwrap();
        std::fs::write(dir.join("entry.ts"), "import './types.d.ts';\nexport {};\n").unwrap();
        let entry_ts = dir.join("entry.ts");
        let err = resolve_import("./types.d.ts", &entry_ts)
            .expect_err("explicit .d.ts import must still be filtered");
        assert_eq!(err.code, "OAM-MOD0001", "got: {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
