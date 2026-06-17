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
mod resolver;
mod tsconfig;
pub use npm::{ModuleKind, module_kind, resolve_require};
pub use resolver::{Resolver, default_resolver};
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};

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
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());

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

    let options = TransformOptions::default();
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
/// always wrong. Suffix-based so the match survives odd casings and
/// platform separators.
fn is_declaration_file(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".d.ts")
        || name.ends_with(".d.mts")
        || name.ends_with(".d.cts")
        || name.ends_with(".d.tsx")
        || name.ends_with(".d.jsx")
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
}
