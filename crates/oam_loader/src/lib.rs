//! oam_loader: the module pipeline.
//!
//! What lives here:
//!
//! - TypeScript strip/transform via oxc (`transpile_typescript`). Source
//!   comes in as .ts/.mts/.cts/.tsx/.jsx and comes out as plain JavaScript:
//!   types removed, non-erasable syntax (enums, namespaces, parameter
//!   properties) lowered -- strictly more than Node's strip-only support --
//!   and JSX compiled for the runtime the nearest tsconfig asks for. Parse
//!   and transform failures surface as ODIF diagnostics (origin: parse),
//!   never as prose.
//! - ESM resolution (`Resolver::resolve_import`): relative/absolute probing
//!   with tsc's extension-substitution rules (`probe_candidates`), tsconfig
//!   `paths`, and the Node ESM node_modules walk with `exports` / `imports`
//!   conditions.
//! - CJS resolution (`Resolver::resolve_require`): Node's require algorithm
//!   under require conditions, sharing the tsconfig and package logic.
//! - tsconfig discovery (`find_tsconfig`) with per-`Resolver` caches, the
//!   `extends` chain merged per option, and deferred warnings
//!   (`take_warnings`) for configs oam cannot honor.
//! - `oam install` (`install`), install-time pre-compilation (`precompile`),
//!   and the lifecycle-script trust store (`trust`).
//!
//! Still ahead: content-addressed transform caches keyed on
//! `transpile_fingerprint`, and bare-package tsconfig `extends`.

// AI-POLICY gate 5: this crate carries no `unsafe`. `forbid` (not `deny`) so it
// can never be silently reintroduced under an inner `#[allow(unsafe_code)]`.
#![forbid(unsafe_code)]
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
pub mod sourcemap;
pub mod transpile_cache;
pub mod trust;
mod tsconfig;
mod warnings;
pub use npm::{ModuleKind, module_kind, resolve_require};
pub use warnings::take_warnings;

/// Whether `specifier` names a Node builtin (bare like `fs` or prefixed
/// like `node:fs`) or an `oam:` runtime module. Engine-side surfaces
/// (`require.resolve.paths` returns null for builtins) need the check
/// without re-owning the builtin list.
pub fn is_builtin_specifier(specifier: &str) -> bool {
    specifier.starts_with("node:")
        || specifier.starts_with("oam:")
        || npm::is_node_builtin(specifier)
        || is_exposed_internal(specifier)
}

/// `internal/...` resolves as a builtin ONLY under `--expose-internals`
/// (the CLI sets OAM_EXPOSE_INTERNALS for the loader). Node gates these the
/// same way, and they stay out of `builtinModules` in both runtimes: they
/// are a test/debug surface, never public API. Without the flag the
/// specifier keeps failing exactly as before -- as a missing package.
pub fn is_exposed_internal(specifier: &str) -> bool {
    specifier.starts_with("internal/")
        && std::env::var("OAM_EXPOSE_INTERNALS").as_deref() == Ok("1")
}
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxOptions, JsxRuntime, TransformOptions, Transformer};
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
    /// .tsx/.jsx -- JSX compiled per the nearest tsconfig's `jsx` settings
    /// (automatic runtime from `react/jsx-runtime` by default), then the
    /// same strip/transform as TypeScript.
    Jsx,
}

pub fn classify(path: &Path) -> SourceKind {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "ts" | "mts" | "cts" => SourceKind::TypeScript,
        "tsx" | "jsx" => SourceKind::Jsx,
        _ => SourceKind::JavaScript,
    }
}

/// True when `path` goes through `transpile_typescript` before the engine
/// sees it (.ts/.mts/.cts/.tsx/.jsx); plain JavaScript loads as-is.
pub fn is_transpiled_source(path: &Path) -> bool {
    classify(path) != SourceKind::JavaScript
}

/// Nearest `tsconfig.json` at or above `referrer` (a file or a directory),
/// through the current thread's default `Resolver`'s discovery cache.
/// Pure discovery: no parsing, no node_modules boundary. Long-lived
/// consumers should call `Resolver::find_tsconfig` on their own `Resolver`
/// so `clear_caches()` governs what this sees.
pub fn find_tsconfig(referrer: &Path) -> Option<PathBuf> {
    default_resolver().find_tsconfig(referrer)
}

/// Bumped whenever the transform pipeline changes in a way the oxc crate
/// versions do not capture: a new `TransformOptions` field, a changed
/// source-type policy in `transpile_config`, a different codegen setting, a
/// new tsconfig option feeding the JSX transform. It is part of every
/// `transpile_fingerprint`, so a cache keyed on the fingerprint invalidates
/// on the bump. History: 1 = module-kind policy + jsx mode/factory/source;
/// 2 = CJS-routed sources pinned to the CommonJS source type (a Cjs-routed
/// .jsx was Unambiguous before, so its injected JSX runtime import could
/// come out ESM-shaped); 3 = source maps generated for module loading and
/// embedded in the cache artifacts (v2 layouts), plus path-addressed
/// `react-jsxdev` fingerprints.
pub const TRANSPILE_FORMAT_VERSION: u32 = 3;

/// The oxc crate versions that shape transpile output, read from Cargo.lock
/// at build time (build.rs) so they can never drift from what is linked.
const OXC_TRANSFORMER_VERSION: &str = env!("OAM_OXC_TRANSFORMER_VERSION");
const OXC_CODEGEN_VERSION: &str = env!("OAM_OXC_CODEGEN_VERSION");

/// Everything outside the source text that shapes `transpile_typescript`
/// output for one file: the oxc source type after oam's module-kind policy,
/// and the JSX settings -- resolved only for JSX files, so a plain .ts never
/// touches tsconfig here.
struct TranspileConfig {
    source_type: SourceType,
    jsx: Option<tsconfig::JsxSettings>,
}

fn transpile_config(resolver: &Resolver, path: &Path) -> TranspileConfig {
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    // MODULE unless the engine routes this file through CommonJS.
    //
    // oxc's JSX transform picks the shape of its injected runtime import
    // from the source type: a Script gets `var _x = require("react/jsx-
    // runtime")`, which is undefined in oam's ESM context, so an import-free
    // .tsx file died with "require is not defined" (3f1ef62). Pinning
    // Module fixes that -- but pinning it for EVERY file turned .cts (oxc:
    // CommonJS) into an ES module at parse time, and the parser grants
    // top-level `return` / `new.target` and `await` as an identifier only to
    // CommonJS. So the policy follows the engine's own routing
    // (`module_kind`): a file that will execute through CJS interop is
    // PINNED to oxc's CommonJS source type; everything else is Module.
    // Pinning (rather than keeping `from_path`'s kind) matters for the
    // extensions from_path leaves Unambiguous: a .jsx routed CommonJS by
    // package "type" would otherwise parse module-or-script on content, and
    // the JSX transform could inject an ESM `import` of the jsx-runtime
    // into what executes inside a require() wrapper. CommonJS keeps the
    // injected runtime require()-shaped and grants Node's CJS grammar
    // (top-level return, await-as-identifier).
    let source_type = if npm::module_kind(path) == ModuleKind::Cjs {
        source_type.with_commonjs(true)
    } else {
        source_type.with_module(true)
    };
    let jsx = source_type
        .is_jsx()
        .then(|| tsconfig::jsx_settings_for(resolver, path));
    TranspileConfig { source_type, jsx }
}

/// Apply the resolved tsconfig JSX settings to oxc's options. A per-file
/// pragma (`@jsxRuntime`, `@jsx`, `@jsxFrag`, `@jsxImportSource`) is parsed
/// by oxc itself and wins over all of this, matching tsc's precedence.
fn apply_jsx_settings(settings: &tsconfig::JsxSettings, jsx: &mut JsxOptions) {
    use tsconfig::JsxMode;
    match settings.mode {
        JsxMode::Classic => {
            jsx.runtime = JsxRuntime::Classic;
            // oxc rejects pragma/pragmaFrag under the automatic runtime, so
            // the classic factories are only handed over here.
            jsx.pragma = settings.factory.clone();
            jsx.pragma_frag = settings.fragment_factory.clone();
        }
        JsxMode::Automatic | JsxMode::AutomaticDev => {
            jsx.runtime = JsxRuntime::Automatic;
            jsx.development = settings.mode == JsxMode::AutomaticDev;
            jsx.import_source = settings.import_source.clone();
        }
    }
}

/// A stable string of every non-source input that changes
/// `transpile_typescript` output for `path`: the oxc transformer and codegen
/// versions (from Cargo.lock at build time), `TRANSPILE_FORMAT_VERSION`, the
/// resolved source type, and the resolved JSX settings (mode, import
/// source, classic factories). Two files with the same source text and the
/// same fingerprint transpile identically, which is what a content-addressed
/// transform cache keys on. Resolved through the thread's default
/// `Resolver`; see `transpile_fingerprint_with`.
pub fn transpile_fingerprint(path: &Path) -> String {
    transpile_fingerprint_with(default_resolver(), path)
}

/// `transpile_fingerprint` against an explicit `Resolver` (its tsconfig
/// caches decide the JSX settings).
pub fn transpile_fingerprint_with(resolver: &Resolver, path: &Path) -> String {
    let TranspileConfig { source_type, jsx } = transpile_config(resolver, path);
    let language = if source_type.is_typescript_definition() {
        "dts"
    } else if source_type.is_typescript() {
        "ts"
    } else {
        "js"
    };
    let module = if source_type.is_commonjs() {
        "commonjs"
    } else if source_type.is_module() {
        "module"
    } else if source_type.is_script() {
        "script"
    } else {
        "unambiguous"
    };
    // Under the automatic-DEV runtime the emitted `_jsxFileName` embeds the
    // SOURCE PATH, so two identical sources at different paths transpile to
    // different output. Fold the path into the fingerprint for exactly that
    // mode -- dev-mode cache entries become path-addressed while the common
    // modes stay content-addressed.
    let path_facet = match &jsx {
        Some(settings) if settings.mode == tsconfig::JsxMode::AutomaticDev => {
            format!(";path={}", path.to_string_lossy())
        }
        _ => String::new(),
    };
    let jsx = match jsx {
        Some(settings) => settings.fingerprint(),
        None => "none".to_string(),
    };
    format!(
        "oxc_transformer={OXC_TRANSFORMER_VERSION};oxc_codegen={OXC_CODEGEN_VERSION};\
         format={TRANSPILE_FORMAT_VERSION};language={language};module={module};\
         jsx_syntax={};jsx={jsx}{path_facet}",
        source_type.is_jsx()
    )
}

/// Transpile TypeScript source to plain JavaScript (types stripped,
/// non-erasable syntax lowered, modern JS syntax preserved as-is).
///
/// Thin shim over `transpile_typescript_with` on the current thread's
/// default `Resolver`; long-lived consumers should own a `Resolver` so
/// `clear_caches()` reaches the tsconfig state the JSX settings come from.
pub fn transpile_typescript(path: &Path, source: &str) -> Result<String, TranspileError> {
    transpile_typescript_with(default_resolver(), path, source)
}

/// `transpile_typescript` against an explicit `Resolver`.
pub fn transpile_typescript_with(
    resolver: &Resolver,
    path: &Path,
    source: &str,
) -> Result<String, TranspileError> {
    transpile_impl(resolver, path, source, false, false).map(|out| out.code)
}

/// `transpile_typescript` plus a source map for runtime position remap.
/// This is what the module-loading paths call: the map is what lets stack
/// traces and uncaught-error reports cite the .ts line the user wrote
/// instead of the codegen line (codegen reflows -- see `sourcemap`).
pub fn transpile_typescript_mapped(
    path: &Path,
    source: &str,
) -> Result<TranspileOutput, TranspileError> {
    transpile_typescript_mapped_with(default_resolver(), path, source)
}

/// `transpile_typescript_mapped` against an explicit `Resolver`.
pub fn transpile_typescript_mapped_with(
    resolver: &Resolver,
    path: &Path,
    source: &str,
) -> Result<TranspileOutput, TranspileError> {
    transpile_impl(resolver, path, source, false, true)
}

/// `transpile_typescript` plus what the REPL needs from the same parse.
#[derive(Debug)]
pub struct TranspileOutput {
    /// The transpiled JavaScript.
    pub code: String,
    /// Source map JSON (generated -> source), present only on the `mapped`
    /// entry points. `sourcesContent` is stripped: runtime position lookup
    /// never needs it, and embedding it would double every cache artifact.
    pub source_map: Option<String>,
    /// True only for REAL top-level await usage -- an `await` expression, a
    /// `for await`, or an `await using` outside every function body. Never
    /// set for `await` as an identifier (`let awaiting = 1`) or for awaits
    /// inside nested function/arrow bodies.
    pub top_level_await: bool,
    /// Names bound by the source's DIRECT top-level declarations (var/let/
    /// const including destructuring, function, class, enum), in source
    /// order. The REPL hoists these onto globalThis when it must wrap an
    /// awaiting line in an async IIFE, so the bindings survive the wrapper.
    pub top_level_bindings: Vec<String>,
}

/// `transpile_typescript` with REPL metadata (real top-level-await
/// detection + top-level binding names) from the same parse. Costs one
/// extra AST walk over `transpile_typescript`, so the plain fn stays the
/// right call for module loading.
pub fn transpile_typescript_rich(
    path: &Path,
    source: &str,
) -> Result<TranspileOutput, TranspileError> {
    transpile_impl(default_resolver(), path, source, true, false)
}

fn transpile_impl(
    resolver: &Resolver,
    path: &Path,
    source: &str,
    collect_repl_meta: bool,
    emit_map: bool,
) -> Result<TranspileOutput, TranspileError> {
    let file = path.to_string_lossy().into_owned();
    let allocator = Allocator::default();
    let TranspileConfig { source_type, jsx } = transpile_config(resolver, path);

    let parsed = Parser::new(&allocator, source, source_type).parse();
    if !parsed.errors.is_empty() {
        return Err(TranspileError {
            diagnostics: to_odif(&file, source, &parsed.errors, "OAM-PARSE0001"),
            file,
        });
    }
    let (top_level_await, top_level_bindings) = if collect_repl_meta {
        repl_meta(&parsed.program)
    } else {
        (false, Vec::new())
    };
    let mut program = parsed.program;

    // with_enum_eval: the enum transform needs pre-computed member values in
    // Scoping or string-valued members produce wrong reverse mappings (oxc#21667).
    let scoping = SemanticBuilder::new()
        .with_enum_eval(true)
        .build(&program)
        .semantic
        .into_scoping();

    let mut options = TransformOptions::default();
    if let Some(settings) = &jsx {
        apply_jsx_settings(settings, &mut options.jsx);
    }
    let transformed =
        Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
    if !transformed.errors.is_empty() {
        return Err(TranspileError {
            diagnostics: to_odif(&file, source, &transformed.errors, "OAM-PARSE0002"),
            file,
        });
    }

    let generated = if emit_map {
        Codegen::new()
            .with_options(CodegenOptions {
                // The path names the map's single `sources` entry; the
                // runtime registry keys on the module path it records
                // under, so this is informational.
                source_map_path: Some(path.to_path_buf()),
                ..CodegenOptions::default()
            })
            .build(&program)
    } else {
        Codegen::new().build(&program)
    };
    let source_map = generated.map.map(|mut map| {
        // Position lookup never reads sourcesContent, and embedding it
        // would carry a full copy of the source into every cache artifact.
        map.as_source_map_mut().set_source_contents(vec![None]);
        map.to_json_string()
    });

    Ok(TranspileOutput {
        code: generated.code,
        source_map,
        top_level_await,
        top_level_bindings,
    })
}

/// REPL metadata from the PARSED (pre-transform) AST. See
/// `TranspileOutput`: `top_level_await` is real usage only (the visitor
/// tracks function depth, so awaits inside function/arrow bodies never
/// count), and the binding names come from the program's DIRECT top-level
/// statements (a `let` inside a top-level block is block-scoped and dies
/// with its block either way).
fn repl_meta(program: &oxc_ast::ast::Program<'_>) -> (bool, Vec<String>) {
    use oxc_ast::ast::{
        ArrowFunctionExpression, AwaitExpression, BindingPattern, ForOfStatement, Function,
        Statement, VariableDeclaration, VariableDeclarationKind,
    };
    use oxc_ast_visit::Visit;

    struct TlaFinder {
        found: bool,
        function_depth: u32,
    }
    impl<'a> Visit<'a> for TlaFinder {
        fn visit_function(&mut self, it: &Function<'a>, flags: oxc_semantic::ScopeFlags) {
            self.function_depth += 1;
            oxc_ast_visit::walk::walk_function(self, it, flags);
            self.function_depth -= 1;
        }
        fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
            self.function_depth += 1;
            oxc_ast_visit::walk::walk_arrow_function_expression(self, it);
            self.function_depth -= 1;
        }
        fn visit_await_expression(&mut self, it: &AwaitExpression<'a>) {
            if self.function_depth == 0 {
                self.found = true;
            }
            oxc_ast_visit::walk::walk_await_expression(self, it);
        }
        fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
            if it.r#await && self.function_depth == 0 {
                self.found = true;
            }
            oxc_ast_visit::walk::walk_for_of_statement(self, it);
        }
        fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
            if it.kind == VariableDeclarationKind::AwaitUsing && self.function_depth == 0 {
                self.found = true;
            }
            oxc_ast_visit::walk::walk_variable_declaration(self, it);
        }
    }
    let mut finder = TlaFinder {
        found: false,
        function_depth: 0,
    };
    finder.visit_program(program);

    fn binding_names(pattern: &BindingPattern<'_>, out: &mut Vec<String>) {
        match pattern {
            BindingPattern::BindingIdentifier(id) => out.push(id.name.to_string()),
            BindingPattern::ObjectPattern(object) => {
                for property in &object.properties {
                    binding_names(&property.value, out);
                }
                if let Some(rest) = &object.rest {
                    binding_names(&rest.argument, out);
                }
            }
            BindingPattern::ArrayPattern(array) => {
                for element in array.elements.iter().flatten() {
                    binding_names(element, out);
                }
                if let Some(rest) = &array.rest {
                    binding_names(&rest.argument, out);
                }
            }
            BindingPattern::AssignmentPattern(assignment) => binding_names(&assignment.left, out),
        }
    }

    let mut names = Vec::new();
    for statement in &program.body {
        match statement {
            Statement::VariableDeclaration(declaration) => {
                for declarator in &declaration.declarations {
                    binding_names(&declarator.id, &mut names);
                }
            }
            Statement::FunctionDeclaration(function) => {
                if let Some(id) = &function.id {
                    names.push(id.name.to_string());
                }
            }
            Statement::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    names.push(id.name.to_string());
                }
            }
            Statement::TSEnumDeclaration(declaration) => {
                names.push(declaration.id.name.to_string());
            }
            _ => {}
        }
    }
    (finder.found, names)
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
pub fn is_declaration_file(p: &Path) -> bool {
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

/// Probe a raw path for the actual module file, in tsc's order. Candidates:
/// exact; the TS-source substitutions tsc applies to a JS extension
/// (`.js` -> `.ts` then `.tsx`, `.jsx` -> `.tsx`, `.mjs` -> `.mts`,
/// `.cjs` -> `.cts`); for an extensionless specifier the APPENDED
/// extensions `.ts .tsx .mts .js .jsx .mjs`, then the directory index
/// `index.ts index.tsx index.js`. Appending (not with_extension) keeps
/// dotted basenames intact: './my.module' probes 'my.module.ts', never
/// clobbers the '.module' segment. Returns (found-absolute, every
/// candidate tried). `oam check` (tsgo) resolves every one of these shapes,
/// so `oam run` must too or the two disagree on the same import.
pub(crate) fn probe_candidates(resolver: &Resolver, raw: &Path) -> (Option<PathBuf>, Vec<PathBuf>) {
    fn append_ext(p: &Path, ext: &str) -> PathBuf {
        let mut s = p.as_os_str().to_os_string();
        s.push(".");
        s.push(ext);
        PathBuf::from(s)
    }

    let mut candidates: Vec<PathBuf> = vec![raw.to_path_buf()];
    match raw.extension().and_then(|e| e.to_str()) {
        Some("js") => {
            candidates.push(raw.with_extension("ts"));
            candidates.push(raw.with_extension("tsx"));
        }
        Some("jsx") => candidates.push(raw.with_extension("tsx")),
        Some("mjs") => candidates.push(raw.with_extension("mts")),
        Some("cjs") => candidates.push(raw.with_extension("cts")),
        Some("ts") | Some("mts") | Some("cts") | Some("tsx") | Some("json") => {}
        _ => {
            // No extension, or a dotted basename ('./my.module'): probe by
            // appending, then directory index.
            for ext in ["ts", "tsx", "mts", "js", "jsx", "mjs"] {
                candidates.push(append_ext(raw, ext));
            }
            for index in ["index.ts", "index.tsx", "index.js"] {
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

    /// A fresh, empty temp dir unique to this test invocation.
    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CNT: AtomicU64 = AtomicU64::new(0);
        let id = CNT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("oam-lib-{tag}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    fn is_transpiled_source_covers_every_transpiled_extension() {
        for transpiled in ["a.ts", "a.mts", "a.cts", "a.tsx", "a.jsx", "dir/x.d.ts"] {
            assert!(is_transpiled_source(Path::new(transpiled)), "{transpiled}");
        }
        for plain in ["a.js", "a.mjs", "a.cjs", "a.json", "a", "a.node"] {
            assert!(!is_transpiled_source(Path::new(plain)), "{plain}");
        }
        assert!(is_declaration_file(Path::new("x/TYPES.D.TS")));
        assert!(!is_declaration_file(Path::new("x/types.ts")));
    }

    /// `.cts` executes through CJS interop, where the module wrapper makes
    /// top-level `return` legal and `await` an ordinary identifier -- as in
    /// `.cjs`. Forcing every file to oxc's Module kind (the 3f1ef62 fix for
    /// import-free .tsx) had turned both into OAM-PARSE0001 for .cts only.
    #[test]
    fn cts_keeps_commonjs_parse_rules() {
        let out = ts(
            "guard.cts",
            "console.log('before');\nif (process.env.SKIP) return;\nconsole.log('after');",
        )
        .expect("top-level return is legal in a CommonJS file");
        assert!(out.contains("return"), "got: {out}");

        let out = ts("await-ident.cts", "var await = 1;\nconsole.log(await);")
            .expect("`await` is an identifier outside modules");
        assert!(out.contains("await = 1"), "got: {out}");

        let out = ts("nt.cts", "console.log(typeof new.target);")
            .expect("top-level new.target is legal in a CommonJS file");
        assert!(out.contains("new.target"), "got: {out}");

        // ESM-routed TypeScript still parses as a module: top-level await
        // works there and a stray top-level return is the error it should be.
        assert!(
            ts(
                "tla.mts",
                "const x = await Promise.resolve(1);\nconsole.log(x);"
            )
            .is_ok()
        );
        let err = ts("ret.ts", "return 1;").expect_err("top-level return in an ES module");
        assert_eq!(err.diagnostics[0].code, "OAM-PARSE0001");
    }

    /// 3f1ef62 must survive the module-kind policy: a .tsx with no
    /// import/export of its own is still compiled as a MODULE, so the JSX
    /// runtime is injected as an `import` (not a `require`, which is
    /// undefined in oam's ESM context).
    #[test]
    fn import_free_tsx_still_gets_esm_jsx_runtime_injection() {
        let out = ts("App.tsx", "const el = <div>hi</div>;\nconsole.log(el);").unwrap();
        assert!(
            out.contains("import ") && out.contains("react/jsx-runtime"),
            "got: {out}"
        );
        assert!(!out.contains("require("), "got: {out}");
    }

    #[test]
    fn offset_to_position_is_one_based() {
        let src = "ab\ncd";
        assert_eq!(offset_to_position(src, 0), Position { line: 1, col: 1 });
        assert_eq!(offset_to_position(src, 3), Position { line: 2, col: 1 });
        assert_eq!(offset_to_position(src, 4), Position { line: 2, col: 2 });
    }

    /// The candidate lists, shape by shape, in tsc's order.
    #[test]
    fn probe_candidates_follow_tsc_extension_substitution() {
        let resolver = Resolver::new();
        let names = |raw: &str| -> Vec<String> {
            probe_candidates(&resolver, Path::new(raw))
                .1
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            names("Button.js"),
            vec!["Button.js", "Button.ts", "Button.tsx"]
        );
        assert_eq!(names("Button.jsx"), vec!["Button.jsx", "Button.tsx"]);
        assert_eq!(names("x.mjs"), vec!["x.mjs", "x.mts"]);
        assert_eq!(names("x.cjs"), vec!["x.cjs", "x.cts"]);
        assert_eq!(names("x.tsx"), vec!["x.tsx"]);
        assert_eq!(
            names("Button"),
            vec![
                "Button",
                "Button.ts",
                "Button.tsx",
                "Button.mts",
                "Button.js",
                "Button.jsx",
                "Button.mjs",
                "index.ts",
                "index.tsx",
                "index.js",
            ]
        );
    }

    /// Button.tsx on disk: './Button', './Button.js' and './Button.jsx' all
    /// resolve to it (tsgo does, so `oam check` passed while `oam run`
    /// failed with OAM-MOD0001), and a directory resolves via index.tsx.
    #[test]
    fn tsx_resolves_from_every_specifier_shape_tsc_accepts() {
        let dir = temp_dir("tsx-shapes");
        std::fs::write(dir.join("entry.tsx"), "export {};\n").unwrap();
        std::fs::write(dir.join("Button.tsx"), "export const Button = 1;\n").unwrap();
        std::fs::create_dir_all(dir.join("widgets")).unwrap();
        std::fs::write(dir.join("widgets/index.tsx"), "export const W = 1;\n").unwrap();
        let entry = dir.join("entry.tsx");
        let resolver = Resolver::new();
        for spec in ["./Button", "./Button.js", "./Button.jsx"] {
            let resolved = resolver
                .resolve_import(spec, &entry)
                .unwrap_or_else(|e| panic!("{spec}: {e:?}"));
            assert!(resolved.ends_with("Button.tsx"), "{spec} -> {resolved:?}");
        }
        let resolved = resolver.resolve_import("./widgets", &entry).unwrap();
        assert!(resolved.ends_with("index.tsx"), "got {resolved:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn jsx_with_tsconfig(tag: &str, tsconfig: &str, source: &str) -> String {
        let dir = temp_dir(tag);
        std::fs::write(dir.join("tsconfig.json"), tsconfig).unwrap();
        let file = dir.join("App.tsx");
        std::fs::write(&file, source).unwrap();
        let out = transpile_typescript_with(&Resolver::new(), &file, source)
            .unwrap_or_else(|e| panic!("{tag}: {e:?}"));
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// `compilerOptions.jsx` selects the runtime the way tsc does; `preserve`
    /// cannot be honored at execution time and compiles as automatic
    /// (docs/node-divergences.md section 12).
    #[test]
    fn tsconfig_jsx_mode_selects_runtime_and_factories() {
        let src = "const el = <><div>hi</div></>;\nconsole.log(el);";

        let classic =
            jsx_with_tsconfig("jsx-classic", r#"{"compilerOptions":{"jsx":"react"}}"#, src);
        assert!(classic.contains("React.createElement("), "got: {classic}");
        assert!(classic.contains("React.Fragment"), "got: {classic}");
        assert!(!classic.contains("jsx-runtime"), "got: {classic}");

        let factories = jsx_with_tsconfig(
            "jsx-factory",
            r#"{"compilerOptions":{"jsx":"react","jsxFactory":"h","jsxFragmentFactory":"Frag"}}"#,
            src,
        );
        assert!(factories.contains("h(Frag,"), "got: {factories}");
        assert!(!factories.contains("React"), "got: {factories}");

        let automatic = jsx_with_tsconfig(
            "jsx-automatic",
            r#"{"compilerOptions":{"jsx":"react-jsx","jsxImportSource":"preact"}}"#,
            src,
        );
        assert!(automatic.contains("preact/jsx-runtime"), "got: {automatic}");
        assert!(!automatic.contains("createElement"), "got: {automatic}");

        let dev = jsx_with_tsconfig(
            "jsx-dev",
            r#"{"compilerOptions":{"jsx":"react-jsxdev"}}"#,
            src,
        );
        assert!(dev.contains("react/jsx-dev-runtime"), "got: {dev}");
        assert!(dev.contains("jsxDEV"), "got: {dev}");

        let preserve = jsx_with_tsconfig(
            "jsx-preserve",
            r#"{"compilerOptions":{"jsx":"preserve"}}"#,
            src,
        );
        assert!(preserve.contains("react/jsx-runtime"), "got: {preserve}");
        assert!(
            !preserve.contains("<div>"),
            "JSX must not survive: {preserve}"
        );
    }

    /// A per-file pragma wins over the tsconfig mode (tsc precedence).
    #[test]
    fn per_file_pragma_wins_over_tsconfig_jsx_mode() {
        let out = jsx_with_tsconfig(
            "jsx-pragma",
            r#"{"compilerOptions":{"jsx":"react"}}"#,
            "/** @jsxRuntime automatic @jsxImportSource solid-js */\nconst el = <div/>;\nconsole.log(el);",
        );
        assert!(out.contains("solid-js/jsx-runtime"), "got: {out}");
        assert!(!out.contains("createElement"), "got: {out}");
    }

    /// `transpile_typescript_with` reads the caller's Resolver: its cached
    /// tsconfig state decides the JSX settings, and `clear_caches` is what
    /// makes a newly-written tsconfig visible. The free function only ever
    /// saw the thread-local default, so an embedder's clear was ignored.
    #[test]
    fn transpile_with_honors_the_callers_resolver_caches() {
        let dir = temp_dir("transpile-with");
        let file = dir.join("App.tsx");
        let src = "const el = <div/>;\nconsole.log(el);";
        std::fs::write(&file, src).unwrap();
        let resolver = Resolver::new();
        let before = transpile_typescript_with(&resolver, &file, src).unwrap();
        assert!(before.contains("react/jsx-runtime"), "got: {before}");

        std::fs::write(
            dir.join("tsconfig.json"),
            r#"{"compilerOptions":{"jsxImportSource":"preact"}}"#,
        )
        .unwrap();
        let stale = transpile_typescript_with(&resolver, &file, src).unwrap();
        assert!(
            stale.contains("react/jsx-runtime"),
            "the discovery miss is cached until cleared: {stale}"
        );
        resolver.clear_caches();
        let fresh = transpile_typescript_with(&resolver, &file, src).unwrap();
        assert!(fresh.contains("preact/jsx-runtime"), "got: {fresh}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fingerprint changes with every non-source input that changes the
    /// output, and only with those: same path twice is identical.
    #[test]
    fn transpile_fingerprint_tracks_resolved_jsx_settings_and_source_type() {
        let a = temp_dir("fp-a");
        let b = temp_dir("fp-b");
        std::fs::write(
            a.join("tsconfig.json"),
            r#"{"compilerOptions":{"jsxImportSource":"preact"}}"#,
        )
        .unwrap();
        std::fs::write(
            b.join("tsconfig.json"),
            r#"{"compilerOptions":{"jsxImportSource":"solid-js"}}"#,
        )
        .unwrap();
        for dir in [&a, &b] {
            std::fs::write(dir.join("App.tsx"), "").unwrap();
            std::fs::write(dir.join("util.ts"), "").unwrap();
            std::fs::write(dir.join("shim.cts"), "").unwrap();
        }
        let resolver = Resolver::new();
        let fp = |p: PathBuf| transpile_fingerprint_with(&resolver, &p);

        let fp_a = fp(a.join("App.tsx"));
        assert_eq!(fp(a.join("App.tsx")), fp_a, "stable across calls");
        assert_ne!(fp_a, fp(b.join("App.tsx")), "different jsxImportSource");
        assert!(fp_a.contains("import_source=Some(\"preact\")"), "{fp_a}");
        assert!(fp_a.contains(&format!("format={TRANSPILE_FORMAT_VERSION};")));
        assert!(fp_a.contains("oxc_transformer=0."), "{fp_a}");
        assert!(fp_a.contains("oxc_codegen=0."), "{fp_a}");

        // A plain .ts never consults tsconfig for JSX: identical across the
        // two projects, distinct from the .tsx.
        let ts_a = fp(a.join("util.ts"));
        assert_eq!(ts_a, fp(b.join("util.ts")));
        assert_ne!(ts_a, fp_a);
        assert!(ts_a.contains("jsx=none"), "{ts_a}");
        assert!(ts_a.contains("module=module"), "{ts_a}");

        // .cts keeps the CommonJS source type, and that is part of the key.
        let cts = fp(a.join("shim.cts"));
        assert!(cts.contains("module=commonjs"), "{cts}");
        assert_ne!(cts, ts_a);

        // The free function agrees with the explicit-resolver form.
        assert_eq!(transpile_fingerprint(&a.join("util.ts")), ts_a);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// `transpile_typescript_rich` reports REAL top-level await usage (the
    /// substring heuristic it replaces wrapped `let awaiting = 1` too) and
    /// the names the line binds at the top level.
    #[test]
    fn rich_transpile_reports_real_top_level_await_and_bindings() {
        let path = PathBuf::from("repl.ts");
        let rich = |src: &str| transpile_typescript_rich(&path, src).unwrap();

        let out = rich("const x = await Promise.resolve(1)");
        assert!(out.top_level_await);
        assert_eq!(out.top_level_bindings, ["x"]);

        let out = rich("let awaiting = 1");
        assert!(!out.top_level_await, "identifier is not usage");
        assert_eq!(out.top_level_bindings, ["awaiting"]);

        let out = rich("async function f() { await g(); }");
        assert!(!out.top_level_await, "await inside a function body");
        assert_eq!(out.top_level_bindings, ["f"]);

        let out = rich("const run = async () => { await g(); };");
        assert!(!out.top_level_await, "await inside an arrow body");
        assert_eq!(out.top_level_bindings, ["run"]);

        let out = rich("if (await ready()) { console.log(1) }");
        assert!(out.top_level_await, "statement-shaped usage");
        assert!(out.top_level_bindings.is_empty());

        let out = rich("for await (const item of gen()) { console.log(item) }");
        assert!(out.top_level_await, "for await is usage");
        assert!(
            out.top_level_bindings.is_empty(),
            "the loop binding is block-scoped, not hoistable"
        );

        let out = rich("const { a, b: [c] } = await load()");
        assert!(out.top_level_await);
        assert_eq!(out.top_level_bindings, ["a", "c"]);

        let out = rich("class Foo {}\nenum Mode { Fast }");
        assert!(!out.top_level_await);
        assert_eq!(out.top_level_bindings, ["Foo", "Mode"]);

        // The thin wrapper agrees with the rich form on the code.
        assert_eq!(
            transpile_typescript(&path, "const n: number = 1;").unwrap(),
            rich("const n: number = 1;").code
        );
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
        // The candidates are exactly `[ /x, /x.ts, /x.tsx, /x.mts, /x.js,
        // /x.jsx, /x.mjs, /x/index.ts, /x/index.tsx, /x/index.js ]` — '/x',
        // NOT '/proj/x'. That's the invariant: the leading '/' is preserved
        // by the root-relative branch and not rewritten as a parent walk.
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
        // `Parser::parse`. `transpile_typescript_with` maps that to
        // OAM-PARSE0002.
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
