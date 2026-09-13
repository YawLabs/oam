//! The kill-on-close job object every non-detached child is placed in, so a
//! Windows child dies with the oam process that spawned it.
//!
//! This is libuv's contract, and node inherits it from `uv_spawn`
//! (`src/win/process.c`, `uv__init_global_job_handle`). Without it a child
//! outlived its killed parent on oam while dying on node: a sidecar that
//! launched a browser left the browser running after the sidecar was killed,
//! and a launcher's nested `oam` lingered. POSIX has no equivalent to match --
//! a child there survives its parent on both runtimes.
//!
//! Semantics, matched to libuv v1.x flag for flag:
//!
//! - ONE process-global job, created lazily on the first spawn. Its handle is
//!   non-inheritable and never handed out, so this process holds the only
//!   reference and the kernel closes it when the process ends by any route --
//!   clean exit, crash, or `TerminateProcess`. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//!   then terminates every process still in the job.
//! - `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`: only processes explicitly assigned
//!   here are members; the processes THEY spawn are not. So killing oam kills
//!   its direct children, not a grandchild a child started on its own. A
//!   libuv-based child (node, oam) runs its own job for its own children, which
//!   is how a whole node tree still goes down level by level.
//! - `JOB_OBJECT_LIMIT_BREAKAWAY_OK`: a member may still spawn with
//!   `CREATE_BREAKAWAY_FROM_JOB`.
//! - `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`: members run with
//!   `SEM_NOGPFAULTERRORBOX`, so a crashing child terminates instead of parking
//!   on an error dialog that would keep it alive.
//! - `detached: true` children are never assigned: they exist to outlive the
//!   parent. The callers own that check.
//! - This process assigns ITSELF as well, as libuv does. libuv's reason, quoted
//!   from its source: a kernel bug made the handle unusable (error 87) when the
//!   first `AssignProcessToJobObject` on it was for a Windows Store program, and
//!   adding the current process first ties the job to this session. Because of
//!   SILENT_BREAKAWAY_OK the self-membership pulls no later child in implicitly
//!   -- including the deliberately detached oam_ts type-check daemon, which is
//!   spawned without going through here.
//!
//! Every failure is non-fatal. libuv ignores `ERROR_ACCESS_DENIED` from the
//! assignment (a parent job that forbids nesting) and aborts the process on any
//! other error; oam ignores both. Failing a spawn -- or the runtime -- over a
//! missing lifetime link is worse than degrading to the pre-job behaviour.
//!
//! Ordering: libuv assigns a non-detached child AFTER a normal, already-running
//! `CreateProcessW`. The raw spawn path (child_win.rs) does better and creates
//! the child suspended, assigns, then resumes, so no instruction of the child
//! runs outside the job. The `std`/`tokio` `Command` paths cannot resume a
//! suspended child (neither exposes the primary thread handle), so they assign
//! immediately after spawn, exactly as libuv does. The window that leaves is
//! narrower than it looks: a process the child creates inside it would not
//! have been a member anyway (SILENT_BREAKAWAY_OK), so the only loss is a
//! parent killed in the microseconds between spawn and assignment.

use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::sync::OnceLock;

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// `None` once creation failed: every later spawn then runs without the job
/// rather than retrying a call that already failed on this process.
static JOB: OnceLock<Option<OwnedHandle>> = OnceLock::new();

fn create_job() -> Option<OwnedHandle> {
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK
        | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: CreateJobObjectW is passed two nulls -- default security with a
    // NON-inheritable handle, and an anonymous job -- and its result is checked
    // for null before `OwnedHandle::from_raw_handle` takes sole ownership of it.
    // SetInformationJobObject reads exactly size_of::<JOBOBJECT_EXTENDED_LIMIT_
    // INFORMATION>() bytes through a pointer to `info`, a live, fully
    // initialized local of that type. GetCurrentProcess returns a pseudo-handle
    // that is always valid for the calling process and must not be closed, and
    // `raw` stays open for both calls because `job` owns it until the return.
    unsafe {
        let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if raw.is_null() {
            return None;
        }
        let job = OwnedHandle::from_raw_handle(raw);
        if SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            // A job without KILL_ON_JOB_CLOSE would only pretend to tie child
            // lifetimes; dropping `job` closes it.
            return None;
        }
        AssignProcessToJobObject(raw, GetCurrentProcess());
        Some(job)
    }
}

/// Put a just-spawned, NON-detached child in the kill-on-close job. Best
/// effort by design (module docs).
pub(crate) fn adopt(process: BorrowedHandle<'_>) {
    let Some(job) = JOB.get_or_init(create_job) else {
        return;
    };
    // SAFETY: `job` lives in a process-lifetime static and is never closed, and
    // `process` is a BorrowedHandle, so the process handle it names stays open
    // for at least this call. Neither argument points into our memory.
    unsafe {
        AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle());
    }
}

/// [`adopt`] for a tokio child, which exposes its process handle only raw.
pub(crate) fn adopt_tokio_child(child: &tokio::process::Child) {
    if let Some(raw) = child.raw_handle() {
        // SAFETY: `raw_handle()` is the process handle `child` owns, and `child`
        // is borrowed for this whole call, so that handle cannot be closed --
        // and its value recycled -- before `adopt` returns.
        adopt(unsafe { BorrowedHandle::borrow_raw(raw) });
    }
}
