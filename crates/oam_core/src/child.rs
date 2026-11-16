use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use super::OpOutcome;

pub struct ChildProcess {
    /// The live child. Taken out of the registry entry by `child_wait` for the
    /// duration of the await, but the entry itself stays so a concurrent
    /// `child_kill` can still reach the kill-notifier below.
    ///
    /// stdin/stdout/stderr are stored as separate fields so that `child_wait`
    /// taking `child` (which has all three set to None) does not race with
    /// concurrent read/write ops. Without this separation a Windows Tokio
    /// scheduler that runs `spawnWait` before `spawnWrite` / `spawnReadStdout`
    /// causes those ops to see `child` as None and silently fail.
    pub child: Option<tokio::process::Child>,
    pub stdin: Option<tokio::process::ChildStdin>,
    pub stdout: Option<tokio::process::ChildStdout>,
    pub stderr: Option<tokio::process::ChildStderr>,
    /// Set while a read holds the stdout pipe out of the registry, so a
    /// concurrent second read sees the stream as busy rather than at EOF.
    pub stdout_busy: bool,
    /// Set while a read holds the stderr pipe out of the registry, so a
    /// concurrent second read sees the stream as busy rather than at EOF.
    pub stderr_busy: bool,
    /// Woken each time a read lets go of the stdout pipe (or finds it gone),
    /// so a second read waiting on `stdout_busy` parks instead of spinning.
    pub stdout_released: Arc<Notify>,
    /// As `stdout_released`, for stderr.
    pub stderr_released: Arc<Notify>,
    /// True once `child_wait` has collected the exit. The entry outlives the
    /// exit while a read pipe (or a read in flight) remains, so the readers
    /// can drain what the child wrote just before it exited; `pid` no longer
    /// names this child from then on and must not be signalled.
    pub exited: bool,
    pub pid: u32,
    /// Wakes the parked `child_wait` future, which owns the `Child` and is the
    /// only place that can signal it. Lets kill survive `child` being taken.
    pub kill: Arc<Notify>,
    /// Signal name the caller requested for the kill (Node reports it on exit).
    pub kill_signal: Option<String>,
}

impl ChildProcess {
    pub fn new(mut child: tokio::process::Child, pid: u32) -> Self {
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        ChildProcess {
            child: Some(child),
            stdin,
            stdout,
            stderr,
            stdout_busy: false,
            stderr_busy: false,
            stdout_released: Arc::new(Notify::new()),
            stderr_released: Arc::new(Notify::new()),
            exited: false,
            pid,
            kill: Arc::new(Notify::new()),
            kill_signal: None,
        }
    }
}

pub type ChildRegistry = Arc<Mutex<HashMap<u64, ChildProcess>>>;

/// Per-fd disposition for a child's stdin/stdout/stderr, mirroring node's
/// `stdio` option.
///
/// `Inherit` is a real OS-level handle hand-off, not a pump: the child writes
/// straight to the parent's fd with no copy through this process, which is both
/// node's semantics and the only way a grandchild's stdio survives an
/// intermediate launcher (npm `bin` shims do exactly this).
///
/// `File` is the same idea for a slot the caller named by NUMBER -- the
/// daemonize-into-a-logfile shape, `stdio: ['ignore', logFd, logFd]`. Those
/// used to collapse to `Inherit`, so the output went to the parent's console
/// instead of the file the caller asked for; the engine now resolves the fd
/// against oam's descriptor registry and hands the child a dup.
///
/// NOT `Copy`: the `File` variant owns a descriptor.
#[derive(Debug, Default)]
pub enum StdioMode {
    /// `/dev/null` (NUL on Windows). Reads see EOF immediately; writes vanish.
    Ignore,
    /// The child gets the parent's own fd for this slot.
    Inherit,
    /// An anonymous pipe this process owns both ends of. Node's default.
    #[default]
    Pipe,
    /// A descriptor resolved from a numeric stdio slot. Already a dup, so
    /// handing it to the child does not disturb the caller's own fd.
    File(std::fs::File),
}

impl StdioMode {
    /// Decode the disposition wire form: 0=ignore, 1=inherit, anything else
    /// =pipe. A numbered slot never arrives here -- the engine resolves it to
    /// `File` before building the spec, because only it can reach the registry.
    pub fn from_code(code: u8) -> Self {
        match code {
            0 => StdioMode::Ignore,
            1 => StdioMode::Inherit,
            _ => StdioMode::Pipe,
        }
    }

    fn into_stdio(self) -> std::process::Stdio {
        match self {
            StdioMode::Ignore => std::process::Stdio::null(),
            StdioMode::Inherit => std::process::Stdio::inherit(),
            StdioMode::Pipe => std::process::Stdio::piped(),
            StdioMode::File(file) => std::process::Stdio::from(file),
        }
    }

    fn is_pipe(&self) -> bool {
        matches!(self, StdioMode::Pipe)
    }
}

/// stdin/stdout/stderr dispositions, in fd order.
pub type StdioSpec = [StdioMode; 3];

/// Node's default when `stdio` is absent: all three piped.
pub fn stdio_pipe_all() -> StdioSpec {
    [StdioMode::Pipe, StdioMode::Pipe, StdioMode::Pipe]
}

/// Spawn a child and return it with its pid. SYNCHRONOUS: node's uv_spawn is
/// synchronous, so `child.pid` must be readable the instant `spawn()` returns
/// (execa, tree-kill and pidusage all read it immediately). There are no
/// awaits in this body -- `tokio::process::Command::spawn` is itself a sync
/// call -- but it MUST run inside a tokio runtime context, because on Unix it
/// registers the child with the signal-driver reactor. Callers on the V8
/// thread must hold a `Handle::enter()` guard; see `CoreRuntime::enter`.
///
/// `env`, when given, is the child's WHOLE environment: nothing of this
/// process's own is added, as node hands libuv its envPairs and libuv hands
/// them to execve / CreateProcessW. The JS layer builds the pairs (process.env
/// when the script passed none, and libuv's Windows additions). `None` leaves
/// the child this process's environment, for callers that are not node APIs.
///
/// `detached` is node's option: on Windows it adds libuv's
/// `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` and keeps the child OUT of
/// the kill-on-close job (job_win.rs) every other child is placed in. It is
/// not yet honored on POSIX, where node would also `setsid()` the child.
///
/// `windows_hide` is node's `windowsHide`, applied as libuv's `uv_spawn`
/// applies it: it decides `CREATE_NO_WINDOW` (see `windows_creation_flags`),
/// and every child starts under `STARTF_USESHOWWINDOW` with `SW_HIDE` for it,
/// else `SW_SHOWDEFAULT` (`child_win::show_window_for`), so a GUI child's
/// first window opens hidden. Ignored on POSIX, as under node.
#[allow(clippy::too_many_arguments)]
pub fn spawn_child(
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Vec<(String, String)>>,
    shell: bool,
    stdio: StdioSpec,
    detached: bool,
    windows_hide: bool,
    argv0: Option<String>,
) -> Result<(tokio::process::Child, u32), String> {
    let (prog, final_args) = if shell {
        shell_command(&command, &args)
    } else {
        (command, args)
    };

    #[cfg(windows)]
    let prog = if shell {
        prog
    } else {
        match resolve_windows_program(&prog, cwd.as_deref()) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => prog,
            Err(e) => return Err(spawn_failure_json(&e)),
        }
    };

    let mut cmd = tokio::process::Command::new(&prog);
    #[cfg(windows)]
    {
        if shell {
            // Verbatim: hand cmd.exe the string as written (node's
            // windowsVerbatimArguments). tokio's Command has raw_arg
            // natively; the sync path below needs the std CommandExt trait.
            for a in &final_args {
                cmd.raw_arg(a);
            }
        } else {
            cmd.args(&final_args);
        }
    }
    #[cfg(not(windows))]
    cmd.args(&final_args);
    // Read before the spec is consumed below.
    #[cfg(windows)]
    let inherits = stdio_inherits(&stdio);
    // Destructured, not indexed: a slot can now OWN a descriptor, so the spec
    // is consumed rather than copied.
    let [slot0, slot1, slot2] = stdio;
    cmd.stdin(slot0.into_stdio());
    cmd.stdout(slot1.into_stdio());
    cmd.stderr(slot2.into_stdio());

    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = env {
        // The child's whole environment (see spawn_child).
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_creation_flags(detached, windows_hide, inherits));
        // tokio's Command has no show_window of its own; it is std's, on the
        // std Command tokio wraps.
        cmd.as_std_mut()
            .show_window(super::child_win::show_window_for(windows_hide));
        // argv0 on Windows requires direct CreateProcessW (lpCommandLine). The
        // tokio/std Command wrapper doesn't expose it; skip for spawn_child.
        let _ = argv0;
    }
    #[cfg(not(windows))]
    {
        if let Some(ref argv0) = argv0 {
            // tokio's Command::arg0 is inherent on Unix (no CommandExt needed).
            cmd.arg0(argv0);
        }
        let _ = (detached, windows_hide);
    }

    let child = cmd.spawn().map_err(|e| spawn_failure_json(&e))?;
    // Right after spawn, not suspended: tokio's Child exposes no thread handle
    // to resume a suspended child with. job_win.rs explains why that window is
    // libuv's own.
    #[cfg(windows)]
    if !detached {
        super::job_win::adopt_tokio_child(&child);
    }
    let pid = child.id().unwrap_or(0);
    Ok((child, pid))
}

/// The program and arguments a `shell: true` spawn runs: the platform shell
/// with `command` and `args` joined by spaces as its one command string. One
/// builder for the async and the sync spawn, so the two cannot drift.
///
/// Windows: cmd.exe parses its own quoting; Rust's default arg escaping
/// (MSVCRT rules) mangles a command that embeds quotes, so the canonical
/// `exec('"C:\path with spaces\app.exe" arg')` fails. Node passes
/// `/d /s /c "<command>"` with verbatim arguments -- /d skips AutoRun, /s +
/// the wrapping quotes make cmd strip exactly the outer pair. The callers
/// apply the arguments with `raw_arg`.
///
/// `pub` for the raw-create spawns (extra-fd stdio, argv0), which the engine
/// rewrites with it before `child_extra::spawn_extra`: on Windows those must
/// then take the verbatim form (`spawn_extra_verbatim`), for the same reason.
pub fn shell_command(command: &str, args: &[String]) -> (String, Vec<String>) {
    let mut full = command.to_string();
    for a in args {
        full.push(' ');
        full.push_str(a);
    }
    #[cfg(windows)]
    {
        (
            "cmd.exe".to_string(),
            vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                format!("\"{full}\""),
            ],
        )
    }
    #[cfg(not(windows))]
    {
        ("/bin/sh".to_string(), vec!["-c".to_string(), full])
    }
}

/// The JSON body a failed async spawn hands the JS layer: the node code, the
/// errno (null when there is none), and the OS message.
///
/// errno rides along: node emits it as the `code` argument of the 'close'
/// event for a child that never started, so the JS layer cannot reproduce
/// node's failure shape without it.
pub(crate) fn spawn_failure_json(e: &std::io::Error) -> String {
    spawn_failure_json_with(e, &e.to_string())
}

/// `spawn_failure_json` with a message of the caller's own.
///
/// `winError`, the Win32 code behind an `UNKNOWN` (`unknown_win32_error`,
/// #450), rides along only when there is one -- a body without it is what an
/// older oam wrote -- and the JS builders define it on the error
/// non-enumerably, as the sync natives do (#442).
pub(crate) fn spawn_failure_json_with(e: &std::io::Error, message: &str) -> String {
    let code = super::node_error_code(e);
    let mut body = serde_json::json!({
        "code": code,
        "errno": super::node_errno(code, e),
        "message": message,
    });
    if let Some(win32) = super::unknown_win32_error(code, e)
        && let Some(fields) = body.as_object_mut()
    {
        fields.insert("winError".to_string(), serde_json::Value::from(win32));
    }
    body.to_string()
}

/// What spawning `prog` must run on Windows: libuv's program search
/// (`search_path` / `path_search_walk_ext` in src/win/process.c) for a name
/// with a path in it.
///
/// node resolves the program itself, before CreateProcessW, and libuv's search
/// is narrower than std's: it tries the name as given only when its last
/// component has an extension, then the name with `.com` appended, then with
/// `.exe` -- skipping anything whose attributes mark a directory, without
/// following a link -- and fails as ERROR_FILE_NOT_FOUND when nothing is
/// left. A relative name is taken from `cwd` when one is given. std instead
/// hands CreateProcessW whatever it was given, so a directory came back as
/// ERROR_ACCESS_DENIED (EPERM, which spawn() throws) where node reports ENOENT,
/// and an extensionless shell shim or a missing UNC share came back as EFTYPE
/// or UNKNOWN.
///
/// - `Ok(None)`: run `prog` as given; std reaches the same file.
/// - `Ok(Some(path))`: run `path`; std would reach a different file or none.
/// - `Err`: nothing libuv would run.
///
/// A bare name is left to std's PATH search: libuv also searches the working
/// directory and the CHILD's PATH first, a separate difference this does not
/// take on.
#[cfg(windows)]
pub(crate) fn resolve_windows_program(
    prog: &str,
    cwd: Option<&str>,
) -> std::io::Result<Option<String>> {
    use std::os::windows::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

    if !(prog.contains(['/', '\\']) || prog == "." || prog == "..") {
        return Ok(None);
    }
    let path = Path::new(prog);
    let from_cwd = cwd.is_some() && path.is_relative();
    let full = match cwd {
        Some(cwd) if path.is_relative() => Path::new(cwd).join(path),
        _ => path.to_path_buf(),
    };
    let last = prog.rsplit(['/', '\\']).next().unwrap_or(prog);
    let has_ext = last.rfind('.').is_some_and(|dot| dot + 1 < last.len());
    let runnable = |p: &Path| {
        std::fs::symlink_metadata(p)
            .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_DIRECTORY == 0)
    };
    let with = |ext: &str| {
        let mut name = full.clone().into_os_string();
        name.push(ext);
        PathBuf::from(name)
    };
    let as_given = |p: PathBuf, std_reaches_it: bool| {
        if std_reaches_it && !from_cwd {
            Ok(None)
        } else {
            Ok(Some(p.to_string_lossy().into_owned()))
        }
    };
    if has_ext && runnable(&full) {
        return as_given(full, true);
    }
    let com = with(".com");
    if runnable(&com) {
        return as_given(com, false);
    }
    let exe = with(".exe");
    if runnable(&exe) {
        // std appends `.exe` to a name without an extension on its own.
        return as_given(exe, !has_ext);
    }
    Err(std::io::Error::from_raw_os_error(
        windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND as i32,
    ))
}

/// The concrete executable path for `prog`, after libuv's program search --
/// the value `CreateProcessW` needs as `lpApplicationName` when an argv0 has
/// taken over the command line's first token (CreateProcessW does NOT search
/// the application name on PATH, and falls back to the first token only when
/// the application name is NULL).
///
/// Mirrors `search_path` in libuv src/win/process.c: a name with a path in it
/// is tried only in its own directory; a bare name is tried in `cwd` and then
/// in every `path_env` (`;`-separated) directory; in each directory the name
/// as given (only when its last component carries an extension), then `.com`,
/// then `.exe`; a directory never names a program; nothing found is ENOENT.
///
/// One deliberate simplification: libuv drops `cwd` from a bare-name search
/// when `NeedCurrentDirectoryForExePathW` says to (a security policy that is
/// off by default); this always searches `cwd` first.
#[cfg(windows)]
pub(crate) fn resolve_program_path(
    prog: &str,
    cwd: Option<&str>,
    path_env: Option<&str>,
) -> std::io::Result<String> {
    use std::os::windows::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

    let runnable = |p: &Path| {
        std::fs::symlink_metadata(p)
            .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_DIRECTORY == 0)
    };
    let base_dir = || {
        cwd.map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
    };

    let has_dir = prog.contains(['/', '\\']) || prog == "." || prog == "..";
    let name: &str;
    let mut dirs: Vec<PathBuf> = Vec::new();
    if has_dir {
        let (dir, leaf) = match prog.rfind(['/', '\\']) {
            Some(i) => (&prog[..i], &prog[i + 1..]),
            None => ("", prog),
        };
        name = if leaf.is_empty() { prog } else { leaf };
        let dir_path = Path::new(if dir.is_empty() { "." } else { dir });
        dirs.push(if dir_path.is_absolute() {
            dir_path.to_path_buf()
        } else {
            base_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(dir_path)
        });
    } else {
        name = prog;
        if let Some(c) = base_dir() {
            dirs.push(c);
        }
        if let Some(pe) = path_env {
            for entry in pe.split(';') {
                if entry.is_empty() {
                    continue;
                }
                let e = Path::new(entry);
                dirs.push(if e.is_absolute() {
                    e.to_path_buf()
                } else {
                    base_dir().unwrap_or_else(|| PathBuf::from(".")).join(e)
                });
            }
        }
    }

    let name_has_ext = name.rfind('.').is_some_and(|dot| dot + 1 < name.len());
    for dir in &dirs {
        let mut candidates: Vec<PathBuf> = Vec::with_capacity(3);
        if name_has_ext {
            candidates.push(dir.join(name));
        }
        candidates.push(dir.join(format!("{name}.com")));
        candidates.push(dir.join(format!("{name}.exe")));
        for cand in candidates {
            if runnable(&cand) {
                return Ok(cand.to_string_lossy().into_owned());
            }
        }
    }
    Err(std::io::Error::from_raw_os_error(
        windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND as i32,
    ))
}

#[cfg(all(test, windows))]
mod program_search_tests {
    use super::resolve_windows_program as resolve;

    #[test]
    fn a_program_path_follows_libuvs_search() {
        let root = std::env::temp_dir().join(format!("oam-program-search-{}", std::process::id()));
        std::fs::create_dir_all(root.join("tool")).unwrap();
        std::fs::create_dir_all(root.join("dir.exe")).unwrap();
        std::fs::write(root.join("shim"), b"#!/bin/sh\n").unwrap();
        std::fs::write(root.join("shim.cmd"), b"@exit 0\r\n").unwrap();
        std::fs::write(root.join("real.exe"), b"MZ").unwrap();
        std::fs::write(root.join("script.txt"), b"hi").unwrap();
        let r = |name: &str| root.join(name).to_str().unwrap().to_string();
        let not_found =
            |result: std::io::Result<Option<String>>| result.unwrap_err().raw_os_error() == Some(2);

        // A directory, with or without an extension, is never the program.
        assert!(not_found(resolve(&r("tool"), None)));
        assert!(not_found(resolve(&r("dir.exe"), None)));
        assert!(not_found(resolve(".", None)));
        // No extension: the name itself is not tried, only .com and .exe.
        assert!(not_found(resolve(&r("shim"), None)));
        assert!(not_found(resolve(&r("missing"), None)));
        // With an extension the name is tried as given, whatever it holds.
        assert_eq!(resolve(&r("script.txt"), None).unwrap(), None);
        assert_eq!(resolve(&r("real.exe"), None).unwrap(), None);
        // std appends .exe itself, so `real` needs no rewrite...
        assert_eq!(resolve(&r("real"), None).unwrap(), None);
        // ...but a relative name is resolved from `cwd`, where std would not.
        let root_s = root.to_str().unwrap();
        assert_eq!(
            resolve(".\\real.exe", Some(root_s)).unwrap(),
            Some(root.join(".\\real.exe").to_string_lossy().into_owned())
        );
        // A bare name is left to the PATH search.
        assert_eq!(resolve("cmd", None).unwrap(), None);
        // libuv tries .com before .exe, and std never tries .com.
        std::fs::write(root.join("tool.com"), b"MZ").unwrap();
        assert_eq!(
            resolve(&r("tool"), None).unwrap(),
            Some(format!("{}.com", r("tool")))
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_bare_name_is_found_on_path_like_libuv() {
        use super::resolve_program_path as resolve;
        let root = std::env::temp_dir().join(format!("oam-path-search-{}", std::process::id()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("mytool.exe"), b"MZ").unwrap();
        std::fs::write(bin.join("shim.cmd"), b"@exit 0\r\n").unwrap();
        std::fs::create_dir_all(bin.join("tool.exe")).unwrap(); // a DIRECTORY named tool.exe
        std::fs::write(bin.join("noext"), b"hi").unwrap();
        let path = bin.to_str().unwrap();
        let path_b = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let not_found =
            |result: std::io::Result<String>| result.unwrap_err().raw_os_error() == Some(2);

        // A bare name with an extension: found only via PATH (not cwd).
        assert_eq!(
            resolve("mytool.exe", None, Some(path)).unwrap(),
            path_b(&bin.join("mytool.exe"))
        );
        // Extensionless bare name: libuv appends .exe and finds it.
        assert_eq!(
            resolve("mytool", None, Some(path)).unwrap(),
            path_b(&bin.join("mytool.exe"))
        );
        // A directory never names a program, even when its name has .exe.
        assert!(not_found(resolve("tool.exe", None, Some(path))));
        // Nothing on PATH: ENOENT.
        assert!(not_found(resolve("nosuchtool", None, Some(path))));
        // A name with a path in it is NOT searched on PATH.
        assert!(not_found(resolve("mytool.exe", None, Some(""))));
        // `cwd` is searched first for a bare name.
        assert_eq!(
            resolve("mytool.exe", Some(path), None).unwrap(),
            path_b(&bin.join("mytool.exe"))
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}

/// The errno for a spawnSync failure oam detects itself (a timeout, an
/// overflowing buffer), which has no OS error to take it from.
pub(crate) fn synthetic_spawn_errno(code: &str) -> Option<i32> {
    #[cfg(windows)]
    {
        // The windows table is keyed on the code alone.
        super::node_errno(code, &std::io::Error::other(code))
    }
    #[cfg(unix)]
    {
        match code {
            "ETIMEDOUT" => Some(-libc::ETIMEDOUT),
            "ENOBUFS" => Some(-libc::ENOBUFS),
            _ => None,
        }
    }
}

/// Windows creation flags for a `child_process` spawn, as libuv's
/// `uv_spawn` (src/win/process.c) computes them from node's options.
///
/// `CREATE_NO_WINDOW` is set for `windowsHide: true` when no stdio slot is
/// inherited (`inherits` -- libuv's `UV_INHERIT_FD`: an `'inherit'` slot or
/// a numbered descriptor). It is NOT set by default: a console child then
/// shares this process's console, where `CREATE_NO_WINDOW` makes Windows
/// start a fresh hidden conhost.exe for every child -- about 35 ms a spawn,
/// which node pays only when the caller asks for `windowsHide`.
///
/// The one place oam still adds it unasked is a process with NO console of
/// its own (started detached, or by a GUI program): there node's child gets a
/// new console with a VISIBLE window (measured: node v22.22.2 run detached,
/// its `windowsHide: false` grandchild reports a visible console window).
/// That child needs a new console either way, so the flag costs nothing and
/// only keeps the window from popping up; docs/node-divergences.md has it.
///
/// `detached` adds libuv's `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` --
/// no console of the parent's and no share of its Ctrl+C. CreateProcessW
/// ignores `CREATE_NO_WINDOW` alongside `DETACHED_PROCESS`, so the
/// no-console rule is skipped there.
#[cfg(windows)]
pub(crate) fn windows_creation_flags(detached: bool, windows_hide: bool, inherits: bool) -> u32 {
    creation_flags_for(detached, windows_hide, inherits, has_console)
}

/// `windows_creation_flags` with the console probe passed in, so the table
/// can be tested whatever console the test runner has. The probe runs only
/// when its answer matters.
#[cfg(windows)]
fn creation_flags_for(
    detached: bool,
    windows_hide: bool,
    inherits: bool,
    has_console: impl FnOnce() -> bool,
) -> u32 {
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
    };
    let mut flags = 0;
    if (windows_hide && !inherits) || (!detached && !has_console()) {
        flags |= CREATE_NO_WINDOW;
    }
    if detached {
        flags |= DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    }
    flags
}

#[cfg(all(test, windows))]
mod creation_flags_tests {
    use super::creation_flags_for;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
    };
    const DETACHED: u32 = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;

    #[test]
    fn follows_libuv_with_a_console() {
        let with = || true;
        // node's default: no flag, so the child shares this console.
        assert_eq!(creation_flags_for(false, false, false, with), 0);
        // windowsHide with every slot piped or ignored.
        assert_eq!(
            creation_flags_for(false, true, false, with),
            CREATE_NO_WINDOW
        );
        // An inherited slot vetoes it, as UV_INHERIT_FD does in libuv.
        assert_eq!(creation_flags_for(false, true, true, with), 0);
        assert_eq!(creation_flags_for(true, false, false, with), DETACHED);
        assert_eq!(
            creation_flags_for(true, true, false, with),
            DETACHED | CREATE_NO_WINDOW
        );
    }

    #[test]
    fn a_console_less_parent_never_gives_a_child_a_window() {
        let without = || false;
        assert_eq!(
            creation_flags_for(false, false, false, without),
            CREATE_NO_WINDOW
        );
        assert_eq!(
            creation_flags_for(false, true, true, without),
            CREATE_NO_WINDOW
        );
        // DETACHED_PROCESS already means no console at all.
        assert_eq!(creation_flags_for(true, false, false, without), DETACHED);
    }
}

/// Whether this process is attached to a console. `GetConsoleWindow` is not
/// the test: a process started with `CREATE_NO_WINDOW` has a console with no
/// window, which its children share without any window appearing.
/// `GetConsoleCP` fails (0) only when there is no console at all.
#[cfg(windows)]
fn has_console() -> bool {
    // SAFETY: GetConsoleCP takes no arguments and touches no memory of ours;
    // it reads the code page of the console this process is attached to, or
    // fails with 0 when there is none.
    unsafe { windows_sys::Win32::System::Console::GetConsoleCP() != 0 }
}

/// libuv's `UV_INHERIT_FD` test over a std spawn's three slots: an inherited
/// slot or a numbered descriptor. It decides whether `windowsHide` may add
/// `CREATE_NO_WINDOW` (see `windows_creation_flags`).
#[cfg(windows)]
fn stdio_inherits(stdio: &StdioSpec) -> bool {
    stdio
        .iter()
        .any(|s| matches!(s, StdioMode::Inherit | StdioMode::File(_)))
}

/// The result of a `spawn_sync` whose child never ran: node's error shape for
/// `e`, with `message` as its text, and no output, status or pid.
fn spawn_sync_failure(e: &std::io::Error, message: String) -> SpawnSyncResult {
    let code = super::node_error_code(e);
    SpawnSyncResult {
        stdout: Vec::new(),
        stderr: Vec::new(),
        status: None,
        signal: None,
        pid: 0,
        error: Some(SpawnSyncError {
            code: code.to_string(),
            errno: super::node_errno(code, e),
            message,
            win_error: super::unknown_win32_error(code, e),
        }),
    }
}

/// node's `spawnSync`: run `command` to its exit, with its piped output.
///
/// `kill_signal` is node's `killSignal`, by name: what ends a child that
/// outlives `timeout_ms` or writes past `max_buffer` (`None` is SIGTERM, the
/// default). See `end_sync_child` for what each platform does with it.
#[allow(clippy::too_many_arguments)]
pub fn spawn_sync(
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    env: Option<&[(String, String)]>,
    input: Option<&[u8]>,
    shell: bool,
    timeout_ms: u64,
    max_buffer: usize,
    kill_signal: Option<&str>,
    stdio: StdioSpec,
    detached: bool,
    windows_hide: bool,
    argv0: Option<String>,
) -> SpawnSyncResult {
    let (prog, final_args) = if shell {
        shell_command(command, args)
    } else {
        (command.to_string(), args.to_vec())
    };

    #[cfg(windows)]
    let (prog, unresolved) = if shell {
        (prog, None)
    } else {
        match resolve_windows_program(&prog, cwd) {
            Ok(Some(resolved)) => (resolved, None),
            Ok(None) => (prog, None),
            Err(e) => (prog, Some(e)),
        }
    };
    let mut cmd = std::process::Command::new(&prog);
    #[cfg(windows)]
    {
        if shell {
            use std::os::windows::process::CommandExt;
            for a in &final_args {
                cmd.raw_arg(a);
            }
        } else {
            cmd.args(&final_args);
        }
    }
    #[cfg(not(windows))]
    cmd.args(&final_args);
    // `input` is pumped ONLY into a slot that is already pipe-shaped.
    //
    // Node's docs say `input` "will override stdio[0]", but v22 does not
    // implement that sentence: it attaches the buffer to the slot-0 descriptor
    // and leaves the descriptor's TYPE alone, and the sync spawn only feeds
    // slots typed 'pipe'. So an explicit 'ignore'/'inherit' wins and the bytes
    // are never delivered. Measured, same script both runtimes:
    //   stdio omitted / 'pipe'        -> child reads "HELLO"
    //   stdio ['ignore','pipe','pipe'] -> child reads "" on node
    // Parity is with the implementation, not with the sentence -- the
    // conformance suite diffs against real node, and matching the doc here
    // would be a divergence that only looks like correctness.
    //
    // A pipe-shaped slot stays a real PIPE even with no input to write: the
    // `drop(stdin)` below closes the write end immediately, so the
    // child still sees EOF -- but over a pipe, as node gives it. Substituting
    // the null device here would have been observably different, not merely
    // "EOF by another route": the child's fd 0 becomes a character device, so
    // `fstatSync(0).isCharacterDevice()` flips and 'pipe' stops being
    // distinguishable from 'ignore' to a child that inspects its own stdin.
    #[cfg(windows)]
    let inherits = stdio_inherits(&stdio);
    let [slot0, slot1, slot2] = stdio;
    cmd.stdin(if slot0.is_pipe() {
        std::process::Stdio::piped()
    } else {
        slot0.into_stdio()
    });
    cmd.stdout(slot1.into_stdio());
    cmd.stderr(slot2.into_stdio());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        // windowsHide as spawn_child applies it: CREATE_NO_WINDOW by libuv's
        // table and libuv's show state for a GUI child's first window.
        //
        // A non-detached child starts SUSPENDED and is resumed only once it is
        // in the kill-on-close job (below). A detached one is never assigned,
        // so it starts running at once.
        let suspended = if detached { 0 } else { CREATE_SUSPENDED };
        cmd.creation_flags(windows_creation_flags(detached, windows_hide, inherits) | suspended);
        cmd.show_window(super::child_win::show_window_for(windows_hide));
        // argv0 on Windows requires direct CreateProcessW (lpCommandLine). The
        // std Command wrapper doesn't expose it; skip for spawn_sync.
        let _ = argv0;
    }
    #[cfg(not(windows))]
    {
        if let Some(ref argv0) = argv0 {
            use std::os::unix::process::CommandExt;
            cmd.arg0(argv0);
        }
        let _ = (detached, windows_hide);
    }

    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = env {
        // The child's whole environment (see spawn_child).
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
    }

    #[cfg(windows)]
    let spawned = match unresolved {
        Some(e) => Err(e),
        None => cmd.spawn(),
    };
    #[cfg(not(windows))]
    let spawned = cmd.spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return spawn_sync_failure(&e, e.to_string()),
    };

    // Node's spawnSync goes through uv_spawn too, so a child it is blocked on
    // still dies if this process is killed mid-wait. The child was created
    // suspended, so it joins the job before its first instruction -- ahead of
    // libuv, which assigns a running child -- and only then runs. A child
    // whose thread cannot be resumed has been terminated by adopt_suspended;
    // it is reaped here and the spawn fails, as the raw path's does. If even
    // the termination failed, the child is still parked and would never
    // exit: waiting would hang spawnSync, so its handles are just dropped.
    #[cfg(windows)]
    if !detached {
        use std::os::windows::io::AsHandle;
        use std::os::windows::process::ChildExt;
        if let Err(failed) =
            super::job_win::adopt_suspended(child.as_handle(), child.main_thread_handle())
        {
            if failed.terminated {
                let _ = child.wait();
            }
            let message = format!(
                "ResumeThread failed (GetLastError={}) for {command}",
                failed.error.raw_os_error().unwrap_or(0)
            );
            return spawn_sync_failure(&failed.error, message);
        }
    }

    let pid = child.id();

    // `input` goes down stdin on its OWN thread, alongside the drains below.
    // Writing it here first deadlocked against any child that answers while it
    // reads (cat, a compressor, `process.stdin.pipe(process.stdout)`): the
    // child fills its stdout pipe, stops reading stdin, and both sides block
    // once the input outgrows the pipe buffers -- before the timeout was even
    // armed. The thread is not joined: it ends when the write completes or
    // the read end goes away with the child -- and on unix also when the
    // child is ended (`SyncDrainCancel`), as node's `Kill` closes stdin with
    // the other two pipes.
    //
    // Unix: the cancel that lets this thread take the read ends back from the
    // drains when it ends the child, and stops the stdin writer. Made before
    // the writer starts, so the writer can watch it.
    #[cfg(unix)]
    let cancel = SyncDrainCancel::new();
    match (input, child.stdin.take()) {
        (Some(data), Some(stdin)) if !data.is_empty() => {
            let data = data.to_vec();
            #[cfg(unix)]
            let watch = cancel.as_ref().map(SyncDrainCancel::watch);
            std::thread::spawn(move || {
                #[cfg(unix)]
                feed_sync_stdin(stdin, &data, watch);
                // Windows: nothing cancels this writer when the child is
                // ended, unlike unix (`feed_sync_stdin`). Terminating the
                // child closes its copy of the read end, so a writer blocked
                // on it fails and stops -- but a grandchild that inherited
                // stdin (`cmd /c`, a shell) keeps the read end open, keeps
                // receiving the queued input after `spawnSync` returned, and
                // this thread lives on until the input is all written or the
                // grandchild exits (blocked for good on one that stops
                // reading without exiting). Node's `Kill` closes stdin with
                // the other two pipes. Recorded in docs/node-divergences.md,
                // entry 65 ("`spawnSync` after `killSignal`"), its Windows
                // paragraph.
                #[cfg(not(unix))]
                {
                    use std::io::Write;
                    let mut stdin = stdin;
                    let _ = stdin.write_all(&data);
                }
                // `stdin` is dropped by now: the child reads EOF after the
                // input.
            });
        }
        // No input, or an empty one: the write end closes now, so the child
        // reads EOF at once -- over a pipe, as node gives it.
        (_, stdin) => drop(stdin),
    }

    // Each piped stream is drained on its own thread, so a child that fills
    // one cannot block against this wait, and each drain stops at `max_buffer`
    // (`drain_capped`) so an endless writer is ended rather than buffered
    // without bound. The child stays REACHABLE here -- never moved into a
    // worker -- so a deadline or an overflow can still end it.
    let (events, pipe_events) = std::sync::mpsc::channel();
    // Unix: each drain watches the cancel made above (`SyncDrainCancel`).
    #[cfg(unix)]
    let (stdout_chunks, stderr_chunks) = {
        let watch = |c: &Option<SyncDrainCancel>| c.as_ref().map(SyncDrainCancel::watch);
        let stdout_chunks = pipe_chunks(child.stdout.take(), watch(&cancel));
        let stderr_chunks = pipe_chunks(child.stderr.take(), watch(&cancel));
        (stdout_chunks, stderr_chunks)
    };
    #[cfg(not(unix))]
    let (stdout_chunks, stderr_chunks) = (
        pipe_chunks(child.stdout.take()),
        pipe_chunks(child.stderr.take()),
    );
    let stdout_sink = drain_capped(stdout_chunks, max_buffer, events.clone());
    let stderr_sink = drain_capped(stderr_chunks, max_buffer, events);
    let deadline = (timeout_ms > 0)
        .then(|| std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms));
    let mut open_pipes = 2usize;
    // The synthetic failure that ended the wait before a plain exit, if any.
    let mut cut_short: Option<&'static str> = None;
    let mut waited: Option<std::io::Result<std::process::ExitStatus>> = None;
    // As node's sync spawn: first every piped stream to its end (a grandchild
    // holding a write end keeps it open), then the exit itself.
    // The read end of a stream that overflowed, kept open until the child has
    // been ended below (`SyncPipeHold`).
    let mut overflowed_pipe = None;
    match await_sync_pipes(&pipe_events, &mut open_pipes, deadline) {
        PipesEnded::Overflow(hold) => {
            cut_short = Some("ENOBUFS");
            overflowed_pipe = Some(hold);
        }
        PipesEnded::TimedOut => cut_short = Some("ETIMEDOUT"),
        PipesEnded::AllEof => match deadline {
            None => waited = Some(child.wait()),
            Some(deadline) => {
                let mut nap = std::time::Duration::from_millis(1);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            waited = Some(Ok(status));
                            break;
                        }
                        Ok(None) if std::time::Instant::now() >= deadline => {
                            cut_short = Some("ETIMEDOUT");
                            break;
                        }
                        Ok(None) => {
                            std::thread::sleep(nap);
                            nap = (nap * 2).min(std::time::Duration::from_millis(5));
                        }
                        Err(e) => {
                            waited = Some(Err(e));
                            break;
                        }
                    }
                }
            }
        },
    }

    if let Some(code) = cut_short {
        // A deadline or an overflow ends the child, as node does, and the
        // result is its error plus whatever was read so far. A child that had
        // already exited by itself (a grandchild kept the pipes open, or it
        // finished just as the limit was crossed) reports its own exit: it was
        // never signalled. Any other is sent `kill_signal` and reaped, so this
        // path leaves no zombie (`end_sync_child`).
        //
        // Between the signal and the reap every read end is let go of, as
        // node's `SyncProcessRunner::Kill` closes its pipes when it kills. The
        // wait is for as long as the child takes, and a child that survives
        // the signal and keeps writing (it ignores SIGTERM, or its handler
        // prints) would otherwise fill a pipe nobody reads and block in
        // write(2) for good, with `spawnSync` blocked on it. With the read
        // ends gone its next write fails (SIGPIPE/EPIPE) and it ends. Signal
        // first, then close: the other order is the one `SyncPipeHold` exists
        // to prevent.
        let (status, signal) = match child.try_wait() {
            Ok(Some(status)) => exit_report(&status, false, None),
            _ => end_sync_child(
                &mut child,
                kill_signal,
                &pipe_events,
                &mut open_pipes,
                |pipe_events, open_pipes| {
                    // An overflowed stream's read end closes only now.
                    drop(overflowed_pipe.take());
                    // Known residual, macOS (and any unix without `pipe2`): std
                    // opens the child's stdio pipes with pipe() and sets
                    // close-on-exec in a second call. A child that ANOTHER thread
                    // of this process forks between those two calls inherits our
                    // read end of the child's stdout or stderr, and keeps it
                    // until it execs or exits -- for good if it never execs. Our
                    // close below is then not the last, the child gets no
                    // EPIPE/SIGPIPE, and a child that ignores the kill signal and
                    // keeps writing can still fill the pipe and block in
                    // write(2), with the wait in `end_sync_child` blocked on it.
                    // Linux opens the pipes with pipe2(O_CLOEXEC) and has no such
                    // window. The cancel pipe itself has the same window for its
                    // write end, which is why `fire` writes a byte as well as
                    // closing it.
                    //
                    // Bounded by `SYNC_CANCEL_SETTLE`, as the stop below is: a
                    // drain whose poll failed sits in a plain blocking read that
                    // the cancel cannot reach, and on a stream a grandchild holds
                    // open without writing an unbounded wait here held
                    // `spawnSync` for as long as the grandchild lived. Such a
                    // drain keeps its read end past the settle and goes on
                    // reading. Once what it read crosses `max_buffer` it stops
                    // reading and hands its read end over as a `SyncPipeHold`;
                    // `end_sync_child` receives and drops every such hold while
                    // it waits for the child (`wait_releasing_holds`), so the
                    // read end closes and a survivor's next write fails rather
                    // than blocking. (The forked-copy residual above is the
                    // one way a survivor can still block.)
                    #[cfg(unix)]
                    if let Some(cancel) = &cancel {
                        stop_sync_drains(cancel, pipe_events, open_pipes, SYNC_CANCEL_SETTLE);
                    }
                    #[cfg(not(unix))]
                    let _ = (pipe_events, open_pipes);
                },
            ),
        };
        drop(overflowed_pipe);
        // The pipes close with the child unless something else holds a write
        // end. That is not waited for past a short grace: node closes its own
        // ends at this point, and joining the drains here kept spawnSync
        // blocked for as long as a grandchild (`cmd /c`, `sh -c`) lived.
        let _ = await_sync_pipes(
            &pipe_events,
            &mut open_pipes,
            Some(std::time::Instant::now() + SYNC_KILL_PIPE_GRACE),
        );
        // Past the grace the drains are told to stop, so a stream a
        // grandchild still holds open does not keep a thread and a read end
        // for as long as it lives. Their final passes are waited for, up to
        // `SYNC_CANCEL_SETTLE`, before the sinks are taken: a child that
        // exited by itself (the `try_wait` branch above, typically with a
        // grandchild holding a pipe) was never cancelled until here, and what
        // a final pass read after the take was lost. On the kill path the
        // stop in `end_sync_child`'s release already fired the cancel, and
        // with every drain gone by now this changes nothing. A drain still
        // open is either merely late -- its final pass not done within that
        // settle and the grace on a loaded box -- or stuck in the
        // poll-failure fallback; one more bounded wait lets a late pass land
        // in its sink before the take rather than after it, at the cost of
        // one more settle for a stuck one.
        #[cfg(unix)]
        if let Some(cancel) = &cancel {
            stop_sync_drains(cancel, &pipe_events, &mut open_pipes, SYNC_CANCEL_SETTLE);
        }
        let mut stdout = take_sink(&stdout_sink);
        let mut stderr = take_sink(&stderr_sink);
        // The retained length is read-granularity dependent under node too,
        // so it is deliberately not part of the contract.
        stdout.truncate(max_buffer);
        stderr.truncate(max_buffer);
        let message = if code == "ETIMEDOUT" {
            format!("spawnSync timed out after {timeout_ms}ms")
        } else {
            format!("spawnSync {command} {code}")
        };
        return SpawnSyncResult {
            stdout,
            stderr,
            status,
            signal,
            pid,
            error: Some(SpawnSyncError {
                code: code.to_string(),
                errno: synthetic_spawn_errno(code),
                message,
                win_error: None,
            }),
        };
    }

    let output = match waited {
        Some(Ok(status)) => Ok(std::process::Output {
            status,
            stdout: take_sink(&stdout_sink),
            stderr: take_sink(&stderr_sink),
        }),
        Some(Err(e)) => Err(e),
        None => Err(std::io::Error::other("spawnSync: child never exited")),
    };
    match output {
        Ok(output) => {
            // Blowing maxBuffer is an ERROR, not a quiet trim: it never gets
            // here. A drain that crosses the limit reports it while the child
            // still runs (`cut_short` above), because truncating and reporting
            // status 0 handed callers a short result that looked complete -- a
            // half JSON document that parses as valid, a log tail that reads
            // as the whole log.
            //
            // A child a signal ended reports the signal's name and a null
            // status, as node does; `signal: None` here hid every crash
            // (SIGSEGV, an out-of-memory SIGKILL) behind `{status: null,
            // signal: null}`.
            let (status, signal) = exit_report(&output.status, false, None);
            SpawnSyncResult {
                stdout: output.stdout,
                stderr: output.stderr,
                status,
                signal,
                pid,
                error: None,
            }
        }
        Err(e) => {
            let code = super::node_error_code(&e);
            SpawnSyncResult {
                stdout: Vec::new(),
                stderr: Vec::new(),
                status: None,
                signal: None,
                pid,
                error: Some(SpawnSyncError {
                    code: code.to_string(),
                    errno: super::node_errno(code, &e),
                    message: e.to_string(),
                    win_error: super::unknown_win32_error(code, &e),
                }),
            }
        }
    }
}

pub struct SpawnSyncResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Option<i32>,
    pub signal: Option<String>,
    pub pid: u32,
    pub error: Option<SpawnSyncError>,
}

pub struct SpawnSyncError {
    pub code: String,
    /// The libuv errno node puts on `result.error.errno`.
    pub errno: Option<i32>,
    pub message: String,
    /// The Win32 code behind an `UNKNOWN` (`unknown_win32_error`, #450); the
    /// JS puts it on `result.error` as a non-enumerable `winError`.
    pub win_error: Option<u32>,
}

/// What one sync-spawn drain thread reports, once, when it stops.
pub(crate) enum SyncPipeEvent {
    /// The stream ended (or there was no pipe to drain).
    Eof,
    /// The stream produced more than `max_buffer`; the drain stopped reading.
    /// The stream itself rides along, still open (see `SyncPipeHold`).
    Overflow(SyncPipeHold),
}

/// The read end of a stream whose drain stopped at `max_buffer`, kept open
/// until the caller lets go of it. The caller holds it until it has ended the
/// child: a read end that closed with the drain reached a child that was
/// still writing first (SIGPIPE on POSIX), so the child could be gone of that
/// before it was ended, and the result then reported that signal where node,
/// which ends the child and only then closes its pipes, reports the kill.
pub(crate) struct SyncPipeHold {
    _stream: Box<dyn std::any::Any + Send>,
}

/// How a wait on the sync-spawn drains ended.
pub(crate) enum PipesEnded {
    /// Every stream reached its end.
    AllEof,
    /// A stream crossed `max_buffer`; its read end is handed over still open.
    Overflow(SyncPipeHold),
    /// The deadline passed with a stream still open.
    TimedOut,
}

/// How long a sync spawn that just ended its child waits for the child's
/// pipes to close before it returns what was read. They close at once unless
/// another process inherited a write end, and that one is not waited for.
pub(crate) const SYNC_KILL_PIPE_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

/// Drain one of a sync spawn's streams on its own thread, into the returned
/// buffer. `next` is one blocking read: the next bytes, or `None` at the end
/// of the stream. With no pipe (`None`: an inherited or ignored slot) the end
/// is reported at once.
///
/// The drain stops, and says so, as soon as the buffer holds more than
/// `max_buffer`: the caller ends the child then, as node does, where reading
/// to EOF first let an endless writer grow the buffer without bound. The
/// buffer is shared rather than returned by a join so the caller can take what
/// was read without waiting for a stream something else still holds open.
pub(crate) fn drain_capped<F>(
    next: Option<F>,
    max_buffer: usize,
    events: std::sync::mpsc::Sender<SyncPipeEvent>,
) -> Arc<Mutex<Vec<u8>>>
where
    F: FnMut() -> Option<Vec<u8>> + Send + 'static,
{
    let sink = Arc::new(Mutex::new(Vec::new()));
    let Some(mut next) = next else {
        let _ = events.send(SyncPipeEvent::Eof);
        return sink;
    };
    let out = Arc::clone(&sink);
    std::thread::spawn(move || {
        while let Some(chunk) = next() {
            let mut buf = out.lock().unwrap_or_else(|e| e.into_inner());
            buf.extend_from_slice(&chunk);
            if buf.len() > max_buffer {
                drop(buf);
                // The reader (and the read end it owns) goes to the caller
                // rather than closing here; with nobody listening the send
                // fails and it closes after all.
                let _ = events.send(SyncPipeEvent::Overflow(SyncPipeHold {
                    _stream: Box::new(next),
                }));
                return;
            }
        }
        // The reader goes before the report: whoever hears `Eof` may count on
        // the read end being closed already (`stop_sync_drains`).
        drop(next);
        let _ = events.send(SyncPipeEvent::Eof);
    });
    sink
}

/// Unix: how `spawn_sync` takes its read ends back from the drain threads.
///
/// A drain owns its pipe and sits in a blocking wait on it, so the thread
/// that ends the child cannot simply close the descriptor. Each drain instead
/// polls its pipe together with the read end of this self-pipe; `fire` makes
/// that readable, and every drain then reads what the pipe already holds --
/// once, without blocking, and no more than it held when the cancel was seen
/// -- and stops and drops its pipe. Node reads as the bytes arrive, so what
/// a child wrote before it was ended is in node's result; a drain that saw
/// the cancel and its data in the same wake-up used to drop that data.
/// The stdin writer of a sync spawn watches the same end and stops writing
/// (`feed_sync_stdin`), so all three pipes close with the kill, as node's do.
///
/// The close happens on the drain's thread, so it trails the signal by a
/// thread wake-up where node, which reads on the thread that kills, closes
/// in the very next call. A child that answers the signal by writing at once
/// can therefore still land that write in the pipe here (and it may be read
/// by the final pass) where under node it would already find the pipe
/// closed.
///
/// `fire` does two things, each enough alone: it writes a byte, which is
/// never consumed, so it reaches every watcher and stays in force; and it
/// closes the write end, so every watcher's poll reports POLLHUP (which a
/// watcher treats as the cancel too). The byte covers a close that is not
/// seen: a close shows only once EVERY copy of the write end is gone, and a
/// child forked by another thread in the window before close-on-exec is set
/// (the platforms without `pipe2`) holds one. The close covers a byte write
/// that failed. Only both failing at once -- a failed write AND an inherited
/// copy of the write end -- leaves a watcher waiting. Dropping the cancel
/// closes the write end too, so watchers left running when `spawn_sync`
/// returns (a stdin writer feeding a grandchild) stop then.
#[cfg(unix)]
pub(crate) struct SyncDrainCancel {
    seen: Arc<std::io::PipeReader>,
    /// `None` once fired: the write end is closed.
    fired: Mutex<Option<std::io::PipeWriter>>,
}

#[cfg(unix)]
impl SyncDrainCancel {
    /// `None` when the process cannot open another pipe: the drains then run
    /// uncancellable, as they did before this existed.
    pub(crate) fn new() -> Option<Self> {
        let (seen, fired) = std::io::pipe().ok()?;
        Some(Self {
            seen: Arc::new(seen),
            fired: Mutex::new(Some(fired)),
        })
    }

    /// The end a drain polls next to its pipe.
    pub(crate) fn watch(&self) -> Arc<std::io::PipeReader> {
        Arc::clone(&self.seen)
    }

    /// Stop every drain watching: write the byte, then close the write end.
    /// A second call changes nothing a watcher can see.
    pub(crate) fn fire(&self) {
        use std::io::Write;
        let fired = self.fired.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut fired) = fired {
            let _ = fired.write(&[1]);
            // `fired` drops here: the write end closes.
        }
    }

    /// Whether `fire` has run (the write end is closed). Only the tests ask:
    /// `spawn_sync` stops the drains again whether or not it has.
    #[cfg(test)]
    pub(crate) fn has_fired(&self) -> bool {
        self.fired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
    }
}

/// Unix: how long `stop_sync_drains` waits, at most, for the drains to finish
/// their final passes. A drain that has seen the cancel never blocks, so this
/// is normally microseconds; the bound only covers a drain that fell back to
/// a plain blocking read after its poll failed (`pipe_chunks`) -- the cancel
/// cannot reach a thread parked in read(2) -- on a stream a grandchild still
/// holds open, which would otherwise hold `spawnSync` for as long as the
/// grandchild lives.
#[cfg(unix)]
pub(crate) const SYNC_CANCEL_SETTLE: std::time::Duration = std::time::Duration::from_secs(1);

/// Unix: fire `cancel` and wait, up to `settle`, for every drain still open
/// to let go of its read end -- by dropping its reader (the drop comes before
/// the `Eof` report) or by handing it over as a `SyncPipeHold` when its final
/// pass crossed `max_buffer`, dropped here. When every drain reports within
/// `settle`, what those passes read is in the sinks when this returns, so a
/// `take_sink` after it keeps output the child wrote before it was ended. A
/// pass still running at the deadline (a loaded box) lands in its sink
/// whenever it finishes, after the return; `spawn_sync` awaits once more
/// before it takes the sinks for that reason.
///
/// A drain that has seen the cancel is at most a bounded final pass of
/// non-blocking reads from done, and the cancel reaches it by two separate
/// means (`SyncDrainCancel`). The exception is a drain whose poll failed: it
/// fell back to a plain blocking read (`pipe_chunks`) that nothing here can
/// interrupt, and it ends only with its stream. `settle` bounds the wait for
/// that one; past it the drain is left running with its read end, and `open`
/// still counts it.
#[cfg(unix)]
pub(crate) fn stop_sync_drains(
    cancel: &SyncDrainCancel,
    events: &std::sync::mpsc::Receiver<SyncPipeEvent>,
    open: &mut usize,
    settle: std::time::Duration,
) {
    cancel.fire();
    let deadline = std::time::Instant::now() + settle;
    while let PipesEnded::Overflow(hold) = await_sync_pipes(events, open, Some(deadline)) {
        drop(hold);
    }
}

/// `drain_capped`'s reader over a std pipe: one blocking read of up to 64 KiB
/// a call, `None` at EOF or on a read error.
///
/// Unix: with a `cancel` end to watch, the wait is a poll on both and a
/// `cancel` that reports anything -- readable (the byte), POLLHUP (its write
/// end closed), an error -- ends the stream as well (`SyncDrainCancel`). The
/// cancel is looked at first, so a stream that always has bytes ready cannot
/// outrun it. Once it is seen, what the pipe already holds is still read, in
/// a final pass that never blocks and takes no more than the pipe held at
/// that moment (`final_pass_chunk`), so output the child wrote before it was
/// ended stays in the result; `drain_capped` still stops that pass at
/// `max_buffer`. A poll that fails for any reason but EINTR falls through to
/// the plain blocking read, as an unwatched drain does, rather than ending
/// the stream: an end there was a short result with no error.
#[cfg(unix)]
pub(crate) fn pipe_chunks<R>(
    pipe: Option<R>,
    cancel: Option<Arc<std::io::PipeReader>>,
) -> Option<impl FnMut() -> Option<Vec<u8>> + Send + 'static>
where
    R: std::io::Read + std::os::fd::AsFd + Send + 'static,
{
    use rustix::event::{PollFd, PollFlags};
    pipe.map(|mut pipe| {
        // Bytes left for the final pass, once the cancel has been seen.
        let mut final_pass: Option<usize> = None;
        move || {
            let mut chunk = vec![0u8; 65536];
            if let Some(left) = final_pass.as_mut() {
                return final_pass_chunk(&mut pipe, &mut chunk, left);
            }
            loop {
                if let Some(cancel) = &cancel {
                    let cancelled = {
                        let mut fds = [
                            PollFd::new(&pipe, PollFlags::IN),
                            PollFd::new(&**cancel, PollFlags::IN),
                        ];
                        match rustix::event::poll(&mut fds, None) {
                            Ok(_) => !fds[1].revents().is_empty(),
                            // A wait cut short by a signal, not an end.
                            Err(rustix::io::Errno::INTR) => continue,
                            // The wait itself failed: read as an unwatched
                            // drain does. The cancel is looked at again on
                            // the next call.
                            Err(_) => false,
                        }
                    };
                    if cancelled {
                        // What the pipe holds now, and no more: a writer
                        // that keeps writing cannot keep the pass going.
                        // Unknown, one chunk's worth.
                        let held = rustix::io::ioctl_fionread(&pipe)
                            .ok()
                            .and_then(|n| usize::try_from(n).ok())
                            .unwrap_or(chunk.len());
                        let left = final_pass.insert(held);
                        return final_pass_chunk(&mut pipe, &mut chunk, left);
                    }
                }
                match pipe.read(&mut chunk) {
                    Ok(0) => return None,
                    Ok(n) => {
                        chunk.truncate(n);
                        return Some(chunk);
                    }
                    // A signal arrived before anything was read: not an end.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return None,
                }
            }
        }
    })
}

/// Unix: one step of a cancelled drain's final pass (`pipe_chunks`): the next
/// bytes the pipe already holds, at most `left` of them, or `None` once the
/// pass is over -- `left` spent, nothing ready right now, EOF, or an error.
/// Never blocks: a zero-timeout poll comes before each read, and the drain is
/// the pipe's only reader, so a pipe the poll found readable still is.
#[cfg(unix)]
fn final_pass_chunk<R>(pipe: &mut R, chunk: &mut Vec<u8>, left: &mut usize) -> Option<Vec<u8>>
where
    R: std::io::Read + std::os::fd::AsFd,
{
    use rustix::event::{PollFd, PollFlags, Timespec};
    loop {
        if *left == 0 {
            return None;
        }
        let ready = {
            let mut fds = [PollFd::new(&*pipe, PollFlags::IN)];
            match rustix::event::poll(&mut fds, Some(&Timespec::default())) {
                Ok(_) => fds[0].revents().contains(PollFlags::IN),
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => false,
            }
        };
        if !ready {
            return None;
        }
        let want = (*left).min(chunk.len());
        match pipe.read(&mut chunk[..want]) {
            Ok(0) => return None,
            Ok(n) => {
                *left -= n;
                let mut out = std::mem::take(chunk);
                out.truncate(n);
                return Some(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

/// Unix: feed a sync spawn's `input` to the child's stdin, then close it.
///
/// With a `cancel` end to watch (`SyncDrainCancel`), the write end is made
/// non-blocking and each write waits in a poll on stdin (POLLOUT) together
/// with the cancel, so the writer can never be parked in write(2) past the
/// cancel: once the cancel reports anything it stops at once, and the write
/// end and the unwritten input go with it -- node's `Kill` closes stdin with
/// the other two pipes. Otherwise it writes everything, then closes: the child
/// reads the whole input and then EOF, as before. A cancel that could not be
/// made, a write end that cannot be made non-blocking, or a poll that fails
/// for any reason but EINTR all fall back to the plain blocking write of
/// what is left.
#[cfg(unix)]
fn feed_sync_stdin(
    mut stdin: std::process::ChildStdin,
    data: &[u8],
    cancel: Option<Arc<std::io::PipeReader>>,
) {
    use rustix::event::{PollFd, PollFlags};
    use std::io::Write;
    let Some(cancel) = cancel else {
        let _ = stdin.write_all(data);
        return;
    };
    if rustix::io::ioctl_fionbio(&stdin, true).is_err() {
        let _ = stdin.write_all(data);
        return;
    }
    let mut rest = data;
    while !rest.is_empty() {
        let writable = {
            let mut fds = [
                PollFd::new(&stdin, PollFlags::OUT),
                PollFd::new(&*cancel, PollFlags::IN),
            ];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) if !fds[1].revents().is_empty() => return,
                Ok(_) => true,
                // A wait cut short by a signal, not an end.
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => false,
            }
        };
        if !writable {
            // The wait itself failed: write the rest as an unwatched writer
            // does, blocking again.
            let _ = rustix::io::ioctl_fionbio(&stdin, false);
            let _ = stdin.write_all(rest);
            return;
        }
        // Whatever the poll reported on stdin (POLLOUT, or POLLERR for a
        // read end that is gone) the write tells which: progress, not yet,
        // or the end.
        match stdin.write(rest) {
            Ok(0) => return,
            Ok(n) => rest = &rest[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return,
        }
    }
    // `stdin` drops here: the child reads EOF after the input.
}

/// `drain_capped`'s reader over a std pipe: one blocking read of up to 64 KiB
/// a call, `None` at EOF or on a read error.
#[cfg(not(unix))]
fn pipe_chunks<R>(pipe: Option<R>) -> Option<impl FnMut() -> Option<Vec<u8>> + Send + 'static>
where
    R: std::io::Read + Send + 'static,
{
    pipe.map(|mut pipe| {
        move || {
            let mut chunk = vec![0u8; 65536];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => return None,
                    Ok(n) => {
                        chunk.truncate(n);
                        return Some(chunk);
                    }
                    // A signal arrived before anything was read: not an end.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return None,
                }
            }
        }
    })
}

/// Wait for the drains still running (`open` of them) to report, until
/// `deadline` when there is one. `open` is left at the number still running.
pub(crate) fn await_sync_pipes(
    events: &std::sync::mpsc::Receiver<SyncPipeEvent>,
    open: &mut usize,
    deadline: Option<std::time::Instant>,
) -> PipesEnded {
    use std::sync::mpsc::RecvTimeoutError;
    while *open > 0 {
        let event = match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match events.recv_timeout(left) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => return PipesEnded::TimedOut,
                    // Every drain is gone without a word (one panicked):
                    // nothing is left to wait for.
                    Err(RecvTimeoutError::Disconnected) => {
                        *open = 0;
                        break;
                    }
                }
            }
            None => match events.recv() {
                Ok(event) => event,
                Err(_) => {
                    *open = 0;
                    break;
                }
            },
        };
        *open -= 1;
        if let SyncPipeEvent::Overflow(hold) = event {
            return PipesEnded::Overflow(hold);
        }
    }
    PipesEnded::AllEof
}

/// What a drain has read so far, taken out of its shared buffer.
pub(crate) fn take_sink(sink: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    std::mem::take(&mut *sink.lock().unwrap_or_else(|e| e.into_inner()))
}

pub async fn child_read_stdout(children: ChildRegistry, handle: u64) -> OpOutcome {
    read_child_pipe(children, handle, "stdout", stdout_slot).await
}

pub async fn child_read_stderr(children: ChildRegistry, handle: u64) -> OpOutcome {
    read_child_pipe(children, handle, "stderr", stderr_slot).await
}

/// One of a child's two read streams inside its registry entry: the pipe, its
/// busy flag, and the notifier a finished read wakes the next one with.
struct ReadSlot<'a, P> {
    pipe: &'a mut Option<P>,
    busy: &'a mut bool,
    released: &'a Arc<Notify>,
}

fn stdout_slot(cp: &mut ChildProcess) -> ReadSlot<'_, tokio::process::ChildStdout> {
    ReadSlot {
        pipe: &mut cp.stdout,
        busy: &mut cp.stdout_busy,
        released: &cp.stdout_released,
    }
}

fn stderr_slot(cp: &mut ChildProcess) -> ReadSlot<'_, tokio::process::ChildStderr> {
    ReadSlot {
        pipe: &mut cp.stderr,
        busy: &mut cp.stderr_busy,
        released: &cp.stderr_released,
    }
}

/// Drop an exited child's entry once neither read stream is left: both pipes
/// gone and no read in flight. `child_wait` keeps the entry past the exit for
/// exactly as long as a reader can still drain what the child wrote.
fn retire_if_drained(children: &mut HashMap<u64, ChildProcess>, handle: u64) {
    let drained = children.get(&handle).is_some_and(|cp| {
        cp.exited
            && cp.stdout.is_none()
            && cp.stderr.is_none()
            && !cp.stdout_busy
            && !cp.stderr_busy
    });
    if drained {
        children.remove(&handle);
    }
}

/// One read of a child's stdout or stderr (`slot` picks which); the shared
/// body of `child_read_stdout` and `child_read_stderr`.
async fn read_child_pipe<P>(
    children: ChildRegistry,
    handle: u64,
    label: &str,
    slot: fn(&mut ChildProcess) -> ReadSlot<'_, P>,
) -> OpOutcome
where
    P: tokio::io::AsyncRead + Unpin,
{
    // Check-and-set atomically under one lock. A concurrent second read sees
    // busy=true and WAITS for the in-flight read to release the pipe rather
    // than failing, so the pump's 'close' path cannot stall on a rejected
    // read promise. A None pipe with busy=false is a real EOF (stream closed
    // / never piped). The guard is dropped before the wait, so no lock is
    // held across the await.
    //
    // The wait is parked on the stream's notifier, which every read wakes as
    // it lets go of the pipe. (It used to spin on `yield_now`, which kept a
    // runtime worker busy for as long as the first read sat on a quiet
    // child.) `notify_one` stores a permit when nobody is parked yet, so a
    // release between this check and the await is not lost; a stale permit
    // costs one extra pass of the loop.
    //
    // `parked_on` is the notifier this read last waited on. A read that was
    // woken and then finds the stream over -- or the whole entry gone, where
    // the slot's own notifier can no longer be reached -- passes the wake-up
    // on through it, so a third read parked behind it is not left waiting on
    // a notifier nobody will signal again.
    let mut parked_on: Option<Arc<Notify>> = None;
    let (mut pipe, released) = loop {
        let taken = {
            let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
            match guard.get_mut(&handle) {
                Some(cp) => {
                    let slot = slot(cp);
                    if *slot.busy {
                        Err(Arc::clone(slot.released))
                    } else {
                        match slot.pipe.take() {
                            Some(pipe) => {
                                *slot.busy = true;
                                Ok(Some((pipe, Arc::clone(slot.released))))
                            }
                            None => {
                                // Pass the wake-up on: another read may be
                                // parked behind the one that reached the end.
                                slot.released.notify_one();
                                Ok(None)
                            }
                        }
                    }
                }
                None => Ok(None),
            }
        };
        match taken {
            Ok(Some(taken)) => break taken,
            Ok(None) => {
                if let Some(released) = parked_on {
                    released.notify_one();
                }
                return OpOutcome::Done;
            }
            Err(released) => {
                released.notified().await;
                parked_on = Some(released);
            }
        }
    };

    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 65536];
    let read = pipe.read(&mut buf).await;
    let outcome = {
        let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = match read {
            Ok(n) if n > 0 => {
                buf.truncate(n);
                if let Some(cp) = guard.get_mut(&handle) {
                    let slot = slot(cp);
                    *slot.pipe = Some(pipe);
                    *slot.busy = false;
                }
                OpOutcome::Bytes(buf)
            }
            // EOF or a read error: the pipe is dropped, not re-inserted.
            ended => {
                if let Some(cp) = guard.get_mut(&handle) {
                    *slot(cp).busy = false;
                }
                drop(pipe);
                match ended {
                    Err(e) => OpOutcome::Failed(format!("read {label}: {e}")),
                    Ok(_) => OpOutcome::Done,
                }
            }
        };
        // The stream may have been the last thing an exited child's entry
        // was kept for.
        retire_if_drained(&mut guard, handle);
        outcome
    };
    released.notify_one();
    outcome
}

pub fn child_close_stdin(children: &ChildRegistry, handle: u64) {
    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cp) = guard.get_mut(&handle) {
        drop(cp.stdin.take());
    }
}

pub async fn child_write_stdin(children: ChildRegistry, handle: u64, data: Vec<u8>) -> OpOutcome {
    let stdin = {
        let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
        guard.get_mut(&handle).and_then(|cp| cp.stdin.take())
    };
    match stdin {
        Some(mut stdin) => {
            use tokio::io::AsyncWriteExt;
            match stdin.write_all(&data).await {
                Ok(()) => {
                    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
                    // Not back into the entry of a child that has exited:
                    // nothing reads it, and the entry is only kept for its
                    // read streams from then on.
                    if let Some(cp) = guard.get_mut(&handle)
                        && !cp.exited
                    {
                        cp.stdin = Some(stdin);
                    }
                    OpOutcome::Done
                }
                Err(e) => OpOutcome::Failed(format!("write stdin: {e}")),
            }
        }
        None => OpOutcome::Failed("stdin not available".to_string()),
    }
}

/// Signal a child to terminate. Wakes the parked `child_wait` future (the only
/// owner of the `Child`), which performs the actual kill. Survives `child_wait`
/// having already taken the `Child` out of the registry entry.
pub fn child_kill(children: &ChildRegistry, handle: u64, signal: Option<String>) {
    let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cp) = guard.get_mut(&handle) {
        // An entry kept past the exit for its read streams has nobody to
        // signal any more.
        if cp.exited {
            return;
        }
        cp.kill_signal = signal;
        // notify_one stores a permit if the wait future has not parked yet, so
        // a kill issued before child_wait starts still lands.
        cp.kill.notify_one();
    }
}

pub async fn child_wait(children: ChildRegistry, handle: u64) -> OpOutcome {
    // Take the Child for the await but leave the registry entry in place so a
    // concurrent child_kill can still reach the kill-notifier. `pid` rides along
    // for the Unix kill path, which delivers the real POSIX signal by pid.
    let taken = {
        let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_mut(&handle) {
            Some(cp) => cp.child.take().map(|c| (c, cp.kill.clone(), cp.pid)),
            None => None,
        }
    };
    let Some((mut child, kill, pid)) = taken else {
        return OpOutcome::Failed("unknown child handle".to_string());
    };

    // A LOOP, so every kill is delivered, not only the first. Awaiting the
    // exit with nothing left listening on the notifier after one kill meant a
    // later one only stored a permit: SIGTERM to a child that traps it and
    // then SIGKILL never sent the SIGKILL (likewise SIGSTOP then SIGCONT, or
    // a repeated SIGUSR1). `Child::wait` is cancel safe, so dropping it for
    // a kill and awaiting it again loses nothing.
    let mut killed = false;
    let status = loop {
        tokio::select! {
            s = child.wait() => break s,
            _ = kill.notified() => {
                // Read the signal child_kill recorded (stored under the lock
                // BEFORE notify_one, so it is visible here) and deliver it for
                // real. A kill that found the child already gone delivered
                // nothing and must not colour the exit report.
                let requested = {
                    let guard = children.lock().unwrap_or_else(|e| e.into_inner());
                    guard.get(&handle).and_then(|cp| cp.kill_signal.clone())
                };
                if deliver_kill(&mut child, pid, requested.as_deref()) {
                    killed = true;
                }
            }
        }
    };

    // The exit is collected: capture the requested kill signal (used by the
    // Windows exit report), close stdin (nothing reads it now), and mark the
    // entry. The entry itself STAYS while a read pipe, or a read in flight,
    // remains: the JS side starts this wait alongside its read pumps, and
    // dropping the entry here threw away whatever the child wrote between the
    // parent's last read and its exit -- the next read found no entry and
    // reported EOF with the tail still in the pipe (the raw registries' #456 /
    // #419, here). Each reader drops the entry with the last stream
    // (`retire_if_drained`); with nothing piped it goes now.
    let requested_signal = {
        let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
        let sig = match guard.get_mut(&handle) {
            Some(cp) => {
                cp.exited = true;
                drop(cp.stdin.take());
                cp.kill_signal.clone()
            }
            None => None,
        };
        retire_if_drained(&mut guard, handle);
        sig
    };

    match status {
        Ok(status) => {
            let (code, signal) = exit_report(&status, killed, requested_signal);
            let json = serde_json::json!({ "code": code, "signal": signal });
            OpOutcome::Json(json.to_string())
        }
        Err(e) => OpOutcome::Failed(format!("wait: {e}")),
    }
}

/// Deliver a kill to `child`. On Unix this sends the caller's REQUESTED POSIX
/// signal (default SIGTERM) via `kill(2)` on the pid, so a child can trap
/// SIGTERM / SIGINT and shut down gracefully -- matching Node. `child.wait()`
/// still reaps the exit afterward: tokio's SIGCHLD reaper observes the death
/// regardless of which signal (or sender) caused it. On Windows there are no
/// POSIX signals, so `start_kill()` (TerminateProcess) is the only mechanism;
/// the requested signal name is surfaced by `exit_report` instead.
///
/// Returns whether a kill was actually issued: false for a child that had
/// already exited, or (Unix) for a signal name this platform does not have.
#[cfg(unix)]
fn deliver_kill(child: &mut tokio::process::Child, pid: u32, signal: Option<&str>) -> bool {
    // Only signal while the child is still alive. If it already exited and the
    // SIGCHLD driver reaped it, the kernel can recycle the pid immediately, so
    // a kill then could hit an unrelated process. On a try_wait error we fall
    // through and signal anyway (conservative).
    if let Ok(Some(_)) = child.try_wait() {
        return false;
    }
    // No signal named is SIGTERM, node's default. A name this platform does
    // not have is NOT: it used to become SIGTERM too, so `kill(SIGWINCH)` --
    // which a number reaches without the JS name check -- ended the child.
    let signum = match signal {
        None => libc::SIGTERM,
        Some(name) => match crate::child_unix::signal_number(name) {
            Some(signum) => signum,
            None => return false,
        },
    };
    crate::child_unix::deliver_signal(pid, signum);
    true
}

#[cfg(windows)]
fn deliver_kill(child: &mut tokio::process::Child, _pid: u32, _signal: Option<&str>) -> bool {
    // A child that has already exited is not killed, and must not be reported
    // as killed: `exit_report` turns a delivered kill into `{code: null,
    // signal}`, which hid the real exit code of a child that finished just
    // before the kill was looked at (`child_win::raw_kill` checks the same
    // way). On a try_wait error we fall through and terminate anyway.
    if let Ok(Some(_)) = child.try_wait() {
        return false;
    }
    // TerminateProcess -- the only kill Windows offers. exit_report maps the
    // requested signal name onto the exit event.
    child.start_kill().is_ok()
}

/// End a `spawn_sync` child that outlived its timeout or overflowed
/// `maxBuffer`, reap it, and report how it went as `(status, signal)`.
///
/// Unix: the child is sent `kill_signal` (`None` is SIGTERM, node's default)
/// with `kill(2)`, as node's sync spawn does, so it can trap the signal and
/// shut down in order. The report is read off the reaped status: a child the
/// signal ended reports that signal, one that caught it and exited reports
/// its exit code. Like node, the wait is for as long as the child takes -- one
/// that ignores the signal and stays quiet holds `spawnSync` until it exits
/// by itself. A signal that could not be delivered (a name this platform
/// lacks) falls back to SIGKILL, which is node's fallback too.
///
/// `release` runs between the signal and the wait: the caller closes its read
/// ends of the child's pipes there, as node does when it kills, so a survivor
/// that keeps writing meets a closed pipe rather than a full one. What that
/// does to the report is node's too (v22.23.2 on Linux, three runs each, all
/// alike): an `sh` that ignores SIGTERM around an endless `yes` reports its
/// own exit code once `yes` has died of SIGPIPE, and a handler that prints
/// before it exits dies of SIGPIPE on that print, which is then the `signal`.
///
/// The wait then takes the drains' reports (`events`, `open` of them still
/// running) as they come, and drops the read end any drain hands over as a
/// `SyncPipeHold` (`wait_releasing_holds`). A drain the release could not
/// stop goes on reading, and once it crosses `max_buffer` its read end sits
/// in such a hold; left unreceived in the channel it would keep the pipe
/// open and full, and a survivor writing to it would block in write(2) with
/// this wait blocked on that.
///
/// The pid is safe to signal: the caller saw the child still running, and
/// only this function reaps it, so the pid cannot have been recycled.
#[cfg(unix)]
fn end_sync_child(
    child: &mut std::process::Child,
    kill_signal: Option<&str>,
    events: &std::sync::mpsc::Receiver<SyncPipeEvent>,
    open: &mut usize,
    release: impl FnOnce(&std::sync::mpsc::Receiver<SyncPipeEvent>, &mut usize),
) -> (Option<i32>, Option<String>) {
    let signum = match kill_signal {
        None => Some(libc::SIGTERM),
        Some(name) => crate::child_unix::signal_number(name),
    };
    let delivered = signum
        .and_then(rustix::process::Signal::from_named_raw)
        .zip(
            i32::try_from(child.id())
                .ok()
                .and_then(rustix::process::Pid::from_raw),
        )
        .is_some_and(|(sig, pid)| rustix::process::kill_process(pid, sig).is_ok());
    if !delivered {
        let _ = child.kill();
    }
    release(events, open);
    match wait_releasing_holds(child, events, open) {
        Ok(status) => exit_report(&status, false, None),
        // Nothing to read the signal off: report the one that was asked for.
        Err(_) => (None, Some(kill_signal.unwrap_or("SIGTERM").to_string())),
    }
}

/// Unix: wait for an ended `spawn_sync` child while drains are still open
/// (`open` of them, reporting on `events`), receiving their reports and
/// dropping the read end in any `SyncPipeHold` at once (`end_sync_child`).
/// With no drain open -- the usual case, the release having stopped them all
/// -- this is a plain blocking `wait`. Otherwise it checks the child and
/// waits on the channel in turns, 1 ms growing to 5 ms, which is the latency
/// of the kill path only and only with a drain the release left running;
/// once the last drain reports it blocks in `wait` as well.
#[cfg(unix)]
fn wait_releasing_holds(
    child: &mut std::process::Child,
    events: &std::sync::mpsc::Receiver<SyncPipeEvent>,
    open: &mut usize,
) -> std::io::Result<std::process::ExitStatus> {
    use std::sync::mpsc::RecvTimeoutError;
    let mut nap = std::time::Duration::from_millis(1);
    while *open > 0 {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        match events.recv_timeout(nap) {
            Ok(event) => {
                *open -= 1;
                // A hold's read end closes here.
                drop(event);
            }
            Err(RecvTimeoutError::Timeout) => {
                nap = (nap * 2).min(std::time::Duration::from_millis(5));
            }
            // Every drain is gone without a word: none can hand a hold over.
            Err(RecvTimeoutError::Disconnected) => *open = 0,
        }
    }
    child.wait()
}

/// Windows has no signals to deliver: the child is terminated, and the result
/// names the signal that was asked for, as libuv records it
/// (`sync_kill_signal_name`). `release` runs once the child has been
/// terminated, as on unix. A terminated child cannot outlive the kill, so the
/// wait needs nothing from the drains (`events`, `open`): it is the plain
/// `wait` it always was.
#[cfg(windows)]
fn end_sync_child(
    child: &mut std::process::Child,
    kill_signal: Option<&str>,
    events: &std::sync::mpsc::Receiver<SyncPipeEvent>,
    open: &mut usize,
    release: impl FnOnce(&std::sync::mpsc::Receiver<SyncPipeEvent>, &mut usize),
) -> (Option<i32>, Option<String>) {
    let _ = child.kill();
    release(events, open);
    let _ = child.wait();
    (None, Some(sync_kill_signal_name(kill_signal)))
}

/// The signal a Windows `spawnSync` reports for a child it terminated.
///
/// libuv's `uv_process_kill` terminates the process for SIGTERM, SIGKILL,
/// SIGINT and SIGQUIT and records that signal as the exit signal; for any
/// other it fails, and node's sync spawn then kills with SIGKILL instead. So
/// the result names the requested signal when it is one of those four (`None`
/// is the default, SIGTERM) and SIGKILL otherwise.
#[cfg(windows)]
pub(crate) fn sync_kill_signal_name(kill_signal: Option<&str>) -> String {
    match kill_signal {
        None => "SIGTERM",
        Some(name @ ("SIGTERM" | "SIGKILL" | "SIGINT" | "SIGQUIT")) => name,
        Some(_) => "SIGKILL",
    }
    .to_string()
}

/// Build the Node-shaped `(code, signal)` exit report from the child's ACTUAL
/// exit status.
///
/// Unix: a signal-terminated child reports `code:null` + the real terminating
/// signal, so a child that caught our SIGTERM and `exit(0)`ed reports
/// `{code:0, signal:null}`, while one killed by the default action reports
/// `{code:null, signal:SIGTERM}`. `killed` / `requested_signal` are unused --
/// `WIFSIGNALED` is the source of truth.
///
/// Windows: the status carries no POSIX signal, so when WE initiated the kill
/// report `code:null` + the requested name (Node's Windows convention);
/// otherwise the exit code.
#[cfg(unix)]
fn exit_report(
    status: &std::process::ExitStatus,
    _killed: bool,
    _requested_signal: Option<String>,
) -> (Option<i32>, Option<String>) {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signum) = status.signal() {
        (None, Some(crate::child_unix::signal_name(signum)))
    } else {
        (status.code(), None)
    }
}

#[cfg(windows)]
fn exit_report(
    status: &std::process::ExitStatus,
    killed: bool,
    requested_signal: Option<String>,
) -> (Option<i32>, Option<String>) {
    if killed {
        (
            None,
            Some(requested_signal.unwrap_or_else(|| "SIGTERM".to_string())),
        )
    } else {
        (status.code(), None)
    }
}

/// The window state and job membership a Windows child starts with, read by
/// the child itself: this test binary re-runs itself as the child, with
/// `PROBE_VAR` set, and only the probe test runs there.
///
/// The recursion guard is the ENVIRONMENT, never argv: every test in this
/// module checks `PROBE_VAR` before anything else, and a process that has it
/// set never spawns.
///
/// What the job assertions prove, and what they cannot: they fail when a
/// spawn_sync child is not in oam's job (`job_flags` tells it from a job the
/// test runner may already run in), and they fail if the suspended child
/// were never resumed (spawnSync's 120 s timeout kills a parked child; the
/// async probes carry the same bound). They do NOT prove the child joined BEFORE its
/// first instruction: the probe runs only after the loader, the CRT and
/// libtest, by which time a post-spawn assignment has long landed too
/// (measured: 10 of 10 runs pass with the suspension removed). That ordering
/// rests on CREATE_SUSPENDED and `job_win::adopt_suspended`.
#[cfg(all(test, windows))]
mod spawn_window_tests {
    use super::{spawn_child, spawn_sync, stdio_pipe_all};
    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        QueryInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetStartupInfoW, STARTF_USESHOWWINDOW, STARTUPINFOW,
    };

    const PROBE_VAR: &str = "OAM_TEST_SPAWN_WINDOW_PROBE";
    const PROBE_TEST: &str = "child::spawn_window_tests::probe_child_reports_its_startup_state";
    const PROBE_LINE: &str = "OAM-SPAWN-PROBE ";
    /// job_win.rs's limit flags: how the child tells oam's kill-on-close job
    /// from a job the test runner itself may run in.
    const OAM_JOB_FLAGS: u32 = JOB_OBJECT_LIMIT_BREAKAWAY_OK
        | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    const SW_HIDE: u16 = 0;
    const SW_SHOWDEFAULT: u16 = 10;

    fn in_probe_child() -> bool {
        std::env::var_os(PROBE_VAR).is_some()
    }

    #[derive(Debug)]
    struct Probe {
        use_show_window: bool,
        show_window: u16,
        /// Whether the child was in any job, and the limit flags of its
        /// immediate job (0 when none), both read before anything else.
        in_job: bool,
        job_flags: u32,
    }

    impl Probe {
        fn in_oam_job(&self) -> bool {
            self.in_job && self.job_flags == OAM_JOB_FLAGS
        }
    }

    /// The child side. A no-op in an ordinary run; in a probe child it prints
    /// one `PROBE_LINE`. It never spawns.
    #[test]
    fn probe_child_reports_its_startup_state() {
        if !in_probe_child() {
            return;
        }
        let mut in_job = 0;
        let mut job = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let mut si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        // SAFETY: GetCurrentProcess returns an always-valid pseudo-handle.
        // IsProcessInJob writes one BOOL through a pointer to the live local
        // `in_job`. QueryInformationJobObject on a null job queries this
        // process's own immediate job and writes at most the length passed,
        // size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(), into `job`, a
        // live local of that type; a null return-length pointer is allowed.
        // GetStartupInfoW fills `si`, a live STARTUPINFOW whose `cb` is set.
        let job_flags = unsafe {
            IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job);
            let queried = QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_mut(&mut job).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            );
            GetStartupInfoW(&mut si);
            if queried == 0 {
                0
            } else {
                job.BasicLimitInformation.LimitFlags
            }
        };
        println!(
            "{PROBE_LINE}use_show_window={} show_window={} in_job={} job_flags={job_flags}",
            u8::from(si.dwFlags & STARTF_USESHOWWINDOW != 0),
            si.wShowWindow,
            u8::from(in_job != 0),
        );
    }

    /// This test binary, told to run only the probe test.
    fn probe_command() -> (String, Vec<String>) {
        let exe = std::env::current_exe()
            .expect("current_exe")
            .to_string_lossy()
            .into_owned();
        let args = ["--exact", PROBE_TEST, "--nocapture", "--test-threads=1"]
            .map(String::from)
            .to_vec();
        (exe, args)
    }

    /// This process's environment plus the recursion guard.
    fn probe_env() -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .filter(|(k, _)| !k.eq_ignore_ascii_case(PROBE_VAR))
            .collect();
        env.push((PROBE_VAR.to_string(), "1".to_string()));
        env
    }

    fn parse(stdout: &[u8]) -> Probe {
        let text = String::from_utf8_lossy(stdout);
        let line = text
            .lines()
            // libtest prints the test's name on the same line first.
            .find_map(|l| l.find(PROBE_LINE).map(|at| &l[at + PROBE_LINE.len()..]))
            .unwrap_or_else(|| panic!("no probe line in the child's stdout:\n{text}"));
        let field = |name: &str| -> u32 {
            line.split(' ')
                .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("no {name} in {line:?}"))
        };
        Probe {
            use_show_window: field("use_show_window") != 0,
            show_window: u16::try_from(field("show_window")).expect("a u16"),
            in_job: field("in_job") != 0,
            job_flags: field("job_flags"),
        }
    }

    fn probe_sync(windows_hide: bool, detached: bool) -> Probe {
        let (exe, args) = probe_command();
        let env = probe_env();
        let result = spawn_sync(
            &exe,
            &args,
            None,
            Some(&env),
            None,
            false,
            120_000,
            1 << 20,
            None,
            stdio_pipe_all(),
            detached,
            windows_hide,
            None,
        );
        if let Some(e) = &result.error {
            panic!("spawn_sync failed: {} {}", e.code, e.message);
        }
        assert_eq!(
            result.status,
            Some(0),
            "probe child failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
        parse(&result.stdout)
    }

    async fn probe_async(windows_hide: bool) -> Probe {
        let (exe, args) = probe_command();
        let (child, _pid) = spawn_child(
            exe,
            args,
            None,
            Some(probe_env()),
            false,
            stdio_pipe_all(),
            false,
            windows_hide,
            None,
        )
        .expect("spawn_child");
        // Bounded like the spawn_sync probes' 120 s timeout: a child that never
        // exits (a spawn path that parks it) fails the test instead of hanging
        // the whole lib test run. A child left running then is in the
        // kill-on-close job spawn_child adopts it into, so it dies with the
        // test process.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            child.wait_with_output(),
        )
        .await
        .expect("probe child did not exit within 120 s")
        .expect("wait_with_output");
        assert!(
            output.status.success(),
            "probe child failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        parse(&output.stdout)
    }

    /// The raw CreateProcessW path (`child_win::spawn_extra_with`), which
    /// builds its own STARTUPINFOEXW instead of going through std's
    /// `Command`: an argv0, or an extra stdio slot past fd 2, routes a spawn
    /// there. Driven through `spawn_sync_argv0`, that path's blocking drain.
    fn probe_raw(windows_hide: bool, argv0: Option<&str>, extra_fd: bool) -> Probe {
        use crate::child_win::{StdioFd, spawn_sync_argv0};
        let (exe, args) = probe_command();
        let env = probe_env();
        let mut stdio = vec![StdioFd::ChildRead, StdioFd::ChildWrite, StdioFd::ChildWrite];
        if extra_fd {
            stdio.push(StdioFd::ChildWrite);
        }
        let result = spawn_sync_argv0(
            &exe,
            &args,
            None,
            Some(&env),
            None,
            &stdio,
            false,
            windows_hide,
            argv0,
            120_000,
            1 << 20,
        );
        if let Some(e) = &result.error {
            panic!("spawn_sync_argv0 failed: {} {}", e.code, e.message);
        }
        assert_eq!(
            result.status,
            Some(0),
            "probe child failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        );
        parse(&result.stdout)
    }

    #[test]
    fn spawn_extra_with_an_argv0_starts_a_hidden_child_with_sw_hide_inside_the_job() {
        if in_probe_child() {
            return;
        }
        let probe = probe_raw(true, Some("oam-probe-argv0"), false);
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_HIDE, "{probe:?}");
        assert!(probe.in_oam_job(), "{probe:?}");
    }

    #[test]
    fn spawn_extra_with_an_argv0_starts_a_shown_child_with_sw_showdefault_inside_the_job() {
        if in_probe_child() {
            return;
        }
        let probe = probe_raw(false, Some("oam-probe-argv0"), false);
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_SHOWDEFAULT, "{probe:?}");
        assert!(probe.in_oam_job(), "{probe:?}");
    }

    #[test]
    fn spawn_extra_with_an_extra_fd_starts_a_hidden_child_with_sw_hide() {
        if in_probe_child() {
            return;
        }
        let probe = probe_raw(true, None, true);
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_HIDE, "{probe:?}");
        assert!(probe.in_oam_job(), "{probe:?}");
    }

    #[test]
    fn spawn_extra_with_an_extra_fd_starts_a_shown_child_with_sw_showdefault() {
        if in_probe_child() {
            return;
        }
        let probe = probe_raw(false, None, true);
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_SHOWDEFAULT, "{probe:?}");
        assert!(probe.in_oam_job(), "{probe:?}");
    }

    #[test]
    fn spawn_sync_starts_a_hidden_child_with_sw_hide_inside_the_job() {
        if in_probe_child() {
            return;
        }
        let probe = probe_sync(true, false);
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_HIDE, "{probe:?}");
        assert!(probe.in_oam_job(), "{probe:?}");
    }

    #[test]
    fn spawn_sync_starts_a_shown_child_with_sw_showdefault_inside_the_job() {
        if in_probe_child() {
            return;
        }
        let probe = probe_sync(false, false);
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_SHOWDEFAULT, "{probe:?}");
        assert!(probe.in_oam_job(), "{probe:?}");
    }

    /// The control for the job assertions above: a detached child is never
    /// assigned, so `in_oam_job` must be false for it. Were it true, the
    /// assertions above would hold whatever spawn_sync did.
    #[test]
    fn spawn_sync_leaves_a_detached_child_out_of_the_job() {
        if in_probe_child() {
            return;
        }
        let probe = probe_sync(false, true);
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_SHOWDEFAULT, "{probe:?}");
        assert!(!probe.in_oam_job(), "{probe:?}");
    }

    #[tokio::test]
    async fn spawn_child_starts_a_hidden_child_with_sw_hide() {
        if in_probe_child() {
            return;
        }
        let probe = probe_async(true).await;
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_HIDE, "{probe:?}");
    }

    #[tokio::test]
    async fn spawn_child_starts_a_shown_child_with_sw_showdefault() {
        if in_probe_child() {
            return;
        }
        let probe = probe_async(false).await;
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_SHOWDEFAULT, "{probe:?}");
    }

    /// A cluster worker gets libuv's show state for a child without
    /// windowsHide (SW_SHOWDEFAULT under STARTF_USESHOWWINDOW). Built by the
    /// same `worker_command` cluster_fork uses, aimed at this test binary:
    /// the probe's libtest arguments stand where `run` goes, and the probe's
    /// test name where the script path goes. Only stdout/stderr are swapped
    /// for pipes, to read the report back.
    #[tokio::test]
    async fn a_cluster_worker_starts_with_sw_showdefault() {
        if in_probe_child() {
            return;
        }
        let (exe, _) = probe_command();
        let mut cmd = crate::cluster::worker_command(
            &exe,
            &["--exact", "--nocapture", "--test-threads=1"],
            PROBE_TEST,
            "1",
            &probe_env(),
        );
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let child = cmd.spawn().expect("spawn the worker command");
        // Bounded as the other async probes are; kill_on_drop reaps it.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            child.wait_with_output(),
        )
        .await
        .expect("probe worker did not exit within 120 s")
        .expect("wait_with_output");
        assert!(
            output.status.success(),
            "probe worker failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let probe = parse(&output.stdout);
        assert!(probe.use_show_window, "{probe:?}");
        assert_eq!(probe.show_window, SW_SHOWDEFAULT, "{probe:?}");
    }
}

#[cfg(all(test, windows))]
mod sync_kill_signal_tests {
    use super::sync_kill_signal_name;

    /// The four signals libuv's `uv_process_kill` terminates for are reported
    /// by their own name; every other request is reported as the SIGKILL node
    /// falls back to; no `killSignal` at all is the default SIGTERM.
    #[test]
    fn a_terminated_child_reports_the_signal_libuv_records() {
        assert_eq!(sync_kill_signal_name(None), "SIGTERM");
        for name in ["SIGTERM", "SIGKILL", "SIGINT", "SIGQUIT"] {
            assert_eq!(sync_kill_signal_name(Some(name)), name);
        }
        for name in ["SIGHUP", "SIGUSR1", "SIGBREAK", "SIGWINCH", "SIGPIPE"] {
            assert_eq!(sync_kill_signal_name(Some(name)), "SIGKILL", "{name}");
        }
    }
}

#[cfg(all(test, unix))]
mod sync_drain_cancel_tests {
    use super::{
        PipesEnded, SyncDrainCancel, SyncPipeEvent, SyncPipeHold, await_sync_pipes, drain_capped,
        feed_sync_stdin, pipe_chunks, stop_sync_drains, take_sink, wait_releasing_holds,
    };
    use std::io::Write;
    use std::time::{Duration, Instant};

    /// Write to a pipe whose read end this process has just closed until the
    /// write fails, and hand back the failure. Not at the first attempt
    /// always: another test's child, forked while the pipe was open, holds a
    /// copy of the read end from its fork until its exec (the pipe is
    /// close-on-exec), and a write in that window still lands. Measured: one
    /// run in two failed on the first write with the cluster tests spawning
    /// beside this one.
    fn write_fails(writer: &mut std::io::PipeWriter, why: &str) -> std::io::Error {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match writer.write_all(b"x") {
                Err(e) => return e,
                Ok(()) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(()) => panic!("a write still lands after 10 s: {why}"),
            }
        }
    }

    /// A drain parked on a quiet pipe stops when the cancel fires and has
    /// closed its read end by the time the release returns: the writer's next
    /// write fails with EPIPE. What it read before that is kept.
    #[test]
    fn a_fired_cancel_closes_the_read_end_of_a_quiet_pipe() {
        let (reader, mut writer) = std::io::pipe().expect("a pipe");
        let cancel = SyncDrainCancel::new().expect("a cancel pipe");
        let (events, pipe_events) = std::sync::mpsc::channel();
        let sink = drain_capped(
            pipe_chunks(Some(reader), Some(cancel.watch())),
            1 << 20,
            events,
        );
        writer.write_all(b"before").expect("the read end is open");
        // Until the cancel the drain is still there, and reads what arrives.
        let mut open = 1usize;
        assert!(matches!(
            await_sync_pipes(
                &pipe_events,
                &mut open,
                Some(Instant::now() + Duration::from_millis(200))
            ),
            PipesEnded::TimedOut
        ));
        assert_eq!(open, 1);

        stop_sync_drains(&cancel, &pipe_events, &mut open, Duration::from_secs(30));
        assert!(cancel.has_fired());
        assert_eq!(open, 0);
        let err = write_fails(&mut writer, "the read end is closed");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(take_sink(&sink), b"before");
    }

    /// A drain that already stopped at the limit handed its read end over;
    /// the release closes that one too, whichever stream it was, and one
    /// cancel reaches every drain watching it.
    #[test]
    fn the_release_closes_overflowed_and_draining_ends_alike() {
        let (full_reader, mut full_writer) = std::io::pipe().expect("a pipe");
        let (quiet_reader, mut quiet_writer) = std::io::pipe().expect("a pipe");
        let cancel = SyncDrainCancel::new().expect("a cancel pipe");
        let (events, pipe_events) = std::sync::mpsc::channel();
        let _full = drain_capped(
            pipe_chunks(Some(full_reader), Some(cancel.watch())),
            4,
            events.clone(),
        );
        let _quiet = drain_capped(
            pipe_chunks(Some(quiet_reader), Some(cancel.watch())),
            4,
            events,
        );
        full_writer.write_all(b"past the limit").expect("open");
        let mut open = 2usize;
        let hold = match await_sync_pipes(&pipe_events, &mut open, None) {
            PipesEnded::Overflow(hold) => hold,
            _ => panic!("the full stream overflows first"),
        };
        // Held: the read end is still open, so a write still lands.
        full_writer.write_all(b"x").expect("the hold keeps it open");
        drop(hold);
        let err = write_fails(&mut full_writer, "closed with the hold");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);

        stop_sync_drains(&cancel, &pipe_events, &mut open, Duration::from_secs(30));
        assert_eq!(open, 0);
        let err = write_fails(&mut quiet_writer, "closed by the cancel");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// Bytes already in the pipe when the drain sees the cancel are still
    /// read: here the cancel has fired before the drain's first poll, so the
    /// poll reports the data and the cancel together -- the case that used to
    /// drop the data. The final pass takes what the pipe held and stops
    /// (the write end is still open, so it must not wait for EOF), and the
    /// read end is closed after it.
    #[test]
    fn bytes_already_in_the_pipe_when_the_cancel_is_seen_are_kept() {
        let (reader, mut writer) = std::io::pipe().expect("a pipe");
        let cancel = SyncDrainCancel::new().expect("a cancel pipe");
        writer
            .write_all(b"written before the kill")
            .expect("the read end is open");
        cancel.fire();
        let (events, pipe_events) = std::sync::mpsc::channel();
        let sink = drain_capped(
            pipe_chunks(Some(reader), Some(cancel.watch())),
            1 << 20,
            events,
        );
        let mut open = 1usize;
        stop_sync_drains(&cancel, &pipe_events, &mut open, Duration::from_secs(30));
        assert_eq!(open, 0);
        assert_eq!(take_sink(&sink), b"written before the kill");
        let err = write_fails(&mut writer, "the read end is closed after the pass");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// `stop_sync_drains` returns only once the final pass is in the sink:
    /// the path where the child exited by itself fires the cancel and takes
    /// the sinks straight after, and a take that did not wait for the pass
    /// lost what it read. The drain is held back from its first read on
    /// purpose, so its whole run (the cancel seen, the pass) comes after the
    /// stop was called; a stop that only fired would find the sink empty. The
    /// settle bound here is far past the hold-back, so load cannot fail it.
    #[test]
    fn the_stop_waits_for_the_final_pass_before_the_sink_is_taken() {
        let (reader, mut writer) = std::io::pipe().expect("a pipe");
        let cancel = SyncDrainCancel::new().expect("a cancel pipe");
        writer
            .write_all(b"in the pipe at the stop")
            .expect("the read end is open");
        let mut inner = pipe_chunks(Some(reader), Some(cancel.watch())).expect("a reader");
        let mut held_back = true;
        let slow = move || {
            if std::mem::take(&mut held_back) {
                std::thread::sleep(Duration::from_millis(100));
            }
            inner()
        };
        let (events, pipe_events) = std::sync::mpsc::channel();
        let sink = drain_capped(Some(slow), 1 << 20, events);
        let mut open = 1usize;
        stop_sync_drains(&cancel, &pipe_events, &mut open, Duration::from_secs(30));
        assert_eq!(open, 0, "the drain did not report within the settle");
        assert_eq!(take_sink(&sink), b"in the pipe at the stop");
        let err = write_fails(&mut writer, "the read end is closed after the pass");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// A final pass that crosses `max_buffer` hands its read end over; the
    /// stop drops it rather than returning with the pipe still held open.
    #[test]
    fn the_stop_drops_a_read_end_handed_over_in_the_final_pass() {
        let (reader, mut writer) = std::io::pipe().expect("a pipe");
        let cancel = SyncDrainCancel::new().expect("a cancel pipe");
        writer.write_all(b"past the limit").expect("open");
        cancel.fire();
        let (events, pipe_events) = std::sync::mpsc::channel();
        let sink = drain_capped(pipe_chunks(Some(reader), Some(cancel.watch())), 4, events);
        let mut open = 1usize;
        stop_sync_drains(&cancel, &pipe_events, &mut open, Duration::from_secs(30));
        assert_eq!(open, 0);
        assert_eq!(take_sink(&sink), b"past the limit");
        let err = write_fails(&mut writer, "the handed-over read end is dropped");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// A drain the cancel cannot reach -- the plain blocking read a drain
    /// falls back to after its poll failed, modelled here by a drain that
    /// watches no cancel -- on a stream whose write end is held open and
    /// quiet (a grandchild's) does not hold the stop past `settle`, and is
    /// still counted open after it. The kill path of `spawn_sync` waits this
    /// way too, where an unbounded wait held it until the writer went away.
    /// The write end is closed at the end, so the drain thread ends with the
    /// test.
    #[test]
    fn the_stop_is_bounded_for_a_drain_the_cancel_cannot_reach() {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        let cancel = SyncDrainCancel::new().expect("a cancel pipe");
        let (events, pipe_events) = std::sync::mpsc::channel();
        let _sink = drain_capped(pipe_chunks(Some(reader), None), 1 << 20, events);
        let mut open = 1usize;
        let settle = Duration::from_millis(200);
        let started = Instant::now();
        stop_sync_drains(&cancel, &pipe_events, &mut open, settle);
        let took = started.elapsed();
        assert_eq!(open, 1, "the unreachable drain is still open");
        assert!(took >= settle, "returned before the settle: {took:?}");
        // Generous: the bound is the settle, and only scheduling adds to it.
        assert!(
            took < Duration::from_secs(20),
            "the stop was not bounded: {took:?}"
        );
        drop(writer);
        assert!(matches!(
            await_sync_pipes(&pipe_events, &mut open, None),
            PipesEnded::AllEof
        ));
        assert_eq!(open, 0);
    }

    /// The cancel's second mechanism alone: its write end closing, with no
    /// byte written, stops a drain parked on a quiet pipe (POLLHUP on the
    /// cancel end counts as the cancel). So a byte write that fails cannot
    /// leave a drain waiting, and a release after it cannot hang.
    #[test]
    fn a_closed_cancel_end_stops_a_drain_without_the_byte() {
        let (reader, mut writer) = std::io::pipe().expect("a pipe");
        let cancel = SyncDrainCancel::new().expect("a cancel pipe");
        let (events, pipe_events) = std::sync::mpsc::channel();
        let _sink = drain_capped(
            pipe_chunks(Some(reader), Some(cancel.watch())),
            1 << 20,
            events,
        );
        let mut open = 1usize;
        assert!(matches!(
            await_sync_pipes(
                &pipe_events,
                &mut open,
                Some(Instant::now() + Duration::from_millis(200))
            ),
            PipesEnded::TimedOut
        ));
        // Dropping the cancel closes its write end and writes nothing.
        drop(cancel);
        assert!(
            matches!(
                await_sync_pipes(
                    &pipe_events,
                    &mut open,
                    Some(Instant::now() + Duration::from_secs(10))
                ),
                PipesEnded::AllEof
            ),
            "the drain did not stop on the closed cancel end"
        );
        assert_eq!(open, 0);
        let err = write_fails(&mut writer, "the read end is closed");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// The kill path's wait (`wait_releasing_holds`) drops a read end a
    /// drain handed over as soon as the report arrives, while the child is
    /// still running: a survivor writing to that pipe meets a closed pipe
    /// instead of blocking in write(2) on a full one. Two drains are counted
    /// open and only one reports, so the wait is still taking reports in
    /// turns with the child's status when the child dies, and returns that
    /// status with the other drain still counted open.
    #[test]
    fn the_kill_path_wait_drops_a_hold_while_the_child_runs() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep starts");
        let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).expect("a pid"))
            .expect("a pid");
        // A failed assertion before the explicit kill below must not leave
        // the `sleep` running and the waiter parked: the guard kills the
        // child on unwind (ESRCH once it is reaped is ignored).
        struct KillOnDrop(rustix::process::Pid);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = rustix::process::kill_process(self.0, rustix::process::Signal::KILL);
            }
        }
        let _guard = KillOnDrop(pid);
        let (reader, mut writer) = std::io::pipe().expect("a pipe");
        let (events, pipe_events) = std::sync::mpsc::channel();
        events
            .send(SyncPipeEvent::Overflow(SyncPipeHold {
                _stream: Box::new(reader),
            }))
            .expect("the receiver is here");
        let (done, waited) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let mut open = 2usize;
            let status = wait_releasing_holds(&mut child, &pipe_events, &mut open);
            let _ = done.send((status.map_err(|e| e.kind()), open));
        });
        // `events` stays alive to the end, so the channel never disconnects:
        // the wait can only return on the child's status.

        let err = write_fails(&mut writer, "the hold is dropped during the wait");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(
            matches!(waited.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)),
            "the wait returned before the child was ended"
        );

        rustix::process::kill_process(pid, rustix::process::Signal::KILL)
            .expect("the child is ours");
        let (status, open) = waited
            .recv_timeout(Duration::from_secs(10))
            .expect("the wait returns once the child is gone");
        let status = status.expect("the child's status");
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
        assert_eq!(
            open, 1,
            "the drain that never reported is still counted open"
        );
        waiter.join().expect("the waiter");
        drop(events);
    }

    /// Every drain gone without a report (the channel disconnected) leaves
    /// nothing to hand a hold over: the wait stops taking reports, counts
    /// none open, and blocks in a plain `wait` until the child exits.
    ///
    /// The child exits when its stdin closes, which the test does only after
    /// the call has stayed blocked for 500 ms with the child running; a wait
    /// that missed the disconnect would not be blocked but spinning, and the
    /// call's first child check and channel read come long before that.
    #[test]
    fn the_kill_path_wait_falls_through_to_wait_when_every_drain_is_gone() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "read line; exit 7"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("sh starts");
        let stdin = child.stdin.take().expect("a stdin pipe");
        let (events, pipe_events) = std::sync::mpsc::channel::<SyncPipeEvent>();
        drop(events);
        let (started, call_started) = std::sync::mpsc::channel();
        let (done, waited) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let mut open = 1usize;
            let _ = started.send(());
            let status = wait_releasing_holds(&mut child, &pipe_events, &mut open);
            let _ = done.send((status.map_err(|e| e.kind()), open));
        });
        call_started
            .recv_timeout(Duration::from_secs(10))
            .expect("the waiter starts");
        assert!(
            matches!(
                waited.recv_timeout(Duration::from_millis(500)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "the wait returned while the child was running"
        );
        drop(stdin);
        let (status, open) = waited
            .recv_timeout(Duration::from_secs(10))
            .expect("the wait returns once the child exits");
        let status = status.expect("the child's status");
        assert_eq!(status.code(), Some(7), "{status:?}");
        assert_eq!(status.signal(), None);
        assert_eq!(open, 0, "the disconnect counts every drain gone");
        waiter.join().expect("the waiter");
    }

    /// A stdin writer parked on a full pipe the child never reads stops when
    /// the cancel fires, closing the child's stdin: it does not sit in
    /// write(2) holding the input until the child exits.
    #[test]
    fn a_fired_cancel_stops_a_stdin_writer_the_child_does_not_read() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sleep");
        let stdin = child.stdin.take().expect("a stdin pipe");
        let cancel = SyncDrainCancel::new().expect("a cancel pipe");
        let watch = cancel.watch();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Far past any pipe buffer.
            feed_sync_stdin(stdin, &vec![b'x'; 4 << 20], Some(watch));
            let _ = done.send(());
        });
        let parked = finished.recv_timeout(Duration::from_millis(300)).is_err();
        cancel.fire();
        let stopped = finished.recv_timeout(Duration::from_secs(10)).is_ok();
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            parked,
            "the writer finished although nothing read the input"
        );
        assert!(stopped, "the writer did not stop on the cancel");
    }

    /// Without a cancel end the reader is the plain blocking one: it ends at
    /// EOF and not before.
    #[test]
    fn an_unwatched_reader_runs_to_eof() {
        let (reader, mut writer) = std::io::pipe().expect("a pipe");
        let mut next = pipe_chunks(Some(reader), None).expect("a reader");
        writer.write_all(b"abc").expect("open");
        drop(writer);
        assert_eq!(next().as_deref(), Some(&b"abc"[..]));
        assert_eq!(next(), None);
    }
}
