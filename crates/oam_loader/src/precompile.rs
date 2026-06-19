//! Install-time TypeScript pre-compilation cache.
//!
//! `precompile_package` walks a just-installed npm package directory and
//! transpiles every `.ts`/`.tsx`/`.mts`/`.cts` file to plain JavaScript,
//! writing the output into `node_modules/.oam/precompile/<pkg-name>/`
//! mirroring the package tree.
//!
//! At run time the CJS and ESM loaders check this cache before calling oxc,
//! so the first `oam run` after `oam install --precompile` pays no transpile
//! cost for node_modules TypeScript.

use std::path::{Path, PathBuf};

/// Pre-compilation failure.
#[derive(Debug)]
pub enum PrecompileError {
    Io(std::io::Error),
    Transpile { path: PathBuf, message: String },
}

impl std::fmt::Display for PrecompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrecompileError::Io(e) => write!(f, "io error: {e}"),
            PrecompileError::Transpile { path, message } => {
                write!(f, "transpile {}: {}", path.display(), message)
            }
        }
    }
}

impl From<std::io::Error> for PrecompileError {
    fn from(e: std::io::Error) -> Self {
        PrecompileError::Io(e)
    }
}

/// TypeScript extensions that are pre-compiled.
fn is_ts_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ts") | Some("tsx") | Some("mts") | Some("cts")
    )
}

/// Return `true` for declaration files (.d.ts, .d.mts, etc.) -- these are
/// types-only and should never be transpiled to a runtime artifact.
fn is_declaration_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".d.ts")
        || name.ends_with(".d.mts")
        || name.ends_with(".d.cts")
        || name.ends_with(".d.tsx")
}

/// Recursively collect all TypeScript source files under `dir`.
/// Uses `std::fs::read_dir` recursively -- no walkdir dep required.
fn collect_ts_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip common non-source directories.
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "node_modules" || name.starts_with('.') {
                continue;
            }
            collect_ts_files(&path, out);
        } else if is_ts_source(&path) && !is_declaration_file(&path) {
            out.push(path);
        }
    }
}

/// Derive the cache path for a TypeScript source file.
///
/// `ts_path` is an absolute path inside `pkg_dir`.
/// `cache_root` is `<project>/node_modules/.oam/precompile/`.
/// The output mirrors the relative path with `.js` extension:
/// `<pkg_dir>/src/index.ts` -> `<cache_root>/<pkg_name>/src/index.js`.
fn cache_path_for(
    ts_path: &Path,
    pkg_dir: &Path,
    pkg_name: &str,
    cache_root: &Path,
) -> Option<PathBuf> {
    let rel = ts_path.strip_prefix(pkg_dir).ok()?;
    let js_rel = rel.with_extension("js");
    Some(cache_root.join(pkg_name).join(js_rel))
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

/// Transpile all `.ts`/`.tsx`/`.mts`/`.cts` files in `pkg_dir` and write
/// `.js` outputs to `cache_root/<pkg_name>/` mirroring the relative paths.
///
/// `cache_root` is typically `<project>/node_modules/.oam/precompile/`.
///
/// Returns the count of files compiled (skipped files are not counted).
/// Skips files whose cache entry already exists and is non-empty (the
/// package version is the real staleness guard -- the caller (install.rs)
/// skips the call entirely for already-cached packages).
pub fn precompile_package(
    pkg_dir: &Path,
    pkg_name: &str,
    cache_root: &Path,
) -> Result<usize, PrecompileError> {
    // Ensure .gitignore is written into node_modules/.oam/ (one level above
    // cache_root which is node_modules/.oam/precompile/).
    if let Some(oam_dir) = cache_root.parent() {
        ensure_gitignore(oam_dir)?;
    }

    let mut ts_files = Vec::new();
    collect_ts_files(pkg_dir, &mut ts_files);

    let mut compiled = 0usize;

    for ts_path in &ts_files {
        let Some(cache_path) = cache_path_for(ts_path, pkg_dir, pkg_name, cache_root) else {
            continue;
        };

        // Staleness guard: skip if cache entry exists and is non-empty.
        if let Ok(meta) = std::fs::metadata(&cache_path) {
            if meta.len() > 0 {
                continue;
            }
        }

        let source = std::fs::read_to_string(ts_path)?;

        let js = super::transpile_typescript(ts_path, &source).map_err(|e| {
            let message = e
                .diagnostics
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| "TypeScript parse/transform error".to_string());
            PrecompileError::Transpile {
                path: ts_path.clone(),
                message,
            }
        })?;

        // Create parent directories and write the output.
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&cache_path, &js)?;
        compiled += 1;
    }

    Ok(compiled)
}

/// Check the pre-compilation cache for a TypeScript file inside node_modules.
///
/// `ts_path` is the absolute path of the `.ts`/`.tsx`/`.mts`/`.cts` file
/// being loaded.  Returns `Some(js_source)` if a non-empty cache entry
/// exists, `None` otherwise (caller falls through to normal oxc transpile).
///
/// Cache layout: `node_modules/.oam/precompile/<pkg>/<rel>.js`
/// where `<rel>` is `ts_path` relative to `node_modules/<pkg>/`.
pub fn try_precompile_cache(ts_path: &Path) -> Option<String> {
    // Walk up to find the node_modules ancestor.
    let nm = ts_path
        .ancestors()
        .find(|p| p.file_name().map(|n| n == "node_modules").unwrap_or(false))?;

    // rel is e.g. "ms/index.ts" (pkg/...path)
    let rel = ts_path.strip_prefix(nm).ok()?;
    let cache_path = nm
        .join(".oam")
        .join("precompile")
        .join(rel)
        .with_extension("js");

    // Only serve non-empty cache entries.
    let meta = std::fs::metadata(&cache_path).ok()?;
    if meta.len() == 0 {
        return None;
    }

    std::fs::read_to_string(&cache_path).ok()
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
        let count = precompile_package(&pkg_dir, "mypkg", &cache_root).unwrap();
        assert_eq!(count, 1);

        let out = cache_root.join("mypkg/index.js");
        assert!(out.exists(), "cache file should exist");
        let js = fs::read_to_string(&out).unwrap();
        assert!(
            !js.contains(": number"),
            "type annotation should be stripped"
        );
        assert!(js.contains("console.log"), "js body preserved");

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
        let count = precompile_package(&pkg_dir, "mypkg", &cache_root).unwrap();
        // Only index.ts should be compiled; index.d.ts is skipped.
        assert_eq!(count, 1);
        assert!(!cache_root.join("mypkg/index.d.js").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn precompile_skips_nonempty_cache_entries() {
        let root = temp_dir("skip-cached");
        let pkg_dir = root.join("node_modules/mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("index.ts"), "export const x = 1;\n").unwrap();

        let cache_root = root.join("node_modules/.oam/precompile");
        let cache_file = cache_root.join("mypkg/index.js");
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(&cache_file, "// sentinel\n").unwrap();

        // Should skip because cache entry is non-empty.
        let count = precompile_package(&pkg_dir, "mypkg", &cache_root).unwrap();
        assert_eq!(count, 0, "should skip already-cached file");

        // Cache file should be unchanged.
        let contents = fs::read_to_string(&cache_file).unwrap();
        assert_eq!(contents, "// sentinel\n");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn precompile_writes_gitignore() {
        let root = temp_dir("gitignore");
        let pkg_dir = root.join("node_modules/mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("index.ts"), "export const x = 1;\n").unwrap();

        let cache_root = root.join("node_modules/.oam/precompile");
        precompile_package(&pkg_dir, "mypkg", &cache_root).unwrap();

        let gi = root.join("node_modules/.oam/.gitignore");
        assert!(gi.exists(), ".gitignore should be written");
        let contents = fs::read_to_string(&gi).unwrap();
        assert!(contents.contains('*'), ".gitignore should match everything");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn try_precompile_cache_returns_cached_js() {
        let root = temp_dir("cache-lookup");
        let nm = root.join("node_modules");
        let pkg_dir = nm.join("mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("index.ts"), "const x: number = 1;\n").unwrap();

        // Write a cache entry manually.
        let cache_file = nm.join(".oam/precompile/mypkg/index.js");
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::write(&cache_file, "const x = 1;\n").unwrap();

        let ts_path = pkg_dir.join("index.ts");
        let cached = try_precompile_cache(&ts_path);
        assert!(cached.is_some(), "cache hit expected");
        assert_eq!(cached.unwrap(), "const x = 1;\n");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn try_precompile_cache_returns_none_for_miss() {
        let root = temp_dir("cache-miss");
        let nm = root.join("node_modules");
        let pkg_dir = nm.join("mypkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("index.ts"), "const x: number = 1;\n").unwrap();

        // No cache entry written -- should be a miss.
        let ts_path = pkg_dir.join("index.ts");
        let cached = try_precompile_cache(&ts_path);
        assert!(cached.is_none(), "cache miss expected");

        let _ = fs::remove_dir_all(&root);
    }
}
