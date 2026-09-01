//! Install-time TypeScript pre-compilation cache.
//!
//! `precompile_package` walks an installed npm package directory and
//! transpiles every transpiled-source file (`.ts`/`.tsx`/`.mts`/`.cts`/
//! `.jsx` -- exactly `is_transpiled_source`, minus declaration files) to
//! plain JavaScript, writing the output into
//! `node_modules/.oam/precompile/<pkg-key>/` mirroring the package tree.
//! `<pkg-key>` is the lockfile key relative to the project's
//! `node_modules` (`react/node_modules/scheduler` for a nested package),
//! so two nested packages with the same name get distinct slots and the
//! run-time reader -- which anchors on the OUTERMOST `node_modules`
//! ancestor -- derives the same path from the file location alone.
//!
//! At run time the CJS and ESM loaders check this cache before calling
//! oxc, so the first `oam run` after `oam install --precompile` pays no
//! transpile cost for node_modules TypeScript.
//!
//! # Artifact format (v1)
//!
//! Each cache entry is a single self-describing file, written atomically
//! (unique temp file + rename):
//!
//! ```text
//! // oam-precompile v1 <64 hex chars>
//! <transpiled JavaScript>
//! ```
//!
//! The hex value is the freshness key:
//! `sha256(transpile_fingerprint(path) || source bytes)`. Folding the
//! fingerprint in (oxc transformer/codegen versions,
//! `TRANSPILE_FORMAT_VERSION`, the resolved source type, the resolved JSX
//! settings) means a package updated in place, an oam/oxc upgrade, and a
//! tsconfig change that retargets `jsxImportSource` all invalidate the
//! entry. The reader recomputes the key, verifies the header, and strips
//! it; any mismatch is a miss, so the caller re-transpiles the same
//! source and stays correct.

use std::io::BufRead;
use std::path::{Path, PathBuf};

/// Pre-compilation failure for one file. `precompile_package` collects
/// these per file instead of aborting the package.
#[derive(Debug)]
pub enum PrecompileError {
    Io {
        path: PathBuf,
        error: std::io::Error,
    },
    Transpile {
        path: PathBuf,
        message: String,
    },
}

impl std::fmt::Display for PrecompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrecompileError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            PrecompileError::Transpile { path, message } => {
                write!(f, "{}: {message}", path.display())
            }
        }
    }
}

/// True for the files the precompile writer produces output for: the same
/// extension set the runtime routes through `transpile_typescript`
/// (`is_transpiled_source`: .ts/.mts/.cts/.tsx/.jsx), minus declaration
/// files. The run-time reader gates on the SAME predicate, so it never
/// hashes or stats for an extension no writer ever emitted.
fn is_precompilable(path: &Path) -> bool {
    crate::is_transpiled_source(path) && !crate::is_declaration_file(path)
}

/// Recursively collect all precompilable source files under `dir`.
/// Uses `std::fs::read_dir` recursively -- no walkdir dep required.
fn collect_transpile_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip common non-source directories. Nested node_modules are
            // separate lockfile entries and get their own pass.
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "node_modules" || name.starts_with('.') {
                continue;
            }
            collect_transpile_sources(&path, out);
        } else if is_precompilable(&path) {
            out.push(path);
        }
    }
}

/// Derive the cache path for a source file.
///
/// `ts_path` is an absolute path inside `pkg_dir`.
/// `cache_root` is `<project>/node_modules/.oam/precompile/`.
/// `pkg_key` is the lockfile key relative to the project's `node_modules`
/// (may itself contain `node_modules/` segments for nested packages).
/// The output mirrors the relative path with `.js` appended:
/// `<pkg_dir>/src/index.ts` -> `<cache_root>/<pkg_key>/src/index.ts.js`.
fn cache_path_for(
    ts_path: &Path,
    pkg_dir: &Path,
    pkg_key: &str,
    cache_root: &Path,
) -> Option<PathBuf> {
    let rel = ts_path.strip_prefix(pkg_dir).ok()?;
    // Append ".js" to the FULL relative path rather than replacing the final
    // extension. with_extension("js") is NOT injective over the source
    // extension set (index.ts/.tsx/.cts/.mts/.jsx all map to index.js, and
    // dotted basenames like my.module.ts collide with my.ts), so colliding
    // files share one cache slot and clobber each other. Appending preserves
    // the original extension -> index.ts.js, index.tsx.js, my.module.ts.js
    // are all distinct.
    let mut js_rel = rel.as_os_str().to_os_string();
    js_rel.push(".js");
    Some(cache_root.join(pkg_key).join(PathBuf::from(js_rel)))
}

/// Ensure the `node_modules/.oam/.gitignore` file exists so the cache is
/// never accidentally committed.
fn ensure_gitignore(oam_dir: &Path) -> Result<(), std::io::Error> {
    let gi = oam_dir.join(".gitignore");
    if !gi.exists() {
        std::fs::create_dir_all(oam_dir)?;
        std::fs::write(&gi, "# oam internal cache -- do not commit\n*\n")?;
    }
    Ok(())
}

/// Artifact header prefix. The `v1` names the HEADER LAYOUT, not the
/// transform pipeline -- transform changes are covered by the fingerprint
/// inside the hash (`TRANSPILE_FORMAT_VERSION` and the oxc crate versions
/// are both part of `transpile_fingerprint`).
const HEADER_PREFIX: &str = "// oam-precompile v1 ";

fn to_hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The freshness key for a cached artifact:
/// `sha256(transpile_fingerprint(path) || source bytes)`, hex-encoded.
///
/// The source-bytes half (content, not mtime) keeps the cache correct
/// across in-place package updates, git checkouts, and npm reinstalls that
/// don't preserve timestamps. The fingerprint half folds in every
/// non-source transpile input (oxc versions, format version, resolved
/// source type, resolved JSX settings), so an oam upgrade or a tsconfig
/// `jsxImportSource` change invalidates the entry too.
fn freshness_key(ts_path: &Path, source: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(crate::transpile_fingerprint(ts_path).as_bytes());
    hasher.update(source.as_bytes());
    to_hex(&hasher.finalize().into())
}

/// The exact header line (without trailing newline) a fresh artifact for
/// `ts_path` + `source` must carry. Public so tests and tooling can seed
/// cache entries without re-deriving the key derivation.
pub fn artifact_header(ts_path: &Path, source: &str) -> String {
    format!("{HEADER_PREFIX}{}", freshness_key(ts_path, source))
}

/// A cached artifact is fresh when its first line equals the expected
/// header -- i.e. it was produced from this exact source by this exact
/// transpiler configuration (see `freshness_key`). Reads only the first
/// line. A missing file, a pre-v1 artifact (no header), or any mismatch
/// (package updated in place, oam/oxc upgraded, JSX settings changed) is
/// stale.
fn artifact_is_fresh(cache_js: &Path, expected_header: &str) -> bool {
    let Ok(file) = std::fs::File::open(cache_js) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::with_capacity(expected_header.len() + 2);
    match reader.read_line(&mut line) {
        Ok(_) => line.trim_end_matches(['\n', '\r']) == expected_header,
        Err(_) => false,
    }
}

/// Write `bytes` to `path` atomically: unique temp file in the same
/// directory, then rename. A crash can leave a stray temp file behind but
/// never a torn or mismatched artifact -- the header inside the file is
/// the authority the reader verifies, so there is no separate sidecar to
/// fall out of sync with.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("cache path has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        std::process::id(),
        CTR.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // rename() won't overwrite an existing file on Windows; remove the old
    // entry first. A racing peer writing the same key is harmless -- both
    // artifacts carry the same header and body.
    let _ = std::fs::remove_file(path);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Transpile every precompilable file in `pkg_dir` and write
/// self-describing `.js` artifacts under `cache_root/<pkg_key>/`
/// mirroring the relative paths.
///
/// `cache_root` is typically `<project>/node_modules/.oam/precompile/`.
/// `pkg_key` is the lockfile key relative to the project's `node_modules`
/// (`foo`, `@scope/bar`, or `react/node_modules/scheduler` for a nested
/// package), so the layout matches what `try_precompile_cache` derives
/// from the file path at run time.
///
/// Returns `(compiled, errors)`: how many files were (re)compiled, plus
/// one `PrecompileError` per file that failed to read, transpile, or
/// write. A failing file never aborts the rest of the package. Files
/// whose cached artifact is already fresh (header matches the current
/// source + transpile fingerprint) are skipped and not counted, which is
/// what keeps a warm re-run cheap: one read + one hash + one open per
/// source file.
pub fn precompile_package(
    pkg_dir: &Path,
    pkg_key: &str,
    cache_root: &Path,
) -> (usize, Vec<PrecompileError>) {
    let mut errors: Vec<PrecompileError> = Vec::new();

    // Ensure .gitignore is written into node_modules/.oam/ (one level above
    // cache_root which is node_modules/.oam/precompile/).
    if let Some(oam_dir) = cache_root.parent()
        && let Err(error) = ensure_gitignore(oam_dir)
    {
        errors.push(PrecompileError::Io {
            path: oam_dir.to_path_buf(),
            error,
        });
    }

    let mut sources = Vec::new();
    collect_transpile_sources(pkg_dir, &mut sources);

    let mut compiled = 0usize;

    for ts_path in &sources {
        let Some(cache_path) = cache_path_for(ts_path, pkg_dir, pkg_key, cache_root) else {
            continue;
        };

        let source = match std::fs::read_to_string(ts_path) {
            Ok(source) => source,
            Err(error) => {
                errors.push(PrecompileError::Io {
                    path: ts_path.clone(),
                    error,
                });
                continue;
            }
        };
        let header = artifact_header(ts_path, &source);

        // Freshness guard: skip only when the cached artifact's header
        // matches the CURRENT source + transpile fingerprint. A package
        // updated in place, an oam/oxc upgrade, or changed JSX settings
        // all mismatch and are recompiled.
        if artifact_is_fresh(&cache_path, &header) {
            continue;
        }

        let js = match crate::transpile_typescript(ts_path, &source) {
            Ok(js) => js,
            Err(e) => {
                let message = e
                    .diagnostics
                    .first()
                    .map(|d| d.message.clone())
                    .unwrap_or_else(|| "TypeScript parse/transform error".to_string());
                errors.push(PrecompileError::Transpile {
                    path: ts_path.clone(),
                    message,
                });
                continue;
            }
        };

        if let Err(error) = write_atomic(&cache_path, format!("{header}\n{js}").as_bytes()) {
            errors.push(PrecompileError::Io {
                path: cache_path.clone(),
                error,
            });
            continue;
        }
        // Pre-v1 caches paired the .js with a `<name>.js.hash` sidecar; the
        // header replaced it, so clean up any leftover when rewriting.
        let mut legacy_sidecar = cache_path.as_os_str().to_os_string();
        legacy_sidecar.push(".hash");
        let _ = std::fs::remove_file(PathBuf::from(legacy_sidecar));
        compiled += 1;
    }

    (compiled, errors)
}

/// Check the pre-compilation cache for a transpiled-source file inside
/// node_modules.
///
/// `ts_path` is the absolute path of the `.ts`/`.tsx`/`.mts`/`.cts`/`.jsx`
/// file being loaded; `source` is its current on-disk contents (the caller
/// has already read it). Returns `Some(js_source)` only when a cached
/// artifact exists AND its header matches the current source + transpile
/// fingerprint -- a stale entry (package updated in place, oam/oxc
/// upgraded, JSX settings changed) is a miss, so the caller re-transpiles
/// the same `source` and stays correct.
///
/// Returns `None` immediately -- no read, no hash -- for any extension the
/// writer never produces output for (plain JS, declaration files) and for
/// any file outside a `node_modules` tree (project sources are never
/// precompiled).
///
/// Cache layout: `<node_modules>/.oam/precompile/<rel>.js` where
/// `<node_modules>` is the OUTERMOST such ancestor of `ts_path` and
/// `<rel>` is `ts_path` relative to it -- so a nested package's file
/// (`node_modules/react/node_modules/scheduler/index.ts`) maps to
/// `react/node_modules/scheduler/index.ts.js`, exactly the slot the writer
/// (keyed on the full lockfile key) wrote.
pub fn try_precompile_cache(ts_path: &Path, source: &str) -> Option<String> {
    // Reader and writer must agree on the extension set: anything the
    // writer would never have produced is an immediate miss.
    if !is_precompilable(ts_path) {
        return None;
    }

    // Anchor on the OUTERMOST node_modules ancestor (`ancestors()` yields
    // innermost first) -- the project-level directory install writes to.
    let nm = ts_path
        .ancestors()
        .filter(|p| p.file_name().is_some_and(|n| n == "node_modules"))
        .last()?;

    // rel is e.g. "ms/index.ts", or "react/node_modules/scheduler/index.ts"
    // for a nested package.
    let rel = ts_path.strip_prefix(nm).ok()?;
    // MUST match the writer (cache_path_for): append ".js" to the full
    // relative path so the lookup is injective over the source extension
    // set and agrees with what precompile_package wrote (index.ts ->
    // index.ts.js).
    let mut js_rel = rel.as_os_str().to_os_string();
    js_rel.push(".js");
    let cache_path = nm
        .join(".oam")
        .join("precompile")
        .join(PathBuf::from(js_rel));

    // Read first (a miss costs one failed open), then verify the header
    // against the CURRENT source + fingerprint and strip it.
    let artifact = std::fs::read_to_string(&cache_path).ok()?;
    let (first_line, js) = artifact.split_once('\n')?;
    if first_line.trim_end_matches('\r') != artifact_header(ts_path, source) {
        return None;
    }
    Some(js.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "oam-precompile-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Hand-write a fresh (correctly headered) artifact for `ts_path`.
    fn seed_artifact(cache_file: &Path, ts_path: &Path, source: &str, js: &str) {
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(
            cache_file,
            format!("{}\n{js}", artifact_header(ts_path, source)),
        )
        .unwrap();
    }

    #[test]
    fn precompile_transpiles_ts_files() {
        let root = temp_dir("transpile");
        let pkg_dir = root.join("node_modules/mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(
            pkg_dir.join("index.ts"),
            "const x: number = 1;\nconsole.log(x);\n",
        )
        .unwrap();

        let cache_root = root.join("node_modules/.oam/precompile");
        let (count, errors) = precompile_package(&pkg_dir, "mypkg", &cache_root);
        assert!(errors.is_empty(), "no failures expected: {errors:?}");
        assert_eq!(count, 1);

        let out = cache_root.join("mypkg/index.ts.js");
        assert!(out.exists(), "cache file should exist");
        let artifact = fs::read_to_string(&out).unwrap();
        let (header, js) = artifact.split_once('\n').expect("artifact has a header");
        assert!(
            header.starts_with(HEADER_PREFIX),
            "self-describing header expected, got: {header}"
        );
        assert_eq!(
            header.len(),
            HEADER_PREFIX.len() + 64,
            "header carries a 64-hex sha256"
        );
        assert!(
            !js.contains(": number"),
            "type annotation should be stripped"
        );
        assert!(js.contains("console.log"), "js body preserved");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn precompile_covers_jsx_and_reader_hits_it() {
        let root = temp_dir("jsx");
        let pkg_dir = root.join("node_modules/mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        let jsx_path = pkg_dir.join("app.jsx");
        let source = "export const el = <div>hi</div>;\n";
        fs::write(&jsx_path, source).unwrap();

        let cache_root = root.join("node_modules/.oam/precompile");
        let (count, errors) = precompile_package(&pkg_dir, "mypkg", &cache_root);
        assert!(errors.is_empty(), "no failures expected: {errors:?}");
        assert_eq!(count, 1, ".jsx is precompiled");
        assert!(cache_root.join("mypkg/app.jsx.js").exists());

        // The reader probes the same extension set the writer produces, so
        // the .jsx entry is actually served.
        let cached = try_precompile_cache(&jsx_path, source);
        assert!(cached.is_some(), ".jsx cache hit expected");
        assert!(
            !cached.unwrap().contains("<div>"),
            "JSX should be transformed in the cached output"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn precompile_skips_declaration_files() {
        let root = temp_dir("skip-dts");
        let pkg_dir = root.join("node_modules/mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(
            pkg_dir.join("index.d.ts"),
            "export declare const x: number;\n",
        )
        .unwrap();
        fs::write(pkg_dir.join("index.ts"), "export const x = 1;\n").unwrap();

        let cache_root = root.join("node_modules/.oam/precompile");
        let (count, errors) = precompile_package(&pkg_dir, "mypkg", &cache_root);
        assert!(errors.is_empty(), "no failures expected: {errors:?}");
        // Only index.ts should be compiled; index.d.ts is skipped.
        assert_eq!(count, 1);
        assert!(!cache_root.join("mypkg/index.d.ts.js").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn precompile_skips_fresh_and_recompiles_stale() {
        let root = temp_dir("skip-fresh");
        let pkg_dir = root.join("node_modules/mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        let src = pkg_dir.join("index.ts");
        fs::write(&src, "export const x: number = 1;\n").unwrap();

        let cache_root = root.join("node_modules/.oam/precompile");

        // First pass compiles the file.
        assert_eq!(precompile_package(&pkg_dir, "mypkg", &cache_root).0, 1);
        let cache_file = cache_root.join("mypkg/index.ts.js");
        assert!(cache_file.is_file());

        // Second pass on UNCHANGED source skips (header still matches).
        assert_eq!(
            precompile_package(&pkg_dir, "mypkg", &cache_root).0,
            0,
            "fresh entry should be skipped"
        );

        // Changing the source invalidates the entry -> recompiled.
        fs::write(&src, "export const x: number = 2;\n").unwrap();
        assert_eq!(
            precompile_package(&pkg_dir, "mypkg", &cache_root).0,
            1,
            "stale entry should be recompiled"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fingerprint_mismatch_is_stale_even_with_matching_source() {
        let root = temp_dir("fp-stale");
        let pkg_dir = root.join("node_modules/mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        let ts_path = pkg_dir.join("index.ts");
        let source = "export const x: number = 1;\n";
        fs::write(&ts_path, source).unwrap();

        let cache_root = root.join("node_modules/.oam/precompile");
        assert_eq!(precompile_package(&pkg_dir, "mypkg", &cache_root).0, 1);
        let cache_file = cache_root.join("mypkg/index.ts.js");
        let js_body = try_precompile_cache(&ts_path, source).expect("fresh entry hits");

        // Simulate an artifact written by a different transpiler
        // configuration: same JS body, but a header whose hash was computed
        // from a different fingerprint. Source is UNCHANGED, yet the entry
        // must be treated as stale by reader and writer alike.
        fs::write(
            &cache_file,
            format!("{HEADER_PREFIX}{}\n{js_body}", "0".repeat(64)),
        )
        .unwrap();
        assert!(
            try_precompile_cache(&ts_path, source).is_none(),
            "different-fingerprint entry must miss despite matching source"
        );
        assert_eq!(
            precompile_package(&pkg_dir, "mypkg", &cache_root).0,
            1,
            "writer must recompile a different-fingerprint entry"
        );
        assert!(
            try_precompile_cache(&ts_path, source).is_some(),
            "recompiled entry is fresh again"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn jsx_settings_change_the_freshness_key() {
        // Two identical .tsx sources whose nearest tsconfigs resolve
        // different jsxImportSource values must get different freshness
        // keys -- the fingerprint half of the hash is load-bearing.
        let root = temp_dir("fp-jsx");
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(
            b.join("tsconfig.json"),
            r#"{"compilerOptions":{"jsx":"react-jsx","jsxImportSource":"preact"}}"#,
        )
        .unwrap();
        let source = "export const el = <div/>;\n";
        fs::write(a.join("x.tsx"), source).unwrap();
        fs::write(b.join("x.tsx"), source).unwrap();

        assert_ne!(
            artifact_header(&a.join("x.tsx"), source),
            artifact_header(&b.join("x.tsx"), source),
            "resolved JSX settings must be part of the key"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn precompile_collects_per_file_errors_and_continues() {
        let root = temp_dir("per-file-errors");
        let pkg_dir = root.join("node_modules/mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("a_good.ts"), "export const a: number = 1;\n").unwrap();
        // Unterminated string literal: guaranteed parse error.
        fs::write(pkg_dir.join("broken.ts"), "const x = \"unterminated\n").unwrap();
        fs::write(pkg_dir.join("z_good.ts"), "export const z: number = 26;\n").unwrap();

        let cache_root = root.join("node_modules/.oam/precompile");
        let (count, errors) = precompile_package(&pkg_dir, "mypkg", &cache_root);
        assert_eq!(count, 2, "both good files compile despite the broken one");
        assert_eq!(errors.len(), 1, "exactly one failure collected");
        match &errors[0] {
            PrecompileError::Transpile { path, .. } => {
                assert!(path.ends_with("broken.ts"), "failure names the file");
            }
            other => panic!("expected a transpile error, got: {other}"),
        }
        assert!(cache_root.join("mypkg/a_good.ts.js").exists());
        assert!(cache_root.join("mypkg/z_good.ts.js").exists());
        assert!(!cache_root.join("mypkg/broken.ts.js").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn precompile_writes_gitignore() {
        let root = temp_dir("gitignore");
        let pkg_dir = root.join("node_modules/mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("index.ts"), "export const x = 1;\n").unwrap();

        let cache_root = root.join("node_modules/.oam/precompile");
        precompile_package(&pkg_dir, "mypkg", &cache_root);

        let gi = root.join("node_modules/.oam/.gitignore");
        assert!(gi.exists(), ".gitignore should be written");
        let contents = fs::read_to_string(&gi).unwrap();
        assert!(contents.contains('*'), ".gitignore should match everything");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_package_writer_and_reader_agree() {
        // Layout mirrors a lockfile key with two node_modules segments:
        // node_modules/react/node_modules/scheduler. The writer keys the
        // slot on the FULL key relative to the project node_modules; the
        // reader anchors on the OUTERMOST node_modules ancestor -- both
        // must land on the same file. A same-named top-level package keeps
        // its own distinct slot.
        let root = temp_dir("nested");
        let nm = root.join("node_modules");
        let nested_dir = nm.join("react/node_modules/scheduler");
        let hoisted_dir = nm.join("scheduler");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::create_dir_all(&hoisted_dir).unwrap();
        let nested_src = "export const version: number = 2;\n";
        let hoisted_src = "export const version: number = 1;\n";
        fs::write(nested_dir.join("index.ts"), nested_src).unwrap();
        fs::write(hoisted_dir.join("index.ts"), hoisted_src).unwrap();

        let cache_root = nm.join(".oam/precompile");
        let (count, errors) =
            precompile_package(&nested_dir, "react/node_modules/scheduler", &cache_root);
        assert!(errors.is_empty(), "no failures expected: {errors:?}");
        assert_eq!(count, 1);
        assert_eq!(
            precompile_package(&hoisted_dir, "scheduler", &cache_root).0,
            1
        );

        // Distinct slots: the nested package did not clobber the hoisted one.
        assert!(
            cache_root
                .join("react/node_modules/scheduler/index.ts.js")
                .exists(),
            "nested slot exists"
        );
        assert!(
            cache_root.join("scheduler/index.ts.js").exists(),
            "hoisted slot exists"
        );

        // The reader finds each from its file path alone.
        let nested_hit = try_precompile_cache(&nested_dir.join("index.ts"), nested_src);
        assert!(nested_hit.is_some(), "nested package cache hit expected");
        assert!(nested_hit.unwrap().contains('2'));
        let hoisted_hit = try_precompile_cache(&hoisted_dir.join("index.ts"), hoisted_src);
        assert!(hoisted_hit.is_some(), "hoisted package cache hit expected");
        assert!(hoisted_hit.unwrap().contains('1'));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn try_precompile_cache_returns_cached_js() {
        let root = temp_dir("cache-lookup");
        let nm = root.join("node_modules");
        let pkg_dir = nm.join("mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        let source = "const x: number = 1;\n";
        let ts_path = pkg_dir.join("index.ts");
        fs::write(&ts_path, source).unwrap();

        // Seed a correctly headered artifact by hand.
        let cache_file = nm.join(".oam/precompile/mypkg/index.ts.js");
        seed_artifact(&cache_file, &ts_path, source, "const x = 1;\n");

        let cached = try_precompile_cache(&ts_path, source);
        assert!(cached.is_some(), "cache hit expected");
        assert_eq!(cached.unwrap(), "const x = 1;\n");

        // A changed source (key mismatch) is a miss even with the .js present.
        assert!(
            try_precompile_cache(&ts_path, "const x: number = 2;\n").is_none(),
            "stale entry should miss"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn try_precompile_cache_returns_none_for_miss() {
        let root = temp_dir("cache-miss");
        let nm = root.join("node_modules");
        let pkg_dir = nm.join("mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        let source = "const x: number = 1;\n";
        fs::write(pkg_dir.join("index.ts"), source).unwrap();

        // No cache entry written -- should be a miss.
        let ts_path = pkg_dir.join("index.ts");
        let cached = try_precompile_cache(&ts_path, source);
        assert!(cached.is_none(), "cache miss expected");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn try_precompile_cache_gates_on_writer_extension_set() {
        let root = temp_dir("reader-gate");
        let nm = root.join("node_modules");
        let pkg_dir = nm.join("mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();

        // Even with a validly headered artifact sitting at the probed slot,
        // extensions the writer never produces output for must be an
        // immediate miss: plain .js and declaration files.
        let js_path = pkg_dir.join("plain.js");
        let js_source = "module.exports = 1;\n";
        fs::write(&js_path, js_source).unwrap();
        seed_artifact(
            &nm.join(".oam/precompile/mypkg/plain.js.js"),
            &js_path,
            js_source,
            "module.exports = 2;\n",
        );
        assert!(
            try_precompile_cache(&js_path, js_source).is_none(),
            "plain .js must never be served from the precompile cache"
        );

        let dts_path = pkg_dir.join("index.d.ts");
        let dts_source = "export declare const x: number;\n";
        fs::write(&dts_path, dts_source).unwrap();
        seed_artifact(
            &nm.join(".oam/precompile/mypkg/index.d.ts.js"),
            &dts_path,
            dts_source,
            "export const x = 1;\n",
        );
        assert!(
            try_precompile_cache(&dts_path, dts_source).is_none(),
            "declaration files must never be served from the precompile cache"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_headerless_artifact_misses_and_is_migrated() {
        let root = temp_dir("legacy");
        let nm = root.join("node_modules");
        let pkg_dir = nm.join("mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        let source = "export const x: number = 1;\n";
        let ts_path = pkg_dir.join("index.ts");
        fs::write(&ts_path, source).unwrap();

        // Pre-v1 layout: raw JS artifact + separate .hash sidecar.
        let cache_root = nm.join(".oam/precompile");
        let cache_file = cache_root.join("mypkg/index.ts.js");
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(&cache_file, "export const x = 1;\n").unwrap();
        let mut sidecar = cache_file.as_os_str().to_os_string();
        sidecar.push(".hash");
        let sidecar = PathBuf::from(sidecar);
        fs::write(&sidecar, [0u8; 32]).unwrap();

        // A headerless artifact can never be verified -> miss.
        assert!(
            try_precompile_cache(&ts_path, source).is_none(),
            "legacy artifact must miss"
        );

        // The writer treats it as stale, rewrites it in the v1 format, and
        // removes the obsolete sidecar.
        assert_eq!(precompile_package(&pkg_dir, "mypkg", &cache_root).0, 1);
        assert!(
            fs::read_to_string(&cache_file)
                .unwrap()
                .starts_with(HEADER_PREFIX),
            "migrated artifact carries the v1 header"
        );
        assert!(!sidecar.exists(), "legacy .hash sidecar is cleaned up");
        assert!(
            try_precompile_cache(&ts_path, source).is_some(),
            "migrated artifact is served"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
