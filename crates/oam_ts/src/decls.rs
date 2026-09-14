//! oam's own TypeScript declarations, and how they reach tsgo.
//!
//! `import { McpServer } from "oam:mcp"` RUNS -- the loader maps the
//! specifier to a snapshot module (oam_loader::npm) -- but tsgo resolves it
//! like any other bare specifier and reports TS2307, so the runtime whose
//! pitch is typed TypeScript rejected its own flagship module. The fix is a
//! declaration file (types/oam.d.ts) that the checker always sees, without
//! the user installing a types package or editing a tsconfig.
//!
//! Two shapes, because tsgo takes them differently:
//!
//! * Bare file (no tsconfig anywhere above the target): the declarations
//!   ride along as one more file argument. tsgo builds a program from the
//!   command line, and a second root file is exactly what we need.
//!
//! * Project (`-p tsconfig.json`): `error TS5042: Option 'project' cannot be
//!   mixed with source files on a command line` -- probed against tsgo
//!   7.0.0-dev, and the same rule tsc has always had. So oam generates a
//!   WRAPPER config that `extends` the user's and adds the declarations
//!   through `files`. Everything else -- compilerOptions, include, exclude,
//!   paths -- is inherited, and TypeScript resolves an inherited relative
//!   path against the config that DECLARED it, so the user's project keeps
//!   its own meaning of "src/**/*".
//!
//! A few things do not survive inheritance, because TypeScript ties them to
//! the ROOT config -- the one on the command line -- and the root of a check
//! is the wrapper, not the user's tsconfig. The wrapper restates each, so
//! the program is the one `tsc -p tsconfig.json` would build:
//!
//! * The DEFAULT include. A config that declares neither `files` nor
//!   `include` means "everything under my directory", and the wrapper
//!   declaring `files` suppresses that default for the whole program
//!   (probed: the user's own sources vanish from the file list). So when the
//!   chain declares neither key, the wrapper restates the default -- the
//!   `**/*` glob and the default excludes -- as absolute paths rooted at the
//!   user's project.
//!
//! * A RELATIVE `compilerOptions.types` entry (`"./typings/local"`). It is
//!   resolved against the root config's directory whichever config in the
//!   chain declared it (probed: declared in `cfg/base.json`, tsgo looks for
//!   `<project>/typings/local`, not `cfg/typings/local`), so the wrapper
//!   restates the whole list with each relative entry made absolute against
//!   the user's tsconfig dir.
//!
//! * `rootDir` under `composite`. Its default is then the root config's
//!   directory, so every source is "not under rootDir" (TS6059, probed);
//!   the wrapper restates TypeScript's own default -- the user's tsconfig
//!   dir -- when the chain sets `composite` and declares no `rootDir`.
//!
//! * `references`, the one top-level key TypeScript excludes from
//!   inheritance. The wrapper restates the user's own list with absolute
//!   paths, so an import into an unbuilt reference reports TS6305 through
//!   oam as it does through `tsc -p` (probed: it checked the reference's
//!   sources and passed before).
//!
//! * `${configDir}` (TypeScript 5.5+), substituted with the root config's
//!   directory wherever it appears. A base config with
//!   `"include": ["${configDir}/src"]` matched nothing through the wrapper,
//!   and the check came back clean with a type error present (probed). The
//!   template can sit in any path-typed key, so rather than restate them
//!   all, a chain that uses it gets no wrapper: the check runs against the
//!   user's tsconfig, with TS2307 on `oam:` imports, which is honest.
//!
//! Those are the only places this file reimplements tsc's config semantics,
//! and they are why `read_chain` reports which keys the chain declares
//! rather than what they contain.
//!
//! WHERE the wrapper lives matters for the same reason (issue #130). A bare
//! `types` entry -- `"node"` -- is looked up from the root config's
//! directory too: the default typeRoots walk and the `node_modules` fallback
//! both start there. With the wrapper under oam's cache dir, every project
//! that declared `"types": ["node"]` looked for `@types/node` beside oam's
//! cache, found nothing, and failed with TS2688 (measured on tsgo and on tsc
//! 6.0.3; this is `extends` semantics, not a tsgo bug). So the wrapper goes
//! under `node_modules/.oam/ts-decls/` at the project's nearest
//! `node_modules` -- the directory oam already owns there for `--precompile`
//! output and install locks, guarded by the same `.gitignore` -- from which
//! every lookup walks exactly the directories the user's own tsconfig would
//! (probed: `@types/node`, a self-typed package and a hoisted monorepo all
//! resolve; `--incremental` is unaffected).
//!
//! oam's cache dir is the fallback when no `node_modules` exists at or above
//! the project, or the in-tree location cannot be written (a read-only
//! install). From there a package named in `types` cannot be resolved --
//! and worse, a stray `~/node_modules/@types` ABOVE the cache dir can be,
//! passing a project the user's own tsconfig fails (probed) -- so a chain
//! that names one gets no wrapper on that path either. The other shapes
//! are as good in oam's cache as anywhere.
//!
//! Nothing here is allowed to fail a check: an unwritable cache dir or a
//! tsconfig this module cannot parse returns None, the check runs exactly as
//! it did before, and the user sees tsgo's own diagnostics (including its
//! parse error, if that is what went wrong) rather than one of ours about
//! plumbing. `OAM_DEBUG=1` says when, and why, a check ran without the
//! wrapper.

use std::path::{Path, PathBuf};

use crate::daemon::{cache_root, fnv1a64, project_key, resolve_extends, strip_jsonc};

/// The declarations, compiled in. Not read from disk at runtime: an
/// installed oam is a single binary with no data directory beside it.
const DECLARATIONS: &str = include_str!("../types/oam.d.ts");

/// The declarations file, and the wrappers of projects that cannot hold
/// theirs in-tree (see `in_tree_oam_dir`), live here under oam's cache.
/// Nothing is ever written into a user's repo outside its `node_modules`,
/// which may be read-only and is not ours to litter.
fn decls_dir() -> PathBuf {
    cache_root().join("ts-decls")
}

/// The declarations directory as a string, for recognizing a diagnostic that
/// landed inside oam's own generated file. `None` when the cache dir cannot be
/// resolved, which just means the hint is skipped.
pub(crate) fn declarations_dir_for_hint() -> Option<String> {
    Some(dts_dir().to_string_lossy().into_owned())
}

/// The declarations file's own directory.
///
/// Deliberately NOT the same directory as the per-project wrappers. tsgo lists
/// the .d.ts as a program file outside the project root, so the daemon
/// fingerprints its parent directory's entry listing -- and if the wrappers
/// lived there too, checking one project for the first time would write a new
/// `project-<key>.json`, change that listing, and evict every OTHER project's
/// warm daemon cache. In a monorepo where each package has its own tsconfig,
/// interleaved checks would keep evicting each other until every wrapper
/// happened to exist. This directory changes only when oam itself changes.
fn dts_dir() -> PathBuf {
    decls_dir().join("dts")
}

/// Where a per-project wrapper goes when the project cannot hold it: a
/// sibling of the .d.ts, so its churn never reaches the fingerprinted
/// listing above.
fn projects_dir() -> PathBuf {
    decls_dir().join("projects")
}

/// oam's own directory under the nearest `node_modules` at or above the
/// project -- `--precompile` output and install locks already live there
/// (oam_loader) -- and where a per-project wrapper goes by preference, in a
/// `ts-decls/` subdirectory. TypeScript looks a `types` entry up from the
/// wrapper's directory (module docs), and from here the default typeRoots
/// walk and the `node_modules` fallback reach the same `node_modules` the
/// user's own tsconfig resolves against: the project's own, or the hoisted
/// one of a monorepo root when the package has none. None when no
/// `node_modules` exists up the tree.
///
/// The daemon's fingerprint walk skips `node_modules`, so the wrapper's own
/// writes never invalidate a cache, same as in oam's cache dir.
fn in_tree_oam_dir(tsconfig: &Path) -> Option<PathBuf> {
    tsconfig
        .parent()?
        .ancestors()
        .map(|dir| dir.join("node_modules"))
        .find(|node_modules| node_modules.is_dir())
        .map(|node_modules| node_modules.join(".oam"))
}

/// `node_modules/.oam/.gitignore`, so a vendored `node_modules` never
/// commits oam's state. Twin of `oam_loader::precompile::ensure_gitignore`
/// (same bytes; this crate must not depend on oam_loader).
fn ensure_gitignore(oam_dir: &Path) -> std::io::Result<()> {
    let gitignore = oam_dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::create_dir_all(oam_dir)?;
        std::fs::write(&gitignore, "# oam internal cache -- do not commit\n*\n")?;
    }
    Ok(())
}

/// Write `contents` to `path` if it is not already exactly that.
///
/// Two checks of the same project can run concurrently (an editor and a
/// terminal, two agents), so the write goes to a pid-unique temp file and is
/// renamed over the target -- an atomic replace on both platforms. A reader
/// therefore sees the old bytes or the new ones, never half a file, which as
/// a tsconfig would be a syntax error attributed to the user's project.
fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|prev| prev == contents) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, contents)?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(true),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            // Losing the race to another oam that wrote the same bytes is
            // success, not failure.
            if std::fs::read_to_string(path).is_ok_and(|prev| prev == contents) {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// The declarations as a file on disk, materialized on first use.
///
/// The name carries a content hash so an oam upgrade lands on a NEW path:
/// the daemon fingerprints the file list tsgo reports, so a new path is a
/// new entry and the cached diagnostics of the previous oam cannot survive
/// an upgrade that changed what the declarations say.
pub(crate) fn declarations_file() -> Option<PathBuf> {
    let path = dts_dir().join(format!(
        "oam-{:016x}.d.ts",
        fnv1a64(DECLARATIONS.as_bytes())
    ));
    if write_if_changed(&path, DECLARATIONS).ok()? {
        // A fresh hash means an oam upgrade (or a local edit): drop the
        // previous release's file so the cache does not accumulate one per
        // version forever. Best-effort -- an older oam still running just
        // rewrites its own on the next check, and Windows refuses to delete
        // a file a live tsgo has open, which is the same no-op.
        prune_older_declarations(&path);
    }
    Some(path)
}

fn prune_older_declarations(keep: &Path) {
    // Sweep the pre-split layout too. Until the dts/ and projects/ split, both
    // the .d.ts and the per-project wrappers sat directly in ts-decls/; an
    // upgraded install still has those files, and nothing else would ever
    // collect them -- the pruner below only walks dts/, so they would sit in
    // the cache forever. Only oam's own two shapes are touched, never a
    // stranger's file.
    if let Ok(entries) = std::fs::read_dir(decls_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let ours = (name.starts_with("oam-") && name.ends_with(".d.ts"))
                || (name.starts_with("project-") && name.ends_with(".json"));
            if ours && entry.path().is_file() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(dts_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path != keep && name.starts_with("oam-") && name.ends_with(".d.ts") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// What the wrapper needs to know about the user's `extends` chain.
#[derive(Default)]
struct ChainFacts {
    /// The `files` list of the nearest config that declares one, absolute.
    /// `Some(vec![])` for a solution-style `"files": []` -- declaring the
    /// key is what suppresses the default glob, not the entries in it.
    files: Option<Vec<PathBuf>>,
    declares_include: bool,
    declares_exclude: bool,
    /// `outDir` / `declarationDir`, absolute. Part of tsc's default exclude,
    /// so the restated default has to name them or a project that emits into
    /// its own tree would type-check its own output.
    out_dirs: Vec<PathBuf>,
    /// The `compilerOptions.types` list of the nearest config that declares
    /// one, verbatim. A relative entry resolves against the ROOT config
    /// (module docs), so the wrapper restates it absolute against the user's
    /// tsconfig dir; a package name is looked up from the wrapper's own
    /// directory, which is why the wrapper lives where it does.
    types: Option<Vec<String>>,
    /// `compilerOptions.composite` of the nearest config that declares it.
    composite: Option<bool>,
    /// Some config in the chain declares `rootDir`: inherited, and resolved
    /// against the config that declared it, so nothing to restate.
    declares_root_dir: bool,
    /// The user's tsconfig's own `references`, absolute. Only the root
    /// config's apply (module docs), so only the leaf's are read.
    references: Vec<PathBuf>,
    /// A config in the chain uses `${configDir}` (module docs): no wrapper.
    uses_config_dir: bool,
}

/// Read `tsconfig` and everything it extends, in TypeScript's precedence
/// order: a config wins over the ones it extends, and a later entry of an
/// `extends` ARRAY wins over an earlier one. First declaration of a key
/// wins, which is why the walk visits the leaf first and array entries
/// right to left.
///
/// None = a config in the chain could not be read or parsed. The caller then
/// runs the check with no declarations rather than guessing at a file set,
/// because guessing wrong drops the user's own sources out of the program.
fn read_chain(tsconfig: &Path) -> Option<ChainFacts> {
    let mut facts = ChainFacts::default();
    let mut seen: Vec<PathBuf> = Vec::new();
    visit(tsconfig, &mut facts, &mut seen)?;
    Some(facts)
}

fn visit(config: &Path, facts: &mut ChainFacts, seen: &mut Vec<PathBuf>) -> Option<()> {
    let leaf = seen.is_empty();
    // Cycle guard and depth cap, matching daemon::tsconfig_chain: a config
    // that extends itself is a user error tsgo reports, not a reason for the
    // checker to hang.
    if seen.contains(&config.to_path_buf()) || seen.len() >= 64 {
        return Some(());
    }
    seen.push(config.to_path_buf());
    let dir = config.parent()?;
    let raw = std::fs::read_to_string(config).ok()?;
    // The raw text, not the parsed value: the template can sit in any
    // path-typed key, and one inside a comment costs only a wrapper-less
    // check, which is the safe side.
    facts.uses_config_dir |= raw.contains("${configDir}");
    let json: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).ok()?;

    if facts.files.is_none()
        && let Some(files) = json.get("files").and_then(serde_json::Value::as_array)
    {
        facts.files = Some(
            files
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|entry| crate::normalize_path(&dir.join(entry)))
                .collect(),
        );
    }
    facts.declares_include |= json.get("include").is_some();
    facts.declares_exclude |= json.get("exclude").is_some();
    for key in ["outDir", "declarationDir"] {
        if let Some(value) = json
            .pointer("/compilerOptions")
            .and_then(|options| options.get(key))
            .and_then(serde_json::Value::as_str)
        {
            facts.out_dirs.push(crate::normalize_path(&dir.join(value)));
        }
    }
    // Nearest declaration wins, like `files`: compilerOptions merge per key,
    // so a leaf's `types` replaces a base's outright (probed for an
    // `extends` array too: the later entry's list replaces the earlier's).
    if facts.types.is_none()
        && let Some(types) = json
            .pointer("/compilerOptions/types")
            .and_then(serde_json::Value::as_array)
    {
        facts.types = Some(
            types
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect(),
        );
    }
    if facts.composite.is_none()
        && let Some(composite) = json
            .pointer("/compilerOptions/composite")
            .and_then(serde_json::Value::as_bool)
    {
        facts.composite = Some(composite);
    }
    facts.declares_root_dir |= json.pointer("/compilerOptions/rootDir").is_some();
    if leaf && let Some(references) = json.get("references").and_then(serde_json::Value::as_array) {
        facts.references = references
            .iter()
            .filter_map(|reference| reference.get("path").and_then(serde_json::Value::as_str))
            .map(|path| crate::normalize_path(&dir.join(path)))
            .collect();
    }

    let extends: Vec<&str> = match json.get("extends") {
        Some(serde_json::Value::String(one)) => vec![one.as_str()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            // Rightmost wins, so it is visited first.
            .rev()
            .collect(),
        _ => Vec::new(),
    };
    for spec in extends {
        // A bare-package `extends` that does not resolve is tsgo's error to
        // report; skipping it here only means the wrapper learns nothing
        // from it, which is the same as it declaring no keys.
        if let Some(path) = resolve_extends(dir, spec) {
            visit(&path, facts, seen)?;
        }
    }
    Some(())
}

/// Paths go into JSON with forward slashes: TypeScript accepts them on
/// Windows, and a backslash in a JSON string is an escape. Only on Windows
/// -- a backslash in a POSIX path is a legal filename character, and
/// rewriting it would name a different file.
fn json_path(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text
    }
}

/// TypeScript's own test for a `types` entry that names a RELATIVE path
/// (`pathIsRelative`): `.` or `..` alone, or either followed by a separator.
/// A rooted path needs no restating, and a package name like `node` is
/// looked up, not joined.
fn is_relative_types_entry(entry: &str) -> bool {
    match entry.strip_prefix("..").or_else(|| entry.strip_prefix('.')) {
        Some(rest) => rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'),
        None => false,
    }
}

/// A `types` entry that names a PACKAGE (`node`, `vitest/globals`): looked
/// up through typeRoots and the `node_modules` walk from the root config's
/// directory, so WHERE the wrapper sits decides whether it resolves. A
/// relative or rooted entry names a file and resolves the same from
/// anywhere once restated.
fn is_package_types_entry(entry: &str) -> bool {
    !is_relative_types_entry(entry) && !Path::new(entry).has_root()
}

/// The wrapper config's text: `extends` the user's, plus the declarations as
/// one more root file, plus whatever the module docs say does not survive
/// inheritance.
fn wrapper_json(tsconfig: &Path, declarations: &Path, facts: &ChainFacts) -> String {
    let project = tsconfig.parent().unwrap_or(Path::new("."));
    let mut files: Vec<String> = facts
        .files
        .iter()
        .flatten()
        .map(|path| json_path(path))
        .collect();
    files.push(json_path(declarations));

    let mut wrapper = serde_json::json!({
        "extends": json_path(tsconfig),
        "files": files,
    });
    let mut options = serde_json::Map::new();
    // A relative `types` entry would resolve against THIS file's directory.
    // Restate the whole list -- `types` is one key, so a partial restatement
    // would drop the package names -- with each relative entry made absolute
    // against the user's tsconfig dir: what it resolved against when that
    // config was the root, whichever config in the chain declared it. A
    // list of package names alone is left to inheritance untouched.
    if let Some(types) = &facts.types
        && types.iter().any(|entry| is_relative_types_entry(entry))
    {
        let restated: Vec<String> = types
            .iter()
            .map(|entry| {
                if is_relative_types_entry(entry) {
                    json_path(&crate::normalize_path(&project.join(entry)))
                } else {
                    entry.clone()
                }
            })
            .collect();
        options.insert("types".into(), serde_json::json!(restated));
    }
    // `composite` defaults `rootDir` to the root config's directory, which
    // would be ours: restate TypeScript's own default for the user's config.
    if facts.composite == Some(true) && !facts.declares_root_dir {
        options.insert("rootDir".into(), serde_json::json!(json_path(project)));
    }
    if !options.is_empty() {
        wrapper["compilerOptions"] = serde_json::Value::Object(options);
    }
    // `references` is never inherited; only the root config's apply.
    if !facts.references.is_empty() {
        wrapper["references"] = serde_json::Value::Array(
            facts
                .references
                .iter()
                .map(|path| serde_json::json!({ "path": json_path(path) }))
                .collect(),
        );
    }
    // The chain declares neither key, so its program is tsc's default glob
    // -- which our `files` would otherwise suppress. Restate it.
    if facts.files.is_none() && !facts.declares_include {
        wrapper["include"] = serde_json::json!([format!("{}/**/*", json_path(project))]);
        // Only when the chain declares no exclude of its own: an exclude
        // here would override theirs, and tsc's defaults do not apply once a
        // config declares one.
        if !facts.declares_exclude {
            let mut exclude: Vec<String> = ["node_modules", "bower_components", "jspm_packages"]
                .iter()
                .map(|dir| json_path(&project.join(dir)))
                .collect();
            exclude.extend(facts.out_dirs.iter().map(|path| json_path(path)));
            wrapper["exclude"] = serde_json::json!(exclude);
        }
    }
    serde_json::to_string_pretty(&wrapper).unwrap_or_default()
}

/// The tsconfig to hand tsgo for a check of `tsconfig`: a generated wrapper
/// that adds oam's declarations to the user's project, under the project's
/// nearest `node_modules/.oam/ts-decls/` when there is one and it can be
/// written, else under oam's cache (`in_tree_oam_dir` and the module docs
/// say why, and which chains get no wrapper at all).
///
/// None = run the user's tsconfig directly (the pre-existing behaviour, with
/// TS2307 on `oam:` imports). Never an error: a checker that refuses to run
/// because it could not attach its own types is worse than one that runs
/// without them.
pub(crate) fn project_config(tsconfig: &Path) -> Option<PathBuf> {
    let facts = read_chain(tsconfig)?;
    if facts.uses_config_dir {
        crate::debug(format_args!(
            "{} uses ${{configDir}}, which a wrapper config cannot preserve; checking without oam's declarations",
            tsconfig.display()
        ));
        return None;
    }
    let declarations = declarations_file()?;
    let name = format!("project-{}.json", project_key(tsconfig));
    let contents = wrapper_json(tsconfig, &declarations, &facts);
    if let Some(oam_dir) = in_tree_oam_dir(tsconfig) {
        let path = oam_dir.join("ts-decls").join(&name);
        match ensure_gitignore(&oam_dir).and_then(|()| write_if_changed(&path, &contents)) {
            Ok(_) => return Some(path),
            Err(e) => crate::debug(format_args!(
                "could not write {}: {e}; the wrapper config falls back to oam's cache dir",
                path.display()
            )),
        }
    }
    if facts
        .types
        .iter()
        .flatten()
        .any(|entry| is_package_types_entry(entry))
    {
        crate::debug(format_args!(
            "{} names a package in compilerOptions.types, which a wrapper config outside the project cannot resolve; checking without oam's declarations",
            tsconfig.display()
        ));
        return None;
    }
    let path = projects_dir().join(&name);
    write_if_changed(&path, &contents).ok().map(|_| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("oam-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn parsed(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("wrapper is valid JSON")
    }

    #[test]
    fn declarations_declare_every_oam_module() {
        // The declaration file is compiled in, so a build that lost it (a
        // bad path, an empty file) must fail here and not at a user's check.
        for specifier in ["oam:mcp", "oam:test", "oam:ai", "oam:permissions"] {
            assert!(
                DECLARATIONS.contains(&format!("declare module \"{specifier}\"")),
                "{specifier} has no declare-module block"
            );
        }
        assert!(DECLARATIONS.contains("declare const oam:"), "oam global");
        // A top-level export would make every block above an augmentation of
        // an unresolvable module, silently switching the declarations off.
        for line in DECLARATIONS.lines() {
            assert!(
                !line.starts_with("export ") && !line.starts_with("import "),
                "top-level {line:?} turns the file into a module"
            );
        }
    }

    #[test]
    fn materialized_declarations_round_trip_and_key_on_content() {
        let first = declarations_file().expect("cache dir is writable in test env");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), DECLARATIONS);
        // Same content, same path: the second call is a no-op, not a rewrite.
        let second = declarations_file().expect("second call");
        assert_eq!(first, second);
    }

    #[test]
    fn wrapper_adds_the_declarations_and_inherits_the_project() {
        let dir = scratch("decls-inherit");
        let tsconfig = dir.join("tsconfig.json");
        let dts = dir.join("oam.d.ts");
        let facts = ChainFacts {
            declares_include: true,
            ..Default::default()
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
        assert_eq!(wrapper["extends"], json_path(&tsconfig));
        assert_eq!(wrapper["files"], serde_json::json!([json_path(&dts)]));
        // The chain has an include: it survives inheritance untouched, and
        // restating it here would override the user's own file set.
        assert!(wrapper.get("include").is_none(), "{wrapper}");
        assert!(wrapper.get("exclude").is_none(), "{wrapper}");
        assert!(wrapper.get("compilerOptions").is_none(), "{wrapper}");
        assert!(wrapper.get("references").is_none(), "{wrapper}");
    }

    #[test]
    fn wrapper_restates_types_only_to_make_a_relative_entry_absolute() {
        let dir = scratch("decls-types");
        let tsconfig = dir.join("tsconfig.json");
        let dts = dir.join("oam.d.ts");
        let packages = ChainFacts {
            declares_include: true,
            types: Some(vec!["node".into(), "vitest/globals".into()]),
            ..Default::default()
        };
        // Package names resolve from wherever the wrapper is, as long as it
        // sits inside the project's node_modules; restating them would only
        // add a key to keep in sync with the user's.
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &packages));
        assert!(wrapper.get("compilerOptions").is_none(), "{wrapper}");

        let relative = ChainFacts {
            types: Some(vec![
                "node".into(),
                "./typings/local".into(),
                "../shared".into(),
                "/rooted".into(),
            ]),
            ..packages
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &relative));
        assert_eq!(
            wrapper["compilerOptions"]["types"],
            serde_json::json!([
                "node",
                json_path(&crate::normalize_path(&dir.join("typings").join("local"))),
                json_path(&crate::normalize_path(&dir.join("..").join("shared"))),
                "/rooted",
            ]),
            "every entry restated, the relative ones against the user's tsconfig dir: {wrapper}"
        );
    }

    #[test]
    fn relative_types_entries_are_typescripts_definition_not_a_substring_test() {
        for entry in [".", "..", "./x", "../x", ".\\x", "..\\x"] {
            assert!(is_relative_types_entry(entry), "{entry}");
            assert!(!is_package_types_entry(entry), "{entry}");
        }
        for entry in ["node", ".hidden", "...", "@types/node", "vitest/globals"] {
            assert!(!is_relative_types_entry(entry), "{entry}");
            assert!(is_package_types_entry(entry), "{entry}");
        }
        for entry in ["/abs", if cfg!(windows) { "C:/abs" } else { "/abs/x" }] {
            assert!(!is_relative_types_entry(entry), "{entry}");
            assert!(!is_package_types_entry(entry), "{entry} is rooted");
        }
    }

    #[test]
    fn wrapper_restates_root_dir_for_a_composite_chain_and_the_leafs_references() {
        let dir = scratch("decls-composite");
        let tsconfig = dir.join("tsconfig.json");
        let dts = dir.join("oam.d.ts");
        let composite = ChainFacts {
            declares_include: true,
            composite: Some(true),
            references: vec![dir.join("lib"), dir.join("..").join("shared")],
            ..Default::default()
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &composite));
        assert_eq!(
            wrapper["compilerOptions"]["rootDir"],
            json_path(&dir),
            "TypeScript's own composite default, for the user's config: {wrapper}"
        );
        assert_eq!(
            wrapper["references"],
            serde_json::json!([
                { "path": json_path(&dir.join("lib")) },
                { "path": json_path(&dir.join("..").join("shared")) },
            ]),
            "{wrapper}"
        );
        // A declared rootDir is inherited (resolved against its own config)
        // and composite: false has no default to restate.
        for facts in [
            ChainFacts {
                declares_root_dir: true,
                ..ChainFacts {
                    declares_include: true,
                    composite: Some(true),
                    ..Default::default()
                }
            },
            ChainFacts {
                declares_include: true,
                composite: Some(false),
                ..Default::default()
            },
        ] {
            let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
            assert!(wrapper.get("compilerOptions").is_none(), "{wrapper}");
        }
    }

    #[test]
    fn wrapper_restates_the_default_glob_when_the_chain_declares_neither_key() {
        let dir = scratch("decls-default");
        let tsconfig = dir.join("tsconfig.json");
        let dts = dir.join("oam.d.ts");
        let facts = ChainFacts {
            out_dirs: vec![dir.join("dist")],
            ..Default::default()
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
        assert_eq!(
            wrapper["include"],
            serde_json::json!([format!("{}/**/*", json_path(&dir))])
        );
        let exclude = wrapper["exclude"].as_array().expect("exclude").clone();
        for name in ["node_modules", "bower_components", "jspm_packages", "dist"] {
            assert!(
                exclude.contains(&serde_json::json!(json_path(&dir.join(name)))),
                "{name} missing from {exclude:?}"
            );
        }
    }

    #[test]
    fn wrapper_keeps_a_declared_exclude_and_a_declared_files_list() {
        let dir = scratch("decls-files");
        let tsconfig = dir.join("tsconfig.json");
        let dts = dir.join("oam.d.ts");
        let facts = ChainFacts {
            files: Some(vec![dir.join("main.ts")]),
            declares_exclude: true,
            ..Default::default()
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
        assert_eq!(
            wrapper["files"],
            serde_json::json!([json_path(&dir.join("main.ts")), json_path(&dts)]),
            "our file is ADDED to theirs, not instead of it"
        );
        // A declared `files` already suppresses the default glob, so there is
        // no default to restate.
        assert!(wrapper.get("include").is_none(), "{wrapper}");
        assert!(wrapper.get("exclude").is_none(), "{wrapper}");
    }

    #[test]
    fn chain_reads_keys_through_extends_with_the_nearest_declaration_winning() {
        let dir = scratch("decls-chain");
        std::fs::create_dir_all(dir.join("cfg")).unwrap();
        std::fs::write(
            dir.join("cfg").join("base.json"),
            r#"{ "compilerOptions": { "outDir": "out", "types": ["./typings"], "composite": true },
                "files": ["base.ts"], "exclude": ["x"],
                "references": [{ "path": "./not-the-leafs" }] }"#,
        )
        .unwrap();
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            r#"{
                // JSONC, like a real one
                "extends": "./cfg/base.json",
                "files": ["leaf.ts"],
                "references": [{ "path": "./lib" }, { "path": "../shared", "prepend": false }, { "notapath": 1 }],
            }"#,
        )
        .unwrap();
        let facts = read_chain(&tsconfig).expect("parses");
        assert_eq!(
            facts.files,
            Some(vec![crate::normalize_path(&dir.join("leaf.ts"))]),
            "the leaf's files wins over the base's"
        );
        assert!(facts.declares_exclude, "the base's exclude still counts");
        assert!(!facts.declares_include);
        assert_eq!(
            facts.out_dirs,
            vec![crate::normalize_path(&dir.join("cfg").join("out"))],
            "outDir resolves against the config that declared it"
        );
        assert_eq!(
            facts.types,
            Some(vec!["./typings".to_string()]),
            "types is recorded verbatim; a relative entry is root-relative, not base-relative"
        );
        assert_eq!(facts.composite, Some(true), "inherited from the base");
        assert!(!facts.declares_root_dir);
        assert_eq!(
            facts.references,
            vec![
                crate::normalize_path(&dir.join("lib")),
                crate::normalize_path(&dir.join("..").join("shared")),
            ],
            "only the LEAF's references count, and only entries with a path"
        );
        assert!(!facts.uses_config_dir);

        // A leaf `types` replaces the base's outright, even when empty; a
        // leaf `composite: false` beats the base's true; a rootDir anywhere
        // in the chain is declared.
        std::fs::write(
            &tsconfig,
            r#"{ "extends": "./cfg/base.json",
                "compilerOptions": { "types": [], "composite": false, "rootDir": "." } }"#,
        )
        .unwrap();
        let facts = read_chain(&tsconfig).expect("parses");
        assert_eq!(facts.types, Some(Vec::new()));
        assert_eq!(facts.composite, Some(false));
        assert!(facts.declares_root_dir);
        assert!(facts.references.is_empty(), "the base's are never read");
    }

    #[test]
    fn chain_using_config_dir_gets_no_wrapper() {
        // `${configDir}` would substitute the wrapper's directory; a chain
        // that uses it is checked bare rather than through a wrong program.
        let dir = scratch("decls-configdir");
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(
            dir.join("base.json"),
            r#"{ "include": ["${configDir}/src"] }"#,
        )
        .unwrap();
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, r#"{ "extends": "./base.json" }"#).unwrap();
        assert!(read_chain(&tsconfig).unwrap().uses_config_dir);
        assert!(project_config(&tsconfig).is_none());
        assert!(
            !dir.join("node_modules").join(".oam").exists(),
            "nothing written for a chain that gets no wrapper"
        );
    }

    #[test]
    fn wrapper_lands_in_the_nearest_node_modules_oam_dir() {
        // The project's own node_modules when it has one...
        let dir = scratch("decls-intree");
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, r#"{ "compilerOptions": { "types": ["node"] } }"#).unwrap();
        let wrapper = project_config(&tsconfig).expect("wrapper written");
        let oam_dir = dir.join("node_modules").join(".oam");
        assert_eq!(
            wrapper,
            oam_dir
                .join("ts-decls")
                .join(format!("project-{}.json", project_key(&tsconfig))),
        );
        assert!(wrapper.is_file());
        assert!(
            std::fs::read_to_string(oam_dir.join(".gitignore"))
                .is_ok_and(|text| text.lines().any(|line| line == "*")),
            "the .oam dir is gitignored the way oam_loader gitignores it"
        );
        assert!(
            !projects_dir().join(wrapper.file_name().unwrap()).exists(),
            "no second copy under oam's cache"
        );

        // ...and the hoisted one of a monorepo root when the package has none.
        let root = scratch("decls-hoisted");
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        let package = root.join("packages").join("foo");
        std::fs::create_dir_all(&package).unwrap();
        let tsconfig = package.join("tsconfig.json");
        std::fs::write(&tsconfig, "{}").unwrap();
        let wrapper = project_config(&tsconfig).expect("wrapper written");
        assert!(
            wrapper.starts_with(root.join("node_modules").join(".oam").join("ts-decls")),
            "{}",
            wrapper.display()
        );
        assert!(
            !package.join("node_modules").exists(),
            "no node_modules invented"
        );
    }

    #[test]
    fn chain_treats_an_empty_files_list_as_declared() {
        // Solution-style `"files": []` means "check nothing here"; reading it
        // as "declares nothing" would make the wrapper glob the whole tree.
        let dir = scratch("decls-solution");
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, r#"{ "files": [], "references": [] }"#).unwrap();
        let facts = read_chain(&tsconfig).expect("parses");
        assert_eq!(facts.files, Some(Vec::new()));
        let wrapper = parsed(&wrapper_json(&tsconfig, &dir.join("oam.d.ts"), &facts));
        assert!(wrapper.get("include").is_none(), "{wrapper}");
    }

    #[test]
    fn chain_gives_up_on_a_config_it_cannot_parse() {
        let dir = scratch("decls-bad");
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, "{ this is not json").unwrap();
        assert!(read_chain(&tsconfig).is_none());
        assert!(
            project_config(&tsconfig).is_none(),
            "no wrapper from a config we could not read"
        );
        assert!(read_chain(&dir.join("absent.json")).is_none());
    }

    #[test]
    fn wrapper_falls_back_to_the_cache_dir_when_the_in_tree_location_is_unwritable() {
        // A read-only node_modules is hard to stage portably; a FILE where
        // the `.oam` directory would go refuses the write the same way
        // (create_dir_all fails), and that is the fallback's whole contract:
        // the check still runs, from oam's cache, never an error.
        let dir = scratch("decls-fallback");
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        let obstacle = dir.join("node_modules").join(".oam");
        std::fs::write(&obstacle, "in the way").unwrap();
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, "{}").unwrap();
        let wrapper = project_config(&tsconfig).expect("wrapper written");
        assert!(
            wrapper.starts_with(projects_dir()),
            "{} should fall back to oam's cache dir",
            wrapper.display()
        );
        assert_eq!(
            std::fs::read_to_string(&obstacle).unwrap(),
            "in the way",
            "the obstacle is left alone"
        );

        // Except when the chain names a package in `types`: from oam's
        // cache dir that cannot resolve (and a stray @types above the cache
        // dir could), so the check runs bare. A relative entry restates
        // fine from anywhere and keeps the fallback wrapper.
        std::fs::write(&tsconfig, r#"{ "compilerOptions": { "types": ["node"] } }"#).unwrap();
        assert!(project_config(&tsconfig).is_none());
        std::fs::write(
            &tsconfig,
            r#"{ "compilerOptions": { "types": ["./typings"] } }"#,
        )
        .unwrap();
        let wrapper = project_config(&tsconfig).expect("wrapper written");
        assert!(wrapper.starts_with(projects_dir()));
        assert_eq!(
            parsed(&std::fs::read_to_string(&wrapper).unwrap())["compilerOptions"]["types"],
            serde_json::json!([json_path(&crate::normalize_path(&dir.join("typings")))])
        );
    }

    #[test]
    fn project_config_writes_a_wrapper_that_names_the_user_config() {
        let dir = scratch("decls-project");
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, r#"{ "compilerOptions": { "strict": true } }"#).unwrap();
        let wrapper = project_config(&tsconfig).expect("wrapper written");
        let text = std::fs::read_to_string(&wrapper).unwrap();
        let json = parsed(&text);
        assert_eq!(json["extends"], json_path(&tsconfig));
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].as_str().unwrap().ends_with(".d.ts"));
        // Stable path per project: a second run must reuse it, or every
        // check would leave another file behind.
        assert_eq!(
            project_config(&tsconfig).as_deref(),
            Some(wrapper.as_path())
        );
    }
}
