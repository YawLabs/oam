//! Worker thread lifecycle: spawn, execute, teardown.
//!
//! Each worker gets its own OS thread, V8 isolate (from the shared
//! snapshot), and CoreRuntime. Communication with the parent is via
//! std::sync::mpsc channels bridged into the op channel.

use oam_core::worker::{WorkerContext, WorkerEvent};
use oam_diagnostics::Diagnostic;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

impl super::JsRuntime {
    /// Execute a worker thread entry as the kind oam's loader gives its file
    /// (oam.fork()'s in-process pool; a `new Worker` thread finds its entry
    /// with [`Self::worker_entry`] first). Called on the WORKER thread, not
    /// the parent.
    pub(crate) fn execute_worker(
        &mut self,
        entry: &Path,
        ctx: WorkerContext,
    ) -> Result<(), Vec<Diagnostic>> {
        // An ESM entry has to go through the module graph. load_cjs compiles
        // it as a classic script, so `new Worker('x.mjs')` threw on its first
        // import statement -- the ESM guard lives in the require() callback,
        // which an entry never passes through. Decided as the CLI routes a
        // program's entry: on the file's real path, a typeless .js by its
        // syntax.
        let kind = self.entry_module_kind(entry);
        self.execute_worker_as(entry, kind, ctx)
    }

    /// The file a `new Worker(filename)` thread runs, and as which kind, by
    /// the rules node's worker applies to its entry (#287): it runs it as
    /// the main module of the thread (`Module.runMain`), so `filename` is
    /// found as node finds a program's entry -- the file itself, then with
    /// `.js`, `.json` or `.node` added, then as a directory (package.json
    /// `main`, index.js) -- and is looked for only once the thread runs. A
    /// missing entry is therefore no throw from the constructor: the Worker
    /// starts and reports `MODULE_NOT_FOUND` as its 'error' event, exiting 1.
    ///
    /// An extension neither of oam's loaders knows (`x.txt`, and `x.js.`,
    /// whose extension node reads as `.`) is node's CommonJS loader's: it
    /// runs such a file as a .js file -- unless the package is
    /// `"type": "module"` or the source has module syntax, when it goes to
    /// the ES module loader, which refuses the extension with
    /// `ERR_UNKNOWN_FILE_EXTENSION`. oam's ES module host refused every such
    /// file as an "unsupported module type", so `x.js.` (which #275 had just
    /// made findable) failed where node runs it.
    pub(crate) fn worker_entry(
        &mut self,
        filename: &Path,
    ) -> Result<(PathBuf, oam_loader::ModuleKind), oam_core::worker::WorkerError> {
        use oam_core::worker::WorkerError;
        use oam_loader::ModuleKind;
        let request = filename.to_string_lossy();
        // `filename` is absolute (the constructor resolved it), so the
        // referrer is never read.
        let resolved = oam_loader::resolve_require(&request, filename);
        directory_package_check(filename, resolved.as_ref().ok())?;
        let Ok(found) = resolved else {
            return Err(WorkerError {
                name: "Error".to_string(),
                message: format!("Cannot find module '{request}'"),
                props: vec![
                    ("code".to_string(), "MODULE_NOT_FOUND".into()),
                    (
                        "requireStack".to_string(),
                        serde_json::Value::Array(Vec::new()),
                    ),
                ],
            });
        };
        // node's runMain decides the loader on the entry's realpath, and names
        // it in ERR_UNKNOWN_FILE_EXTENSION. resolve_require realpaths most
        // finds but leaves a trailing-dot name (`w.js.`) as spelled, so one
        // reached through a junction or symlink was named by its link path
        // (#287). module_key canonicalizes through the namespaced form, which
        // keeps the trailing dot.
        let real = crate::modules::module_key(&found).unwrap_or_else(|_| found.clone());
        let extension = node_extname(&real);
        let loader_knows = matches!(extension, ".js" | ".mjs" | ".cjs" | ".json" | ".node")
            || oam_loader::is_transpiled_source(&real);
        if loader_knows {
            // A typeless .js by its syntax, as node's runMain decides it.
            return Ok((found, self.entry_module_kind(&real)));
        }
        let type_module = oam_loader::node_declared_module_kind(&real) == Some(ModuleKind::Esm);
        let module_syntax = !type_module && {
            let source = std::fs::read_to_string(oam_loader::fs_os_path(&real)).unwrap_or_default();
            v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
            crate::cjs::contains_module_syntax(scope, &source, &real.to_string_lossy())
        };
        if !type_module && !module_syntax {
            return Ok((found, ModuleKind::Cjs));
        }
        if extension.is_empty() {
            // node runs an extensionless module (`bin`, `.hidden`) as an ES
            // module; oam's ES module host does not load one yet, so it
            // keeps the kind oam gives it and fails there.
            return Ok((found, oam_loader::module_kind(&real)));
        }
        Err(WorkerError {
            name: "TypeError".to_string(),
            message: format!(
                "Unknown file extension \"{extension}\" for {}",
                real.display()
            ),
            props: vec![("code".to_string(), "ERR_UNKNOWN_FILE_EXTENSION".into())],
        })
    }

    /// Run a worker's entry `entry` as `kind`. Sets up the worker context
    /// (parentPort, isMainThread=false, workerData) before loading it.
    pub(crate) fn execute_worker_as(
        &mut self,
        entry: &Path,
        kind: oam_loader::ModuleKind,
        ctx: WorkerContext,
    ) -> Result<(), Vec<Diagnostic>> {
        if kind == oam_loader::ModuleKind::Esm {
            let host = crate::worker_host().ok_or_else(|| {
                vec![Diagnostic::new(
                    "OAM-MOD0003",
                    oam_diagnostics::Severity::Error,
                    oam_diagnostics::Origin::Resolve,
                    format!(
                        "no module host registered for ESM worker entries: {}",
                        entry.display()
                    ),
                )]
            })?;
            self.reset_run_slots()?;
            self.isolate.set_slot(ctx);
            return self.execute_module(entry, host.as_ref());
        }
        self.reset_run_slots()?;
        self.isolate.set_slot(ctx);
        // A worker's CommonJS entry is the main module of its thread, as in
        // node, which runs the `require.main === module` guard there. The
        // same path runs the scripts of oam.fork()'s in-process pool.
        // (child_process.fork() is not this path: it starts a separate
        // `oam run` process, whose entry execute_cjs names.)
        crate::cjs::name_main_entry(&mut self.isolate, entry);

        v8::scope_with_context!(let scope, &mut self.isolate, &self.context);
        v8::tc_scope!(let tc, scope);

        if crate::cjs::load_cjs(tc, entry).is_none() {
            return Err(vec![crate::modules::catch_to_diagnostic(
                tc,
                &entry.to_string_lossy(),
            )]);
        }
        if let Some(failure) = crate::modules::run_ticks_and_microtasks(tc) {
            return Err(failure);
        }
        if let Some(failure) = crate::modules::drain_uncaught(tc) {
            return Err(failure);
        }
        crate::modules::pump_event_loop(tc, None, true)?;
        match crate::modules::unhandled_rejection_failures(tc) {
            Some(failures) => Err(failures),
            None => Ok(()),
        }
    }
}

/// node's `tryPackage` for an entry found as a directory (#287): when neither
/// `filename` nor `filename` with an extension is a file, node reads the
/// directory's package.json before looking for its `main` or index, and
///
/// * a package.json that is not a JSON object fails the entry with
///   `ERR_INVALID_PACKAGE_CONFIG`, even when an index.js is there;
/// * a non-empty string `main` that resolves to nothing, with no index to
///   fall back on, fails `MODULE_NOT_FOUND` naming `main`'s path and asking
///   to "verify that the package.json has a valid \"main\" entry", with own
///   props `path` (the package.json, namespaced as node's reader has it) and
///   `requestPath` -- not the plain "Cannot find module '<dir>'" with
///   `requireStack` a missing directory gives.
///
/// `found` is what resolve_require found; the package was consulted only
/// when that is inside the directory. node's DEP0128 warning, for a bad
/// `main` that falls back to the index, is not emitted (oam's require()
/// does not emit it either; docs/node-divergences.md).
fn directory_package_check(
    filename: &Path,
    found: Option<&PathBuf>,
) -> Result<(), oam_core::worker::WorkerError> {
    use oam_core::worker::WorkerError;
    let is_dir = std::fs::metadata(oam_loader::fs_os_path(filename)).is_ok_and(|m| m.is_dir());
    if !is_dir {
        return Ok(());
    }
    // node tries the path as a file, then with each extension, and only then
    // as a directory: a sibling file it takes first means the package was
    // never read. Decided on the unresolved names, not on where `found`
    // resolves to -- an index.js that is a link out of the directory is
    // still the directory's.
    if found.is_some() {
        let as_file =
            |p: &Path| std::fs::metadata(oam_loader::fs_os_path(p)).is_ok_and(|m| m.is_file());
        let with_ext = filename.as_os_str().to_owned();
        let taken_as_file = [".js", ".json", ".node"].iter().any(|ext| {
            let mut candidate = with_ext.clone();
            candidate.push(ext);
            as_file(Path::new(&candidate))
        });
        if taken_as_file {
            return Ok(());
        }
    }
    let manifest = filename.join("package.json");
    let Ok(bytes) = std::fs::read(oam_loader::fs_os_path(&manifest)) else {
        return Ok(());
    };
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let shown_manifest = oam_loader::fs_os_path(&manifest).display().to_string();
    let Ok(serde_json::Value::Object(package)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        // node v22.22.2, measured: invalid JSON, `null`, `5`, `[]`, a string
        // and an empty file all fail so.
        return Err(WorkerError {
            name: "Error".to_string(),
            message: format!("Invalid package config {shown_manifest}."),
            props: vec![("code".to_string(), "ERR_INVALID_PACKAGE_CONFIG".into())],
        });
    };
    if found.is_some() {
        return Ok(());
    }
    // A `main` that is not a non-empty string is ignored (`5`, `""`): the
    // directory is then looked up by its index alone.
    let Some(main) = package
        .get("main")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
    else {
        return Ok(());
    };
    // path.resolve(requestPath, main): an absolute `main` replaces the
    // directory, and `.`/`..` and a trailing separator are dropped.
    let mut main_path = PathBuf::new();
    for component in filename.join(main).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                main_path.pop();
            }
            other => main_path.push(other.as_os_str()),
        }
    }
    Err(WorkerError {
        name: "Error".to_string(),
        message: format!(
            "Cannot find module '{}'. Please verify that the package.json has a valid \"main\" entry",
            main_path.display()
        ),
        props: vec![
            ("code".to_string(), "MODULE_NOT_FOUND".into()),
            ("path".to_string(), shown_manifest.into()),
            (
                "requestPath".to_string(),
                filename.to_string_lossy().into_owned().into(),
            ),
        ],
    })
}

/// node's `path.extname` of `path`'s file name: from its last dot, unless
/// that dot leads the name (`.hidden` has none) or the name is `..`. Unlike
/// `Path::extension` it keeps the dot, so `x.js.` gives `.` and `noext` an
/// empty string -- the two node tells apart (#287).
fn node_extname(path: &Path) -> &str {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return "";
    };
    match name.rfind('.') {
        Some(0) | None => "",
        Some(_) if name == ".." => "",
        Some(dot) => &name[dot..],
    }
}

/// The environment a worker thread's process.env starts as: node's Worker
/// `env` option, resolved by the JS Worker constructor (workerEnvOption).
/// Set as an isolate slot before the runtime globals are installed; op_env
/// reads it in place of the process environment.
pub(crate) struct WorkerEnv {
    /// The worker's own pairs (a copy of its creator's process.env, or the
    /// given object). Empty when `shared` is set.
    pub pairs: Vec<(String, String)>,
    /// SHARE_ENV: the id of the store shared with the creator.
    pub shared: Option<u64>,
    /// Whether names fold case (Windows, a store that leads back to the main
    /// thread's environment).
    pub fold: bool,
    /// The values libuv's Windows additions to a child's environment are
    /// read from: the creator's, when the worker was made.
    pub base: Vec<(String, String)>,
}

impl WorkerEnv {
    /// From the `env` member of workerNew's options JSON; `None` when it is
    /// absent or malformed.
    pub(crate) fn from_json(value: &serde_json::Value) -> Option<Self> {
        let pairs = |key: &str| -> Option<Vec<(String, String)>> {
            match value.get(key) {
                None | Some(serde_json::Value::Null) => Some(Vec::new()),
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        let pair = item.as_array()?;
                        match pair.as_slice() {
                            [name, value] => {
                                Some((name.as_str()?.to_string(), value.as_str()?.to_string()))
                            }
                            _ => None,
                        }
                    })
                    .collect(),
                Some(_) => None,
            }
        };
        let shared = match value.get("shared") {
            None | Some(serde_json::Value::Null) => None,
            Some(id) => Some(id.as_u64()?),
        };
        Some(Self {
            pairs: pairs("pairs")?,
            shared,
            fold: value.get("fold").and_then(serde_json::Value::as_bool) == Some(true),
            base: pairs("base")?,
        })
    }
}

/// An environment store shared between threads through SHARE_ENV. The
/// version moves on every write, so a thread re-reads the pairs only when
/// another has changed them.
struct SharedEnv {
    version: u64,
    pairs: Vec<(String, String)>,
}

static SHARED_ENVS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<u64, SharedEnv>>,
> = std::sync::LazyLock::new(Default::default);
static NEXT_SHARED_ENV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn shared_envs() -> std::sync::MutexGuard<'static, std::collections::HashMap<u64, SharedEnv>> {
    SHARED_ENVS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A new shared store holding `pairs`; returns its id. A store lives as long
/// as the process: a thread shares its environment at most once, and
/// every thread that shares it keeps using the same one.
pub(crate) fn shared_env_new(pairs: Vec<(String, String)>) -> u64 {
    let id = NEXT_SHARED_ENV.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    shared_envs().insert(id, SharedEnv { version: 0, pairs });
    id
}

/// The store's version, or `None` for an unknown id.
pub(crate) fn shared_env_version(id: u64) -> Option<u64> {
    shared_envs().get(&id).map(|env| env.version)
}

/// The store's version and pairs, read together.
pub(crate) fn shared_env_read(id: u64) -> Option<(u64, Vec<(String, String)>)> {
    shared_envs()
        .get(&id)
        .map(|env| (env.version, env.pairs.clone()))
}

/// Set `name` (exactly as spelled: the caller has already folded case) to
/// `value`, or remove it with `None`. Returns the new version.
pub(crate) fn shared_env_set(id: u64, name: &str, value: Option<String>) -> Option<u64> {
    let mut envs = shared_envs();
    let env = envs.get_mut(&id)?;
    let at = env.pairs.iter().position(|(key, _)| key == name);
    match (at, value) {
        (Some(at), Some(value)) => env.pairs[at].1 = value,
        (None, Some(value)) => env.pairs.push((name.to_string(), value)),
        (Some(at), None) => {
            env.pairs.remove(at);
        }
        (None, None) => {}
    }
    env.version += 1;
    Some(env.version)
}

/// The stack every thread that runs a non-main isolate is spawned with (a
/// worker, a fork's prewarmed or cold-started isolate): 4 MiB, node's own
/// worker thread size (`kStackSize` in src/node_worker.cc, the default of
/// `resourceLimits.stackSizeMb`).
///
/// Rust's default for a spawned thread is 2 MiB. V8 puts its JS stack limit
/// ~984 KiB below where the isolate starts and checks only JS frames against
/// it, so the native frames of an op called at that limit get whatever lies
/// below. 4 MiB leaves ~3 MiB, so deep recursion through a sync op on a
/// worker ends in V8's RangeError and never in a native stack overflow that
/// kills the process (#395, the main thread's side is oam_cli/build.rs).
///
/// It is a reservation: pages are committed as the stack grows into them.
/// node additionally moves the worker's V8 limit down to 192 KiB above the
/// stack's end, giving worker JS ~3.8 MiB to recurse in where oam's keeps
/// V8's default; that depth difference is recorded in
/// docs/node-divergences.md.
pub(crate) const ISOLATE_THREAD_STACK_SIZE: usize = 4 * 1024 * 1024;

/// Spawn a worker thread. Returns (worker_id, thread_id). The caller
/// must store the returned handles in the parent's WorkerRegistry.
pub(crate) struct WorkerOptions {
    pub pipe_stdout: bool,
    pub pipe_stderr: bool,
    pub exec_argv: Vec<String>,
    /// `None` only for a caller that passed no `env` (the worker then reads
    /// the process environment, as before).
    pub env: Option<WorkerEnv>,
}

pub(crate) fn spawn_worker(
    script_path: PathBuf,
    worker_data: Option<String>,
    worker_id: u64,
    parent_rx: mpsc::Receiver<Vec<u8>>,
    worker_tx: mpsc::Sender<WorkerEvent>,
    options: WorkerOptions,
    permissions: std::sync::Arc<crate::permissions::Permissions>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("oam-worker-{worker_id}"))
        .stack_size(ISOLATE_THREAD_STACK_SIZE)
        .spawn(move || {
            let exit_code = run_worker(
                &script_path,
                worker_data,
                worker_id,
                parent_rx,
                worker_tx.clone(),
                options,
                permissions,
            );
            let _ = worker_tx.send(WorkerEvent::Exit(exit_code));
        })
        .expect("worker thread spawns")
}

fn run_worker(
    script_path: &Path,
    worker_data: Option<String>,
    thread_id: u64,
    from_parent: mpsc::Receiver<Vec<u8>>,
    to_parent: mpsc::Sender<WorkerEvent>,
    options: WorkerOptions,
    permissions: std::sync::Arc<crate::permissions::Permissions>,
) -> i32 {
    let WorkerOptions {
        pipe_stdout,
        pipe_stderr,
        exec_argv,
        env,
    } = options;
    // Worker isolate: no fork pool (same recursion hazard as the prewarm
    // path -- a worker that installed a pool would spawn prewarm threads that
    // each spawn more pools). A nested oam.fork() inside a worker cold-spawns
    // via op_fork_spawn's None branch.
    // INHERIT the spawning isolate's permissions: a worker used to run
    // all-granted, which made `new Worker(...)` a one-line escape from
    // --permission.
    let mut rt = super::JsRuntime::new_worker_thread_runtime(permissions, env);
    rt.set_process_argv(vec![
        "oam".to_string(),
        script_path.to_string_lossy().into_owned(),
    ]);

    let ctx = WorkerContext {
        inbox: std::sync::Arc::new(std::sync::Mutex::new(Some(from_parent))),
        outbox: to_parent.clone(),
        thread_id,
        worker_data,
        pipe_stdout,
        pipe_stderr,
        exec_argv: exec_argv.clone(),
    };

    if !exec_argv.is_empty() {
        let json = serde_json::to_string(&exec_argv).unwrap_or_else(|_| "[]".into());
        let _ = rt.execute_script(
            "<worker-exec-argv>",
            &format!("globalThis.process.execArgv = {json};"),
        );
    }
    // Inherit the parent's process-level flags (--no-warnings etc.); a
    // fresh isolate would otherwise start with none of them.
    if let Some(js) = crate::inherited_flags_js() {
        let _ = rt.execute_script("<flags>", js);
    }
    // The worker's own `execArgv: ["--no-warnings"]`: its process has no
    // default 'warning' listener, as node's has none. (The other flags a
    // worker may be given are not applied per worker.)
    if exec_argv.iter().any(|arg| arg == "--no-warnings") {
        let _ = rt.execute_script(
            "<worker-flags>",
            "for (const f of globalThis.process.listeners('warning')) \
               if (f.name === 'onWarning') globalThis.process.removeListener('warning', f);",
        );
    }
    // The entry is looked for on the worker's own thread, as node's is
    // (#287): its failure is the thread's error, handed to the parent whole,
    // and the thread exits 1.
    let (entry, kind) = match rt.worker_entry(script_path) {
        Ok(entry) => entry,
        Err(error) => {
            let _ = to_parent.send(WorkerEvent::Error(error));
            return 1;
        }
    };
    if let Err(diagnostics) = rt.execute_worker_as(&entry, kind, ctx) {
        for d in &diagnostics {
            eprintln!("worker {thread_id}: {}", d.message);
        }
        let _ = to_parent.send(WorkerEvent::Error(oam_core::worker::WorkerError::message(
            diagnostics
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| "unknown worker error".to_string()),
        )));
        return 1;
    }
    // Node emits 'exit' on the worker's own process object before the thread
    // ends, and re-reads exitCode after (a handler may set it).
    rt.emit_process_exit();
    rt.process_exit_code().unwrap_or(0)
}

#[cfg(test)]
mod entry_tests {
    use super::*;

    #[test]
    fn node_extname_keeps_the_dot_and_skips_a_leading_one() {
        // node v22.22.2's path.extname, measured.
        for (name, ext) in [
            ("w.js", ".js"),
            ("w.js.", "."),
            ("x.txt", ".txt"),
            ("noext", ""),
            (".hidden", ""),
            ("..x", ".x"),
            ("...", "."),
            ("a.b.c", ".c"),
        ] {
            assert_eq!(node_extname(&Path::new("dir").join(name)), ext, "{name}");
        }
        assert_eq!(node_extname(Path::new("dir/..")), "");
    }
}

#[cfg(test)]
mod env_tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn worker_env_reads_the_constructor_json() {
        let json = serde_json::json!({
            "pairs": [["B", "2"], ["A", "1"]],
            "fold": false,
            "base": [["PATH", "p"]],
        });
        let env = WorkerEnv::from_json(&json).expect("well-formed");
        assert_eq!(env.pairs, pairs(&[("B", "2"), ("A", "1")]), "order kept");
        assert_eq!(env.shared, None);
        assert!(!env.fold);
        assert_eq!(env.base, pairs(&[("PATH", "p")]));

        let shared = serde_json::json!({ "shared": 7, "fold": true, "base": [] });
        let env = WorkerEnv::from_json(&shared).expect("well-formed");
        assert_eq!(env.shared, Some(7));
        assert!(env.fold);
        assert!(env.pairs.is_empty());

        for bad in [
            serde_json::json!({ "pairs": [["A"]] }),
            serde_json::json!({ "pairs": [["A", 1]] }),
            serde_json::json!({ "pairs": "A=1" }),
            serde_json::json!({ "shared": "7" }),
        ] {
            assert!(WorkerEnv::from_json(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_shared_env_store_versions_every_write() {
        let id = shared_env_new(pairs(&[("A", "1")]));
        assert_eq!(shared_env_version(id), Some(0));
        assert_eq!(shared_env_set(id, "B", Some("2".into())), Some(1));
        assert_eq!(shared_env_set(id, "A", Some("one".into())), Some(2));
        assert_eq!(shared_env_set(id, "a", None), Some(3), "names are exact");
        assert_eq!(
            shared_env_read(id),
            Some((3, pairs(&[("A", "one"), ("B", "2")])))
        );
        assert_eq!(shared_env_set(id, "A", None), Some(4));
        assert_eq!(shared_env_read(id), Some((4, pairs(&[("B", "2")]))));
        assert_eq!(shared_env_version(u64::MAX), None);
        assert_eq!(shared_env_set(u64::MAX, "A", None), None);
    }
}
