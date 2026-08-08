//! Windows extra-fd stdio: raw `CreateProcessW` + the MSVCRT `lpReserved2`
//! fd-table inheritance, so a child can receive numbered fds beyond 0/1/2.
//!
//! `std`/`tokio` `Command` only wire stdin/stdout/stderr. Chromium's
//! `--remote-debugging-pipe` reads CDP off fd 3 and writes off fd 4 (via
//! `_get_osfhandle(3/4)`), so driving a browser over the (faster, no-TCP) pipe
//! transport requires giving the child those fds. This is the mechanism libuv
//! implements; the proven topology (anonymous pipes + per-fd direction):
//!   fd 3 = child-read  / parent-write  (parent sends CDP commands)
//!   fd 4 = child-write / parent-read   (child sends CDP responses)
//!
//! A raw child is NOT a `tokio::process::Child`, so it has its own registry and
//! op set: wait via `WaitForSingleObject` on the blocking pool, kill via
//! `TerminateProcess`, pipe I/O via blocking `ReadFile`/`WriteFile` on the
//! blocking pool. Anonymous pipes are unidirectional, so extra fds carry one
//! direction each (sufficient for CDP); full-duplex extra fds (named pipes)
//! are a future enhancement no current consumer needs.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TYPE_CHAR, FILE_TYPE_PIPE, GetFileType, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};

use super::OpOutcome;

// MSVCRT fd-info flags packed into lpReserved2 (one byte per fd).
const FOPEN: u8 = 0x01;
const FPIPE: u8 = 0x08;
const FDEV: u8 = 0x40;

/// HANDLE is `*mut c_void`, not Send. We only ever touch a given handle from
/// one thread at a time (taken out of the registry for the op), so sending the
/// raw value across the blocking pool is sound.
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

/// What to do with one child fd index.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StdioFd {
    /// No stream: child fd points at NUL.
    Ignore,
    /// Child inherits the parent's std handle for this fd (0/1/2 only).
    Inherit,
    /// Pipe the child READS (parent writes). e.g. stdin, CDP-in (fd 3).
    ChildRead,
    /// Pipe the child WRITES (parent reads). e.g. stdout/stderr, CDP-out (fd 4).
    ChildWrite,
    /// A descriptor the CALLER named -- `stdio: ['ignore','pipe','pipe', logFd]`.
    /// Carried as a raw HANDLE value (isize) so the variant stays `Send`.
    ///
    /// Without this a numbered slot fell through to `Inherit`, whose fd>2 arm
    /// hands the child STD_ERROR_HANDLE -- so the child quietly got stderr where
    /// the caller asked for a file.
    Descriptor(isize),
}

/// Parent-side end of one piped fd.
struct FdEnd {
    handle: Option<SendHandle>,
    /// true => parent reads this end (child writes); false => parent writes.
    readable: bool,
}

pub struct RawChild {
    process: Option<SendHandle>,
    pub pid: u32,
    fds: HashMap<u32, FdEnd>,
    kill_signal: Option<String>,
}

pub type RawChildRegistry = Arc<Mutex<HashMap<u64, RawChild>>>;

fn crt_flag(h: HANDLE) -> u8 {
    match unsafe { GetFileType(h) } {
        FILE_TYPE_PIPE => FOPEN | FPIPE,
        FILE_TYPE_CHAR => FOPEN | FDEV,
        _ => FOPEN,
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Canonical Windows argv quoting (CommandLineToArgvW-compatible).
fn append_quoted(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{0b}', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let chars: Vec<char> = arg.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let mut backslashes = 0;
        while i < chars.len() && chars[i] == '\\' {
            backslashes += 1;
            i += 1;
        }
        if i == chars.len() {
            for _ in 0..backslashes * 2 {
                out.push('\\');
            }
            break;
        } else if chars[i] == '"' {
            for _ in 0..backslashes * 2 + 1 {
                out.push('\\');
            }
            out.push('"');
        } else {
            for _ in 0..backslashes {
                out.push('\\');
            }
            out.push(chars[i]);
        }
        i += 1;
    }
    out.push('"');
}

/// Build a double-null-terminated UTF-16 environment block (sorted, as Windows
/// expects). Merges the inherited environment with overrides unless cleared.
fn build_env_block(env: &[(String, String)], clear: bool) -> Vec<u16> {
    let mut map: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    if !clear {
        for (k, v) in std::env::vars() {
            map.insert(k, v);
        }
    }
    for (k, v) in env {
        map.insert(k.clone(), v.clone());
    }
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in map {
        for u in OsStr::new(&format!("{k}={v}")).encode_wide() {
            block.push(u);
        }
        block.push(0);
    }
    block.push(0); // final terminator (block is "k=v\0k=v\0\0")
    block
}

fn open_nul() -> HANDLE {
    let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
    sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
    sa.bInheritHandle = 1;
    let name = to_wide("NUL");
    unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &sa,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    }
}

/// Spawn a child with an arbitrary stdio fd layout. Pipes are created per the
/// `stdio` spec; the child inherits the appropriate ends as numbered fds via
/// the CRT fd-table, and the parent keeps the other ends for I/O.
#[allow(clippy::too_many_arguments)]
pub fn spawn_extra(
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    env: Option<&[(String, String)]>,
    clear_env: bool,
    stdio: &[StdioFd],
) -> Result<RawChild, String> {
    unsafe {
        let mut sa: SECURITY_ATTRIBUTES = std::mem::zeroed();
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.bInheritHandle = 1;

        // Child-side handles (inherited, indexed by fd) and parent-side ends.
        let mut child_handles: Vec<HANDLE> = Vec::with_capacity(stdio.len());
        let mut parent_fds: HashMap<u32, FdEnd> = HashMap::new();
        // Track handles to close on the spawn path (child ends after spawn; all
        // on failure).
        let mut child_close: Vec<HANDLE> = Vec::new();
        let mut nul_handles: Vec<HANDLE> = Vec::new();

        let cleanup_fail =
            |child_close: &[HANDLE], nul: &[HANDLE], parent: &HashMap<u32, FdEnd>| {
                for h in child_close {
                    CloseHandle(*h);
                }
                for h in nul {
                    CloseHandle(*h);
                }
                for fd in parent.values() {
                    if let Some(SendHandle(h)) = fd.handle {
                        CloseHandle(h);
                    }
                }
            };

        for (i, spec) in stdio.iter().enumerate() {
            let fd = i as u32;
            match spec {
                StdioFd::Ignore => {
                    let nul = open_nul();
                    nul_handles.push(nul);
                    child_handles.push(nul);
                }
                StdioFd::Inherit => {
                    let h = match fd {
                        0 => GetStdHandle(STD_INPUT_HANDLE),
                        1 => GetStdHandle(STD_OUTPUT_HANDLE),
                        _ => GetStdHandle(STD_ERROR_HANDLE),
                    };
                    child_handles.push(h);
                }
                StdioFd::Descriptor(raw) => {
                    // Re-duplicated as INHERITABLE: the caller's handle is not,
                    // and the child's CRT fd table can only receive handles
                    // marked for inheritance. The dup is ours, so it lands in
                    // child_close and the caller's descriptor is untouched.
                    let mut dup: HANDLE = std::ptr::null_mut();
                    if DuplicateHandle(
                        GetCurrentProcess(),
                        *raw as HANDLE,
                        GetCurrentProcess(),
                        &mut dup,
                        0,
                        1, // inheritable
                        DUPLICATE_SAME_ACCESS,
                    ) == 0
                    {
                        cleanup_fail(&child_close, &nul_handles, &parent_fds);
                        return Err(format!(
                            "DuplicateHandle(fd{fd}) failed: {}",
                            GetLastError()
                        ));
                    }
                    child_close.push(dup);
                    child_handles.push(dup);
                }
                StdioFd::ChildRead => {
                    // child reads -> child gets READ end; parent WRITES.
                    let mut child_read: HANDLE = std::ptr::null_mut();
                    let mut parent_write: HANDLE = std::ptr::null_mut();
                    if CreatePipe(&mut child_read, &mut parent_write, &sa, 0) == 0 {
                        cleanup_fail(&child_close, &nul_handles, &parent_fds);
                        return Err(format!("CreatePipe(fd{fd}) failed: {}", GetLastError()));
                    }
                    SetHandleInformation(parent_write, HANDLE_FLAG_INHERIT, 0);
                    child_close.push(child_read);
                    child_handles.push(child_read);
                    parent_fds.insert(
                        fd,
                        FdEnd {
                            handle: Some(SendHandle(parent_write)),
                            readable: false,
                        },
                    );
                }
                StdioFd::ChildWrite => {
                    // child writes -> child gets WRITE end; parent READS.
                    let mut parent_read: HANDLE = std::ptr::null_mut();
                    let mut child_write: HANDLE = std::ptr::null_mut();
                    if CreatePipe(&mut parent_read, &mut child_write, &sa, 0) == 0 {
                        cleanup_fail(&child_close, &nul_handles, &parent_fds);
                        return Err(format!("CreatePipe(fd{fd}) failed: {}", GetLastError()));
                    }
                    SetHandleInformation(parent_read, HANDLE_FLAG_INHERIT, 0);
                    child_close.push(child_write);
                    child_handles.push(child_write);
                    parent_fds.insert(
                        fd,
                        FdEnd {
                            handle: Some(SendHandle(parent_read)),
                            readable: true,
                        },
                    );
                }
            }
        }

        // MSVCRT lpReserved2 block: int count; u8 flags[count]; HANDLE[count].
        let count = child_handles.len() as i32;
        let ptr_size = std::mem::size_of::<usize>();
        let mut blob: Vec<u8> = Vec::with_capacity(4 + child_handles.len() * (1 + ptr_size));
        blob.extend_from_slice(&count.to_ne_bytes());
        for h in &child_handles {
            blob.push(crt_flag(*h));
        }
        for h in &child_handles {
            blob.extend_from_slice(&(*h as usize).to_ne_bytes());
        }

        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdInput = child_handles
            .first()
            .copied()
            .unwrap_or(INVALID_HANDLE_VALUE);
        si.hStdOutput = child_handles
            .get(1)
            .copied()
            .unwrap_or(INVALID_HANDLE_VALUE);
        si.hStdError = child_handles
            .get(2)
            .copied()
            .unwrap_or(INVALID_HANDLE_VALUE);
        si.cbReserved2 = blob.len() as u16;
        si.lpReserved2 = blob.as_mut_ptr();

        // Command line.
        let mut cmdline = String::new();
        append_quoted(command, &mut cmdline);
        for a in args {
            cmdline.push(' ');
            append_quoted(a, &mut cmdline);
        }
        let mut cmd_w = to_wide(&cmdline);

        // Environment block (optional).
        let mut env_block;
        let (env_ptr, creation_flags) = match env {
            Some(env) => {
                env_block = build_env_block(env, clear_env);
                (
                    env_block.as_mut_ptr() as *const std::ffi::c_void,
                    CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                )
            }
            None if clear_env => {
                env_block = build_env_block(&[], true);
                (
                    env_block.as_mut_ptr() as *const std::ffi::c_void,
                    CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                )
            }
            None => (std::ptr::null(), CREATE_NO_WINDOW),
        };

        let cwd_w = cwd.map(to_wide);
        let cwd_ptr = cwd_w.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());

        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            std::ptr::null(),
            cmd_w.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // bInheritHandles
            creation_flags,
            env_ptr,
            cwd_ptr,
            &si,
            &mut pi,
        );
        if ok == 0 {
            let err = GetLastError();
            cleanup_fail(&child_close, &nul_handles, &parent_fds);
            let code = if err == 2 { "ENOENT" } else { "UNKNOWN" };
            return Err(format!(
                "{{\"code\":\"{code}\",\"message\":\"CreateProcessW failed (GetLastError={err}) for {}\"}}",
                command.replace('"', "\\\"")
            ));
        }

        // Parent no longer needs the child's pipe ends or the NUL handles.
        for h in &child_close {
            CloseHandle(*h);
        }
        for h in &nul_handles {
            CloseHandle(*h);
        }
        CloseHandle(pi.hThread);

        Ok(RawChild {
            process: Some(SendHandle(pi.hProcess)),
            pid: pi.dwProcessId,
            fds: parent_fds,
            kill_signal: None,
        })
    }
}

/// Read up to 64 KiB from a parent-readable fd end. EOF (0 bytes) -> Done.
pub async fn raw_read(reg: RawChildRegistry, id: u64, fd: u32) -> OpOutcome {
    let handle = {
        let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
            Some(end) if end.readable => end.handle.take(),
            Some(_) => return OpOutcome::Failed(format!("fd {fd} is not readable")),
            None => return OpOutcome::Done,
        }
    };
    let Some(h) = handle else {
        return OpOutcome::Done;
    };
    // Copy to a usize so the closure captures a Send value, not the raw
    // pointer field (2021 disjoint capture would otherwise grab `h.0`).
    let raw = h.0 as usize;
    let result = tokio::task::spawn_blocking(move || {
        let hh = raw as HANDLE;
        let mut buf = vec![0u8; 65536];
        let mut n: u32 = 0;
        let ok = unsafe { ReadFile(hh, buf.as_mut_ptr(), 65536, &mut n, std::ptr::null_mut()) };
        if ok == 0 {
            // ERROR_BROKEN_PIPE (109) on a closed write end == clean EOF.
            let err = unsafe { GetLastError() };
            if err == 109 {
                Ok(None)
            } else {
                Err(format!("ReadFile: {err}"))
            }
        } else if n == 0 {
            Ok(None)
        } else {
            buf.truncate(n as usize);
            Ok(Some(buf))
        }
    })
    .await;

    match result {
        Ok(Ok(Some(bytes))) => {
            let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(end) = guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
                end.handle = Some(h);
            } else {
                unsafe { CloseHandle(h.0) };
            }
            OpOutcome::Bytes(bytes)
        }
        Ok(Ok(None)) => {
            unsafe { CloseHandle(h.0) };
            OpOutcome::Done
        }
        Ok(Err(e)) => {
            unsafe { CloseHandle(h.0) };
            OpOutcome::Failed(e)
        }
        Err(e) => {
            unsafe { CloseHandle(h.0) };
            OpOutcome::Failed(format!("read join: {e}"))
        }
    }
}

/// Write all bytes to a parent-writable fd end.
pub async fn raw_write(reg: RawChildRegistry, id: u64, fd: u32, data: Vec<u8>) -> OpOutcome {
    let handle = {
        let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
            Some(end) if !end.readable => end.handle.take(),
            Some(_) => return OpOutcome::Failed(format!("fd {fd} is not writable")),
            None => return OpOutcome::Failed(format!("unknown fd {fd}")),
        }
    };
    let Some(h) = handle else {
        return OpOutcome::Failed(format!("fd {fd} not available"));
    };
    let raw = h.0 as usize;
    let result = tokio::task::spawn_blocking(move || {
        let hh = raw as HANDLE;
        let mut off = 0usize;
        while off < data.len() {
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    hh,
                    data[off..].as_ptr(),
                    (data.len() - off) as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(format!("WriteFile: {}", unsafe { GetLastError() }));
            }
            if written == 0 {
                return Err("WriteFile wrote 0".to_string());
            }
            off += written as usize;
        }
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(end) = guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd)) {
                end.handle = Some(h);
            } else {
                unsafe { CloseHandle(h.0) };
            }
            OpOutcome::Done
        }
        Ok(Err(e)) => {
            unsafe { CloseHandle(h.0) };
            OpOutcome::Failed(e)
        }
        Err(e) => {
            unsafe { CloseHandle(h.0) };
            OpOutcome::Failed(format!("write join: {e}"))
        }
    }
}

/// Close one parent-side fd end (e.g. end stdin / a CDP pipe).
pub fn raw_close_fd(reg: &RawChildRegistry, id: u64, fd: u32) {
    let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(end) = guard.get_mut(&id).and_then(|c| c.fds.get_mut(&fd))
        && let Some(SendHandle(h)) = end.handle.take()
    {
        unsafe { CloseHandle(h) };
    }
}

/// Record the kill signal and terminate the process. WaitForSingleObject in
/// `raw_wait` then returns and reports the exit.
pub fn raw_kill(reg: &RawChildRegistry, id: u64, signal: Option<String>) {
    let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(child) = guard.get_mut(&id) {
        child.kill_signal = signal;
        if let Some(SendHandle(h)) = child.process {
            unsafe { TerminateProcess(h, 1) };
        }
    }
}

/// Wait for exit; returns Node-shaped {code, signal}. Closes all remaining
/// handles and drops the registry entry.
pub async fn raw_wait(reg: RawChildRegistry, id: u64) -> OpOutcome {
    let process = {
        let guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(&id).and_then(|c| c.process)
    };
    let Some(proc_h) = process else {
        return OpOutcome::Failed("unknown child handle".to_string());
    };

    let raw = proc_h.0 as usize;
    let exit = tokio::task::spawn_blocking(move || {
        let hh = raw as HANDLE;
        let wait = unsafe { WaitForSingleObject(hh, INFINITE) };
        if wait != WAIT_OBJECT_0 {
            return Err(format!("WaitForSingleObject: {wait}"));
        }
        let mut code: u32 = 0;
        unsafe { GetExitCodeProcess(hh, &mut code) };
        Ok(code)
    })
    .await;

    // Drop the entry, close every handle we still own.
    let (kill_signal, fds, proc_handle) = {
        let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        match guard.remove(&id) {
            Some(c) => (c.kill_signal, c.fds, c.process),
            None => (None, HashMap::new(), None),
        }
    };
    for end in fds.values() {
        if let Some(SendHandle(h)) = end.handle {
            unsafe { CloseHandle(h) };
        }
    }
    if let Some(SendHandle(h)) = proc_handle {
        unsafe { CloseHandle(h) };
    }

    match exit {
        Ok(Ok(code)) => {
            // A TerminateProcess(.,1) kill yields exit code 1; report Node-style
            // code:null + signal when we initiated the kill.
            let json = if let Some(sig) = kill_signal {
                serde_json::json!({ "code": null, "signal": sig })
            } else {
                serde_json::json!({ "code": code as i32, "signal": null })
            };
            OpOutcome::Json(json.to_string())
        }
        Ok(Err(e)) => OpOutcome::Failed(e),
        Err(e) => OpOutcome::Failed(format!("wait join: {e}")),
    }
}
