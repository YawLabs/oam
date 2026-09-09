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
//!   WRAPPER config in its own cache that `extends` the user's and adds the
//!   declarations through `files`. Everything else -- compilerOptions,
//!   include, exclude, references, paths -- is inherited, and TypeScript
//!   resolves an inherited relative path against the config that DECLARED
//!   it, so the user's project keeps its own meaning of "src/**/*".
//!
//! The one thing inheritance does not survive is the DEFAULT include: a
//! config that declares neither `files` nor `include` means "everything
//! under my directory", and the wrapper declaring `files` suppresses that
//! default for the whole program (probed: the user's own sources vanish from
//! the file list). So when the chain declares neither key, the wrapper
//! restates the default -- the `**/*` glob and the default excludes -- as
//! absolute paths rooted at the user's project. That is the only place this
//! file reimplements tsc's config semantics, and it is why `read_chain`
//! reports which keys the chain declares rather than what they contain.
//!
//! Nothing here is allowed to fail a check: an unwritable cache dir or a
//! tsconfig this module cannot parse returns None, the check runs exactly as
//! it did before, and the user sees tsgo's own diagnostics (including its
//! parse error, if that is what went wrong) rather than one of ours about
//! plumbing.

use std::path::{Path, PathBuf};

use crate::daemon::{cache_root, fnv1a64, project_key, resolve_extends, strip_jsonc};

/// The declarations, compiled in. Not read from disk at runtime: an
/// installed oam is a single binary with no data directory beside it.
const DECLARATIONS: &str = include_str!("../types/oam.d.ts");

/// Everything oam writes for the checker lives here, under oam's cache --
/// never in the user's repo, which may be read-only and is not ours to
/// litter.
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

/// Where the per-project wrapper configs go: a sibling of the .d.ts, so their
/// churn never reaches the fingerprinted listing above.
fn projects_dir() -> PathBuf {
    decls_dir().join("projects")
}

/// Write `contents` to `path` if it is not already exactly that.
///
/// Two checks of the same project can run concurrently (an editor and a
/// terminal, two agents), so the write goes to a pid-unique temp file and is
/// renamed over the target -- an atomic replace on both platforms. A reader
/// therefore sees the old bytes or the new ones, never half a file, which as
/// a tsconfig would be a syntax error attributed to the user's project.
fn write_if_changed(path: &Path, contents: &str) -> Option<bool> {
    if std::fs::read_to_string(path).is_ok_and(|prev| prev == contents) {
        return Some(false);
    }
    std::fs::create_dir_all(path.parent()?).ok()?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, contents).ok()?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Some(true),
        Err(_) => {
            let _ = std::fs::remove_file(&temp);
            // Losing the race to another oam that wrote the same bytes is
            // success, not failure.
            std::fs::read_to_string(path)
                .is_ok_and(|prev| prev == contents)
                .then_some(false)
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
    if write_if_changed(&path, DECLARATIONS)? {
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
    let mut facts = ChainFacts {
        files: None,
        declares_include: false,
        declares_exclude: false,
        out_dirs: Vec::new(),
    };
    let mut seen: Vec<PathBuf> = Vec::new();
    visit(tsconfig, &mut facts, &mut seen)?;
    Some(facts)
}

fn visit(config: &Path, facts: &mut ChainFacts, seen: &mut Vec<PathBuf>) -> Option<()> {
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

/// The wrapper config's text: `extends` the user's, plus the declarations as
/// one more root file.
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
/// that adds oam's declarations to the user's project.
///
/// None = run the user's tsconfig directly (the pre-existing behaviour, with
/// TS2307 on `oam:` imports). Never an error: a checker that refuses to run
/// because it could not attach its own types is worse than one that runs
/// without them.
pub(crate) fn project_config(tsconfig: &Path) -> Option<PathBuf> {
    let declarations = declarations_file()?;
    let facts = read_chain(tsconfig)?;
    let path = projects_dir().join(format!("project-{}.json", project_key(tsconfig)));
    let contents = wrapper_json(tsconfig, &declarations, &facts);
    write_if_changed(&path, &contents).map(|_| path)
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
            files: None,
            declares_include: true,
            declares_exclude: false,
            out_dirs: Vec::new(),
        };
        let wrapper = parsed(&wrapper_json(&tsconfig, &dts, &facts));
        assert_eq!(wrapper["extends"], json_path(&tsconfig));
        assert_eq!(wrapper["files"], serde_json::json!([json_path(&dts)]));
        // The chain has an include: it survives inheritance untouched, and
        // restating it here would override the user's own file set.
        assert!(wrapper.get("include").is_none(), "{wrapper}");
        assert!(wrapper.get("exclude").is_none(), "{wrapper}");
    }

    #[test]
    fn wrapper_restates_the_default_glob_when_the_chain_declares_neither_key() {
        let dir = scratch("decls-default");
        let tsconfig = dir.join("tsconfig.json");
        let dts = dir.join("oam.d.ts");
        let facts = ChainFacts {
            files: None,
            declares_include: false,
            declares_exclude: false,
            out_dirs: vec![dir.join("dist")],
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
            declares_include: false,
            declares_exclude: true,
            out_dirs: Vec::new(),
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
        std::fs::write(
            dir.join("base.json"),
            r#"{ "compilerOptions": { "outDir": "out" }, "files": ["base.ts"], "exclude": ["x"] }"#,
        )
        .unwrap();
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            r#"{
                // JSONC, like a real one
                "extends": "./base.json",
                "files": ["leaf.ts"],
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
            vec![crate::normalize_path(&dir.join("out"))],
            "outDir resolves against the config that declared it"
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
