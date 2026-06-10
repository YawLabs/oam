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

mod npm;
mod tsconfig;
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
/// M1 slice: relative + absolute paths only. Candidate order for './x':
/// exact (if it has an extension), TS-source fallback for JS extensions
/// ('./x.js' -> x.ts, the tsgo rewrite convention), then extensionless
/// probing (.ts, .mts, .js, .mjs) and directory index (index.ts, index.js).
/// Bare and node: specifiers are a clear diagnostic until npm resolution
/// lands (M2).
pub fn resolve_import(specifier: &str, referrer: &Path) -> Result<PathBuf, Diagnostic> {
    // Shapes that are invalid as ESM specifiers everywhere — npm resolution
    // will never fix these, so they get their own diagnostic, not MOD0002.
    if specifier.is_empty() || specifier == "." || specifier == ".." || specifier.contains('\\') {
        return Err(Diagnostic::new(
            "OAM-MOD0004",
            Severity::Error,
            Origin::Resolve,
            format!(
                "invalid module specifier '{specifier}' in {}: use './name' / '../name' with forward slashes",
                referrer.display()
            ),
        ));
    }

    let is_relative = specifier.starts_with("./") || specifier.starts_with("../");
    let is_root_relative = specifier.starts_with('/');
    if !is_relative && !is_root_relative && !Path::new(specifier).is_absolute() {
        // Bare specifier: tsconfig paths get first crack (plan §2.6 — the
        // resolver honors tsconfig exactly as tsgo does), then the Node ESM
        // node_modules walk.
        let mut consulted_paths = false;
        if let Some(config) = tsconfig::load_for(referrer) {
            consulted_paths = true;
            for raw in tsconfig::match_specifier(&config, specifier) {
                if let (Some(found), _) = probe_candidates(&raw) {
                    return Ok(found);
                }
            }
        }
        return npm::resolve_bare(specifier, referrer).map_err(|mut failure| {
            if consulted_paths && failure.code == "OAM-MOD0002" {
                failure
                    .message
                    .push_str(" (tsconfig paths were consulted; no pattern produced a file)");
            }
            failure
        });
    }

    let raw = if is_relative {
        let base = referrer.parent().unwrap_or_else(|| Path::new("."));
        base.join(specifier)
    } else if is_root_relative && !Path::new(specifier).is_absolute() {
        // Windows: '/x' is drive-relative — anchor it at the referrer's root
        // so behavior matches POSIX ('/' = filesystem root of the referrer).
        let mut root: PathBuf = referrer
            .components()
            .take_while(|c| {
                matches!(
                    c,
                    std::path::Component::Prefix(_) | std::path::Component::RootDir
                )
            })
            .collect();
        root.push(specifier.trim_start_matches('/'));
        root
    } else {
        PathBuf::from(specifier)
    };

    let (found, candidates) = probe_candidates(&raw);
    found.ok_or_else(|| {
        Diagnostic::new(
            "OAM-MOD0001",
            Severity::Error,
            Origin::Resolve,
            format!(
                "cannot resolve '{specifier}' from {} (tried {})",
                referrer.display(),
                candidates
                    .iter()
                    .map(|c| c.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    })
}

/// Probe a raw path for the actual module file. Candidate order: exact,
/// TS-source fallback for JS extensions, then APPENDED extensions +
/// directory index. Appending (not with_extension) keeps dotted basenames
/// intact: './my.module' probes 'my.module.ts', never clobbers the
/// '.module' segment. Returns (found-absolute, every candidate tried).
fn probe_candidates(raw: &Path) -> (Option<PathBuf>, Vec<PathBuf>) {
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

    let found = candidates
        .iter()
        .find(|c| c.is_file())
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
