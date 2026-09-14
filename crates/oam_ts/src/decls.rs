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
//! * `rootDir`'s default. TypeScript 7 defaults it to the root config's
//!   directory (measured: with `outDir` and no `rootDir`, emit lands at
//!   `dist/src/a.js`), and checks it eagerly whenever an option makes emit
//!   paths matter -- `outDir`, `declarationDir`, `sourceRoot`, `mapRoot`
//!   (each measured to report TS6059 through a wrapper in another
//!   directory, `--noEmit` notwithstanding), and `composite` -- so every
//!   source was "not under rootDir". The wrapper restates TypeScript's own
//!   default -- the user's tsconfig dir -- when the chain declares any of
//!   those and no `rootDir`.
//!
//! * `references`, the one top-level key TypeScript excludes from
//!   inheritance. The wrapper restates the user's own list with absolute
//!   paths, so an import into an unbuilt reference reports TS6305 through
//!   oam as it does through `tsc -p` (probed: it checked the reference's
//!   sources and passed before).
//!
//! * `${configDir}` (TypeScript 5.5+): "the directory of the ROOT config",
//!   substituted wherever a path-typed value STARTS with it -- and through
//!   the wrapper that is the wrapper's directory, so a base config's
//!   `"include": ["${configDir}/src"]` matched nothing and the check came
//!   back clean with a type error present (probed). The wrapper restates
//!   every key whose effective value names the template, substituted with
//!   the user's tsconfig dir, and any other relative entry of that key made
//!   absolute against the config that declared it. Which keys, measured on
//!   tsgo (`--showConfig` through a wrapper in another directory shows what
//!   moved): `files`, `include`, `exclude`, and of compilerOptions `outDir`,
//!   `declarationDir`, `rootDir`, `rootDirs`, `outFile`, `tsBuildInfoFile`,
//!   `baseUrl`, `generateTrace`, `typeRoots` and the values of `paths`. NOT
//!   substituted, so nothing to restate: `types` (a template entry is looked
//!   up as a package name, and fails as one), `references[].path` (the
//!   literal directory, TS6053), `mapRoot` and `sourceRoot` (emitted
//!   verbatim). Two quirks the wrapper mirrors: the template is recognized
//!   without regard to case but replaced only in its exact spelling, so
//!   `${CONFIGDIR}/x` keeps its text and is resolved against the root dir
//!   instead of the declaring config's; and it counts only at the START of
//!   a value -- `sub/${configDir}/x` is a literal directory name, resolved
//!   against the declaring config like any other relative entry.
//!
//! Those are the only places this file reimplements tsc's config semantics,
//! and they are why `read_chain` reports which keys the chain declares, and
//! resolves only the entries the wrapper may have to restate.
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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::daemon::{cache_root, fnv1a64, project_key, resolve_extends, strip_jsonc};

/// TypeScript 5.5's template for the ROOT config's directory (module docs).
const CONFIG_DIR_TEMPLATE: &str = "${configDir}";

/// The string-valued path-typed compilerOptions tsgo substitutes the template
/// in (module docs, measured). `baseUrl` and `outFile` stay on the list
/// although TypeScript 7 rejects them outright (TS5102): the substitution
/// still runs before the rejection, and restating them keeps the report the
/// user's own tsconfig gets.
const PATH_OPTIONS: [&str; 7] = [
    "outDir",
    "declarationDir",
    "rootDir",
    "outFile",
    "tsBuildInfoFile",
    "baseUrl",
    "generateTrace",
];

/// The list-valued ones.
const PATH_LIST_OPTIONS: [&str; 2] = ["rootDirs", "typeRoots"];

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

/// One path-typed value, resolved the way tsc resolves it when the user's
/// tsconfig is the root of the compilation (`resolve_entry`).
#[derive(Debug, PartialEq)]
struct PathValue {
    path: PathBuf,
    /// The value named `${configDir}`. Inherited through the wrapper it
    /// would resolve against the wrapper's directory, so it is restated.
    config_dir: bool,
}

/// A path-typed list, resolved the same way.
#[derive(Default, Debug, PartialEq)]
struct PathList {
    entries: Vec<PathBuf>,
    /// Some entry named `${configDir}`: the whole list is restated, since a
    /// key is inherited or replaced as a unit.
    config_dir: bool,
}

/// `compilerOptions.paths` with its values resolved (`paths_map`), in the
/// shape the wrapper writes.
#[derive(Default, Debug, PartialEq)]
struct PathsMap {
    map: serde_json::Map<String, serde_json::Value>,
    config_dir: bool,
}

/// What the wrapper needs to know about the user's `extends` chain.
#[derive(Default)]
struct ChainFacts {
    /// The `files` list of the nearest config that declares one, absolute.
    /// `Some(vec![])` for a solution-style `"files": []` -- declaring the
    /// key is what suppresses the default glob, not the entries in it.
    files: Option<Vec<PathBuf>>,
    /// `include` / `exclude` of the nearest config that declares each.
    /// Declared means inherited untouched -- unless an entry names
    /// `${configDir}`, when the list is restated (module docs).
    include: Option<PathList>,
    exclude: Option<PathList>,
    /// The string-valued path-typed compilerOptions (`PATH_OPTIONS`), the
    /// nearest declaration per key. `outDir` / `declarationDir` are part of
    /// tsc's default exclude, so the restated default has to name them or a
    /// project that emits into its own tree would type-check its own
    /// output; a declared `rootDir` is inherited, resolved against the
    /// config that declared it, so no default to restate
    /// (`needs_root_dir_default`); and any of them naming `${configDir}`
    /// is restated.
    path_options: BTreeMap<&'static str, PathValue>,
    /// The list-valued ones (`PATH_LIST_OPTIONS`), restated when an entry
    /// names `${configDir}`.
    path_lists: BTreeMap<&'static str, PathList>,
    /// `compilerOptions.paths` of the nearest config that declares it,
    /// restated when a value names `${configDir}`.
    paths: Option<PathsMap>,
    /// The `compilerOptions.types` list of the nearest config that declares
    /// one, verbatim. A relative entry resolves against the ROOT config
    /// (module docs), so the wrapper restates it absolute against the user's
    /// tsconfig dir; a package name is looked up from the wrapper's own
    /// directory, which is why the wrapper lives where it does.
    types: Option<Vec<String>>,
    /// `compilerOptions.composite` of the nearest config that declares it.
    composite: Option<bool>,
    /// Some config in the chain declares `sourceRoot` or `mapRoot`: neither
    /// is a path to resolve (module docs), but either makes tsc check
    /// `rootDir` eagerly.
    declares_emit_roots: bool,
    /// The user's tsconfig's own `references`, absolute. Only the root
    /// config's apply (module docs), so only the leaf's are read.
    references: Vec<PathBuf>,
}

impl ChainFacts {
    /// The chain leaves `rootDir` to its default -- the ROOT config's
    /// directory in TypeScript 7 -- and declares something that makes tsc
    /// check it (module docs): the wrapper has to restate the default for
    /// the user's config, or every source is "not under rootDir" (TS6059).
    fn needs_root_dir_default(&self) -> bool {
        !self.path_options.contains_key("rootDir")
            && (self.composite == Some(true)
                || self.declares_emit_roots
                || ["outDir", "declarationDir"]
                    .iter()
                    .any(|key| self.path_options.contains_key(key)))
    }

    /// The keys whose effective value names `${configDir}` -- what the
    /// wrapper restates on that account.
    fn config_dir_keys(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if self.include.as_ref().is_some_and(|list| list.config_dir) {
            keys.push("include");
        }
        if self.exclude.as_ref().is_some_and(|list| list.config_dir) {
            keys.push("exclude");
        }
        keys.extend(
            self.path_options
                .iter()
                .filter(|(_, value)| value.config_dir)
                .map(|(key, _)| *key),
        );
        keys.extend(
            self.path_lists
                .iter()
                .filter(|(_, list)| list.config_dir)
                .map(|(key, _)| *key),
        );
        if self.paths.as_ref().is_some_and(|paths| paths.config_dir) {
            keys.push("paths");
        }
        keys
    }
}

/// TypeScript's own test for the template (`startsWithConfigDirTemplate`):
/// at the start of the value, compared without regard to case.
fn names_config_dir(entry: &str) -> bool {
    entry
        .get(..CONFIG_DIR_TEMPLATE.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(CONFIG_DIR_TEMPLATE))
}

/// One entry of a path-typed key, resolved as tsc resolves it when the
/// user's tsconfig is the root of the compilation: `${configDir}` at the
/// start means `root`; anything else is relative to `dir`, the config that
/// declared it (and a rooted path is itself). Only the exact spelling is
/// replaced -- any other capitalisation passes tsc's test but not its
/// replacement, so the text is kept and just the base moves to `root`
/// (measured: `${CONFIGDIR}/upper` resolves to `<root>/${CONFIGDIR}/upper`).
fn resolve_entry(entry: &str, dir: &Path, root: &Path) -> PathValue {
    if names_config_dir(entry) {
        let rest = entry
            .strip_prefix(CONFIG_DIR_TEMPLATE)
            .map_or(entry, |rest| rest.trim_start_matches(['/', '\\']));
        PathValue {
            path: crate::normalize_path(&root.join(rest)),
            config_dir: true,
        }
    } else {
        PathValue {
            path: crate::normalize_path(&dir.join(entry)),
            config_dir: false,
        }
    }
}

fn path_list(list: &[serde_json::Value], dir: &Path, root: &Path) -> PathList {
    let mut out = PathList::default();
    for entry in list.iter().filter_map(serde_json::Value::as_str) {
        let value = resolve_entry(entry, dir, root);
        out.config_dir |= value.config_dir;
        out.entries.push(value.path);
    }
    out
}

/// `paths` values, each resolved like any other path-typed entry -- against
/// the config that declared the map, `baseUrl` (the other base) being gone
/// in TypeScript 7 -- except one that is neither relative nor rooted
/// (`lib/*`): tsgo reports TS5090 for it whatever it resolves to, and only
/// the verbatim text keeps that report. Anything that is not a string passes
/// through for tsgo to reject as it would.
fn paths_map(
    map: &serde_json::Map<String, serde_json::Value>,
    dir: &Path,
    root: &Path,
) -> PathsMap {
    let mut out = PathsMap::default();
    for (pattern, targets) in map {
        let restated = match targets {
            serde_json::Value::Array(targets) => serde_json::Value::Array(
                targets
                    .iter()
                    .map(|target| match target.as_str() {
                        Some(text) if names_config_dir(text) || is_relative_path(text) => {
                            let value = resolve_entry(text, dir, root);
                            out.config_dir |= value.config_dir;
                            serde_json::Value::String(json_path(&value.path))
                        }
                        _ => target.clone(),
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        out.map.insert(pattern.clone(), restated);
    }
    out
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
    // The root of the compilation `tsc -p tsconfig.json` would run, which is
    // what `${configDir}` means whichever config in the chain says it.
    let root = tsconfig.parent()?;
    let mut facts = ChainFacts::default();
    let mut seen: Vec<PathBuf> = Vec::new();
    visit(tsconfig, root, &mut facts, &mut seen)?;
    Some(facts)
}

fn visit(
    config: &Path,
    root: &Path,
    facts: &mut ChainFacts,
    seen: &mut Vec<PathBuf>,
) -> Option<()> {
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
    let json: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).ok()?;

    if facts.files.is_none()
        && let Some(files) = json.get("files").and_then(serde_json::Value::as_array)
    {
        facts.files = Some(path_list(files, dir, root).entries);
    }
    if facts.include.is_none()
        && let Some(include) = json.get("include").and_then(serde_json::Value::as_array)
    {
        facts.include = Some(path_list(include, dir, root));
    }
    if facts.exclude.is_none()
        && let Some(exclude) = json.get("exclude").and_then(serde_json::Value::as_array)
    {
        facts.exclude = Some(path_list(exclude, dir, root));
    }
    // Nearest declaration wins, like `files`: compilerOptions merge per key,
    // so a leaf's `types` (or `paths`, or `outDir`) replaces a base's
    // outright (probed for an `extends` array too: the later entry's list
    // replaces the earlier's).
    let options = json.get("compilerOptions");
    let option = |key: &str| options.and_then(|options| options.get(key));
    for key in PATH_OPTIONS {
        if !facts.path_options.contains_key(key)
            && let Some(value) = option(key).and_then(serde_json::Value::as_str)
        {
            facts
                .path_options
                .insert(key, resolve_entry(value, dir, root));
        }
    }
    for key in PATH_LIST_OPTIONS {
        if !facts.path_lists.contains_key(key)
            && let Some(list) = option(key).and_then(serde_json::Value::as_array)
        {
            facts.path_lists.insert(key, path_list(list, dir, root));
        }
    }
    if facts.paths.is_none()
        && let Some(map) = option("paths").and_then(serde_json::Value::as_object)
    {
        facts.paths = Some(paths_map(map, dir, root));
    }
    if facts.types.is_none()
        && let Some(types) = option("types").and_then(serde_json::Value::as_array)
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
        && let Some(composite) = option("composite").and_then(serde_json::Value::as_bool)
    {
        facts.composite = Some(composite);
    }
    facts.declares_emit_roots |= option("sourceRoot").is_some() || option("mapRoot").is_some();
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
            visit(&path, root, facts, seen)?;
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

/// TypeScript's own `pathIsRelative`: `.` or `..` alone, or either followed
/// by a separator. What a `types` entry has to look like to name a FILE
/// rather than a package (a rooted path needs no restating, and a package
/// name like `node` is looked up, not joined), and what a `paths` value has
/// to look like for TypeScript 7 not to report TS5090.
fn is_relative_path(entry: &str) -> bool {
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
    !is_relative_path(entry) && !Path::new(entry).has_root()
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
        && types.iter().any(|entry| is_relative_path(entry))
    {
        let restated: Vec<String> = types
            .iter()
            .map(|entry| {
                if is_relative_path(entry) {
                    json_path(&crate::normalize_path(&project.join(entry)))
                } else {
                    entry.clone()
                }
            })
            .collect();
        options.insert("types".into(), serde_json::json!(restated));
    }
    // A path-typed option naming `${configDir}` would be substituted with
    // THIS file's directory (module docs): restate it resolved, the user's
    // tsconfig dir standing in for the template. One that does not is
    // inherited untouched, resolved against the config that declared it.
    for (key, value) in &facts.path_options {
        if value.config_dir {
            options.insert((*key).into(), serde_json::json!(json_path(&value.path)));
        }
    }
    for (key, list) in &facts.path_lists {
        if list.config_dir {
            let entries: Vec<String> = list.entries.iter().map(|path| json_path(path)).collect();
            options.insert((*key).into(), serde_json::json!(entries));
        }
    }
    if let Some(paths) = &facts.paths
        && paths.config_dir
    {
        options.insert("paths".into(), serde_json::Value::Object(paths.map.clone()));
    }
    // `rootDir` defaults to the root config's directory, which would be
    // ours: restate TypeScript's own default for the user's config wherever
    // tsc would check it (module docs).
    if facts.needs_root_dir_default() {
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
    let restated = |list: &PathList| -> serde_json::Value {
        let entries: Vec<String> = list.entries.iter().map(|path| json_path(path)).collect();
        serde_json::json!(entries)
    };
    match &facts.include {
        // An include naming `${configDir}` would match under THIS file's
        // directory (module docs): restate it resolved.
        Some(include) if include.config_dir => wrapper["include"] = restated(include),
        // Declared without it: inherited, resolved against its own config.
        Some(_) => {}
        // The chain declares neither key, so its program is tsc's default
        // glob -- which our `files` would otherwise suppress. Restate it.
        None if facts.files.is_none() => {
            wrapper["include"] = serde_json::json!([format!("{}/**/*", json_path(project))]);
        }
        None => {}
    }
    match &facts.exclude {
        Some(exclude) if exclude.config_dir => wrapper["exclude"] = restated(exclude),
        Some(_) => {}
        // With the default glob restated above, and only then: an exclude
        // here would override theirs, and tsc's defaults do not apply once a
        // config declares one.
        None if facts.files.is_none() && facts.include.is_none() => {
            let mut exclude: Vec<String> = ["node_modules", "bower_components", "jspm_packages"]
                .iter()
                .map(|dir| json_path(&project.join(dir)))
                .collect();
            exclude.extend(
                ["outDir", "declarationDir"]
                    .iter()
                    .filter_map(|key| facts.path_options.get(key))
                    .map(|value| json_path(&value.path)),
            );
            wrapper["exclude"] = serde_json::json!(exclude);
        }
        None => {}
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
    let config_dir_keys = facts.config_dir_keys();
    if !config_dir_keys.is_empty() {
        crate::debug(format_args!(
            "{} names ${{configDir}} in {}; the wrapper restates each against the project dir",
            tsconfig.display(),
            config_dir_keys.join(", ")
        ));
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
            include: Some(PathList::default()),
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
            include: Some(PathList::default()),
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
    fn relative_entries_are_typescripts_definition_not_a_substring_test() {
        for entry in [".", "..", "./x", "../x", ".\\x", "..\\x"] {
            assert!(is_relative_path(entry), "{entry}");
            assert!(!is_package_types_entry(entry), "{entry}");
        }
        for entry in ["node", ".hidden", "...", "@types/node", "vitest/globals"] {
            assert!(!is_relative_path(entry), "{entry}");
            assert!(is_package_types_entry(entry), "{entry}");
        }
        for entry in ["/abs", if cfg!(windows) { "C:/abs" } else { "/abs/x" }] {
            assert!(!is_relative_path(entry), "{entry}");
            assert!(!is_package_types_entry(entry), "{entry} is rooted");
        }
    }

    #[test]
    fn wrapper_restates_root_dir_for_a_composite_chain_and_the_leafs_references() {
        let dir = scratch("decls-composite");
        let tsconfig = dir.join("tsconfig.json");
        let dts = dir.join("oam.d.ts");
        let composite = ChainFacts {
            include: Some(PathList::default()),
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
        // The same default whenever tsc checks rootDir eagerly (module
        // docs): an out dir or a declaration dir, or sourceRoot / mapRoot.
        let plain = |key: &'static str| {
            BTreeMap::from([(
                key,
                PathValue {
                    path: dir.join("dist"),
                    config_dir: false,
                },
            )])
        };
        for facts in [
            ChainFacts {
                include: Some(PathList::default()),
                path_options: plain("outDir"),
                ..Default::default()
            },
            ChainFacts {
                include: Some(PathList::default()),
                path_options: plain("declarationDir"),
                ..Default::default()
            },
            ChainFacts {
                include: Some(PathList::default()),
                declares_emit_roots: true,
                ..Default::default()
            },
        ] {
            let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
            assert_eq!(
                wrapper["compilerOptions"]["rootDir"],
                json_path(&dir),
                "{wrapper}"
            );
        }
        // A declared rootDir is inherited (resolved against its own config);
        // composite: false has no default to restate; and a chain that
        // declares none of the triggers gets no rootDir either -- the
        // default is never checked, and the wrapper stays what it was.
        for facts in [
            ChainFacts {
                path_options: BTreeMap::from([
                    (
                        "rootDir",
                        PathValue {
                            path: dir.join("src"),
                            config_dir: false,
                        },
                    ),
                    (
                        "outDir",
                        PathValue {
                            path: dir.join("dist"),
                            config_dir: false,
                        },
                    ),
                ]),
                ..ChainFacts {
                    include: Some(PathList::default()),
                    composite: Some(true),
                    ..Default::default()
                }
            },
            ChainFacts {
                include: Some(PathList::default()),
                composite: Some(false),
                ..Default::default()
            },
            ChainFacts {
                include: Some(PathList::default()),
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
            path_options: BTreeMap::from([(
                "outDir",
                PathValue {
                    path: dir.join("dist"),
                    config_dir: false,
                },
            )]),
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
            exclude: Some(PathList::default()),
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
        assert_eq!(
            facts.exclude,
            Some(PathList {
                entries: vec![crate::normalize_path(&dir.join("cfg").join("x"))],
                config_dir: false,
            }),
            "the base's exclude still counts, resolved against the config that declared it"
        );
        assert!(facts.include.is_none());
        assert_eq!(
            facts.path_options.get("outDir"),
            Some(&PathValue {
                path: crate::normalize_path(&dir.join("cfg").join("out")),
                config_dir: false,
            }),
            "outDir resolves against the config that declared it"
        );
        assert_eq!(
            facts.types,
            Some(vec!["./typings".to_string()]),
            "types is recorded verbatim; a relative entry is root-relative, not base-relative"
        );
        assert_eq!(facts.composite, Some(true), "inherited from the base");
        assert!(!facts.path_options.contains_key("rootDir"));
        assert_eq!(
            facts.references,
            vec![
                crate::normalize_path(&dir.join("lib")),
                crate::normalize_path(&dir.join("..").join("shared")),
            ],
            "only the LEAF's references count, and only entries with a path"
        );
        assert!(facts.config_dir_keys().is_empty());

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
        assert_eq!(
            facts.path_options.get("rootDir"),
            Some(&PathValue {
                path: crate::normalize_path(&dir),
                config_dir: false,
            })
        );
        assert!(facts.references.is_empty(), "the base's are never read");
    }

    #[test]
    fn entries_resolve_the_template_the_way_typescript_does() {
        let root = scratch("decls-resolve");
        let declaring = root.join("node_modules").join("@acme").join("tsconfig");
        let resolved = |entry: &str| resolve_entry(entry, &declaring, &root);
        let at = |base: &Path, rest: &str| crate::normalize_path(&base.join(rest));
        // The exact spelling at the start: the root config's directory,
        // whichever config declared it -- and only that spelling.
        for (entry, path) in [
            ("${configDir}/src", at(&root, "src")),
            ("${configDir}", crate::normalize_path(&root)),
            ("${configDir}/", crate::normalize_path(&root)),
            ("${configDir}src", at(&root, "src")),
            ("${configDir}/../sibling", at(&root, "../sibling")),
        ] {
            assert_eq!(
                resolved(entry),
                PathValue {
                    path,
                    config_dir: true
                },
                "{entry}"
            );
        }
        // Any other capitalisation passes TypeScript's test but not its
        // replacement (measured): the text stays, the base moves to root.
        for entry in ["${CONFIGDIR}/upper", "${ConfigDir}/c"] {
            assert_eq!(
                resolved(entry),
                PathValue {
                    path: at(&root, entry),
                    config_dir: true
                },
                "{entry}"
            );
        }
        // Not at the start: a literal directory name, relative to the
        // config that declared it like any other entry.
        for entry in ["sub/${configDir}/x", "./${configDir}", "plain/dir", "."] {
            assert_eq!(
                resolved(entry),
                PathValue {
                    path: at(&declaring, entry),
                    config_dir: false
                },
                "{entry}"
            );
        }
        // A rooted entry is itself.
        let rooted = if cfg!(windows) {
            "C:/elsewhere"
        } else {
            "/elsewhere"
        };
        assert_eq!(resolved(rooted).path, PathBuf::from(rooted));
        assert!(names_config_dir("${configdir}x"));
        assert!(!names_config_dir("${configDi"));
        assert!(!names_config_dir(""));
    }

    #[test]
    fn chain_records_the_template_per_key_with_the_nearest_declaration_winning() {
        // A shared base under node_modules names the template everywhere it
        // can; a middle config names it in one key; the leaf overrides one
        // of the base's. Every key is read leaf-first, and only the keys
        // whose EFFECTIVE value names the template are flagged.
        let dir = scratch("decls-template");
        let base_dir = dir.join("node_modules").join("@acme").join("tsconfig");
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::write(
            base_dir.join("base.json"),
            r#"{
                "compilerOptions": {
                    "outDir": "${configDir}/dist",
                    "declarationDir": "types-out",
                    "rootDir": "${configDir}",
                    "tsBuildInfoFile": "${configDir}/.cache/tsbuildinfo",
                    "rootDirs": ["${configDir}/src", "gen"],
                    "typeRoots": ["${configDir}/typings", "more"],
                    "paths": { "@lib/*": ["${configDir}/lib/*"], "@rel/*": ["./rel/*"], "@bare/*": ["bare/*"], "@root/*": ["/rooted/*"], "@odd": 1 },
                    "types": ["${configDir}/typings/mine"]
                },
                "files": ["${configDir}/src/main.ts", "extra.ts"],
                "include": ["${configDir}/src"],
                "exclude": ["${configDir}/src/skipped", "tmp"]
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("tsconfig.middle.json"),
            r#"{ "extends": "@acme/tsconfig/base.json",
                "compilerOptions": { "outDir": "build", "typeRoots": ["mine"] } }"#,
        )
        .unwrap();
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            r#"{ "extends": "./tsconfig.middle.json", "include": ["app"] }"#,
        )
        .unwrap();
        let facts = read_chain(&tsconfig).expect("parses");
        let at = |base: &Path, rest: &str| crate::normalize_path(&base.join(rest));

        assert_eq!(
            facts.files,
            Some(vec![at(&dir, "src/main.ts"), at(&base_dir, "extra.ts")]),
            "the template means the project dir; a plain entry means its own config's"
        );
        assert_eq!(
            facts.include,
            Some(PathList {
                entries: vec![at(&dir, "app")],
                config_dir: false
            }),
            "the leaf's include wins, and it does not name the template"
        );
        assert_eq!(
            facts.exclude,
            Some(PathList {
                entries: vec![at(&dir, "src/skipped"), at(&base_dir, "tmp")],
                config_dir: true
            })
        );
        let option = |key: &str| facts.path_options.get(key).expect(key);
        assert_eq!(
            option("outDir"),
            &PathValue {
                path: at(&dir, "build"),
                config_dir: false
            },
            "the middle config's outDir replaces the base's template one"
        );
        assert_eq!(
            option("declarationDir"),
            &PathValue {
                path: at(&base_dir, "types-out"),
                config_dir: false
            }
        );
        assert_eq!(
            option("rootDir"),
            &PathValue {
                path: crate::normalize_path(&dir),
                config_dir: true
            }
        );
        assert_eq!(
            option("tsBuildInfoFile"),
            &PathValue {
                path: at(&dir, ".cache/tsbuildinfo"),
                config_dir: true
            }
        );
        assert!(!facts.path_options.contains_key("outFile"));
        assert_eq!(
            facts.path_lists.get("rootDirs"),
            Some(&PathList {
                entries: vec![at(&dir, "src"), at(&base_dir, "gen")],
                config_dir: true
            })
        );
        assert_eq!(
            facts.path_lists.get("typeRoots"),
            Some(&PathList {
                entries: vec![at(&dir, "mine")],
                config_dir: false
            }),
            "the middle config's list replaces the base's as a unit"
        );
        let paths = facts.paths.as_ref().expect("paths");
        assert!(paths.config_dir);
        assert_eq!(
            paths.map,
            serde_json::json!({
                "@lib/*": [json_path(&at(&dir, "lib/*"))],
                "@rel/*": [json_path(&at(&base_dir, "rel/*"))],
                "@bare/*": ["bare/*"],
                "@root/*": ["/rooted/*"],
                "@odd": 1,
            })
            .as_object()
            .unwrap()
            .clone(),
            "a relative value resolves against its own config; a bare one (TS5090) and a rooted one pass through"
        );
        assert_eq!(
            facts.types,
            Some(vec!["${configDir}/typings/mine".to_string()]),
            "types is never substituted (measured): kept verbatim, looked up as a package name"
        );
        assert_eq!(
            facts.config_dir_keys(),
            vec!["exclude", "rootDir", "tsBuildInfoFile", "rootDirs", "paths"]
        );
    }

    #[test]
    fn wrapper_restates_exactly_the_keys_that_name_the_template() {
        let dir = scratch("decls-restate");
        let tsconfig = dir.join("tsconfig.json");
        let dts = dir.join("oam.d.ts");
        let value = |rest: &str, config_dir: bool| PathValue {
            path: dir.join(rest),
            config_dir,
        };
        let list = |rest: &[&str], config_dir: bool| PathList {
            entries: rest.iter().map(|rest| dir.join(rest)).collect(),
            config_dir,
        };
        let facts = ChainFacts {
            include: Some(list(&["src"], true)),
            // Declared without the template: inherited, not restated.
            exclude: Some(list(&["src/skipped"], false)),
            path_options: BTreeMap::from([
                ("outDir", value("dist", true)),
                ("rootDir", value("src", false)),
                ("tsBuildInfoFile", value(".cache/tsbuildinfo", true)),
            ]),
            path_lists: BTreeMap::from([
                ("typeRoots", list(&["typings"], true)),
                ("rootDirs", list(&["src", "gen"], false)),
            ]),
            paths: Some(PathsMap {
                map: serde_json::json!({ "@lib/*": [json_path(&dir.join("lib").join("*"))], "@bare/*": ["bare/*"] })
                    .as_object()
                    .unwrap()
                    .clone(),
                config_dir: true,
            }),
            composite: Some(true),
            ..Default::default()
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
        assert_eq!(
            wrapper["include"],
            serde_json::json!([json_path(&dir.join("src"))])
        );
        assert!(
            wrapper.get("exclude").is_none(),
            "declared without the template, so inherited: {wrapper}"
        );
        assert_eq!(
            wrapper["compilerOptions"],
            serde_json::json!({
                "outDir": json_path(&dir.join("dist")),
                "tsBuildInfoFile": json_path(&dir.join(".cache").join("tsbuildinfo")),
                "typeRoots": [json_path(&dir.join("typings"))],
                "paths": { "@lib/*": [json_path(&dir.join("lib").join("*"))], "@bare/*": ["bare/*"] },
            }),
            "rootDir and rootDirs are declared without the template (inherited), \
             and a declared rootDir means no composite default to restate: {wrapper}"
        );
        assert_eq!(wrapper["files"], serde_json::json!([json_path(&dts)]));
        // Every restated path is absolute, and under the project.
        let project = json_path(&dir);
        let mut restated: Vec<String> = Vec::new();
        restated.extend(
            wrapper["include"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string()),
        );
        for key in ["outDir", "tsBuildInfoFile"] {
            restated.push(
                wrapper["compilerOptions"][key]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        restated.push(
            wrapper["compilerOptions"]["typeRoots"][0]
                .as_str()
                .unwrap()
                .to_string(),
        );
        restated.push(
            wrapper["compilerOptions"]["paths"]["@lib/*"][0]
                .as_str()
                .unwrap()
                .to_string(),
        );
        for path in restated {
            assert!(
                path.starts_with(&project) && Path::new(&path).is_absolute(),
                "{path} should be absolute under {project}"
            );
        }

        // The default glob's exclude names the SUBSTITUTED out dirs.
        let facts = ChainFacts {
            path_options: BTreeMap::from([
                ("outDir", value("dist", true)),
                ("declarationDir", value("types-out", true)),
            ]),
            ..Default::default()
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
        let exclude = wrapper["exclude"].as_array().expect("exclude").clone();
        for name in ["dist", "types-out"] {
            assert!(
                exclude.contains(&serde_json::json!(json_path(&dir.join(name)))),
                "{name} missing from {exclude:?}"
            );
        }
        assert_eq!(
            wrapper["compilerOptions"],
            serde_json::json!({
                "outDir": json_path(&dir.join("dist")),
                "declarationDir": json_path(&dir.join("types-out")),
                // An out dir makes tsc check rootDir: its default, restated.
                "rootDir": json_path(&dir),
            })
        );
        // An exclude naming the template is restated even when the include
        // is the default glob; an include naming it is restated instead of
        // the default.
        let facts = ChainFacts {
            exclude: Some(list(&["skip"], true)),
            ..Default::default()
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
        assert_eq!(
            wrapper["include"],
            serde_json::json!([format!("{}/**/*", json_path(&dir))])
        );
        assert_eq!(
            wrapper["exclude"],
            serde_json::json!([json_path(&dir.join("skip"))])
        );
        let facts = ChainFacts {
            include: Some(list(&["${CONFIGDIR}/src"], true)),
            ..Default::default()
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
        assert_eq!(
            wrapper["include"],
            serde_json::json!([json_path(&dir.join("${CONFIGDIR}/src"))]),
            "the literal text of a case variant, under the project dir"
        );
        assert!(wrapper.get("exclude").is_none(), "{wrapper}");
    }

    #[test]
    fn chain_using_config_dir_gets_a_wrapper_rooted_at_the_project() {
        // A shared base config's `${configDir}` would substitute the
        // wrapper's directory: the wrapper restates it, resolved against the
        // user's tsconfig dir, and lands in-tree like any other.
        let dir = scratch("decls-configdir");
        let base_dir = dir.join("node_modules").join("@acme").join("tsconfig");
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::write(
            base_dir.join("base.json"),
            r#"{ "compilerOptions": { "typeRoots": ["${configDir}/typings"] },
                "include": ["${configDir}/src"] }"#,
        )
        .unwrap();
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, r#"{ "extends": "@acme/tsconfig/base.json" }"#).unwrap();
        let wrapper = project_config(&tsconfig).expect("wrapper written");
        assert!(
            wrapper.starts_with(dir.join("node_modules").join(".oam")),
            "{}",
            wrapper.display()
        );
        let json = parsed(&std::fs::read_to_string(&wrapper).unwrap());
        assert_eq!(
            json["include"],
            serde_json::json!([json_path(&crate::normalize_path(&dir.join("src")))])
        );
        assert_eq!(
            json["compilerOptions"]["typeRoots"],
            serde_json::json!([json_path(&crate::normalize_path(&dir.join("typings")))])
        );
        assert!(json.get("exclude").is_none(), "{json}");
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
