//! oam_core: the engine-agnostic async substrate.
//!
//! Owns the tokio runtime and the op-completion channel. Deliberately
//! v8-free: ops are spawned as plain futures producing [`OpOutcome`]
//! payloads; oam_engine bridges completions to V8 promises on the isolate
//! thread. Completions travel over a std::sync::mpsc channel because the
//! event loop consumes them from synchronous code (recv_timeout doubles as
//! the loop's idle sleep).
//!
//! Roadmap: the #[op] macro, the io_uring/IOCP completion IoDriver, and
//! per-workload tokio tuning land here as the op surface grows.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::mpsc;
use std::time::Instant;

use futures_util::FutureExt;

pub use oam_diagnostics as diagnostics;

pub mod byte_pipe;
pub mod child;
pub mod cluster;
pub mod dns;
/// oam's own HTTP client transport for the `fetch` op (#143).
pub mod http_client;
/// node's acceptance rules for inbound HTTP/1 request heads, and its
/// `maxHeaderSize` count.
pub mod http_conn;
pub mod http_head;
pub mod http_server;
pub mod inspector;
/// The outbound TCP connector net.connect and tls.connect share: node's
/// lookupAndConnectMultiple algorithm and its error shapes.
pub mod net_connect;
/// Inbound OS signal delivery (SIGTERM/SIGINT/SIGHUP). Unix uses
/// tokio::signal::unix; Windows uses SetConsoleCtrlHandler. Both feed the op
/// channel with an OpCompletion{ id: SIGNAL_OP_ID, .. } that the engine
/// dispatches to process's JS listeners.
pub mod signal;
/// `process.stdin`'s blocking read and the pending-read gate that lets a
/// Windows console-mode switch cancel a read already blocked under the old
/// mode (libuv's `uv__cancel_read_console`).
pub mod stdin;
pub mod tcp;
pub mod tls;
pub mod udp;
pub mod websocket;
pub mod worker;

/// io_uring FS fast path (Linux-only, opt-in via OAM_IO_URING). See
/// docs/design/io_uring.md. cfg'd out on every other platform.
#[cfg(target_os = "linux")]
mod io_uring_fs;

#[cfg(unix)]
pub mod child_unix;
/// Extra-fd stdio (numbered child fds beyond 0/1/2, for CDP-over-pipe). Two
/// platform backends with one shared public surface, re-exported as
/// `child_extra` so the engine ops are `cfg(any(windows, unix))` over one path.
/// Windows: raw CreateProcessW + lpReserved2 (child_win.rs). Unix: Command +
/// pre_exec dup2 (child_unix.rs).
#[cfg(windows)]
pub mod child_win;
#[cfg(unix)]
pub use child_unix as child_extra;
#[cfg(windows)]
pub use child_win as child_extra;
/// The kill-on-close job object that ties a non-detached Windows child's
/// lifetime to this process (libuv parity). Every Windows spawn path that
/// backs `child_process` or `cluster` goes through it.
#[cfg(windows)]
mod job_win;

pub type OpId = u64;

/// Sentinel op id for inbound OS-signal completions. `next_id` starts at 1, so
/// no real spawned op ever uses 0 — the engine's settle path uses this to
/// recognize a signal (which has no parked PromiseResolver and was never
/// counted in `inflight`) and route it to `process.emit(name)`.
pub const SIGNAL_OP_ID: OpId = 0;

/// One Node system error, fully shaped on the native side: the fields node
/// puts on the error a libuv or resolver failure produces.
///
/// Two of node's error classes are built from exactly these fields (node
/// v22.22.2 lib/internal/errors.js): `ExceptionWithHostPort` for a connect
/// failure (`errno, code, syscall, address, port`, message `connect
/// ECONNREFUSED 127.0.0.1:8080`) and `DNSException` for a resolver failure
/// (`errno, code, syscall, hostname`, message `getaddrinfo ENOTFOUND host`).
/// The engine hands the fields to the JS factory `__oamMakeSysError`
/// (js/bootstrap.js), which picks the class from which of `address` / `port` /
/// `hostname` is present. One of these is also a child of an aggregate
/// (`OpOutcome::NodeAggregateFailed`).
///
/// `port` is `None` for port 0: node sets the key only when the port is
/// truthy, and the message then carries the address alone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeSysError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errno: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syscall: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// A `--permission` refusal an async op raised after its synchronous gate had
/// already passed -- a `fetch` whose redirect leads to a host the net grant
/// does not cover (`http_client::NetCheck`).
///
/// The engine rejects with the same error its synchronous gates throw
/// (`throw_permission_denied`): Node's `ERR_ACCESS_DENIED`, message `Access to
/// this API has been restricted`, carrying `permission` and `resource`. oam_core
/// has no permission model of its own (the grant lives in the engine), so this
/// only carries the verdict the engine's check returned.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccessDenial {
    /// The permission's name as the refusal reports it (`Net`).
    pub permission: String,
    /// What was refused, as the check saw it (a host).
    pub resource: String,
}

/// What an async op produced. v8-free by design; the engine maps these to
/// promise resolutions (Done -> undefined, Text -> string, Json -> the
/// parsed value via V8's own JSON parser, Failed -> reject with
/// Error(message)).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum OpOutcome {
    Done,
    Text(String),
    /// A JSON document; the engine parses it on the isolate thread. The
    /// structured-payload path until a zero-copy transfer lands with the
    /// op-macro work.
    Json(String),
    /// Raw bytes; the engine surfaces a Uint8Array over a fresh backing
    /// store (JS wraps it in Buffer where node: semantics apply).
    Bytes(Vec<u8>),
    Failed(String),
    /// A failure carrying a Node errno code (ENOENT, EACCES, ...). The
    /// engine rejects with an Error whose `.code` property is set —
    /// ecosystem code branches on err.code constantly (graceful-fs et al).
    ///
    /// `syscall`/`path`/`errno` complete the shape node puts on a system
    /// error. They were absent, so every ASYNC fs rejection carried `code`
    /// alone while its sync twin (throw_node_error) set all four —
    /// `fs.promises.stat("missing")` gave `syscall: undefined`, and the
    /// packages that branch on `err.syscall === "open"` or read `err.path`
    /// (graceful-fs, chokidar, rimraf) saw nothing. Optional because the
    /// non-fs producers (dns/net/tls) have no path to report.
    ///
    /// `hostname` / `address` / `port` name the peer the way node does on a
    /// resolver or connect failure (see `NodeSysError`). All three are serde
    /// defaults, so a replay file recorded before they existed still loads.
    NodeFailed {
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        syscall: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        errno: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hostname: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
    /// Every address of a multi-address connect failed: node's
    /// `NodeAggregateError` (lib/net.js `internalConnectMultiple`), one child
    /// per attempt, in attempt order. The aggregate itself has no message and
    /// takes `code` from its first child; the engine builds it through the JS
    /// factory `__oamMakeAggregateError`. A new variant rather than more
    /// fields, so a replay file recorded before it existed still loads.
    NodeAggregateFailed {
        errors: Vec<NodeSysError>,
    },
    /// A TLS certificate refusal that also carries the peer's chain, so the
    /// error the engine rejects with reports `err.cert` the way node does for
    /// a caught `ERR_TLS_CERT_ALTNAME_INVALID` (#198). The engine builds the
    /// same coded error as `NodeFailed` and hangs the two base64 chains on it
    /// (`peerCertificates` leaf-first, `storeIssuers` from the store), which
    /// tls.connect's JS reads into the socket before `getPeerCertificate(true)`.
    /// A new variant, so a replay file recorded before it existed still loads.
    NodeCertRefused {
        code: String,
        message: String,
        peer_certificates: Vec<String>,
        store_issuers: Vec<String>,
    },
    /// A `--permission` refusal raised mid-op (see [`AccessDenial`]): the
    /// engine rejects with the `ERR_ACCESS_DENIED` error its synchronous gates
    /// throw. A new variant, so a replay file recorded before it existed still
    /// loads.
    AccessDenied(AccessDenial),
    /// An inbound OS signal (payload is the Node signal name, e.g. "SIGTERM").
    /// Only ever carried on a completion whose id == SIGNAL_OP_ID; the engine
    /// maps it to `process.emit(name)` rather than resolving a promise. serde-
    /// derived so it survives worker IPC, but workers never produce it.
    Signal(String),
}

impl OpOutcome {
    /// A coded failure with no filesystem context (dns / net / tls).
    pub fn node_failed(code: impl Into<String>, message: impl Into<String>) -> Self {
        OpOutcome::NodeFailed {
            code: code.into(),
            message: message.into(),
            syscall: None,
            path: None,
            errno: None,
            hostname: None,
            address: None,
            port: None,
        }
    }

    /// A fully shaped system error (a connect or resolver failure). It never
    /// carries a filesystem path.
    pub fn sys(err: NodeSysError) -> Self {
        OpOutcome::NodeFailed {
            code: err.code,
            message: err.message,
            syscall: err.syscall,
            path: None,
            errno: err.errno,
            hostname: err.hostname,
            address: err.address,
            port: err.port,
        }
    }

    /// A coded failure carrying node's full system-error shape. Use this
    /// wherever a syscall name and path are known -- it is what makes an
    /// async rejection indistinguishable from its sync twin.
    ///
    /// `path` is `None` for an fd-based call, which has no path at all, and
    /// `Some("")` for a genuinely EMPTY path. Those are different shapes in
    /// node -- `fs.promises.open("")` rejects with `path: ''` present, while a
    /// bad-descriptor write has no `path` property -- so the caller states
    /// which it means instead of it being inferred from emptiness.
    pub fn node_failed_at(
        code: impl Into<String>,
        message: impl Into<String>,
        syscall: &str,
        path: Option<&str>,
        errno: Option<i32>,
    ) -> Self {
        OpOutcome::NodeFailed {
            code: code.into(),
            message: message.into(),
            syscall: Some(syscall.to_string()),
            path: path.map(|p| p.to_string()),
            errno,
            hostname: None,
            address: None,
            port: None,
        }
    }
}

#[derive(Debug)]
pub struct OpCompletion {
    pub id: OpId,
    pub outcome: OpOutcome,
}

/// Live streaming response bodies, keyed by handle. A std (not tokio)
/// Mutex on purpose: a reader REMOVES the body under a short lock, reads one
/// chunk with no lock held, then reinserts it — no guard ever crosses an
/// await. Single-reader discipline is guaranteed by ReadableStream's lock.
pub type BodyRegistry = http_client::body::FetchBodies;

/// Cancel tombstones for the remove-await-reinsert race: fetchBodyCancel on
/// a handle whose read is IN FLIGHT (absent from the registry) records the
/// handle here; the returning read sees it and drops the response instead of
/// reinserting -- otherwise a cancelled body silently revives and holds its
/// connection open for the rest of the run.
pub type CancelledBodies = std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u64>>>;
/// Outbound request-body channels: JS writes chunks, the fetch transport
/// drains them. The receiver is taken by `fetch` when the request goes out;
/// the sender stays here so later writes reach the in-flight request.
/// Slice 4 of docs/design/streaming-bodies.md.
///
/// Entry lifecycle (`http_client::body::StreamSlot`): a fetch that fails
/// before it sends drops the receiver, so writes resolve instead of blocking
/// on a full channel; a request that fails after it sent removes the entry;
/// `fetchBodyChannelEnd` removes it once the receiver is taken, and
/// `fetchBodyChannelCancel` removes it outright. A successful request whose
/// body JS never ends keeps `(sender, None)`: that body is still open.
pub type OutboundBodies = std::sync::Arc<
    std::sync::Mutex<
        HashMap<
            u64,
            (
                Option<tokio::sync::mpsc::Sender<Result<Vec<u8>, String>>>,
                Option<tokio::sync::mpsc::Receiver<Result<Vec<u8>, String>>>,
            ),
        >,
    >,
>;
/// Wakes an in-flight `fetch_body_read`. The tombstone set above is
/// checked only AFTER `chunk()` resolves, so a server that simply stops
/// sending leaves the read parked forever and pins the event loop. This
/// notifier makes cancellation preemptive.
pub type BodyCancelSignal = std::sync::Arc<tokio::sync::Notify>;

/// Open file handles for fs streams -- same remove-await-reinsert
/// discipline as BodyRegistry (node:stream's write queue serializes
/// access per handle). The `closed` set is the generation guard: a chunk
/// op removes the File, awaits IO unlocked, then reinserts -- but if
/// fsClose landed during that await (stream.destroy() racing an in-flight
/// read), the reinsert would resurrect a leaked fd. closed tracks ids
/// retired mid-flight so the reinsert drops the File instead.
#[derive(Default)]
pub struct FileState {
    pub files: HashMap<u64, std::fs::File>,
    pub closed: std::collections::HashSet<u64>,
}
pub type FileRegistry = std::sync::Arc<std::sync::Mutex<FileState>>;

/// The SAME registry as `FileRegistry`, kept as a name because the sync fs
/// family reads better with it at the call sites.
///
/// These were once two registries -- a tokio-backed one for the async ops and a
/// std-backed one for the sync ops -- and a descriptor allocated in one was
/// INVISIBLE to the other. Node has a single descriptor space, so
/// `fs.readSync(fdFromAsyncOpen, ...)` works there and threw EBADF here; the
/// reverse failed too. Both families always drew from the same id counter
/// (`body_ids`), so the numbers never collided -- it was purely a failed
/// lookup, which is why it failed loudly rather than reading the wrong file.
///
/// Unifying costs nothing: `tokio::fs::File` IS a `std::fs::File` operated on
/// `spawn_blocking`, so the async ops do that explicitly now and get one
/// descriptor space for free. It also deletes a real hazard on the write path
/// -- see `fs_write_chunk`.
pub type SyncFileRegistry = FileRegistry;

/// First id the runtime hands out for its OWN descriptors.
///
/// oam's fds are synthetic -- registry keys, not OS descriptors -- and the
/// counter used to start at 3. That is exactly where a parent's inherited
/// descriptors live: a launcher that spawns us with
/// `stdio: [...,'pipe','pipe']` hands us real OS fds 3 and 4, and oam's first
/// `openSync` claimed key 3 and shadowed one of them. Starting above the
/// inheritable window keeps the two spaces disjoint, so an unknown low fd is
/// unambiguously "the parent gave me this" rather than "not open yet".
///
/// 0/1/2 never reach the registry -- the JS layer routes them to the process
/// std sinks -- so the window is 3..OWN_FD_BASE.
pub const OWN_FD_BASE: u64 = 64;

/// Largest fd oam will try to adopt from its parent. Above this a miss is a
/// genuine EBADF. The CDP pipe convention uses 3 and 4; nothing real goes
/// anywhere near the ceiling.
pub const MAX_INHERITED_FD: u64 = OWN_FD_BASE - 1;

#[cfg(windows)]
mod crt_fd {
    // The CRT fd table is not enumerable, so `_get_osfhandle` is the only way to
    // ask whether a numbered fd is backed by anything. Both it and `_close`
    // invoke the invalid-parameter handler on a bad fd, which ABORTS under a
    // debug CRT, so the handler is swapped for a no-op around each call.
    // ABI: these match the CRT's own signatures; no memory of ours is passed.
    unsafe extern "C" {
        pub fn _close(fd: i32) -> i32;
        fn _get_osfhandle(fd: i32) -> isize;
        pub fn _set_thread_local_invalid_parameter_handler(
            handler: Option<unsafe extern "C" fn()>,
        ) -> Option<unsafe extern "C" fn()>;
    }
    pub unsafe extern "C" fn ignore_invalid_parameter() {}

    /// The OS handle behind CRT fd `raw`, or -1 if the fd is not open.
    ///
    /// Safe: only an integer is passed, nothing of ours is dereferenced, and
    /// the no-op invalid-parameter handler turns a bad fd into a -1 return
    /// instead of an abort. Any `i32` is an acceptable argument.
    pub fn osfhandle(raw: i32) -> isize {
        // SAFETY: `_get_osfhandle` reads the CRT fd table for `raw`; the
        // invalid-parameter handler is swapped to a no-op around the probe so a
        // bad fd yields -1 instead of aborting, then restored. No pointer of
        // ours is dereferenced.
        unsafe {
            let prev = _set_thread_local_invalid_parameter_handler(Some(ignore_invalid_parameter));
            let h = _get_osfhandle(raw);
            _set_thread_local_invalid_parameter_handler(prev);
            h
        }
    }
}

/// Duplicate a descriptor the PARENT owns into one we own.
///
/// Deliberately a dup rather than `from_raw_fd(fd)`: the inherited descriptor
/// still belongs to the parent's plumbing (on Windows the CRT fd table owns
/// the HANDLE), so taking it would double-close at exit. The dup is ours to
/// close whenever the JS side closes its fd.
///
/// Returns None when the descriptor is not open, which is the EBADF the caller
/// should surface.
#[cfg(unix)]
fn dup_inherited(fd: u64) -> Option<std::fs::File> {
    let raw = i32::try_from(fd).ok()?;
    if raw < 0 {
        return None;
    }
    // F_GETFD is the cheapest "is this open" question, and it is deliberately
    // still the RAW call: it is the probe that establishes openness, so it is
    // the one place that cannot presuppose it. `fcntl` on an arbitrary integer
    // is fully defined -- a closed or never-opened descriptor answers EBADF.
    //
    // SAFETY: `fcntl(F_GETFD)` only reads the flags of the integer fd `raw`; no
    // pointers are involved, and an invalid `raw` is reported as -1/EBADF
    // rather than being undefined.
    if unsafe { libc::fcntl(raw, libc::F_GETFD) } == -1 {
        return None;
    }
    // SAFETY: the F_GETFD probe on the line above returned successfully, so
    // `raw` names an open descriptor in this process, and it is non-negative
    // (checked above) -- both of `BorrowedFd::borrow_raw`'s requirements. The
    // borrow is confined to the `fcntl_dupfd_cloexec` call on the next line and
    // nothing in between can close `raw`: this function owns no descriptor and
    // the value came from the parent's inherited set, which is fixed at exec.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
    // FD_CLOEXEC on the dup: an fd the parent handed US is not automatically
    // something our own children should receive.
    //
    // rustix hands back an `OwnedFd`, so the descriptor arrives already owned
    // and `File::from` is an infallible, safe move of that ownership -- where
    // `File::from_raw_fd` used to be a bare assertion that the integer was
    // ours to close.
    let dup = rustix::io::fcntl_dupfd_cloexec(borrowed, 0).ok()?;
    Some(std::fs::File::from(dup))
}

#[cfg(windows)]
fn dup_inherited(fd: u64) -> Option<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let raw = i32::try_from(fd).ok()?;
    let handle = crt_fd::osfhandle(raw);
    if handle == -1 || handle as HANDLE == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut dup: HANDLE = std::ptr::null_mut();
    // SAFETY: `handle` is the live OS handle probed just above; `&mut dup` is a
    // live out-param the call fills; the other arguments are our own process
    // handle and constants.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle as HANDLE,
            GetCurrentProcess(),
            &mut dup,
            0,
            0, // not inheritable: see the FD_CLOEXEC note on the unix arm
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 || dup.is_null() {
        return None;
    }
    // SAFETY: `dup` is a freshly duplicated, non-null, owned HANDLE (checked
    // just above); the new File takes sole ownership and closes it on drop.
    Some(unsafe { std::fs::File::from_raw_handle(dup) })
}

/// Close the PARENT's original descriptor, after our dup of it is dropped.
///
/// Closing only our dup would leave the real descriptor open, and for a pipe
/// that is the difference between the peer seeing EOF and hanging forever --
/// a CDP child closing fd 4 is exactly how it says "no more messages". The CRT
/// (Windows) / kernel (unix) marks its own entry closed here, so nothing
/// double-closes it at exit.
pub fn close_inherited_fd(fd: u64) {
    #[cfg(unix)]
    if let Ok(raw) = i32::try_from(fd) {
        // SAFETY: closes the integer fd `raw`; no pointers are involved. The
        // caller guarantees this is an inherited descriptor we still own.
        unsafe { libc::close(raw) };
    }
    #[cfg(windows)]
    if let Ok(raw) = i32::try_from(fd) {
        use crt_fd::{
            _close, _set_thread_local_invalid_parameter_handler, ignore_invalid_parameter,
        };
        // SAFETY: `_close` closes the CRT fd `raw`; the invalid-parameter handler
        // is swapped to a no-op around the call so a stale fd returns an error
        // instead of aborting, then restored. Only an integer fd is passed.
        unsafe {
            let prev = _set_thread_local_invalid_parameter_handler(Some(ignore_invalid_parameter));
            _close(raw);
            _set_thread_local_invalid_parameter_handler(prev);
        }
    }
    #[cfg(not(any(unix, windows)))]
    let _ = fd;

    // The parent's original descriptor is now closed and its fd number is free
    // for the kernel to reissue. Consume the number so it can never be adopted
    // again -- see INHERITED_CONSUMED.
    consume_inherited_fd(fd);
}

/// The descriptors that were already open when the process started -- i.e. the
/// ones a PARENT handed us, snapshotted before oam can open anything of its own.
static INHERITED_AT_START: std::sync::OnceLock<std::collections::HashSet<u64>> =
    std::sync::OnceLock::new();

/// Inherited fd numbers that have since been closed via `close_inherited_fd`.
/// Once we close the parent's original, the kernel is free to reissue that fd
/// number to a later `open()` -- so re-adopting it would let a second
/// `closeSync(n)` close a descriptor the runtime now owns (a stale-snapshot
/// double-close-with-reuse). Consuming the number on close makes re-adoption
/// impossible; membership only grows, so it never falsely blocks a live fd.
static INHERITED_CONSUMED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<u64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Mark an inherited fd number as closed, so `adopt_inherited_fd` refuses it.
fn consume_inherited_fd(fd: u64) {
    INHERITED_CONSUMED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(fd);
}

/// True when `fd` is one we were born holding AND have not since closed. The
/// snapshot decides "the parent gave me this"; the consumed set removes it again
/// the moment we close it, which is what keeps a reused fd number from being
/// re-adopted.
fn inherited_eligible(fd: u64) -> bool {
    (3..=MAX_INHERITED_FD).contains(&fd)
        && INHERITED_AT_START
            .get()
            .is_some_and(|set| set.contains(&fd))
        && !INHERITED_CONSUMED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&fd)
}

/// Record which descriptors we were born holding. Call ONCE, as early in `main`
/// as possible and before opening any file.
///
/// Adoption cannot be a plain "is this fd open?" test, because on unix there is
/// a single descriptor space: a file oam opens for ITSELF lands on OS fd 3, and
/// a later `writeSync(3)` would then adopt oam's own descriptor. Reads and
/// writes through it happen to match Node (Node's fd 3 is that same file), but
/// `closeSync(3)` would not: it closes the descriptor the registry entry at 64
/// still owns, leaving a dangling `File` whose fd the OS is free to hand to the
/// next `open`. Aliasing two unrelated files is a corruption bug, not a parity
/// one.
///
/// Snapshotting at startup is what makes "the parent gave me this" decidable.
/// Windows happens to be immune -- Rust's `File` holds a HANDLE and never takes
/// a CRT fd slot, so only real inherited descriptors ever appear there -- but
/// the invariant should not depend on that.
pub fn snapshot_inherited_fds() {
    let mut found = std::collections::HashSet::new();
    for fd in 3..=MAX_INHERITED_FD {
        if probe_open(fd) {
            found.insert(fd);
        }
    }
    let _ = INHERITED_AT_START.set(found);
}

/// Is this descriptor open right now? Cheapest available question per platform.
fn probe_open(fd: u64) -> bool {
    let Ok(raw) = i32::try_from(fd) else {
        return false;
    };
    #[cfg(unix)]
    {
        // SAFETY: `fcntl(F_GETFD)` only reads the flags of the integer fd `raw`
        // to test whether it is open; no pointers are involved.
        unsafe { libc::fcntl(raw, libc::F_GETFD) != -1 }
    }
    #[cfg(windows)]
    {
        crt_fd::osfhandle(raw) != -1
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = raw;
        false
    }
}

/// Adopt an fd the parent handed us into `registry`, so the ordinary fs ops can
/// find it. No-op (returning true) if it is already there.
///
/// This is the receive half of extra-fd stdio: oam could always SPAWN a child
/// with numbered fds above 2 (the CDP pipe transport), but a child that WAS one
/// hit EBADF on `fs.writeSync(4, ...)` because its descriptors are registry
/// keys and nothing had ever put the inherited fd in the registry.
pub fn adopt_inherited_fd(registry: &SyncFileRegistry, fd: u64) -> bool {
    // Eligible = born holding it AND not since closed. The range, snapshot, and
    // consumed checks all live in inherited_eligible: without the snapshot an fd
    // oam opened for itself after startup is indistinguishable from one the
    // parent passed (see snapshot_inherited_fds); without the consumed gate a
    // second closeSync(n) after fd-number reuse would adopt -- then close -- a
    // descriptor the runtime now owns. A missing snapshot adopts nothing, which
    // is the safe answer.
    if !inherited_eligible(fd) {
        return false;
    }
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.files.contains_key(&fd) {
        return true;
    }
    // Held across the dup so two ops racing on the same fd cannot both adopt.
    match dup_inherited(fd) {
        Some(file) => {
            guard.files.insert(fd, file);
            true
        }
        None => false,
    }
}

/// Incremental zlib/brotli stream state. Each entry is an encoder or decoder
/// that accepts chunks one at a time. The JS Transform wires _transform to
/// zlibStreamWrite and _flush to zlibStreamFlush.
///
/// Variants:
/// - Compress/Decompress: flate2 gzip/deflate/deflateRaw, truly incremental.
/// - BrotliCompress/BrotliDecompress: pure-Rust brotli via the `brotli` crate.
pub enum ZlibStream {
    Compress(zlib::StreamCompressor),
    Decompress(zlib::StreamDecompressor),
    // Brotli state is large (~5 KB for the compressor); Box keeps the enum
    // discriminant compact so the gzip/deflate variants (the hot path) don't
    // pay for brotli's footprint in the HashMap registry. Heap indirection
    // is paid once per brotli stream, never on the per-chunk write path.
    BrotliCompress(Box<BrotliCompressor>),
    BrotliDecompress(Box<BrotliDecompressor>),
    HandleCompress(flate2::Compress),
    HandleDecompress(flate2::Decompress),
}

pub type ZlibRegistry = std::sync::Arc<std::sync::Mutex<HashMap<u64, ZlibStream>>>;

/// A handle whose in-flight ops JS can `ref()` / `unref()` as a unit -- the
/// key [`CoreRuntime::spawn_handle_op`] files an op under. Tagged by kind
/// even though every id below comes from the one `body_ids` counter: the tag
/// costs nothing and keeps a stdin read from ever sharing a key with a
/// socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HandleKey {
    /// `process.stdin` (one read at a time; see [`crate::stdin::stdin_read`]).
    Stdin,
    /// A `net.Socket` stream: its parked read.
    Tcp(u64),
    /// A `net.Server` / `tls.Server` listener: its parked accept.
    TcpServer(u64),
    /// A `tls.TLSSocket`: its parked read.
    Tls(u64),
}

/// What [`CoreRuntime`] remembers per [`HandleKey`].
struct HandleRef {
    /// Counts toward `inflight` -- true until JS calls `unref()`.
    referenced: bool,
    /// The ops in flight under this key.
    ops: HashSet<OpId>,
}

impl HandleRef {
    fn new() -> Self {
        Self {
            referenced: true,
            ops: HashSet::new(),
        }
    }
}

pub struct CoreRuntime {
    /// Option so Drop can take it for shutdown_background (see below).
    tokio: Option<tokio::runtime::Runtime>,
    /// The fetch transport: oam's pooled HTTP client (#143). Owned per
    /// CoreRuntime so pooled connections, which live on this runtime's tokio,
    /// never outlive it.
    http: http_client::HttpTransport,
    /// Hook-mode fetches parked on their `connect.lookup` hook
    /// (`http_client::send`); dropped with the run.
    fetch_continuations: http_client::send::FetchContinuations,
    /// Names `netResolve` resolved for a net / tls connect, by ticket, until
    /// the connect redeems them (`net_connect::ResolvedAnswers`); dropped
    /// with the run.
    resolved_answers: net_connect::ResolvedAnswers,
    /// http.request exchanges over a JS socket (`http_client::bridge`);
    /// dropped with the run.
    http_bridges: http_client::bridge::Bridges,
    /// Pipes TLS runs over for `tls.connect({ socket })` (`byte_pipe`) --
    /// and, the same kind of pipe, what an http2.connect session and a fetch
    /// over an undici connect function's socket run over; dropped with the
    /// run.
    tls_pipes: byte_pipe::Pipes,
    /// http2.connect sessions over a pipe (`http_client::h2_session`);
    /// dropped with the run.
    h2_sessions: http_client::h2_session::H2Sessions,
    tx: mpsc::Sender<OpCompletion>,
    rx: mpsc::Receiver<OpCompletion>,
    next_id: OpId,
    inflight: usize,
    /// Ops that must NOT keep the event loop alive. A passive watcher (e.g.
    /// the http response close-watcher) observes something that may never
    /// happen; counting it in `inflight` pins the process forever. Same
    /// rationale as SIGNAL_OP_ID, but per-op rather than a fixed id.
    unref_ops: HashSet<OpId>,
    /// Per-handle ref accounting: the ops in flight under each handle JS can
    /// `ref()` / `unref()` (a socket's parked read, a server's parked accept,
    /// the stdin read) and whether they count toward `inflight`. Node lets
    /// an unref'd handle go on working without keeping the process alive;
    /// here the op cannot be cancelled while it is blocked in the OS, so it
    /// keeps running and merely stops counting. Knowing the live ids is what
    /// makes [`Self::set_handle_ref`] safe -- flipping the ref-ness of an op
    /// that has already settled would corrupt `inflight`. Self-pruning: an
    /// entry in its default state (referenced, nothing in flight) is dropped,
    /// so the map only ever holds live or unref'd handles.
    handles: HashMap<HandleKey, HandleRef>,
    /// Reverse index so [`Self::note_settled`] finds an op's handle in O(1).
    op_handles: HashMap<OpId, HandleKey>,
    /// Installed OS-signal watchers keyed by Node signal name (SIGTERM, ...).
    /// Each value keeps a native handler alive; dropping it uninstalls (Unix
    /// aborts the tokio recv task, Windows removes the name from the active
    /// set). A signal watcher deliberately does NOT count toward `inflight` —
    /// a bare listener must not pin the event loop (Node parity).
    signals: HashMap<String, signal::SignalHandle>,
    bodies: BodyRegistry,
    cancelled_bodies: CancelledBodies,
    body_cancel_signal: BodyCancelSignal,
    files: FileRegistry,
    zlib_streams: ZlibRegistry,
    http_state: std::sync::Arc<http_server::HttpState>,
    tcp: tcp::TcpRegistry,
    tls: tls::TlsRegistry,
    udp: udp::UdpRegistry,
    ws: websocket::WsRegistry,
    workers: worker::WorkerRegistry,
    children: child::ChildRegistry,
    #[cfg(any(windows, unix))]
    raw_children: child_extra::RawChildRegistry,
    // Generic opaque-handle allocator (HTTP body ids, sync file fds, ...).
    // Starts at 3 so file descriptors returned by openSync never collide with
    // the reserved stdio fds 0 (stdin) / 1 (stdout) / 2 (stderr): the fs write
    // shim routes fd 1/2 to the stdout/stderr sinks, which is only correct if a
    // real file can never be handed those numbers (Node reserves them too).
    next_body: std::sync::Arc<std::sync::atomic::AtomicU64>,
    outbound_bodies: OutboundBodies,
}

impl CoreRuntime {
    pub fn new() -> Result<Self, String> {
        // Process-wide TLS provider: ring (see workspace Cargo.toml for why
        // not aws-lc-rs), with its suites in Node's order of preference
        // (`tls::node_crypto_provider`). Err means a provider is already
        // installed: fine -- every config this crate builds names the
        // provider itself, so the default only serves code that asks for
        // one by default.
        static TLS_PROVIDER: std::sync::Once = std::sync::Once::new();
        TLS_PROVIDER.call_once(|| {
            let _ = tls::node_crypto_provider_value().install_default();
        });
        // NODE_EXTRA_CA_CERTS: read once, here at boot, with Node's warning
        // on stderr if the file will not load -- before any script runs and
        // whether or not a TLS connection ever follows, as Node does.
        tls::extra_ca_certs();
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("oam-io")
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        // Infallible and spawns nothing: the platform TLS configs (with the
        // NODE_EXTRA_CA_CERTS roots read above) are built on the first https
        // request, so a verifier that cannot be built fails that request, not
        // boot. No `localhost` override: node dials the addresses in resolver
        // order (::1 then 127.0.0.1 on Windows), and a refused ::1 costs about
        // a millisecond with the loopback RTO ioctl net_connect sets.
        let http = http_client::HttpTransport::new();
        let (tx, rx) = mpsc::channel();
        Ok(Self {
            tokio: Some(tokio),
            http,
            fetch_continuations: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            resolved_answers: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            http_bridges: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            tls_pipes: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            h2_sessions: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            tx,
            rx,
            next_id: 1,
            inflight: 0,
            unref_ops: HashSet::new(),
            handles: HashMap::new(),
            op_handles: HashMap::new(),
            signals: HashMap::new(),
            bodies: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            cancelled_bodies: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            body_cancel_signal: std::sync::Arc::new(tokio::sync::Notify::new()),
            files: std::sync::Arc::new(std::sync::Mutex::new(FileState::default())),
            zlib_streams: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            http_state: std::sync::Arc::new(http_server::HttpState::default()),
            tcp: std::sync::Arc::new(std::sync::Mutex::new(tcp::TcpState::default())),
            tls: std::sync::Arc::new(std::sync::Mutex::new(tls::TlsState::default())),
            udp: std::sync::Arc::new(std::sync::Mutex::new(udp::UdpState::default())),
            ws: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            workers: std::sync::Arc::new(std::sync::Mutex::new(worker::WorkerState::default())),
            children: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            #[cfg(any(windows, unix))]
            raw_children: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            // Starts above the inheritable-fd window, not at 3: see OWN_FD_BASE.
            next_body: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(OWN_FD_BASE)),
            outbound_bodies: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    /// Open an outbound request-body channel and return its handle. Lives
    /// here rather than in the engine so channel construction stays with the
    /// tokio runtime that owns it. Bounded: a producer outrunning the socket
    /// awaits instead of buffering the whole body.
    pub fn new_outbound_body(&self) -> u64 {
        let handle = self
            .next_body
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, String>>(8);
        self.outbound_bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle, (Some(tx), Some(rx)));
        handle
    }

    /// Outbound request-body channels (Arc clone). Slice 4.
    pub fn outbound_bodies(&self) -> OutboundBodies {
        self.outbound_bodies.clone()
    }

    /// `fetchBodyChannelEnd`: end the request body under `handle`, and drop
    /// its channel entry if the fetch already took the receiver.
    pub fn end_outbound_body(&self, handle: u64) {
        http_client::body::end_outbound(&self.outbound_bodies, handle);
    }

    /// Cheap Arc clone for ops that need the pooled HTTP transport.
    pub fn http_client(&self) -> http_client::HttpTransport {
        self.http.clone()
    }

    /// Hook-mode fetches waiting on their `connect.lookup` hook (Arc clone).
    pub fn fetch_continuations(&self) -> http_client::send::FetchContinuations {
        self.fetch_continuations.clone()
    }

    /// Addresses resolved ahead of a net / tls connect, by ticket (Arc
    /// clone).
    pub fn resolved_answers(&self) -> net_connect::ResolvedAnswers {
        self.resolved_answers.clone()
    }

    /// http.request exchanges over a JS socket (Arc clone).
    pub fn http_bridges(&self) -> http_client::bridge::Bridges {
        self.http_bridges.clone()
    }

    /// The pipes TLS runs over for `tls.connect({ socket })` (Arc clone);
    /// http2.connect sessions and supplied fetch connections use them too.
    pub fn tls_pipes(&self) -> byte_pipe::Pipes {
        self.tls_pipes.clone()
    }

    /// http2.connect sessions (Arc clone).
    pub fn h2_sessions(&self) -> http_client::h2_session::H2Sessions {
        self.h2_sessions.clone()
    }

    /// The streaming-body registry (Arc clone). Dies with the CoreRuntime,
    /// so per-run resets drop any unread bodies.
    pub fn bodies(&self) -> BodyRegistry {
        self.bodies.clone()
    }

    /// Wakes any parked fetch body read so cancellation is preemptive.
    pub fn body_cancel_signal(&self) -> BodyCancelSignal {
        self.body_cancel_signal.clone()
    }

    /// Cancel tombstones paired with `bodies()` (see CancelledBodies).
    pub fn cancelled_bodies(&self) -> CancelledBodies {
        self.cancelled_bodies.clone()
    }

    /// Allocator handle for new streaming bodies AND file handles (one id
    /// space; the registries are separate).
    pub fn body_ids(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.next_body.clone()
    }

    /// Open-file registry for fs streams (Arc clone; dies with the run).
    pub fn files(&self) -> FileRegistry {
        self.files.clone()
    }

    /// Synchronous open-file registry for fs.openSync & friends (Arc clone;
    /// dies with the run).
    /// The open-file registry, as seen by the SYNC fs family.
    ///
    /// The same registry `files()` returns -- there is one descriptor space, as
    /// in node. Kept as a distinct name only because it reads better at the
    /// sync call sites; see the note on `SyncFileRegistry`.
    pub fn sync_files(&self) -> SyncFileRegistry {
        self.files.clone()
    }

    /// Incremental zlib stream registry (Arc clone; dies with the run).
    pub fn zlib_streams(&self) -> ZlibRegistry {
        self.zlib_streams.clone()
    }

    /// HTTP server state (Arc clone; servers die with the run).
    pub fn http(&self) -> std::sync::Arc<http_server::HttpState> {
        self.http_state.clone()
    }

    /// TCP socket registry (Arc clone; dies with the run).
    pub fn tcp(&self) -> tcp::TcpRegistry {
        self.tcp.clone()
    }

    /// TLS socket registry (Arc clone; dies with the run).
    pub fn tls(&self) -> tls::TlsRegistry {
        self.tls.clone()
    }

    /// UDP socket registry (Arc clone; dies with the run).
    pub fn udp(&self) -> udp::UdpRegistry {
        self.udp.clone()
    }

    /// WebSocket connection registry (Arc clone; dies with the run).
    pub fn ws(&self) -> websocket::WsRegistry {
        self.ws.clone()
    }

    /// Worker thread registry (Arc clone; parent side).
    pub fn workers(&self) -> worker::WorkerRegistry {
        self.workers.clone()
    }

    /// Child process registry (Arc clone; dies with the run).
    /// Enter the tokio runtime context for the duration of the returned
    /// guard. Required around any tokio type constructed on the V8 thread --
    /// notably `tokio::process::Command::spawn`, which registers the child
    /// with the signal-driver reactor on Unix and panics with
    /// "there is no reactor running" outside a runtime context.
    pub fn enter(&self) -> Option<tokio::runtime::EnterGuard<'_>> {
        self.tokio.as_ref().map(|rt| rt.enter())
    }

    pub fn children(&self) -> child::ChildRegistry {
        self.children.clone()
    }

    /// Raw (extra-fd) child process registry (Arc clone). Windows + Unix.
    #[cfg(any(windows, unix))]
    pub fn raw_children(&self) -> child_extra::RawChildRegistry {
        self.raw_children.clone()
    }

    /// Spawn an async op; its completion will surface via try_recv /
    /// recv_deadline tagged with the returned id.
    pub fn spawn_op<F>(&mut self, op: F) -> OpId
    where
        F: Future<Output = OpOutcome> + Send + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        self.inflight += 1;
        let tx = self.tx.clone();
        self.tokio
            .as_ref()
            .expect("runtime alive")
            .spawn(async move {
                let outcome = match std::panic::AssertUnwindSafe(op).catch_unwind().await {
                    Ok(outcome) => outcome,
                    Err(payload) => {
                        let msg = payload
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
                            .unwrap_or("internal panic in async op");
                        OpOutcome::Failed(format!("panic: {msg}"))
                    }
                };
                let _ = tx.send(OpCompletion { id, outcome });
            });
        id
    }

    /// Spawn an op that does NOT keep the event loop alive. For passive
    /// watchers whose trigger may never arrive -- the process must still be
    /// able to exit. Completion is delivered normally if it does arrive.
    pub fn spawn_op_unref<F>(&mut self, op: F) -> OpId
    where
        F: Future<Output = OpOutcome> + Send + 'static,
    {
        let id = self.spawn_op(op);
        // spawn_op counted it; undo that and remember to skip the matching
        // decrement when the completion lands.
        self.inflight -= 1;
        self.unref_ops.insert(id);
        id
    }

    /// Spawn an op that belongs to a handle JS can `ref()` / `unref()`
    /// (`socket.unref()`, `server.unref()`, `process.stdin.unref()`): counted
    /// toward `inflight` only while `key` is referenced -- an op issued while
    /// the handle is unref'd must not pin the loop either -- and remembered
    /// under the key so [`Self::set_handle_ref`] can flip it while it is
    /// still blocked in the OS. Several ops may be in flight per key.
    pub fn spawn_handle_op<F>(&mut self, key: HandleKey, op: F) -> OpId
    where
        F: Future<Output = OpOutcome> + Send + 'static,
    {
        let referenced = match self.handles.get(&key) {
            Some(handle) => handle.referenced,
            None => true,
        };
        let id = if referenced {
            self.spawn_op(op)
        } else {
            self.spawn_op_unref(op)
        };
        self.handles
            .entry(key)
            .or_insert_with(HandleRef::new)
            .ops
            .insert(id);
        self.op_handles.insert(id, key);
        id
    }

    /// node's `handle.ref()` / `handle.unref()`: stop (or resume) the
    /// handle's ops holding the loop open. Applies to every op in flight
    /// under `key` AND to the ones issued after it. Nothing is cancelled --
    /// as in node, where an unref'd socket still reads and a stdin fd stays
    /// readable -- it just no longer counts. Idempotent, and safe after the
    /// ops settled: a settled op has already left the key's set in
    /// [`Self::note_settled`], so it is never re-counted.
    pub fn set_handle_ref(&mut self, key: HandleKey, referenced: bool) {
        let handle = self.handles.entry(key).or_insert_with(HandleRef::new);
        handle.referenced = referenced;
        for id in &handle.ops {
            if referenced {
                if self.unref_ops.remove(id) {
                    self.inflight += 1;
                }
            } else if self.unref_ops.insert(*id) {
                self.inflight -= 1;
            }
        }
        self.prune_handle(key);
    }

    /// The handle is closed: drop its bookkeeping so the maps never grow with
    /// sockets that came and went. An op still in flight under it keeps the
    /// ref-ness it has until it settles (a closed socket's parked read comes
    /// back with EOF or an error, or is woken by the close itself).
    pub fn forget_handle(&mut self, key: HandleKey) {
        if let Some(handle) = self.handles.remove(&key) {
            for id in handle.ops {
                self.op_handles.remove(&id);
            }
        }
    }

    /// Drop a key whose entry says nothing (referenced, nothing in flight).
    /// An UNREF'D key with nothing in flight is kept on purpose: the state
    /// has to survive the gap between one parked read and the next.
    fn prune_handle(&mut self, key: HandleKey) {
        if self
            .handles
            .get(&key)
            .is_some_and(|handle| handle.referenced && handle.ops.is_empty())
        {
            self.handles.remove(&key);
        }
    }

    /// The `process.stdin` read: [`Self::spawn_handle_op`] under
    /// [`HandleKey::Stdin`]. One op spans the whole of
    /// [`crate::stdin::stdin_read`], the retries the pending-read gate drives
    /// included: when a console-mode switch cancels the blocked read, the
    /// discarded result is dropped and the read re-issued INSIDE this same
    /// op. So the id stays valid across a cancel, the gate and this
    /// bookkeeping never race over it, a read retired here stays retired
    /// across those retries, and a cancel never resurrects one.
    pub fn spawn_stdin_op<F>(&mut self, op: F) -> OpId
    where
        F: Future<Output = OpOutcome> + Send + 'static,
    {
        self.spawn_handle_op(HandleKey::Stdin, op)
    }

    /// node's `stdin.ref()` / `stdin.unref()`, and what destroying the stream
    /// does: [`Self::set_handle_ref`] on [`HandleKey::Stdin`].
    pub fn set_stdin_ref(&mut self, referenced: bool) {
        self.set_handle_ref(HandleKey::Stdin, referenced);
    }

    pub fn has_inflight(&self) -> bool {
        self.inflight > 0
    }

    /// Bookkeeping shared by `try_recv` and `recv_deadline`: a settled op
    /// stops counting, and leaves its handle's in-flight set (its id must
    /// never be re-counted by `set_handle_ref` again).
    fn note_settled(&mut self, completion: &OpCompletion) {
        if completion.id == SIGNAL_OP_ID {
            // Never counted in `inflight` (a bare listener must not pin the
            // loop), so it must not decrement -- that would underflow at 0.
            return;
        }
        if let Some(key) = self.op_handles.remove(&completion.id) {
            if let Some(handle) = self.handles.get_mut(&key) {
                handle.ops.remove(&completion.id);
            }
            self.prune_handle(key);
        }
        if !self.unref_ops.remove(&completion.id) {
            self.inflight -= 1;
        }
    }

    pub fn try_recv(&mut self) -> Option<OpCompletion> {
        let completion = self.rx.try_recv().ok();
        if let Some(ref c) = completion {
            self.note_settled(c);
        }
        completion
    }

    /// Block until a completion arrives or `deadline` passes (None = wait
    /// indefinitely — only call that way when has_inflight() is true, or
    /// it blocks forever).
    pub fn recv_deadline(&mut self, deadline: Option<Instant>) -> Option<OpCompletion> {
        let completion = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if deadline <= now {
                    return self.try_recv();
                }
                self.rx.recv_timeout(deadline - now).ok()
            }
            None => self.rx.recv().ok(),
        };
        if let Some(ref c) = completion {
            self.note_settled(c);
        }
        completion
    }

    /// Begin delivering OS signal `name` (SIGTERM, SIGINT, SIGHUP, ...) to the
    /// op channel as OpCompletion{ id: SIGNAL_OP_ID, outcome: Signal(name) }.
    /// Idempotent: a second call for an already-watched signal is a no-op.
    /// Installing the native handler suppresses the OS default action while at
    /// least one watcher is live (Unix: replaces SIG_DFL; Windows: the console
    /// ctrl handler returns TRUE for the mapped event).
    pub fn start_signal(&mut self, name: &str) {
        if let Some(existing) = self.signals.get(name) {
            // A DORMANT unix handle (every listener removed, task still
            // alive) is re-armed, not recreated: tokio's signal registration
            // is a one-time global, so the task owning the stream has to
            // outlive the JS listeners.
            existing.set_watched(true);
            return;
        }
        let runtime = self.tokio.as_ref().expect("runtime alive");
        if let Some(handle) = signal::start_signal(runtime, &self.tx, name) {
            self.signals.insert(name.to_string(), handle);
        }
    }

    /// Stop delivering OS signal `name`, restoring the OS default action.
    /// Backend-specific (see `signal::stop_signal`): Windows drops the handle
    /// so its ctrl handler falls through; unix keeps it dormant for a later
    /// listener to re-arm, and `signal::serve_default_action` reproduces the
    /// default for a delivery no listener in the process is watching.
    pub fn stop_signal(&mut self, name: &str) {
        signal::stop_signal(&mut self.signals, name);
    }
}

impl Drop for CoreRuntime {
    fn drop(&mut self) {
        // The run is over: nothing on the IO runtime may block process
        // exit. A plain Runtime::drop WAITS — an idle keep-alive
        // connection (oam's fetch transport pool, 90s idle timeout; a hyper server
        // conn) turned exit into a 90-second hang. shutdown_background
        // drops everything without waiting.
        if let Some(runtime) = self.tokio.take() {
            runtime.shutdown_background();
        }
    }
}

pub use stdin::stdin_read;

/// Artifacts the process must delete before a HARD exit.
///
/// `oam -e` writes its source to a REAL file, and that file has to sit in the
/// CWD because node's eval resolves `require()` from there. The CLI deletes it
/// once the script returns -- but a script that calls `process.exit()` never
/// returns: V8 tears the process down from inside the op, so the file was left
/// behind in whatever directory the user happened to be in. Every hard-exit
/// path drains this list first.
static EXIT_CLEANUP: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Register a path to remove on a hard exit. Directories are removed
/// recursively; a path that no longer exists is ignored.
pub fn register_exit_cleanup(path: std::path::PathBuf) {
    let mut guard = EXIT_CLEANUP.lock().unwrap_or_else(|e| e.into_inner());
    guard.push(path);
}

/// Callbacks the process must run before a HARD exit -- work that would
/// otherwise be silently lost when `process.exit()` tears the process down
/// from inside an op (the CLI registers its queued loader-warning drain
/// here). Run before the artifact removal, each at most once.
static EXIT_HOOKS: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(Vec::new());

/// Callbacks that put PROCESS state back -- the terminal's cooked mode -- on
/// every way out, a signal death included. Kept apart from `EXIT_HOOKS`: a
/// death by signal restores the terminal and nothing else, as node's
/// `SignalExit` does, where the report-shaped hooks (a diagnostics drain to
/// stderr) belong to the exits that print anyway.
static PROCESS_STATE_HOOKS: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(Vec::new());

/// Register a callback to run on a hard exit, before the artifact drain.
pub fn register_exit_hook(hook: impl FnOnce() + Send + 'static) {
    let mut guard = EXIT_HOOKS.lock().unwrap_or_else(|e| e.into_inner());
    guard.push(Box::new(hook));
}

/// Register a callback that restores process state; it runs first on every
/// hard exit and alone on a signal death (`run_process_state_hooks`).
pub fn register_process_state_hook(hook: impl FnOnce() + Send + 'static) {
    let mut guard = PROCESS_STATE_HOOKS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard.push(Box::new(hook));
}

fn drain(list: &std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>>) {
    let hooks = {
        let mut guard = list.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };
    for hook in hooks {
        // A panicking hook must not abort the exit path mid-drain: swallow
        // it and keep going -- the process is exiting either way.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(hook));
    }
}

/// Run the process-state hooks only: what a death by signal owes the
/// terminal, and nothing that prints. Idempotent -- the list is taken.
pub fn run_process_state_hooks() {
    drain(&PROCESS_STATE_HOOKS);
}

/// Run every registered hook, leaving the artifact list alone.
///
/// Split out of `run_exit_cleanup` for the exit paths that must restore
/// PROCESS state -- the terminal's cooked mode, above all -- while a caller
/// further up is still holding paths it may want on disk. `main`'s fatal
/// sub-code returns render their diagnostics around this call. Idempotent:
/// both lists are taken, so a later `run_exit_cleanup` does no double work.
pub fn run_exit_hooks() {
    run_process_state_hooks();
    drain(&EXIT_HOOKS);
}

/// Run every registered hook, then remove every registered artifact.
/// Idempotent -- both lists are taken, so a normal-path cleanup followed by
/// an exit-path drain does no double work.
pub fn run_exit_cleanup() {
    run_exit_hooks();
    let paths = {
        let mut guard = EXIT_CLEANUP.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };
    for path in paths {
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// `std::process::exit`, with the artifact drain that a hard exit would
/// otherwise skip. Every `process.exit()`, fatal banner and EPIPE bail routes
/// through here rather than calling `std::process::exit` directly.
pub fn exit_process(code: i32) -> ! {
    run_exit_cleanup();
    std::process::exit(code)
}

/// The ONE place a `rustix::io::Errno` becomes a `std::io::Error`.
///
/// Load-bearing, and the reason it is a named function rather than an inline
/// `.map_err(Into::into)` at each site: on Linux rustix defaults to its
/// `linux_raw` backend, which issues raw syscalls and NEVER writes libc's
/// thread-local `errno`. So after a failed rustix call
/// `std::io::Error::last_os_error()` holds whatever unrelated value was left
/// there earlier -- the number the failing call produced is reachable ONLY
/// through the returned `Errno`.
///
/// Everything downstream of an fs op reads the error through
/// `std::io::Error::raw_os_error()` (directly in `node_errno`, and indirectly
/// via `ErrorKind` in `node_error_code`), which is exactly what
/// `from_raw_os_error` populates. Routing every rustix call site through this
/// one function is what keeps Node's `code` / `errno` / `syscall` / `path`
/// triple intact across the libc-to-rustix move.
///
/// `Errno::raw_os_error()` is POSITIVE on both backends: the `linux_raw`
/// backend stores the kernel's negated value internally and negates it back
/// here, and the libc backend (macOS) never negated it in the first place.
#[cfg(unix)]
#[inline]
pub fn io_from_errno(e: rustix::io::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e.raw_os_error())
}

/// `-1` in a `chown` uid/gid field means "leave this one alone". It reaches us
/// from JS as an unsigned `u32::MAX`, which is the same bit pattern libc's
/// `(uid_t)-1` sentinel uses -- so the old raw `libc::chown` got the behaviour
/// for free by passing the value straight through.
///
/// rustix spells the sentinel `None` instead, and `Uid::from_raw` only rejects
/// `!0` behind a `debug_assert!` -- which is compiled OUT in release. So a
/// `Uid::from_raw(u32::MAX)` would sail through a release build and be passed
/// to the kernel as a literal uid of 4294967295 rather than "no change". The
/// mapping has to be explicit here, and is covered by a test.
#[cfg(unix)]
#[inline]
pub fn chown_uid(uid: u32) -> Option<rustix::fs::Uid> {
    (uid != u32::MAX).then(|| rustix::fs::Uid::from_raw(uid))
}

/// `chown_uid`'s twin for the group field; same `-1` sentinel, same reasoning.
#[cfg(unix)]
#[inline]
pub fn chown_gid(gid: u32) -> Option<rustix::fs::Gid> {
    (gid != u32::MAX).then(|| rustix::fs::Gid::from_raw(gid))
}

/// Node's `err.errno` -- the libuv error NUMBER (negative).
///
/// On Unix it is just `-errno`: libuv's `UV__E*` macros borrow the platform's
/// own errno whenever it defines one, so `UV_ENOENT == -ENOENT`. On Windows
/// every one of those macros is guarded on `!defined(_WIN32)`, so the host
/// NEVER lends its CRT/Winsock number and libuv falls back to its own fixed
/// -4000/-3000-range value for EVERY code. The complete fallback table is
/// transcribed below; the numbers are already negative and are returned
/// verbatim (no negation is applied on this path).
///
/// This is the same table the JS side carries as `UV_FALLBACK_ERRNO` in
/// js/node_compat.js, which is what `util.getSystemErrorName` /
/// `getSystemErrorMessage` / `getSystemErrorMap` invert. The two must agree
/// entry for entry: a code missing here stamps a raw Win32/Winsock number onto
/// `err.errno` that the JS side cannot decode, and the lookup silently
/// degrades to "Unknown system error N".
///
/// A code outside the table (e.g. the DNS-only `ENOTFOUND`, which is not a
/// libuv errno at all) still falls back to `-raw_os_error` so the property is
/// at least present and negative.
///
/// Lives here rather than in oam_engine so the child-spawn failure body can
/// carry it too: node emits the errno as the `code` argument of the 'close'
/// event for a child that never started, so a spawn error without it cannot
/// reproduce node's shape.
pub fn node_errno(code: &str, error: &std::io::Error) -> Option<i32> {
    #[cfg(windows)]
    {
        // libuv's Windows fallback table, verbatim from uv/errno.h -- values
        // are the negative libuv numbers, so they are returned as-is.
        let uv = match code {
            "E2BIG" => -4093,
            "EACCES" => -4092,
            "EADDRINUSE" => -4091,
            "EADDRNOTAVAIL" => -4090,
            "EAFNOSUPPORT" => -4089,
            "EAGAIN" => -4088,
            "EAI_ADDRFAMILY" => -3000,
            "EAI_AGAIN" => -3001,
            "EAI_BADFLAGS" => -3002,
            "EAI_BADHINTS" => -3013,
            "EAI_CANCELED" => -3003,
            "EAI_FAIL" => -3004,
            "EAI_FAMILY" => -3005,
            "EAI_MEMORY" => -3006,
            "EAI_NODATA" => -3007,
            "EAI_NONAME" => -3008,
            "EAI_OVERFLOW" => -3009,
            "EAI_PROTOCOL" => -3014,
            "EAI_SERVICE" => -3010,
            "EAI_SOCKTYPE" => -3011,
            "EALREADY" => -4084,
            "EBADF" => -4083,
            "EBUSY" => -4082,
            "ECANCELED" => -4081,
            "ECHARSET" => -4080,
            "ECONNABORTED" => -4079,
            "ECONNREFUSED" => -4078,
            "ECONNRESET" => -4077,
            "EDESTADDRREQ" => -4076,
            "EEXIST" => -4075,
            "EFAULT" => -4074,
            "EFBIG" => -4036,
            "EFTYPE" => -4028,
            "EHOSTDOWN" => -4031,
            "EHOSTUNREACH" => -4073,
            "EILSEQ" => -4027,
            "EINTR" => -4072,
            "EINVAL" => -4071,
            "EIO" => -4070,
            "EISCONN" => -4069,
            "EISDIR" => -4068,
            "ELOOP" => -4067,
            "EMFILE" => -4066,
            "EMLINK" => -4032,
            "EMSGSIZE" => -4065,
            "ENAMETOOLONG" => -4064,
            "ENETDOWN" => -4063,
            "ENETUNREACH" => -4062,
            "ENFILE" => -4061,
            "ENOBUFS" => -4060,
            "ENODATA" => -4024,
            "ENODEV" => -4059,
            "ENOENT" => -4058,
            "ENOEXEC" => -4022,
            "ENOMEM" => -4057,
            "ENONET" => -4056,
            "ENOPROTOOPT" => -4035,
            "ENOSPC" => -4055,
            "ENOSYS" => -4054,
            "ENOTCONN" => -4053,
            "ENOTDIR" => -4052,
            "ENOTEMPTY" => -4051,
            "ENOTSOCK" => -4050,
            "ENOTSUP" => -4049,
            "ENOTTY" => -4029,
            "ENXIO" => -4033,
            "EOF" => -4095,
            "EOVERFLOW" => -4026,
            "EPERM" => -4048,
            "EPIPE" => -4047,
            "EPROTO" => -4046,
            "EPROTONOSUPPORT" => -4045,
            "EPROTOTYPE" => -4044,
            "ERANGE" => -4034,
            "EREMOTEIO" => -4030,
            "EROFS" => -4043,
            "ESHUTDOWN" => -4042,
            "ESOCKTNOSUPPORT" => -4025,
            "ESPIPE" => -4041,
            "ESRCH" => -4040,
            "ETIMEDOUT" => -4039,
            "ETXTBSY" => -4038,
            "EUNATCH" => -4023,
            "EXDEV" => -4037,
            "UNKNOWN" => -4094,
            _ => return error.raw_os_error().map(|e| -e),
        };
        Some(uv)
    }
    #[cfg(not(windows))]
    {
        let _ = code;
        error.raw_os_error().map(|e| -e)
    }
}

/// Map an I/O error to the Node errno code ecosystem code branches on.
/// Shared by the async fs ops below and oam_engine's sync fs natives.
///
/// An error that carries a raw OS code is translated from THAT code, the way
/// libuv does it -- not from `std::io::ErrorKind`, which is a coarser, lossy
/// view of the same number. Going through the kind is what made oam report
/// `EACCES` for every Windows access denial where node reports `EPERM`
/// (std folds ERROR_ACCESS_DENIED into `PermissionDenied`, and libuv maps it
/// to UV_EPERM), and `EIO` for a sharing violation where node reports `EBUSY`
/// (std has no kind for it). On unix the kind is right almost everywhere, but
/// std folds EPERM into `PermissionDenied` there too.
///
/// Errors with no raw code -- the ones oam builds itself, and those from
/// libraries such as rustls -- keep the `ErrorKind` mapping.
pub fn node_error_code(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    #[cfg(windows)]
    if let Some(raw) = error.raw_os_error() {
        return windows_error_code(raw);
    }
    // std's kinds fold several errnos together (EPERM into PermissionDenied
    // with EACCES) or have none for them (EMFILE, EXDEV, ENOSPC, ...), while
    // node_errno reports the real -errno, so the code has to come from the same
    // number.
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error().and_then(unix_errno_code) {
        return code;
    }
    match error.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        ErrorKind::NotADirectory => "ENOTDIR",
        ErrorKind::IsADirectory => "EISDIR",
        ErrorKind::InvalidInput => "EINVAL",
        ErrorKind::TimedOut => "ETIMEDOUT",
        ErrorKind::Interrupted => "EINTR",
        ErrorKind::Unsupported => "ENOSYS",
        ErrorKind::BrokenPipe => "EPIPE",
        ErrorKind::WouldBlock => "EAGAIN",
        // Network error kinds (std maps POSIX errno on Unix to these reliably;
        // on Windows a raw winsock code never reaches this match).
        ErrorKind::ConnectionRefused => "ECONNREFUSED",
        ErrorKind::ConnectionReset => "ECONNRESET",
        ErrorKind::ConnectionAborted => "ECONNABORTED",
        ErrorKind::NotConnected => "ENOTCONN",
        ErrorKind::AddrInUse => "EADDRINUSE",
        ErrorKind::AddrNotAvailable => "EADDRNOTAVAIL",
        ErrorKind::HostUnreachable => "EHOSTUNREACH",
        ErrorKind::NetworkUnreachable => "ENETUNREACH",
        _ => "EIO",
    }
}

/// The node code for a raw unix errno: the name libuv gives it. Only codes
/// libuv names are listed, and only one spelling of an alias (EAGAIN, not
/// EWOULDBLOCK; ENOTSUP, not EOPNOTSUPP, which is the same number on Linux).
#[cfg(unix)]
fn unix_errno_code(raw: i32) -> Option<&'static str> {
    Some(match raw {
        libc::E2BIG => "E2BIG",
        libc::EACCES => "EACCES",
        libc::EADDRINUSE => "EADDRINUSE",
        libc::EADDRNOTAVAIL => "EADDRNOTAVAIL",
        libc::EAFNOSUPPORT => "EAFNOSUPPORT",
        libc::EAGAIN => "EAGAIN",
        libc::EALREADY => "EALREADY",
        libc::EBADF => "EBADF",
        libc::EBUSY => "EBUSY",
        libc::ECANCELED => "ECANCELED",
        libc::ECONNABORTED => "ECONNABORTED",
        libc::ECONNREFUSED => "ECONNREFUSED",
        libc::ECONNRESET => "ECONNRESET",
        libc::EDESTADDRREQ => "EDESTADDRREQ",
        libc::EEXIST => "EEXIST",
        libc::EFAULT => "EFAULT",
        libc::EFBIG => "EFBIG",
        libc::EHOSTDOWN => "EHOSTDOWN",
        libc::EHOSTUNREACH => "EHOSTUNREACH",
        libc::EILSEQ => "EILSEQ",
        libc::EINTR => "EINTR",
        libc::EINVAL => "EINVAL",
        libc::EIO => "EIO",
        libc::EISCONN => "EISCONN",
        libc::EISDIR => "EISDIR",
        libc::ELOOP => "ELOOP",
        libc::EMFILE => "EMFILE",
        libc::EMLINK => "EMLINK",
        libc::EMSGSIZE => "EMSGSIZE",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::ENETDOWN => "ENETDOWN",
        libc::ENETUNREACH => "ENETUNREACH",
        libc::ENFILE => "ENFILE",
        libc::ENOBUFS => "ENOBUFS",
        libc::ENODEV => "ENODEV",
        libc::ENOENT => "ENOENT",
        libc::ENOEXEC => "ENOEXEC",
        libc::ENOMEM => "ENOMEM",
        libc::ENOPROTOOPT => "ENOPROTOOPT",
        libc::ENOSPC => "ENOSPC",
        libc::ENOSYS => "ENOSYS",
        libc::ENOTCONN => "ENOTCONN",
        libc::ENOTDIR => "ENOTDIR",
        libc::ENOTEMPTY => "ENOTEMPTY",
        libc::ENOTSOCK => "ENOTSOCK",
        libc::ENOTSUP => "ENOTSUP",
        libc::ENOTTY => "ENOTTY",
        libc::ENXIO => "ENXIO",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::EPERM => "EPERM",
        libc::EPIPE => "EPIPE",
        libc::EPROTO => "EPROTO",
        libc::EPROTONOSUPPORT => "EPROTONOSUPPORT",
        libc::EPROTOTYPE => "EPROTOTYPE",
        libc::ERANGE => "ERANGE",
        libc::EROFS => "EROFS",
        libc::ESHUTDOWN => "ESHUTDOWN",
        libc::ESPIPE => "ESPIPE",
        libc::ESRCH => "ESRCH",
        libc::ETIMEDOUT => "ETIMEDOUT",
        libc::ETXTBSY => "ETXTBSY",
        libc::EXDEV => "EXDEV",
        _ => return None,
    })
}

/// libuv's `uv_translate_sys_error` (src/win/error.c, v1.51.0) for a raw Win32
/// or Winsock error code: what node reports for that code, entry for entry,
/// with `UNKNOWN` for a code libuv does not list.
///
/// Two groups of rows depart from libuv's GENERIC table:
///
/// - WSAHOST_NOT_FOUND / WSANO_DATA / WSATRY_AGAIN come from getaddrinfo, and
///   node's dns layer names them `ENOTFOUND` / `EAI_AGAIN`; the generic table
///   would say `ENOENT` / `UNKNOWN`.
/// - ERROR_BROKEN_PIPE and ERROR_NO_DATA are `EPIPE`, from libuv's WRITE table
///   (`uv_translate_write_sys_error`); the generic table says `EOF` /
///   `EAGAIN`. oam reports a broken pipe on a read as end-of-stream before it
///   gets here -- std's handle read returns `Ok(0)` for ERROR_BROKEN_PIPE --
///   so what reaches this table is a write's.
///
/// Where libuv changes the answer for one operation only -- readdir's
/// `ENOTDIR`, mkdir's `EINVAL` -- that is `fs_error_at`'s job, not this
/// table's: the same code means something else at another call site
/// (ERROR_DIRECTORY from CreateProcessW is a bad working directory, `ENOENT`).
#[cfg(windows)]
fn windows_error_code(raw: i32) -> &'static str {
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Networking::WinSock::*;
    match raw {
        WSAHOST_NOT_FOUND | WSANO_DATA => return "ENOTFOUND",
        WSATRY_AGAIN => return "EAI_AGAIN",
        WSAEACCES => return "EACCES",
        WSAEADDRINUSE => return "EADDRINUSE",
        WSAEADDRNOTAVAIL => return "EADDRNOTAVAIL",
        WSAEAFNOSUPPORT => return "EAFNOSUPPORT",
        WSAEWOULDBLOCK => return "EAGAIN",
        WSAEALREADY => return "EALREADY",
        WSAEINTR => return "ECANCELED",
        WSAECONNABORTED => return "ECONNABORTED",
        WSAECONNREFUSED => return "ECONNREFUSED",
        WSAECONNRESET => return "ECONNRESET",
        WSAEFAULT => return "EFAULT",
        WSAEHOSTUNREACH => return "EHOSTUNREACH",
        WSAEINVAL | WSAEPFNOSUPPORT => return "EINVAL",
        WSAEISCONN => return "EISCONN",
        WSAEMFILE => return "EMFILE",
        WSAEMSGSIZE => return "EMSGSIZE",
        WSAENETUNREACH => return "ENETUNREACH",
        WSAENOBUFS => return "ENOBUFS",
        WSAENOTCONN => return "ENOTCONN",
        WSAENOTSOCK => return "ENOTSOCK",
        WSAESHUTDOWN => return "EPIPE",
        WSAEPROTONOSUPPORT => return "EPROTONOSUPPORT",
        WSAETIMEDOUT => return "ETIMEDOUT",
        WSAESOCKTNOSUPPORT => return "ESOCKTNOSUPPORT",
        _ => {}
    }
    let Ok(raw) = u32::try_from(raw) else {
        return "UNKNOWN";
    };
    match raw {
        ERROR_BROKEN_PIPE | ERROR_NO_DATA => "EPIPE",
        ERROR_NOACCESS => "EFAULT",
        ERROR_ELEVATION_REQUIRED | ERROR_CANT_ACCESS_FILE => "EACCES",
        ERROR_ADDRESS_ALREADY_ASSOCIATED => "EADDRINUSE",
        ERROR_INVALID_FLAGS | ERROR_INVALID_HANDLE => "EBADF",
        ERROR_LOCK_VIOLATION | ERROR_PIPE_BUSY | ERROR_SHARING_VIOLATION => "EBUSY",
        ERROR_OPERATION_ABORTED => "ECANCELED",
        ERROR_NO_UNICODE_TRANSLATION => "ECHARSET",
        ERROR_CONNECTION_ABORTED => "ECONNABORTED",
        ERROR_CONNECTION_REFUSED => "ECONNREFUSED",
        ERROR_NETNAME_DELETED => "ECONNRESET",
        ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS => "EEXIST",
        ERROR_HOST_UNREACHABLE => "EHOSTUNREACH",
        ERROR_INSUFFICIENT_BUFFER
        | ERROR_INVALID_DATA
        | ERROR_INVALID_PARAMETER
        | ERROR_SYMLINK_NOT_SUPPORTED => "EINVAL",
        ERROR_BEGINNING_OF_MEDIA
        | ERROR_BUS_RESET
        | ERROR_CRC
        | ERROR_DEVICE_DOOR_OPEN
        | ERROR_DEVICE_REQUIRES_CLEANING
        | ERROR_DISK_CORRUPT
        | ERROR_EOM_OVERFLOW
        | ERROR_FILEMARK_DETECTED
        | ERROR_GEN_FAILURE
        | ERROR_INVALID_BLOCK_LENGTH
        | ERROR_IO_DEVICE
        | ERROR_NO_DATA_DETECTED
        | ERROR_NO_SIGNAL_SENT
        | ERROR_OPEN_FAILED
        | ERROR_SETMARK_DETECTED
        | ERROR_SIGNAL_REFUSED => "EIO",
        ERROR_CANT_RESOLVE_FILENAME => "ELOOP",
        ERROR_TOO_MANY_OPEN_FILES => "EMFILE",
        ERROR_BUFFER_OVERFLOW | ERROR_FILENAME_EXCED_RANGE => "ENAMETOOLONG",
        ERROR_NETWORK_UNREACHABLE => "ENETUNREACH",
        ERROR_BAD_PATHNAME
        | ERROR_DIRECTORY
        | ERROR_ENVVAR_NOT_FOUND
        | ERROR_FILE_NOT_FOUND
        | ERROR_INVALID_NAME
        | ERROR_INVALID_DRIVE
        | ERROR_INVALID_REPARSE_DATA
        | ERROR_MOD_NOT_FOUND
        | ERROR_PATH_NOT_FOUND => "ENOENT",
        ERROR_NOT_ENOUGH_MEMORY | ERROR_OUTOFMEMORY => "ENOMEM",
        ERROR_CANNOT_MAKE
        | ERROR_DISK_FULL
        | ERROR_EA_TABLE_FULL
        | ERROR_END_OF_MEDIA
        | ERROR_HANDLE_DISK_FULL => "ENOSPC",
        ERROR_NOT_CONNECTED => "ENOTCONN",
        ERROR_DIR_NOT_EMPTY => "ENOTEMPTY",
        ERROR_NOT_SUPPORTED => "ENOTSUP",
        ERROR_ACCESS_DENIED | ERROR_PRIVILEGE_NOT_HELD => "EPERM",
        ERROR_BAD_PIPE | ERROR_PIPE_NOT_CONNECTED => "EPIPE",
        ERROR_WRITE_PROTECT => "EROFS",
        ERROR_SEM_TIMEOUT => "ETIMEDOUT",
        ERROR_NOT_SAME_DEVICE => "EXDEV",
        ERROR_INVALID_FUNCTION => "EISDIR",
        ERROR_META_EXPANSION_TOO_LONG => "E2BIG",
        ERROR_BAD_EXE_FORMAT => "EFTYPE",
        _ => "UNKNOWN",
    }
}

/// The human-readable half of a node system-error message, keyed by code.
///
/// For an error that carries a raw OS code this is libuv's `uv_strerror` text,
/// which is what node prints -- the OS's own wording ("Access is denied. (os
/// error 5)") must not leak into a node-shaped message. An error with NO raw
/// code keeps its own text unless the code is one of the eight oam has always
/// spelled out: those come from libraries (rustls, for one) whose message is
/// the only account of what happened, and code downstream reads it -- the TLS
/// read path tells a peer's abrupt close from a real failure by it.
fn node_error_reason(code: &str, error: &std::io::Error) -> String {
    let always = matches!(
        code,
        "ENOENT" | "EACCES" | "EEXIST" | "ENOTEMPTY" | "ENOTDIR" | "EISDIR" | "EINVAL" | "EBADF"
    );
    // A unix errno with no name of its own is coded EIO as a fallback; there
    // the OS's text is the only account of what went wrong.
    #[cfg(unix)]
    let translated = code != "EIO" || error.raw_os_error() == Some(libc::EIO);
    #[cfg(not(unix))]
    let translated = true;
    if (always || (translated && error.raw_os_error().is_some()))
        && let Some(text) = uv_strerror(code)
    {
        return text.to_string();
    }
    error.to_string()
}

/// libuv's `uv_strerror` text for a code (include/uv.h, v1.51.0).
pub fn uv_strerror(code: &str) -> Option<&'static str> {
    UV_ERROR_MESSAGES
        .iter()
        .find(|(name, _)| *name == code)
        .map(|(_, text)| *text)
}

/// The Rust copy of `UV_ERROR_MESSAGES` in js/node_compat.js. A test compares
/// the two in both directions -- every entry here present there with the same
/// text, and the same number of entries -- so neither can drift.
const UV_ERROR_MESSAGES: &[(&str, &str)] = &[
    ("E2BIG", "argument list too long"),
    ("EACCES", "permission denied"),
    ("EADDRINUSE", "address already in use"),
    ("EADDRNOTAVAIL", "address not available"),
    ("EAFNOSUPPORT", "address family not supported"),
    ("EAGAIN", "resource temporarily unavailable"),
    ("EAI_ADDRFAMILY", "address family not supported"),
    ("EAI_AGAIN", "temporary failure"),
    ("EAI_BADFLAGS", "bad ai_flags value"),
    ("EAI_BADHINTS", "invalid value for hints"),
    ("EAI_CANCELED", "request canceled"),
    ("EAI_FAIL", "permanent failure"),
    ("EAI_FAMILY", "ai_family not supported"),
    ("EAI_MEMORY", "out of memory"),
    ("EAI_NODATA", "no address"),
    ("EAI_NONAME", "unknown node or service"),
    ("EAI_OVERFLOW", "argument buffer overflow"),
    ("EAI_PROTOCOL", "resolved protocol is unknown"),
    ("EAI_SERVICE", "service not available for socket type"),
    ("EAI_SOCKTYPE", "socket type not supported"),
    ("EALREADY", "connection already in progress"),
    ("EBADF", "bad file descriptor"),
    ("EBUSY", "resource busy or locked"),
    ("ECANCELED", "operation canceled"),
    ("ECHARSET", "invalid Unicode character"),
    ("ECONNABORTED", "software caused connection abort"),
    ("ECONNREFUSED", "connection refused"),
    ("ECONNRESET", "connection reset by peer"),
    ("EDESTADDRREQ", "destination address required"),
    ("EEXIST", "file already exists"),
    ("EFAULT", "bad address in system call argument"),
    ("EFBIG", "file too large"),
    ("EHOSTUNREACH", "host is unreachable"),
    ("EILSEQ", "illegal byte sequence"),
    ("EINTR", "interrupted system call"),
    ("EINVAL", "invalid argument"),
    ("EIO", "i/o error"),
    ("EISCONN", "socket is already connected"),
    ("EISDIR", "illegal operation on a directory"),
    ("ELOOP", "too many symbolic links encountered"),
    ("EMFILE", "too many open files"),
    ("EMLINK", "too many links"),
    ("EMSGSIZE", "message too long"),
    ("ENAMETOOLONG", "name too long"),
    ("ENETDOWN", "network is down"),
    ("ENETUNREACH", "network is unreachable"),
    ("ENFILE", "file table overflow"),
    ("ENOBUFS", "no buffer space available"),
    ("ENODATA", "no data available"),
    ("ENODEV", "no such device"),
    ("ENOENT", "no such file or directory"),
    ("ENOEXEC", "exec format error"),
    ("ENOMEM", "not enough memory"),
    ("ENOPROTOOPT", "protocol not available"),
    ("ENOSPC", "no space left on device"),
    ("ENOSYS", "function not implemented"),
    ("ENOTCONN", "socket is not connected"),
    ("ENOTDIR", "not a directory"),
    ("ENOTEMPTY", "directory not empty"),
    ("ENOTSOCK", "socket operation on non-socket"),
    ("ENOTSUP", "operation not supported on socket"),
    ("ENOTTY", "inappropriate ioctl for device"),
    ("ENXIO", "no such device or address"),
    ("EOF", "end of file"),
    ("EOVERFLOW", "value too large for defined data type"),
    ("EPERM", "operation not permitted"),
    ("EPIPE", "broken pipe"),
    ("EPROTO", "protocol error"),
    ("EPROTONOSUPPORT", "protocol not supported"),
    ("EPROTOTYPE", "protocol wrong type for socket"),
    ("ERANGE", "result too large"),
    ("EROFS", "read-only file system"),
    ("ESPIPE", "invalid seek"),
    ("ESRCH", "no such process"),
    ("ETIMEDOUT", "connection timed out"),
    ("ETXTBSY", "text file is busy"),
    ("EXDEV", "cross-device link not permitted"),
    ("UNKNOWN", "unknown error"),
    ("EFTYPE", "inappropriate file type or format"),
    ("EHOSTDOWN", "host is down"),
    ("ENONET", "machine is not on the network"),
    ("EREMOTEIO", "remote I/O error"),
    ("ESHUTDOWN", "cannot send after transport endpoint shutdown"),
    ("ESOCKTNOSUPPORT", "socket type not supported"),
    ("EUNATCH", "protocol driver not attached"),
];

/// Which filesystem operation failed, for `fs_error_at`: the few places where
/// node's answer for an OS error depends on the operation, not just the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsSite<'a> {
    /// `fs.open` with these flags (`r`, `w+`, `wx`, ...).
    Open(&'a str),
    /// `fs.readFile`: open, then read everything.
    ReadFile,
    /// `fs.writeFile`: open with `w`, then write.
    WriteFile,
    /// `fs.appendFile`: open with `a`, then write.
    AppendFile,
    /// `fs.mkdir`; `recursive` is the `{ recursive: true }` form.
    Mkdir {
        recursive: bool,
    },
    Readlink,
    /// `fs.readdir`.
    Scandir,
}

/// What node reports for a failed filesystem operation: the code, the syscall
/// it names, and whether the error carries the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsError {
    pub code: &'static str,
    pub syscall: &'static str,
    pub has_path: bool,
}

/// `node_error_code` plus the call-site rules libuv applies on top of its
/// generic table (src/win/fs.c, v1.51.0), for the operations that have them.
/// `syscall` is what the site names when no rule applies.
///
/// Every rule is Windows-only, and each is a place where the generic table
/// alone gives node's code for the wrong operation:
///
/// - libuv opens every path with FILE_FLAG_BACKUP_SEMANTICS, so a DIRECTORY
///   opens. `w` then fails with ERROR_FILE_EXISTS, which fs__open turns into
///   `EISDIR` (and `wx` into `EEXIST`); `readFile` fails on the read
///   (ERROR_INVALID_FUNCTION, `EISDIR`, syscall `read`, no path) and
///   `appendFile` on the write. std opens without that flag, so every one of
///   these arrives here as ERROR_ACCESS_DENIED instead -- which the table
///   reads as `EPERM`.
/// - fs__mkdir reports ERROR_INVALID_NAME and ERROR_DIRECTORY as `EINVAL`. Not
///   for a recursive mkdir: node's MKDirp re-stats the path after the failure
///   and reports `ENOENT`.
/// - fs__readlink reports ERROR_NOT_A_REPARSE_POINT as `EINVAL`.
/// - fs__scandir reports a file as `ENOTDIR`; libuv's table says `ENOENT` for
///   the ERROR_DIRECTORY std gets.
///
/// Not reproduced: `fs.open(dir)` with `r`, `r+`, `a` or `a+` SUCCEEDS in
/// node, which would take directory descriptors oam does not have. oam fails
/// those with `EPERM`.
pub fn fs_error_at(
    site: FsSite<'_>,
    syscall: &'static str,
    path: &str,
    error: &std::io::Error,
) -> FsError {
    let plain = FsError {
        code: node_error_code(error),
        syscall,
        has_path: true,
    };
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_DIRECTORY, ERROR_INVALID_NAME, ERROR_NOT_A_REPARSE_POINT,
        };
        let raw = error.raw_os_error().and_then(|r| u32::try_from(r).ok());
        let at = |code, syscall, has_path| FsError {
            code,
            syscall,
            has_path,
        };
        match site {
            FsSite::Mkdir { recursive: false }
                if matches!(raw, Some(ERROR_INVALID_NAME | ERROR_DIRECTORY)) =>
            {
                return at("EINVAL", syscall, true);
            }
            FsSite::Scandir if raw == Some(ERROR_DIRECTORY) => {
                return at("ENOTDIR", syscall, true);
            }
            FsSite::Readlink if raw == Some(ERROR_NOT_A_REPARSE_POINT) => {
                return at("EINVAL", syscall, true);
            }
            _ => {}
        }
        if raw == Some(ERROR_ACCESS_DENIED) && std::path::Path::new(path).is_dir() {
            match site {
                FsSite::ReadFile => return at("EISDIR", "read", false),
                FsSite::AppendFile => return at("EISDIR", "write", false),
                FsSite::WriteFile => return at("EISDIR", syscall, true),
                FsSite::Open(flags) if flags.contains('w') => {
                    let exclusive = flags.contains('x');
                    return at(if exclusive { "EEXIST" } else { "EISDIR" }, syscall, true);
                }
                _ => {}
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (site, path);
    plain
}

/// `fs_error_at`'s verdict as a full node message: with the path segment when
/// the error carries one, without it when it does not.
pub fn fs_error_message(failure: FsError, path: &str, error: &std::io::Error) -> String {
    if failure.has_path {
        node_error_message(failure.code, failure.syscall, path, error)
    } else {
        node_error_message_fd(failure.code, failure.syscall, error)
    }
}

/// Node-style error message for a PATH operation: "ENOENT: no such file or
/// directory, open 'p'".
///
/// The path segment is always printed, including for a genuinely empty path --
/// `fs.openSync("")` gives node `ENOENT: no such file or directory, open ''`,
/// quotes and all. Absence of a path is a DIFFERENT thing from an empty one and
/// gets its own function (`node_error_message_fd`) rather than being inferred
/// from `path.is_empty()`; keying on emptiness conflated the two and silently
/// dropped the quotes from every empty-path error.
pub fn node_error_message(code: &str, syscall: &str, path: &str, error: &std::io::Error) -> String {
    let reason = node_error_reason(code, error);
    format!("{code}: {reason}, {syscall} '{path}'")
}

/// Node-style error message for an FD operation, which has no path at all:
/// "EBADF: bad file descriptor, write" -- no trailing segment, not an empty
/// one. Pairs with `fd_error_code`.
pub fn node_error_message_fd(code: &str, syscall: &str, error: &std::io::Error) -> String {
    let reason = node_error_reason(code, error);
    format!("{code}: {reason}, {syscall}")
}

/// Error code for an operation on an ALREADY-OPEN descriptor.
///
/// A descriptor was opened successfully, so there is no path left to be denied:
/// the only way an fd read/write comes back "access denied" is that the handle
/// is open in the wrong mode -- writing to an `r` fd, reading from a `w` one.
/// libuv reports that as EBADF, and so does node:
///
///   writeSync(fdOpenedForRead, buf)
///     node: EBADF: bad file descriptor, write
///     oam:  EACCES: permission denied, write ''
///
/// Windows is what makes this show up: it returns ERROR_ACCESS_DENIED for a
/// wrong-mode handle where POSIX returns EBADF directly.
/// fopen-style flag string -> OpenOptions. Mirrors Node's fs flag set; an
/// unknown flag defaults to read-only (Node would throw, but lenient is
/// safer for compat). `+`/`w`/`a`/`x` imply write.
///
/// Lives here, not in the engine, because BOTH opens need it. The async
/// `fs_open` used to carry its own three-arm r/w/a table, so a descriptor the
/// sync family could open was rejected by the async one:
/// `fs.promises.open(p, "r+")` failed with `fs_open: unknown mode 'r+'` --
/// not even a node-shaped error -- and every read/write-mode flag was
/// unreachable from `fs.promises.open` and callback `fs.open`.
pub fn open_options_for(flags: &str) -> std::fs::OpenOptions {
    let mut oo = std::fs::OpenOptions::new();
    match flags {
        "r" => {
            oo.read(true);
        }
        "r+" | "rs+" | "sr+" => {
            oo.read(true).write(true);
        }
        "w" => {
            oo.write(true).create(true).truncate(true);
        }
        "wx" | "xw" => {
            oo.write(true).create_new(true);
        }
        "w+" => {
            oo.read(true).write(true).create(true).truncate(true);
        }
        "wx+" | "xw+" => {
            oo.read(true).write(true).create_new(true);
        }
        "a" => {
            oo.append(true).create(true);
        }
        "ax" | "xa" => {
            oo.append(true).create_new(true);
        }
        "a+" => {
            oo.read(true).append(true).create(true);
        }
        "ax+" | "xa+" => {
            oo.read(true).append(true).create_new(true);
        }
        _ => {
            oo.read(true);
        }
    }
    oo
}

/// `write_all`, except that an EMPTY buffer still issues one write syscall.
///
/// `std`'s `write_all` loops `while !buf.is_empty()`, so a zero-byte write
/// never reaches the OS and never validates the descriptor. Node's does:
/// `writevSync(fd, [Buffer.alloc(0)])` reports EBADF on a closed or wrong-mode
/// fd rather than quietly returning 0. Only an EMPTY LIST skips the descriptor
/// entirely, and that case never gets here.
pub fn write_all_checked(file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if bytes.is_empty() {
        return file.write(bytes).map(|_| ());
    }
    file.write_all(bytes)
}

///
/// POSIX needs its own arm even though it reports EBADF honestly, because
/// `std::io::ErrorKind` has no BadFileDescriptor variant: raw errno 9 lands in
/// `Uncategorized`, and `node_error_code` falls through to EIO. The same
/// `writeSync(fdOpenedForRead, buf)` therefore gave `EBADF` on Windows and
/// `EIO: Bad file descriptor (os error 9), write` -- leaked OS text and all --
/// on macOS and Linux. `node_errno` was already deriving -9 from the raw errno
/// there, so the code and the errno disagreed with each other on top of
/// disagreeing with node.
pub fn fd_error_code(error: &std::io::Error) -> &'static str {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return "EBADF";
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::EBADF) {
        return "EBADF";
    }
    node_error_code(error)
}

/// fs.access semantics: existence always; W_OK (mode & 2) additionally
/// requires the file not be read-only — Node throws EPERM on Windows for
/// W_OK against a read-only file, and programs gate writes on exactly this
/// call. X_OK is approximated as existence (wave 1). Err is (code, message).
/// Node's `fs.access`. On failure returns (code, message, errno).
///
/// The errno rides along because both callers need it to build node's system-
/// error shape and only this function still holds the `io::Error` it came
/// from. Without it `fs.access`/`fs.promises.access` rejected with no `errno`
/// at all (and the async form with no `syscall` or `path` either), where every
/// sibling fs op carries all four.
pub fn check_access(path: &str, mode: i32) -> Result<(), (String, String, Option<i32>)> {
    let meta = std::fs::metadata(path).map_err(|e| {
        let code = node_error_code(&e);
        (
            code.to_string(),
            node_error_message(code, "access", path, &e),
            node_errno(code, &e),
        )
    })?;
    if mode & 2 != 0 && meta.permissions().readonly() {
        let code = if cfg!(windows) { "EPERM" } else { "EACCES" };
        return Err((
            code.to_string(),
            format!("{code}: operation not permitted, access '{path}'"),
            // Synthesised (no io::Error behind it): the libuv numbers node
            // reports for these two.
            Some(if cfg!(windows) { -4048 } else { -13 }),
        ));
    }
    Ok(())
}

/// rm with Node semantics: file or directory, optional recursion.
pub fn remove_path(path: &str, recursive: bool) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    }
}

/// std::fs::canonicalize returns \\?\-prefixed paths on Windows, which leak
/// into user-visible strings and break naive comparisons — strip the prefix.
pub fn strip_unc_prefix(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// node:zlib backend (flate2 + brotli). Sync fns serve the *Sync natives
/// directly; the async ops below wrap them in spawn_blocking -- compression
/// is CPU work and must not sit on the isolate thread for the callback forms.
///
/// Incremental streaming (StreamCompressor / StreamDecompressor /
/// BrotliCompressor / BrotliDecompressor) backs the JS Transform classes.
/// Each JS Transform stream creates one handle in the ZlibRegistry;
/// _transform feeds chunks via zlibStreamWrite and _flush finalizes via
/// zlibStreamFlush.
pub mod zlib {
    use flate2::Compression;
    use std::io::{Read, Write};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Format {
        Gzip,
        Deflate,
        DeflateRaw,
    }

    impl Format {
        pub fn parse(name: &str) -> Option<Self> {
            Some(match name {
                "gzip" => Format::Gzip,
                "deflate" => Format::Deflate,
                "deflateRaw" => Format::DeflateRaw,
                _ => return None,
            })
        }
    }

    pub fn compress(bytes: &[u8], format: Format, level: i32) -> std::io::Result<Vec<u8>> {
        // Node levels: -1 default, 0..=9. flate2 default is 6, same as zlib.
        let level = if (0..=9).contains(&level) {
            Compression::new(level as u32)
        } else {
            Compression::default()
        };
        match format {
            Format::Gzip => {
                let mut encoder = flate2::write::GzEncoder::new(Vec::new(), level);
                encoder.write_all(bytes)?;
                encoder.finish()
            }
            Format::Deflate => {
                let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), level);
                encoder.write_all(bytes)?;
                encoder.finish()
            }
            Format::DeflateRaw => {
                let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
                encoder.write_all(bytes)?;
                encoder.finish()
            }
        }
    }

    /// The message of the `io::Error` `decompress_capped` returns when the
    /// output would exceed `max_output`. The op layer matches on it to raise
    /// node's `RangeError [ERR_BUFFER_TOO_LARGE]`; nothing else produces it.
    pub const OUTPUT_TOO_LARGE: &str = "zlib output exceeds maxOutputLength";

    pub fn decompress(bytes: &[u8], format: Format) -> std::io::Result<Vec<u8>> {
        decompress_capped(bytes, format, None)
    }

    /// Decompress, giving up as soon as the output passes `max_output` bytes.
    ///
    /// node's `maxOutputLength` (the one-shot zlib APIs). The cap is enforced
    /// while inflating, not on the finished buffer: a 200 KB gzip of 200 MiB
    /// of spaces must fail after ~`max_output` bytes of work, not after the
    /// whole 200 MiB has been allocated -- that allocation is the OOM the
    /// option exists to prevent. Reads are bounded by what the cap still
    /// allows, plus one byte so overflow is seen without a second pass.
    pub fn decompress_capped(
        bytes: &[u8],
        format: Format,
        max_output: Option<usize>,
    ) -> std::io::Result<Vec<u8>> {
        let mut reader: Box<dyn Read + '_> = match format {
            Format::Gzip => Box::new(flate2::read::GzDecoder::new(bytes)),
            Format::Deflate => Box::new(flate2::read::ZlibDecoder::new(bytes)),
            Format::DeflateRaw => Box::new(flate2::read::DeflateDecoder::new(bytes)),
        };
        match max_output {
            None => {
                let mut out = Vec::new();
                reader.read_to_end(&mut out)?;
                Ok(out)
            }
            Some(cap) => read_capped(&mut reader, cap),
        }
    }

    /// Read `reader` to its end, giving up with [`OUTPUT_TOO_LARGE`] the moment
    /// the output would pass `cap`. It never buffers more than `cap` (plus one
    /// byte, and a read buffer capped at 64 KiB), so a reader that inflates
    /// without bound -- a decompression bomb -- is stopped at the cap, not run
    /// to exhaustion. Split out from `decompress_capped` so the bound can be
    /// tested against an endless reader, which a `read_to_end` regression would
    /// run forever.
    fn read_capped(reader: &mut dyn Read, cap: usize) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        // Never hand the decoder more room than the cap plus one byte, so
        // memory stays bounded by `cap` whatever the input inflates to.
        let mut buf = vec![0u8; (cap + 1).min(64 * 1024)];
        loop {
            let want = (cap + 1 - out.len()).min(buf.len());
            let n = reader.read(&mut buf[..want])?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > cap {
                return Err(std::io::Error::other(OUTPUT_TOO_LARGE));
            }
        }
    }

    // Kept next to `read_capped` (the code it guards) rather than at the module
    // end past the streaming and brotli code.
    #[cfg(test)]
    #[allow(clippy::items_after_test_module)]
    mod capped_tests {
        use super::{Format, OUTPUT_TOO_LARGE, compress, decompress_capped, read_capped};
        use std::io::Read;

        /// A reader that never returns 0: `read_to_end` would allocate without
        /// bound and never return. It counts the bytes it was asked for.
        struct Endless {
            byte: u8,
            pulled: usize,
        }
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(self.byte);
                self.pulled += buf.len();
                Ok(buf.len())
            }
        }

        #[test]
        fn read_capped_bounds_an_endless_stream() {
            let cap = 4096;
            let mut src = Endless {
                byte: b' ',
                pulled: 0,
            };
            // The whole point of the fix: an endless (bomb) stream is stopped
            // at the cap, not inflated to exhaustion. A `read_to_end` regression
            // would never return here.
            let err = read_capped(&mut src, cap).unwrap_err();
            assert_eq!(err.to_string(), OUTPUT_TOO_LARGE);
            // Memory (and reads) bounded by the cap plus one buffer, not the
            // unbounded stream.
            assert!(
                src.pulled <= cap + 1 + 64 * 1024,
                "pulled {} bytes past the {cap}-byte cap",
                src.pulled
            );
        }

        #[test]
        fn decompress_capped_stops_a_gzip_bomb_at_the_cap() {
            // 16 MiB of spaces gzips to a few KB. Under a 1 KiB cap the real
            // decoder path returns the cap error having buffered ~1 KiB, not
            // 16 MiB; and the cap boundary is exact.
            let size = 16 * 1024 * 1024;
            let bomb = compress(&vec![b' '; size], Format::Gzip, 6).unwrap();
            let err = decompress_capped(&bomb, Format::Gzip, Some(1024)).unwrap_err();
            assert_eq!(err.to_string(), OUTPUT_TOO_LARGE);
            assert!(decompress_capped(&bomb, Format::Gzip, Some(size)).is_ok());
            assert!(decompress_capped(&bomb, Format::Gzip, Some(size - 1)).is_err());
        }
    }

    /// `compress` with node's `maxOutputLength`, which node applies to the
    /// encoders as well. Checked on the finished buffer: compressed output is
    /// bounded by the input, so there is no bomb to stop early.
    pub fn compress_capped(
        bytes: &[u8],
        format: Format,
        level: i32,
        max_output: Option<usize>,
    ) -> std::io::Result<Vec<u8>> {
        let out = compress(bytes, format, level)?;
        match max_output {
            Some(cap) if out.len() > cap => Err(std::io::Error::other(OUTPUT_TOO_LARGE)),
            _ => Ok(out),
        }
    }

    /// Node's unzip*: auto-detect gzip (1f 8b magic) vs zlib-wrapped.
    pub fn unzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        unzip_capped(bytes, None)
    }

    /// `unzip` with node's `maxOutputLength`; see `decompress_capped`.
    pub fn unzip_capped(bytes: &[u8], max_output: Option<usize>) -> std::io::Result<Vec<u8>> {
        if bytes.starts_with(&[0x1f, 0x8b]) {
            decompress_capped(bytes, Format::Gzip, max_output)
        } else {
            decompress_capped(bytes, Format::Deflate, max_output)
        }
    }

    // ----------------------------------------------------------------
    // Incremental streaming: gzip / deflate / deflateRaw
    //
    // We use flate2's write-based encoders (GzEncoder, ZlibEncoder,
    // DeflateEncoder) for compression, draining the backing Vec<u8>
    // via get_mut() + mem::take() after each write_all. This is truly
    // incremental: compressed bytes are emitted per-chunk with no need
    // to buffer the full input.
    //
    // For decompression we likewise use the write-based decoders
    // (GzDecoder, ZlibDecoder, DeflateDecoder). Each decoder accepts
    // a chunk, runs it through the inflate state machine, and appends
    // decompressed bytes to the inner Vec<u8>. We drain via mem::take
    // after each write so memory stays bounded (~64 kB per stream plus
    // the decompressed output for that chunk).
    //
    // The "unzip" auto-detect variant peeks at the first two bytes on
    // the initial write_chunk call to resolve the format, then creates
    // the appropriate decoder.
    //
    // Send requirement: all flate2 encoder/decoder types are Send, and
    // our wrappers hold no thread-local state.
    // ----------------------------------------------------------------

    /// Wraps any of the three flate2 write-encoders behind a uniform
    /// interface. Created via `StreamCompressor::new`; consumes chunks via
    /// `write_chunk`; finalizes via `finish` (emits the trailing CRC /
    /// checksum bytes the format requires).
    pub struct StreamCompressor {
        inner: CompressorInner,
    }

    enum CompressorInner {
        Gzip(flate2::write::GzEncoder<Vec<u8>>),
        Deflate(flate2::write::ZlibEncoder<Vec<u8>>),
        DeflateRaw(flate2::write::DeflateEncoder<Vec<u8>>),
    }

    impl StreamCompressor {
        pub fn new(format: Format, level: i32) -> Self {
            let level = if (0..=9).contains(&level) {
                Compression::new(level as u32)
            } else {
                Compression::default()
            };
            let inner = match format {
                Format::Gzip => {
                    CompressorInner::Gzip(flate2::write::GzEncoder::new(Vec::new(), level))
                }
                Format::Deflate => {
                    CompressorInner::Deflate(flate2::write::ZlibEncoder::new(Vec::new(), level))
                }
                Format::DeflateRaw => CompressorInner::DeflateRaw(
                    flate2::write::DeflateEncoder::new(Vec::new(), level),
                ),
            };
            Self { inner }
        }

        /// Feed a chunk. Returns whatever bytes the encoder produced
        /// immediately (may be empty -- the encoder buffers internally
        /// until it has a full deflate block ready).
        #[inline]
        pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
            match &mut self.inner {
                CompressorInner::Gzip(enc) => {
                    enc.write_all(chunk)?;
                    Ok(std::mem::take(enc.get_mut()))
                }
                CompressorInner::Deflate(enc) => {
                    enc.write_all(chunk)?;
                    Ok(std::mem::take(enc.get_mut()))
                }
                CompressorInner::DeflateRaw(enc) => {
                    enc.write_all(chunk)?;
                    Ok(std::mem::take(enc.get_mut()))
                }
            }
        }

        /// Flush and finalize. Consumes self; returns the tail bytes
        /// (including the gzip/zlib trailer). After this the stream handle
        /// is dropped -- close is implicit.
        pub fn finish(self) -> std::io::Result<Vec<u8>> {
            match self.inner {
                CompressorInner::Gzip(enc) => enc.finish(),
                CompressorInner::Deflate(enc) => enc.finish(),
                CompressorInner::DeflateRaw(enc) => enc.finish(),
            }
        }
    }

    // `StreamCompressor` is `Send` by auto-derivation: `CompressorInner` holds
    // only flate2 encoders over `Vec<u8>`, every one of which is `Send`, and the
    // wrapper adds no thread-affine state. Deliberately NOT a manual
    // `unsafe impl Send` -- that would suppress the compiler's own auto-trait
    // check and silently keep asserting `Send` if the inner types ever stopped
    // being it.

    // ----------------------------------------------------------------
    // Truly incremental decompressor -- slice A.
    //
    // Uses flate2's write-based decoders (GzDecoder, ZlibDecoder,
    // DeflateDecoder) so each write_chunk call invokes the inflate state
    // machine immediately and returns whatever bytes were decoded, bounded
    // by the chunk size. The full compressed stream never needs to live
    // in memory simultaneously.
    //
    // The `Unzip` variant defers decoder creation until the first
    // non-empty write_chunk, at which point it peeks the magic bytes to
    // choose Gzip or Deflate.
    // ----------------------------------------------------------------

    /// Truly incremental flate2 decompressor: memory usage bounded by
    /// ~64 kB scratch per stream regardless of input size.
    pub struct StreamDecompressor {
        inner: DecompressorInner,
    }

    enum DecompressorInner {
        Gzip(flate2::write::GzDecoder<Vec<u8>>),
        Deflate(flate2::write::ZlibDecoder<Vec<u8>>),
        DeflateRaw(flate2::write::DeflateDecoder<Vec<u8>>),
        /// Pending auto-detect: first chunk resolves to Gzip or Deflate.
        Unzip,
    }

    impl StreamDecompressor {
        pub fn new_gzip() -> Self {
            Self {
                inner: DecompressorInner::Gzip(flate2::write::GzDecoder::new(Vec::new())),
            }
        }
        pub fn new_deflate() -> Self {
            Self {
                inner: DecompressorInner::Deflate(flate2::write::ZlibDecoder::new(Vec::new())),
            }
        }
        pub fn new_deflate_raw() -> Self {
            Self {
                inner: DecompressorInner::DeflateRaw(
                    flate2::write::DeflateDecoder::new(Vec::new()),
                ),
            }
        }
        pub fn new_unzip() -> Self {
            Self {
                inner: DecompressorInner::Unzip,
            }
        }

        /// Feed one chunk of compressed data. Returns the decompressed bytes
        /// produced by this chunk (may be smaller than expected if the
        /// deflate block spans multiple chunks -- the remaining bytes arrive
        /// on subsequent calls). Memory usage stays bounded: we drain the
        /// inner Vec via mem::take after each write.
        #[inline]
        pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
            if chunk.is_empty() {
                return Ok(Vec::new());
            }
            // Resolve auto-detect on first non-empty chunk.
            if matches!(self.inner, DecompressorInner::Unzip) {
                if chunk.starts_with(&[0x1f, 0x8b]) {
                    self.inner = DecompressorInner::Gzip(flate2::write::GzDecoder::new(Vec::new()));
                } else {
                    self.inner =
                        DecompressorInner::Deflate(flate2::write::ZlibDecoder::new(Vec::new()));
                }
            }
            match &mut self.inner {
                DecompressorInner::Gzip(dec) => {
                    dec.write_all(chunk)?;
                    Ok(std::mem::take(dec.get_mut()))
                }
                DecompressorInner::Deflate(dec) => {
                    dec.write_all(chunk)?;
                    Ok(std::mem::take(dec.get_mut()))
                }
                DecompressorInner::DeflateRaw(dec) => {
                    dec.write_all(chunk)?;
                    Ok(std::mem::take(dec.get_mut()))
                }
                DecompressorInner::Unzip => unreachable!("resolved above"),
            }
        }

        /// Finalize: flush the inflate state and return any remaining
        /// decompressed bytes. For gzip this verifies the CRC/ISIZE trailer.
        pub fn finish(self) -> std::io::Result<Vec<u8>> {
            match self.inner {
                DecompressorInner::Gzip(dec) => dec.finish(),
                DecompressorInner::Deflate(dec) => dec.finish(),
                DecompressorInner::DeflateRaw(dec) => dec.finish(),
                // No data was ever written (empty stream).
                DecompressorInner::Unzip => Ok(Vec::new()),
            }
        }
    }

    // `StreamDecompressor` is `Send` by auto-derivation: `DecompressorInner`
    // holds only flate2 write-decoders over `Vec<u8>` (all `Send`) plus the unit
    // `Unzip` variant. No manual `unsafe impl` -- see the note on
    // `StreamCompressor` above.

    // Compile-time proof of both notes. A manual `unsafe impl Send` asserts
    // `Send` forever; this instead FAILS THE BUILD the day an inner type stops
    // being `Send`, which is the behaviour we actually want.
    const _: () = {
        const fn assert_send<T: Send>() {}
        assert_send::<StreamCompressor>();
        assert_send::<StreamDecompressor>();
    };
}

// ----------------------------------------------------------------
// Brotli incremental stream types -- slice B.
//
// Uses the `brotli` crate (pure-Rust, default features: std +
// alloc-stdlib; no ffi-api, no simd, no native deps) with its
// write-based CompressorWriter / DecompressorWriter API.
//
// Memory: both types use a 64 kB internal buffer; decompressed output
// drains via mem::take on every write_chunk. The full input never
// needs to live in memory.
// ----------------------------------------------------------------

/// Buffer size for both brotli encoder and decoder: 64 kB scratch.
const BROTLI_BUF: usize = 65536;

/// Brotli quality: 4 is a good default (fast, reasonable ratio).
/// Node's default is 11 (max quality) but that is very slow for large
/// streams -- 4 gives ~10x the throughput at reasonable compression.
const BROTLI_QUALITY: u32 = 4;

/// lgwin: log2 of the sliding window. 22 = 4 MB window (brotli default).
const BROTLI_LGWIN: u32 = 22;

/// Incremental brotli compressor wrapping `brotli::CompressorWriter`.
pub struct BrotliCompressor {
    inner: brotli::CompressorWriter<Vec<u8>>,
}

impl Default for BrotliCompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl BrotliCompressor {
    pub fn new() -> Self {
        Self {
            inner: brotli::CompressorWriter::new(
                Vec::new(),
                BROTLI_BUF,
                BROTLI_QUALITY,
                BROTLI_LGWIN,
            ),
        }
    }

    /// Feed one chunk. Returns compressed bytes produced so far.
    /// Memory stays bounded: inner Vec is drained via mem::take.
    #[inline]
    pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
        use std::io::Write;
        self.inner.write_all(chunk)?;
        Ok(std::mem::take(self.inner.get_mut()))
    }

    /// Finalize: write the brotli stream-end marker and return all tail bytes.
    /// `into_inner()` calls BROTLI_OPERATION_FINISH internally and returns
    /// the inner Vec containing the final compressed bytes (stream-end marker).
    pub fn finish(self) -> std::io::Result<Vec<u8>> {
        Ok(self.inner.into_inner())
    }
}

// `BrotliCompressor` is `Send` by auto-derivation: its
// `brotli::CompressorWriter<Vec<u8>>` wraps only a `Vec<u8>` and self-contained
// brotli state, with no thread-local references. No manual `unsafe impl` --
// see the note on `StreamCompressor`.

/// Incremental brotli decompressor wrapping `brotli::DecompressorWriter`.
pub struct BrotliDecompressor {
    inner: brotli::DecompressorWriter<Vec<u8>>,
}

impl Default for BrotliDecompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl BrotliDecompressor {
    pub fn new() -> Self {
        Self {
            inner: brotli::DecompressorWriter::new(Vec::new(), BROTLI_BUF),
        }
    }

    /// Feed one chunk of compressed data. Returns decompressed bytes
    /// produced so far. Memory stays bounded: inner Vec is drained via
    /// mem::take after each write.
    #[inline]
    pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
        use std::io::Write;
        self.inner.write_all(chunk)?;
        Ok(std::mem::take(self.inner.get_mut()))
    }

    /// Finalize: close the brotli decompressor and return any remaining
    /// output bytes. `into_inner()` calls `close()` and returns the inner
    /// Vec; on decompressor error it returns `Err(Vec)` which we convert
    /// to an io::Error (the partial bytes are discarded on corruption).
    pub fn finish(self) -> std::io::Result<Vec<u8>> {
        self.inner.into_inner().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "brotli decompressor: stream is incomplete or corrupt",
            )
        })
    }
}

// `BrotliDecompressor` is `Send` by auto-derivation: its
// `brotli::DecompressorWriter<Vec<u8>>` wraps only a `Vec<u8>` and
// self-contained brotli state, with no thread-local references. No manual
// `unsafe impl` -- see the note on `StreamCompressor`.

// Compile-time proof of both brotli notes; see the zlib assertion above for why
// this is preferable to an `unsafe impl`.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<BrotliCompressor>();
    assert_send::<BrotliDecompressor>();
};

/// Built-in op implementations. Plain futures; the engine decides how their
/// outcomes surface in JS.
pub mod ops {
    use super::OpOutcome;
    use std::time::Duration;

    pub async fn sleep(ms: u64) -> OpOutcome {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        OpOutcome::Done
    }

    fn node_fail(error: std::io::Error, syscall: &str, path: &str) -> OpOutcome {
        let code = super::node_error_code(&error);
        OpOutcome::node_failed_at(
            code,
            super::node_error_message(code, syscall, path, &error),
            syscall,
            Some(path),
            super::node_errno(code, &error),
        )
    }

    /// `node_fail` for an operation with call-site error rules (see
    /// `fs_error_at`).
    fn node_fail_at(
        site: super::FsSite<'_>,
        error: std::io::Error,
        syscall: &'static str,
        path: &str,
    ) -> OpOutcome {
        let failure = super::fs_error_at(site, syscall, path, &error);
        OpOutcome::node_failed_at(
            failure.code,
            super::fs_error_message(failure, path, &error),
            failure.syscall,
            failure.has_path.then_some(path),
            super::node_errno(failure.code, &error),
        )
    }

    /// EBADF for a handle that is not (or no longer) in the registry -- node's
    /// answer for any fd call on a closed descriptor.
    ///
    /// This surfaced as `Failed("fs stream: handle N is gone")`, an internal
    /// diagnostic with no `code`/`syscall`/`errno` at all, so an async read on
    /// a closed fd rejected with a bare Error where node gives EBADF.
    fn node_fail_ebadf(syscall: &str) -> OpOutcome {
        // POSIX EBADF; the windows errno table keys on the code string instead
        // of the raw value, so this is correct on both.
        let error = std::io::Error::from_raw_os_error(9);
        OpOutcome::node_failed_at(
            "EBADF",
            super::node_error_message_fd("EBADF", syscall, &error),
            syscall,
            None,
            super::node_errno("EBADF", &error),
        )
    }

    /// `node_fail` for an operation on an already-open descriptor: no path, and
    /// a wrong-mode denial reads as EBADF (see `fd_error_code`).
    ///
    /// These used to pass the HANDLE NUMBER as the path, so a failed stream
    /// read reported `... read '64'` where node has no path at all.
    fn node_fail_fd(error: std::io::Error, syscall: &str) -> OpOutcome {
        let code = super::fd_error_code(&error);
        OpOutcome::node_failed_at(
            code,
            super::node_error_message_fd(code, syscall, &error),
            syscall,
            None,
            super::node_errno(code, &error),
        )
    }

    /// Where the fields `std::fs::Metadata` does not carry can be read from.
    ///
    /// POSIX has all of them on the metadata already. Windows needs a file
    /// HANDLE -- either one the caller is already holding (fstat) or one opened
    /// from the path.
    pub enum StatSource<'a> {
        Path(&'a str),
        File(&'a std::fs::File),
    }

    /// The stat fields node reports that `std::fs::Metadata` does not expose.
    struct StatExtras {
        dev: u64,
        ino: u64,
        nlink: u64,
        uid: u64,
        gid: u64,
        rdev: u64,
        blksize: u64,
        blocks: u64,
        /// Inode-change time in ms, or None if it could not be read.
        ctime_ms: Option<f64>,
    }

    #[cfg(unix)]
    fn stat_extras(meta: &std::fs::Metadata, _source: StatSource<'_>) -> Option<StatExtras> {
        use std::os::unix::fs::MetadataExt;
        Some(StatExtras {
            dev: meta.dev(),
            ino: meta.ino(),
            nlink: meta.nlink(),
            uid: u64::from(meta.uid()),
            gid: u64::from(meta.gid()),
            rdev: meta.rdev(),
            blksize: meta.blksize(),
            blocks: meta.blocks(),
            // st_ctim is the inode-change time and is NOT mtime: chmod moves it
            // and leaves mtime alone.
            ctime_ms: Some(meta.ctime() as f64 * 1000.0 + meta.ctime_nsec() as f64 / 1_000_000.0),
        })
    }

    /// FILETIME (100ns ticks since 1601-01-01) to milliseconds since the epoch.
    #[cfg(windows)]
    fn filetime_to_ms(ticks: i64) -> f64 {
        const TICKS_PER_MS: f64 = 10_000.0;
        const EPOCH_DIFFERENCE_MS: f64 = 11_644_473_600_000.0;
        ticks as f64 / TICKS_PER_MS - EPOCH_DIFFERENCE_MS
    }

    #[cfg(windows)]
    fn stat_extras(meta: &std::fs::Metadata, source: StatSource<'_>) -> Option<StatExtras> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_STANDARD_INFO, FileBasicInfo,
            FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        };

        let opened;
        let file = match source {
            StatSource::File(file) => file,
            // Without OPEN_REPARSE_POINT an lstat on a symlink would open --
            // and then describe -- the target instead of the link itself.
            StatSource::Path(path) => {
                opened = open_for_stat(path, meta.is_symlink()).ok()?;
                &opened
            }
        };
        let handle = file.as_raw_handle().cast::<std::ffi::c_void>();

        // Rust's Metadata does carry these when it came from a File, but only
        // behind the unstable `windows_by_handle` feature, and oam builds on
        // stable -- so the call is made explicitly rather than reaching for a
        // nightly accessor.
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `handle` is owned by a live File for the whole call, and
        // `info` is a correctly sized, writable BY_HANDLE_FILE_INFORMATION.
        if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
            return None;
        }

        let mut standard = FILE_STANDARD_INFO::default();
        // `blocks` counts 512-byte units of ALLOCATED space, which is not
        // ceil(size / 512): a small file resident in the MFT allocates none,
        // and node duly reports 0 blocks for it.
        // SAFETY: as above; the size passed matches the struct written.
        let blocks = if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
        } != 0
        {
            (standard.AllocationSize as u64) >> 9
        } else {
            0
        };

        let mut basic = FILE_BASIC_INFO::default();
        // SAFETY: as above.
        let ctime_ms = if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } != 0
        {
            Some(filetime_to_ms(basic.ChangeTime))
        } else {
            None
        };

        Some(StatExtras {
            dev: u64::from(info.dwVolumeSerialNumber),
            ino: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            nlink: u64::from(info.nNumberOfLinks),
            // Windows has no POSIX ownership or device numbers; libuv reports
            // zeros here and node passes them straight through.
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            blocks,
            ctime_ms,
        })
    }

    /// Opens a handle suitable for stat: attributes only, and able to open a
    /// DIRECTORY (which needs BACKUP_SEMANTICS) or a symlink itself rather than
    /// its target (OPEN_REPARSE_POINT).
    #[cfg(windows)]
    fn open_for_stat(path: &str, keep_reparse_point: bool) -> std::io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if keep_reparse_point {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        std::fs::OpenOptions::new()
            // Attributes only. Asking for read access would fail on a file the
            // caller is allowed to stat but not to read.
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(flags)
            .open(path)
    }

    /// stat/lstat of a path as the JSON payload the JS side consumes.
    ///
    /// Blocking; async callers must run it on a blocking thread.
    pub fn stat_path_json(path: &str, lstat: bool) -> std::io::Result<String> {
        // On Windows a plain stat reads EVERYTHING off one handle. Calling
        // std::fs::metadata first and then opening again for dev/ino/blocks
        // meant two opens per stat -- measured at 2x the cost -- because
        // std::fs::metadata opens the very same kind of handle internally.
        //
        // lstat keeps std's symlink_metadata as the source of truth for the
        // file kind and reopens for the extras. Symlinks cannot be created on
        // the machine this was written on, so the single-handle reparse-point
        // path is unverified, and one syscall is not worth guessing with.
        #[cfg(windows)]
        if !lstat {
            let file = open_for_stat(path, false)?;
            let meta = file.metadata()?;
            return Ok(stat_to_json(&meta, StatSource::File(&file)));
        }
        let meta = if lstat {
            std::fs::symlink_metadata(path)?
        } else {
            std::fs::metadata(path)?
        };
        Ok(stat_to_json(&meta, StatSource::Path(path)))
    }

    /// statfs payload: the seven `uv_statfs_t` fields node's `fs.StatFs`
    /// carries, each as a DECIMAL STRING.
    ///
    /// Strings, not JSON numbers, because `fs.statfs(path, {bigint: true})`
    /// promises exact u64s. A JSON number is a double, so a filesystem with
    /// more than 2^53 blocks (or an f_type magic above it) would round on the
    /// way through and the BigInt built from it would be quietly wrong --
    /// which is precisely the case bigint mode exists to serve. The JS side
    /// picks `Number(s)` or `BigInt(s)`; both are exact from the string, and
    /// the non-bigint form rounds identically to node's own double.
    fn statfs_fields_json(
        f_type: u64,
        bsize: u64,
        blocks: u64,
        bfree: u64,
        bavail: u64,
        files: u64,
        ffree: u64,
    ) -> String {
        format!(
            "{{\"type\":\"{f_type}\",\"bsize\":\"{bsize}\",\"blocks\":\"{blocks}\",\
             \"bfree\":\"{bfree}\",\"bavail\":\"{bavail}\",\"files\":\"{files}\",\
             \"ffree\":\"{ffree}\"}}"
        )
    }

    /// statfs of the filesystem `path` lives on, as the JSON payload the JS
    /// side consumes. Blocking; async callers must run it on a blocking thread.
    ///
    /// Field-for-field what libuv's `uv_fs_statfs` reports, because that is
    /// what node hands to `fs.StatFs` verbatim.
    #[cfg(windows)]
    pub fn statfs_json(path: &str) -> std::io::Result<String> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceW;

        /// ERROR_DIRECTORY: "the directory name is invalid" -- what
        /// GetDiskFreeSpaceW answers when handed a path to a FILE.
        const ERROR_DIRECTORY: i32 = 267;

        fn query(dir: &std::path::Path) -> std::io::Result<(u32, u32, u32, u32)> {
            let wide: Vec<u16> = dir
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
            // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives
            // the call, and the four out-params are live u32s the API only
            // writes on success.
            let ok = unsafe {
                GetDiskFreeSpaceW(wide.as_ptr(), &mut spc, &mut bps, &mut free, &mut total)
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((spc, bps, free, total))
        }

        // An empty path is "no such file" to node, on every platform. Left to
        // std::path::absolute it comes back InvalidInput -> EINVAL, where the
        // POSIX branch gets ENOENT straight from the kernel.
        if path.is_empty() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        }

        // GetDiskFreeSpaceW wants a DIRECTORY, but node accepts any path on
        // the volume. Resolve to absolute first: a bare relative path like
        // "Cargo.toml" has no parent to fall back to, and node answers it
        // from the cwd's volume.
        let absolute = std::path::absolute(path)?;
        let (sectors_per_cluster, bytes_per_sector, free_clusters, total_clusters) =
            match query(&absolute) {
                Ok(values) => values,
                // Retry against the parent ONLY for "you gave me a file".
                // Retrying on every error would turn a missing path into the
                // stats of its parent's volume, where node reports ENOENT.
                Err(e) if e.raw_os_error() == Some(ERROR_DIRECTORY) => {
                    let parent = absolute.parent().ok_or(e)?;
                    query(parent)?
                }
                Err(e) => return Err(e),
            };

        Ok(statfs_fields_json(
            // Windows exposes no filesystem type id here; libuv reports 0.
            0,
            u64::from(bytes_per_sector) * u64::from(sectors_per_cluster),
            u64::from(total_clusters),
            u64::from(free_clusters),
            // No per-user quota view from this API, so free == available.
            u64::from(free_clusters),
            // Inode counts do not exist on NTFS/FAT; libuv reports zeros.
            0,
            0,
        ))
    }

    /// statfs of the filesystem `path` lives on (see the windows twin).
    #[cfg(unix)]
    pub fn statfs_json(path: &str) -> std::io::Result<String> {
        let c_path = std::ffi::CString::new(path)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;

        // Linux and macOS both have the BSD `statfs`, which carries the
        // filesystem type id (`f_type`) node reports. The remaining unix
        // targets only have POSIX `statvfs`, which has no type field --
        // libuv reports 0 there, so this does too rather than invent one.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // rustix owns the out-parameter: it allocates the `statfs`, makes
            // the call, and hands back an initialized struct or an `Errno` --
            // so neither the zeroed-buffer assumption nor the "fields are only
            // valid after a 0 return" obligation exists on this side any more.
            let buf = rustix::fs::statfs(c_path.as_c_str()).map_err(super::io_from_errno)?;
            Ok(statfs_fields_json(
                buf.f_type as u64,
                buf.f_bsize as u64,
                buf.f_blocks as u64,
                buf.f_bfree as u64,
                buf.f_bavail as u64,
                buf.f_files as u64,
                buf.f_ffree as u64,
            ))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            // Same out-parameter ownership note as the BSD `statfs` arm above.
            let buf = rustix::fs::statvfs(c_path.as_c_str()).map_err(super::io_from_errno)?;
            Ok(statfs_fields_json(
                0,
                buf.f_bsize as u64,
                buf.f_blocks as u64,
                buf.f_bfree as u64,
                buf.f_bavail as u64,
                buf.f_files as u64,
                buf.f_ffree as u64,
            ))
        }
    }

    /// stat/lstat payload, shared with the sync native in oam_engine for a
    /// single wire shape ({kind, size, mtimeMs, ...}).
    pub fn stat_to_json(meta: &std::fs::Metadata, source: StatSource<'_>) -> String {
        fn ms(time: std::io::Result<std::time::SystemTime>) -> f64 {
            time.ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0)
        }
        let kind = if meta.is_symlink() {
            "symlink"
        } else if meta.is_dir() {
            "dir"
        } else {
            "file"
        };
        // `mode` was hardcoded to 0, which quietly broke every permission and
        // file-type test a caller might run on it (`mode & 0o111`, `mode &
        // S_IFMT`) -- and 0 is not "unknown", it reads as a real answer.
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt;
            meta.mode()
        };
        #[cfg(windows)]
        let mode = {
            // Windows has no POSIX mode, so libuv synthesizes one from the
            // read-only attribute plus the file type, and node reports that.
            // Verified against node on win32: writable file 0o100666,
            // read-only file 0o100444, directory 0o40666 (no execute bits).
            let permission_bits: u32 = if meta.permissions().readonly() {
                0o444
            } else {
                0o666
            };
            let type_bits: u32 = if meta.is_symlink() {
                0o120000
            } else if meta.is_dir() {
                0o040000
            } else {
                0o100000
            };
            type_bits | permission_bits
        };
        let extras = stat_extras(meta, source);
        let mut payload = serde_json::json!({
            "kind": kind,
            "size": meta.len(),
            "mtimeMs": ms(meta.modified()),
            "atimeMs": ms(meta.accessed()),
            // Falls back to mtime only when the real change time could not be
            // read; the two genuinely differ (chmod moves ctime, not mtime).
            "ctimeMs": extras
                .as_ref()
                .and_then(|extras| extras.ctime_ms)
                .unwrap_or_else(|| ms(meta.modified())),
            "birthtimeMs": ms(meta.created()),
            "mode": mode,
        });
        if let Some(extras) = extras {
            let fields = payload
                .as_object_mut()
                .expect("json! macro built an object literal");
            for (name, value) in [
                ("dev", extras.dev),
                ("ino", extras.ino),
                ("nlink", extras.nlink),
                ("uid", extras.uid),
                ("gid", extras.gid),
                ("rdev", extras.rdev),
                ("blksize", extras.blksize),
                ("blocks", extras.blocks),
            ] {
                fields.insert(name.to_string(), value.into());
            }
        }
        payload.to_string()
    }

    pub fn readdir_to_json(path: &str) -> std::io::Result<String> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = match entry.file_type() {
                Ok(t) if t.is_symlink() => "symlink",
                Ok(t) if t.is_dir() => "dir",
                _ => "file",
            };
            entries.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": kind,
            }));
        }
        Ok(serde_json::Value::Array(entries).to_string())
    }

    pub async fn fs_read_file(path: String) -> OpOutcome {
        // Always raw bytes: encodings decode JS-side via Buffer#toString
        // (a Rust-side utf8-lossy decode was silently wrong for base64/
        // hex/latin1 requests).
        //
        // io_uring fast path (Linux, opt-in via OAM_IO_URING): on success use
        // the bytes; on ANY error (incl. io_uring unavailable / worker gone)
        // fall through to the std path below, which is authoritative for the
        // node-shaped error mapping. io_uring is a pure optimization here.
        #[cfg(target_os = "linux")]
        {
            if let Some(uring) = crate::io_uring_fs::global()
                && let Ok(bytes) = uring.read_file(path.clone()).await
            {
                return OpOutcome::Bytes(bytes);
            }
        }
        match tokio::fs::read(&path).await {
            Ok(bytes) => OpOutcome::Bytes(bytes),
            Err(e) => node_fail_at(super::FsSite::ReadFile, e, "open", &path),
        }
    }

    pub async fn fs_write_file(path: String, data: Vec<u8>, append: bool) -> OpOutcome {
        // io_uring fast path (Linux, opt-in): non-append writes only (create +
        // truncate + write_all_at). `data` is moved in -- no clone on the happy
        // path. On a worker-channel failure write_file hands the un-consumed
        // `data` back (a non-empty Vec) so we fall through to the std path with
        // it; a genuine io error from the worker comes back with an empty Vec
        // and is surfaced directly (node_fail), matching the read path's
        // "io_uring is a pure optimization, never a correctness dependency"
        // contract. Append and the no-io_uring case use the std path below.
        #[cfg(target_os = "linux")]
        {
            if !append && let Some(uring) = crate::io_uring_fs::global() {
                match uring.write_file(path.clone(), data).await {
                    Ok(()) => return OpOutcome::Done,
                    // Channel failure with the buffer recovered: retry via std.
                    Err((_chan_err, recovered)) if !recovered.is_empty() => {
                        return fs_write_file_std(path, recovered, append).await;
                    }
                    // Genuine io error (empty buffer), or an unrecoverable
                    // channel failure where the data is gone: surface directly.
                    Err((e, _)) => {
                        return node_fail_at(super::FsSite::WriteFile, e, "open", &path);
                    }
                }
            }
        }
        fs_write_file_std(path, data, append).await
    }

    /// std write path (blocking-pool append / `tokio::fs::write`). Factored out
    /// so the io_uring fast path can fall through to it with a recovered buffer
    /// on a worker-channel failure.
    async fn fs_write_file_std(path: String, data: Vec<u8>, append: bool) -> OpOutcome {
        let result = if append {
            tokio::task::spawn_blocking({
                let path = path.clone();
                move || {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .and_then(|mut f| f.write_all(&data))
                }
            })
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        } else {
            tokio::fs::write(&path, data).await
        };
        let site = if append {
            super::FsSite::AppendFile
        } else {
            super::FsSite::WriteFile
        };
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail_at(site, e, "open", &path),
        }
    }

    pub async fn fs_stat(path: String, lstat: bool) -> OpOutcome {
        // One hop to a blocking thread for the whole operation. Awaiting
        // tokio's metadata and THEN opening a handle here would do the second
        // (blocking) open on a runtime thread.
        let owned = path.clone();
        let result = tokio::task::spawn_blocking(move || stat_path_json(&owned, lstat)).await;
        match result {
            Ok(Ok(json)) => OpOutcome::Json(json),
            Ok(Err(e)) => node_fail(e, if lstat { "lstat" } else { "stat" }, &path),
            Err(e) => node_fail(
                std::io::Error::other(e.to_string()),
                if lstat { "lstat" } else { "stat" },
                &path,
            ),
        }
    }

    /// fstat of an already-open descriptor, off the loop thread.
    ///
    /// Takes an OWNED (try_clone'd) handle rather than the registry so the
    /// caller never holds the file-registry lock across the blocking read.
    pub async fn fs_fstat(file: std::fs::File) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            file.metadata()
                .map(|meta| stat_to_json(&meta, StatSource::File(&file)))
        })
        .await;
        match result {
            Ok(Ok(json)) => OpOutcome::Json(json),
            Ok(Err(e)) => node_fail(e, "fstat", ""),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), "fstat", ""),
        }
    }

    pub async fn fs_statfs(path: String) -> OpOutcome {
        let owned = path.clone();
        let result = tokio::task::spawn_blocking(move || statfs_json(&owned)).await;
        match result {
            Ok(Ok(json)) => OpOutcome::Json(json),
            Ok(Err(e)) => node_fail(e, "statfs", &path),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), "statfs", &path),
        }
    }

    pub async fn fs_readdir(path: String) -> OpOutcome {
        match tokio::task::spawn_blocking({
            let path = path.clone();
            move || readdir_to_json(&path)
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        {
            Ok(json) => OpOutcome::Json(json),
            Err(e) => node_fail_at(super::FsSite::Scandir, e, "scandir", &path),
        }
    }

    pub async fn fs_mkdir(path: String, recursive: bool) -> OpOutcome {
        let result = if recursive {
            tokio::fs::create_dir_all(&path).await
        } else {
            tokio::fs::create_dir(&path).await
        };
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail_at(super::FsSite::Mkdir { recursive }, e, "mkdir", &path),
        }
    }

    pub async fn fs_rm(path: String, recursive: bool, force: bool) -> OpOutcome {
        let result = tokio::task::spawn_blocking({
            let path = path.clone();
            move || super::remove_path(&path, recursive)
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => OpOutcome::Done,
            Err(e) => node_fail(e, "rm", &path),
        }
    }

    pub async fn fs_unlink(path: String) -> OpOutcome {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "unlink", &path),
        }
    }

    pub async fn fs_rename(from: String, to: String) -> OpOutcome {
        match tokio::fs::rename(&from, &to).await {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "rename", &from),
        }
    }

    pub async fn fs_copy_file(from: String, to: String) -> OpOutcome {
        match tokio::fs::copy(&from, &to).await {
            Ok(_) => OpOutcome::Done,
            Err(e) => node_fail(e, "copyfile", &from),
        }
    }

    pub async fn fs_access(path: String, mode: i32) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || match super::check_access(&path, mode) {
            Ok(()) => OpOutcome::Done,
            Err((code, message, errno)) => {
                OpOutcome::node_failed_at(code, message, "access", Some(path.as_str()), errno)
            }
        })
        .await;
        result.unwrap_or_else(|e| OpOutcome::Failed(format!("access: {e}")))
    }

    pub async fn fs_realpath(path: String) -> OpOutcome {
        match tokio::fs::canonicalize(&path).await {
            Ok(real) => OpOutcome::Text(super::strip_unc_prefix(&real)),
            Err(e) => node_fail(e, "realpath", &path),
        }
    }

    /// The directory `mkdtemp(prefix)` will create. Exposed so the op layer
    /// can permission-check the path that is actually written rather than the
    /// caller's prefix -- with a relative prefix the two differ (the prefix
    /// resolves under the system temp dir), so checking the prefix denied
    /// writes inside a correctly-granted temp dir and vice versa.
    pub fn mkdtemp_target(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "{}{}",
            prefix,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    /// `dir` comes from `mkdtemp_target(&prefix)`, resolved ONCE by the op
    /// layer so the path it permission-checked is the path created here.
    /// Resolving it again would mint a fresh timestamp, leaving the checked
    /// path and the created path different strings.
    pub async fn fs_mkdtemp(dir: std::path::PathBuf, prefix: String) -> OpOutcome {
        match tokio::fs::create_dir(&dir).await {
            Ok(()) => OpOutcome::Text(super::strip_unc_prefix(&dir)),
            Err(e) => node_fail(e, "mkdtemp", &prefix),
        }
    }

    pub async fn fs_symlink(target: String, path: String) -> OpOutcome {
        #[cfg(windows)]
        let result = {
            let is_dir = tokio::fs::metadata(&target)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if is_dir {
                tokio::fs::symlink_dir(&target, &path).await
            } else {
                tokio::fs::symlink_file(&target, &path).await
            }
        };
        #[cfg(not(windows))]
        let result = tokio::fs::symlink(&target, &path).await;
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "symlink", &path),
        }
    }

    pub async fn fs_readlink(path: String) -> OpOutcome {
        match tokio::fs::read_link(&path).await {
            Ok(target) => OpOutcome::Text(super::strip_unc_prefix(&target)),
            Err(e) => node_fail_at(super::FsSite::Readlink, e, "readlink", &path),
        }
    }

    pub async fn fs_link(existing: String, new_path: String) -> OpOutcome {
        match tokio::fs::hard_link(&existing, &new_path).await {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "link", &new_path),
        }
    }

    pub async fn fs_chmod(path: String, mode: u32) -> OpOutcome {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).await {
                Ok(()) => OpOutcome::Done,
                Err(e) => node_fail(e, "chmod", &path),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            let readonly = mode & 0o200 == 0;
            match tokio::fs::metadata(&path).await {
                Ok(meta) => {
                    let mut perms = meta.permissions();
                    perms.set_readonly(readonly);
                    match tokio::fs::set_permissions(&path, perms).await {
                        Ok(()) => OpOutcome::Done,
                        Err(e) => node_fail(e, "chmod", &path),
                    }
                }
                Err(e) => node_fail(e, "chmod", &path),
            }
        }
    }

    pub async fn fs_truncate(path: String, len: u64) -> OpOutcome {
        match tokio::fs::OpenOptions::new().write(true).open(&path).await {
            Ok(f) => match f.set_len(len).await {
                Ok(()) => OpOutcome::Done,
                Err(e) => node_fail(e, "ftruncate", &path),
            },
            Err(e) => node_fail(e, "open", &path),
        }
    }

    // ------------------------------------------------- fd-based fs operations
    //
    // Each takes an owned `std::fs::File` the caller cloned out of the sync
    // registry, the same shape `fs_fstat` uses. The descriptor can only have
    // come from an `open` that was permission-checked, so these are
    // exempt-by-capability in `permission_audit`: there is no path here to
    // re-check, and node draws the line in the same place.
    //
    // The error `path` is empty for all of them because an fd genuinely has no
    // path -- node reports these as `EBADF: bad file descriptor, fsync` with no
    // path either.

    /// `fchmod(2)`. Windows has no mode bits, only a read-only flag, so the
    /// owner-write bit decides it -- the same reduction `fs_chmod` already makes
    /// for the path form.
    fn fchmod_file(file: &std::fs::File, mode: u32) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))
        }
        #[cfg(not(unix))]
        {
            let mut perms = file.metadata()?.permissions();
            perms.set_readonly(mode & 0o200 == 0);
            file.set_permissions(perms)
        }
    }

    /// `fchown(2)`. Windows has no uid/gid and libuv reports success there
    /// rather than failing, so portable code does not have to branch on
    /// platform. Mirrored deliberately: a no-op that reports success is node's
    /// documented behaviour here, not an omission.
    fn fchown_file(file: &std::fs::File, uid: u32, gid: u32) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            // `&std::fs::File` already IS an `AsFd`, so rustix borrows the live
            // descriptor directly -- there is no raw-fd round trip to justify.
            // -1 ("leave this one alone") becomes `None`; see `chown_uid`.
            rustix::fs::fchown(file, super::chown_uid(uid), super::chown_gid(gid))
                .map_err(super::io_from_errno)
        }
        #[cfg(not(unix))]
        {
            let _ = (file, uid, gid);
            Ok(())
        }
    }

    /// `futimens(2)` / `SetFileTime`. Times arrive as milliseconds since the
    /// unix epoch -- what the JS layer produces from a number, a Date or a
    /// numeric string.
    /// Milliseconds since the epoch -> `timespec`.
    ///
    /// floor + remainder rather than a plain cast: for a negative time (before
    /// 1970, which node permits) a truncating cast rounds toward zero and lands
    /// a nanosecond field whose sign disagrees with its seconds.
    ///
    /// `rustix::fs::Timespec` rather than `libc::timespec`: on Linux rustix
    /// runs the raw-syscall backend and carries its own (identically laid out)
    /// struct, so this has to be the type the call actually takes. `tv_sec` is
    /// `i64` on every target; `tv_nsec` is `i64` or `c_long` depending on the
    /// backend, hence the inferred cast.
    #[cfg(unix)]
    fn to_timespec(ms: f64) -> rustix::fs::Timespec {
        let secs = (ms / 1000.0).floor();
        let nanos = ((ms - secs * 1000.0) * 1_000_000.0).round();
        rustix::fs::Timespec {
            tv_sec: secs as _,
            tv_nsec: nanos as _,
        }
    }

    /// The `(atime, mtime)` pair `futimens`/`utimensat` take, in rustix's named
    /// form -- which replaces the positional two-element array the raw calls
    /// used and removes the chance of stamping them in the wrong order.
    #[cfg(unix)]
    fn timestamps(atime_ms: f64, mtime_ms: f64) -> rustix::fs::Timestamps {
        rustix::fs::Timestamps {
            last_access: to_timespec(atime_ms),
            last_modification: to_timespec(mtime_ms),
        }
    }

    fn futimes_file(file: &std::fs::File, atime_ms: f64, mtime_ms: f64) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::futimens(file, &timestamps(atime_ms, mtime_ms))
                .map_err(super::io_from_errno)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::FILETIME;
            use windows_sys::Win32::Storage::FileSystem::SetFileTime;
            // FILETIME counts 100ns ticks from 1601-01-01; the unix epoch is
            // 11644473600 seconds later.
            fn to_filetime(ms: f64) -> FILETIME {
                let ticks = ((ms + 11_644_473_600_000.0) * 10_000.0).round().max(0.0) as u64;
                FILETIME {
                    dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
                    dwHighDateTime: (ticks >> 32) as u32,
                }
            }
            let atime = to_filetime(atime_ms);
            let mtime = to_filetime(mtime_ms);
            // Null creation time leaves it untouched, which is what futimes
            // means -- it sets access and modification only.
            // SAFETY: `file` is a live File whose handle is valid for the call;
            // the null creation-time pointer leaves it untouched, and
            // `&atime`/`&mtime` are live FILETIME locals the call only reads.
            let ok =
                unsafe { SetFileTime(file.as_raw_handle() as _, std::ptr::null(), &atime, &mtime) };
            if ok != 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
    }

    /// `fsync(2)` / `fdatasync(2)`. std spells them `sync_all` and `sync_data`
    /// and handles the platform mapping (`FlushFileBuffers` on Windows, where
    /// there is no data-only variant and both collapse to the same call).
    pub async fn fs_fsync(file: std::fs::File, data_only: bool) -> OpOutcome {
        let syscall = if data_only { "fdatasync" } else { "fsync" };
        let result = tokio::task::spawn_blocking(move || {
            if data_only {
                file.sync_data()
            } else {
                file.sync_all()
            }
        })
        .await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, syscall, ""),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), syscall, ""),
        }
    }

    pub async fn fs_ftruncate(file: std::fs::File, len: u64) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || file.set_len(len)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, "ftruncate", ""),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), "ftruncate", ""),
        }
    }

    pub async fn fs_fchmod(file: std::fs::File, mode: u32) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || fchmod_file(&file, mode)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, "fchmod", ""),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), "fchmod", ""),
        }
    }

    pub async fn fs_fchown(file: std::fs::File, uid: u32, gid: u32) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || fchown_file(&file, uid, gid)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, "fchown", ""),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), "fchown", ""),
        }
    }

    pub async fn fs_futimes(file: std::fs::File, atime_ms: f64, mtime_ms: f64) -> OpOutcome {
        let result =
            tokio::task::spawn_blocking(move || futimes_file(&file, atime_ms, mtime_ms)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, "futime", ""),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), "futime", ""),
        }
    }

    /// Synchronous twins. The sync fs family runs inline in the op callback
    /// (no `spawn_op`), so these are called directly rather than awaited.
    pub fn fs_fchmod_sync(file: &std::fs::File, mode: u32) -> std::io::Result<()> {
        fchmod_file(file, mode)
    }

    pub fn fs_fchown_sync(file: &std::fs::File, uid: u32, gid: u32) -> std::io::Result<()> {
        fchown_file(file, uid, gid)
    }

    pub fn fs_futimes_sync(
        file: &std::fs::File,
        atime_ms: f64,
        mtime_ms: f64,
    ) -> std::io::Result<()> {
        futimes_file(file, atime_ms, mtime_ms)
    }

    // ----------------------------------------------- path-based ownership/time
    //
    // chown / lchown / utimes / lutimes / lchmod. Unlike the fd family above,
    // these name a path, so they DO take a permission check at the op layer.
    //
    // Behaviours below were measured against node v22.22.2 on win32 rather than
    // reasoned about, because two of them are counter-intuitive:
    //   - chownSync on a NONEXISTENT path returns cleanly on Windows. libuv's
    //     chown is a no-op there and never touches the path, so there is no
    //     ENOENT to report. Validating the path first would be MORE correct in
    //     the abstract and would diverge from node.
    //   - the error `syscall` is the singular `utime` / `lutime`, not the
    //     plural spelling of the JS function.

    /// `chown(2)` / `lchown(2)`, selected by `follow`.
    fn chown_path(path: &str, uid: u32, gid: u32, follow: bool) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let c = std::ffi::CString::new(path).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path contains an interior NUL byte",
                )
            })?;
            // -1 in either field means "leave it alone"; see `chown_uid` for why
            // that sentinel has to be spelled `None` explicitly here.
            //
            // `lchown` has no standalone spelling in rustix, so the no-follow
            // arm is the portable `fchownat(AT_FDCWD, .., AT_SYMLINK_NOFOLLOW)`
            // -- the same call libc's `lchown` is a wrapper around. `CWD` is a
            // safe const `BorrowedFd`, so no raw fd is constructed.
            let (owner, group) = (super::chown_uid(uid), super::chown_gid(gid));
            let r = if follow {
                rustix::fs::chown(c.as_c_str(), owner, group)
            } else {
                rustix::fs::chownat(
                    rustix::fs::CWD,
                    c.as_c_str(),
                    owner,
                    group,
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
            };
            r.map_err(super::io_from_errno)
        }
        #[cfg(not(unix))]
        {
            // See the note above: node succeeds here even for a path that does
            // not exist. Deliberately no stat.
            let _ = (path, uid, gid, follow);
            Ok(())
        }
    }

    /// `utimensat(2)` / `SetFileTime`, selected by `follow`.
    fn utimes_path(path: &str, atime_ms: f64, mtime_ms: f64, follow: bool) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let c = std::ffi::CString::new(path).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path contains an interior NUL byte",
                )
            })?;
            let times = timestamps(atime_ms, mtime_ms);
            let flags = if follow {
                rustix::fs::AtFlags::empty()
            } else {
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW
            };
            // `CWD` is rustix's safe const `BorrowedFd` for `AT_FDCWD`.
            rustix::fs::utimensat(rustix::fs::CWD, c.as_c_str(), &times, flags)
                .map_err(super::io_from_errno)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_WRITE_ATTRIBUTES is the minimum right SetFileTime needs --
            // asking for GENERIC_WRITE would fail on a read-only file that node
            // can still stamp.
            const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
            // Without BACKUP_SEMANTICS a DIRECTORY cannot be opened at all, and
            // node's utimes works on directories (measured).
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            // lutimes: stamp the link itself rather than following it.
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
            if !follow {
                flags |= FILE_FLAG_OPEN_REPARSE_POINT;
            }
            let file = std::fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(flags)
                .open(path)?;
            // Same SetFileTime path the fd form uses -- one epoch conversion,
            // not two spellings of it.
            futimes_file(&file, atime_ms, mtime_ms)
        }
    }

    /// `lchmod`, macOS only -- node gates its own on `O_SYMLINK`, which the BSD
    /// family has and Linux does not, and exposes the name bound to `undefined`
    /// elsewhere (measured on win32 v22.22.2). The JS layer mirrors that, so
    /// this is never reached off macOS.
    ///
    /// Spelled `fchmodat(AT_FDCWD, .., AT_SYMLINK_NOFOLLOW)` rather than
    /// `lchmod`: the libc crate does NOT expose `lchmod` for
    /// aarch64-apple-darwin, so the obvious spelling is a hard compile error on
    /// the macOS leg -- caught here by cross-target clippy, which is the only
    /// way to see it from a Windows box. `fchmodat` is the portable spelling of
    /// the same call and is what the BSDs document.
    #[cfg(target_os = "macos")]
    fn lchmod_path(path: &str, mode: u32) -> std::io::Result<()> {
        let c = std::ffi::CString::new(path).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains an interior NUL byte",
            )
        })?;
        // `from_bits_retain` rather than `Mode::from` / `from_raw_mode`: those
        // mask off the S_IFMT bits, where the raw `fchmodat` this replaces
        // passed the caller's value through untouched. Retaining every bit
        // keeps the kernel -- not this wrapper -- the thing that decides what a
        // malformed mode means.
        rustix::fs::chmodat(
            rustix::fs::CWD,
            c.as_c_str(),
            rustix::fs::Mode::from_bits_retain(mode as _),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(super::io_from_errno)
    }

    pub async fn fs_chown(path: String, uid: u32, gid: u32, follow: bool) -> OpOutcome {
        let syscall = if follow { "chown" } else { "lchown" };
        let owned = path.clone();
        let result =
            tokio::task::spawn_blocking(move || chown_path(&owned, uid, gid, follow)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, syscall, &path),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), syscall, &path),
        }
    }

    pub async fn fs_utimes(path: String, atime_ms: f64, mtime_ms: f64, follow: bool) -> OpOutcome {
        // node reports the SINGULAR syscall name here (measured).
        let syscall = if follow { "utime" } else { "lutime" };
        let owned = path.clone();
        let result =
            tokio::task::spawn_blocking(move || utimes_path(&owned, atime_ms, mtime_ms, follow))
                .await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, syscall, &path),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), syscall, &path),
        }
    }

    pub fn fs_chown_sync(path: &str, uid: u32, gid: u32, follow: bool) -> std::io::Result<()> {
        chown_path(path, uid, gid, follow)
    }

    pub fn fs_utimes_sync(
        path: &str,
        atime_ms: f64,
        mtime_ms: f64,
        follow: bool,
    ) -> std::io::Result<()> {
        utimes_path(path, atime_ms, mtime_ms, follow)
    }

    /// macOS-only; the JS layer never calls this elsewhere (the export is bound
    /// to `undefined` off macOS, matching node).
    pub fn fs_lchmod_sync(path: &str, mode: u32) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            lchmod_path(path, mode)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (path, mode);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "lchmod is only available on macOS",
            ))
        }
    }

    pub async fn fs_lchmod(path: String, mode: u32) -> OpOutcome {
        let owned = path.clone();
        let result = tokio::task::spawn_blocking(move || fs_lchmod_sync(&owned, mode)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, "lchmod", &path),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), "lchmod", &path),
        }
    }

    pub async fn read_text_file(path: String) -> OpOutcome {
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => OpOutcome::Text(text),
            Err(e) => OpOutcome::Failed(format!("could not read {path}: {e}")),
        }
    }

    /// Parse the wire JSON from the JS `fetch` wrapper. Kept here so the
    /// engine never needs a serde dependency.
    pub fn parse_fetch_request(json: &str) -> Result<FetchRequest, String> {
        serde_json::from_str(json).map_err(|e| format!("fetch: malformed request: {e}"))
    }

    /// The fetch op and its wire request: oam's own transport (#143). The
    /// body reader is [`fetch_body_read`].
    pub use crate::http_client::send::{
        FetchContinuations, FetchRequest, RedirectMode, fetch, fetch_abandon, fetch_continue,
        fetch_supply,
    };

    /// zlibStreamCreate: allocate an incremental compressor or decompressor.
    /// Returns Json {handle} on success. compress=true for encoding,
    /// false for decoding. format must be "gzip", "deflate", "deflateRaw",
    /// "unzip" (decompress only), or "brotli".
    pub async fn zlib_stream_create(
        streams: super::ZlibRegistry,
        ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
        format: String,
        level: i32,
        compress: bool,
    ) -> OpOutcome {
        // Stream allocation is cheap: do it inline (no IO).
        let stream = if format == "brotli" {
            // Brotli: pure-Rust incremental backend.
            if compress {
                super::ZlibStream::BrotliCompress(Box::default())
            } else {
                super::ZlibStream::BrotliDecompress(Box::default())
            }
        } else if compress {
            let Some(fmt) = super::zlib::Format::parse(&format) else {
                return OpOutcome::Failed(format!("zlib stream: unknown format '{format}'"));
            };
            super::ZlibStream::Compress(super::zlib::StreamCompressor::new(fmt, level))
        } else {
            let dec = match format.as_str() {
                "gzip" => super::zlib::StreamDecompressor::new_gzip(),
                "deflate" => super::zlib::StreamDecompressor::new_deflate(),
                "deflateRaw" => super::zlib::StreamDecompressor::new_deflate_raw(),
                "unzip" => super::zlib::StreamDecompressor::new_unzip(),
                _ => return OpOutcome::Failed(format!("zlib stream: unknown format '{format}'")),
            };
            super::ZlibStream::Decompress(dec)
        };
        let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        streams
            .lock()
            .expect("zlib stream registry lock")
            .insert(handle, stream);
        OpOutcome::Json(serde_json::json!({ "handle": handle }).to_string())
    }

    /// zlibStreamWrite: feed one chunk into an incremental stream.
    /// Resolves with the bytes produced immediately (may be empty for
    /// compressors that buffer internally). Runs on spawn_blocking --
    /// the CPU work is non-trivial for large chunks.
    pub async fn zlib_stream_write(
        streams: super::ZlibRegistry,
        handle: u64,
        chunk: Vec<u8>,
    ) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = streams.lock().unwrap_or_else(|e| e.into_inner());
            let Some(stream) = guard.get_mut(&handle) else {
                return Err(format!("zlib stream: handle {handle} not found"));
            };
            match stream {
                super::ZlibStream::Compress(enc) => enc
                    .write_chunk(&chunk)
                    .map_err(|e| format!("zlib stream write: {e}")),
                super::ZlibStream::Decompress(dec) => dec
                    .write_chunk(&chunk)
                    .map_err(|e| format!("zlib stream write: {e}")),
                super::ZlibStream::BrotliCompress(enc) => enc
                    .write_chunk(&chunk)
                    .map_err(|e| format!("brotli stream write: {e}")),
                super::ZlibStream::BrotliDecompress(dec) => dec
                    .write_chunk(&chunk)
                    .map_err(|e| format!("brotli stream write: {e}")),
                super::ZlibStream::HandleCompress(_) | super::ZlibStream::HandleDecompress(_) => {
                    Err("zlib handle: use zlibHandleWriteSync, not zlibStreamWrite".into())
                }
            }
        })
        .await;
        match result {
            Ok(Ok(bytes)) => OpOutcome::Bytes(bytes),
            Ok(Err(msg)) => OpOutcome::Failed(msg),
            Err(e) => OpOutcome::Failed(format!("zlib stream write task: {e}")),
        }
    }

    /// zlibStreamFlush: finalize and remove the stream. Returns the tail
    /// bytes. For compressors, this emits the format trailer (CRC etc.).
    /// For decompressors, this finalizes the inflate/brotli state machine
    /// and returns any remaining output bytes.
    pub async fn zlib_stream_flush(streams: super::ZlibRegistry, handle: u64) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            let stream = streams
                .lock()
                .expect("zlib stream registry lock")
                .remove(&handle);
            let Some(stream) = stream else {
                return Err(format!("zlib stream: handle {handle} not found"));
            };
            match stream {
                super::ZlibStream::Compress(enc) => {
                    enc.finish().map_err(|e| format!("zlib stream flush: {e}"))
                }
                super::ZlibStream::Decompress(dec) => {
                    dec.finish().map_err(|e| format!("zlib stream flush: {e}"))
                }
                super::ZlibStream::BrotliCompress(enc) => enc
                    .finish()
                    .map_err(|e| format!("brotli stream flush: {e}")),
                super::ZlibStream::BrotliDecompress(dec) => dec
                    .finish()
                    .map_err(|e| format!("brotli stream flush: {e}")),
                super::ZlibStream::HandleCompress(_) | super::ZlibStream::HandleDecompress(_) => {
                    Err("zlib handle: use close(), not zlibStreamFlush".into())
                }
            }
        })
        .await;
        match result {
            Ok(Ok(bytes)) => OpOutcome::Bytes(bytes),
            Ok(Err(msg)) => OpOutcome::Failed(msg),
            Err(e) => OpOutcome::Failed(format!("zlib stream flush task: {e}")),
        }
    }

    /// zlibStreamClose: drop a stream without flushing. Synchronous --
    /// just removes the entry from the registry. Called by the JS side
    /// when the Transform stream is destroyed before completion.
    pub fn zlib_stream_close(streams: &super::ZlibRegistry, handle: u64) {
        streams
            .lock()
            .expect("zlib stream registry lock")
            .remove(&handle);
    }

    /// zlibHandleCreate: allocate a low-level flate2 Compress or Decompress
    /// handle for Node's zlib binding interface (used by ssh2 etc.).
    /// mode: 1=DEFLATE, 2=INFLATE, 5=DEFLATERAW, 6=INFLATERAW.
    pub fn zlib_handle_create(
        streams: &super::ZlibRegistry,
        ids: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        mode: i32,
        level: i32,
    ) -> Result<u64, String> {
        let zlib_header = mode == 1 || mode == 2;
        let stream = match mode {
            1 | 5 => {
                let lvl = if (0..=9).contains(&level) {
                    flate2::Compression::new(level as u32)
                } else {
                    flate2::Compression::default()
                };
                super::ZlibStream::HandleCompress(flate2::Compress::new(lvl, zlib_header))
            }
            2 | 6 => super::ZlibStream::HandleDecompress(flate2::Decompress::new(zlib_header)),
            _ => return Err(format!("zlib handle: unknown mode {mode}")),
        };
        let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        streams
            .lock()
            .expect("zlib registry lock")
            .insert(handle, stream);
        Ok(handle)
    }

    /// zlibHandleWriteSync: synchronous incremental compress/decompress.
    /// Returns (availOutAfter, availInAfter). The caller provides a mutable
    /// output slice; compressed/decompressed bytes are written into it.
    pub fn zlib_handle_write_sync(
        streams: &super::ZlibRegistry,
        handle: u64,
        flush: i32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize), String> {
        let mut guard = streams.lock().unwrap_or_else(|e| e.into_inner());
        let stream = guard
            .get_mut(&handle)
            .ok_or_else(|| format!("zlib handle {handle} not found"))?;
        match stream {
            super::ZlibStream::HandleCompress(c) => {
                let before_in = c.total_in();
                let before_out = c.total_out();
                let fl = match flush {
                    0 => flate2::FlushCompress::None,
                    1 => flate2::FlushCompress::Partial,
                    2 => flate2::FlushCompress::Sync,
                    3 => flate2::FlushCompress::Full,
                    4 => flate2::FlushCompress::Finish,
                    _ => flate2::FlushCompress::None,
                };
                c.compress(input, output, fl)
                    .map_err(|e| format!("zlib handle compress: {e}"))?;
                let consumed = (c.total_in() - before_in) as usize;
                let produced = (c.total_out() - before_out) as usize;
                Ok((output.len() - produced, input.len() - consumed))
            }
            super::ZlibStream::HandleDecompress(d) => {
                let before_in = d.total_in();
                let before_out = d.total_out();
                let fl = match flush {
                    2 => flate2::FlushDecompress::Sync,
                    4 => flate2::FlushDecompress::Finish,
                    _ => flate2::FlushDecompress::None,
                };
                d.decompress(input, output, fl)
                    .map_err(|e| format!("zlib handle decompress: {e}"))?;
                let consumed = (d.total_in() - before_in) as usize;
                let produced = (d.total_out() - before_out) as usize;
                Ok((output.len() - produced, input.len() - consumed))
            }
            _ => Err(format!("zlib handle {handle} is not a handle variant")),
        }
    }

    /// Async zlib: CPU-bound, so spawn_blocking off the op channel
    /// (Node's threadpool model). compress=true encodes, false decodes;
    /// format "unzip" auto-detects on the decode side.
    /// `max_output` is node's `maxOutputLength` for a decode; `None` is no cap.
    pub async fn zlib_transform(
        bytes: Vec<u8>,
        format: String,
        level: i32,
        compress: bool,
        max_output: Option<usize>,
    ) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            if !compress && format == "unzip" {
                return super::zlib::unzip_capped(&bytes, max_output);
            }
            let Some(parsed) = super::zlib::Format::parse(&format) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown zlib format '{format}'"),
                ));
            };
            if compress {
                super::zlib::compress_capped(&bytes, parsed, level, max_output)
            } else {
                super::zlib::decompress_capped(&bytes, parsed, max_output)
            }
        })
        .await;
        match result {
            Ok(Ok(out)) => OpOutcome::Bytes(out),
            // The over-cap sentinel travels unprefixed: the shim matches on it
            // to raise node's ERR_BUFFER_TOO_LARGE.
            Ok(Err(e)) if e.to_string() == super::zlib::OUTPUT_TOO_LARGE => {
                OpOutcome::Failed(super::zlib::OUTPUT_TOO_LARGE.to_string())
            }
            Ok(Err(e)) => OpOutcome::Failed(format!("zlib: {e}")),
            Err(e) => OpOutcome::Failed(format!("zlib task: {e}")),
        }
    }

    /// Open a file for streaming. Takes node's full fopen-style flag set via
    /// the shared `open_options_for` table -- the same one the sync open uses,
    /// so a flag that works on `fs.openSync` works here too. Resolves with
    /// Json {handle}.
    pub async fn fs_open(
        files: super::FileRegistry,
        ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
        path: String,
        mode: String,
    ) -> OpOutcome {
        let options = super::open_options_for(&mode);
        // std OpenOptions on the blocking pool rather than tokio::fs, so the
        // descriptor lands in the ONE registry the sync family also reads.
        // tokio::fs::open is this exact call on this exact pool.
        let owned = path.clone();
        let opened = tokio::task::spawn_blocking(move || options.open(&owned)).await;
        let opened = match opened {
            Ok(r) => r,
            Err(e) => return node_fail(std::io::Error::other(e.to_string()), "open", &path),
        };
        match opened {
            Ok(file) => {
                let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                files
                    .lock()
                    .expect("file registry lock")
                    .files
                    .insert(handle, file);
                OpOutcome::Json(serde_json::json!({ "handle": handle }).to_string())
            }
            Err(e) => node_fail_at(super::FsSite::Open(&mode), e, "open", &path),
        }
    }

    /// Reinsert a File ONLY if it was not closed mid-flight. Returns
    /// whether it was kept (false = the handle was retired by fsClose
    /// during the IO await, so the File is dropped here, closing the fd).
    fn reinsert_file(files: &super::FileRegistry, handle: u64, file: std::fs::File) -> bool {
        let mut guard = files.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed.remove(&handle) {
            drop(file); // closed during the await: do not resurrect
            false
        } else {
            guard.files.insert(handle, file);
            true
        }
    }

    /// Read up to `len` bytes. Bytes = data, Done = EOF (handle stays open
    /// until fs_close — the JS side closes explicitly).
    /// Read up to `len` bytes. `position` = None reads from (and advances) the
    /// cursor; Some(p) is a `pread` -- reads at p and leaves the cursor alone.
    ///
    /// The position parameter used to not exist, so the JS `fs.read` callback
    /// form had nowhere to put the one it was given and silently dropped it:
    /// `fs.read(fd, buf, 0, 3, 10, cb)` read from the CURSOR instead of offset
    /// 10 and returned the wrong bytes with no error. Worse than the sync twin
    /// fixed alongside it, which at least read the right bytes and only left
    /// the cursor misplaced.
    pub async fn fs_read_chunk(
        files: super::FileRegistry,
        handle: u64,
        len: usize,
        position: Option<u64>,
    ) -> OpOutcome {
        use std::io::{Read, Seek, SeekFrom};
        let file = files
            .lock()
            .expect("file registry lock")
            .files
            .remove(&handle);
        let Some(mut file) = file else {
            return node_fail_ebadf("read");
        };
        // Exactly `len`, unclamped -- same as the sync twin's `vec![0u8; length]`.
        // An 8 MiB ceiling here silently short-read anything bigger (a 10 MiB
        // readv reported 8388608 where readvSync and node both report
        // 10485760), and the old min-of-1 turned a zero-length read into a
        // one-byte one. `len` is bounded by the caller's own destination buffer
        // on the JS side, which is where node bounds it too (ERR_OUT_OF_RANGE).
        let mut buf = vec![0u8; len];
        // The File moves onto the blocking pool and comes back with the read's
        // result, because the remove-operate-reinsert dance needs it returned
        // whichever way the read went.
        let done = tokio::task::spawn_blocking(move || {
            // pread when a position is given: save, seek, read, restore. Same
            // rule the sync family follows -- node's positional read does not
            // disturb the cursor.
            let r = match position {
                None => file.read(&mut buf),
                Some(p) => {
                    let saved = file.stream_position();
                    match file.seek(SeekFrom::Start(p)) {
                        Ok(_) => {
                            let r = file.read(&mut buf);
                            if let Ok(prev) = saved {
                                let _ = file.seek(SeekFrom::Start(prev));
                            }
                            r
                        }
                        Err(e) => Err(e),
                    }
                }
            };
            (file, buf, r)
        })
        .await;
        let (file, mut buf, result) = match done {
            Ok(t) => t,
            Err(e) => {
                return node_fail(
                    std::io::Error::other(e.to_string()),
                    "read",
                    &handle.to_string(),
                );
            }
        };
        // Reinstate the descriptor BEFORE branching on the outcome. A failed
        // read does not close the file in node, but the error arm used to drop
        // `file` here: the OS handle closed and the registry entry vanished, so
        // the fd that had just reported EBADF was then genuinely dead and every
        // later write/close on it failed too. Only fsClose retires a handle.
        reinsert_file(&files, handle, file);
        match result {
            Ok(0) => OpOutcome::Done,
            Ok(n) => {
                buf.truncate(n);
                OpOutcome::Bytes(buf)
            }
            Err(e) => node_fail_fd(e, "read"),
        }
    }

    /// Write one chunk to an open handle (the node:stream write queue
    /// serializes callers).
    ///
    /// `position` = None appends at the cursor; Some(p) is a `pwrite` -- writes
    /// at p and leaves the cursor alone, which is what a positional
    /// `FileHandle.write` means. A descriptor opened in APPEND mode ignores the
    /// position and always writes at the end; that is the OS's behaviour and
    /// node's, and restoring the cursor afterwards does not change it.
    pub async fn fs_write_chunk(
        files: super::FileRegistry,
        handle: u64,
        bytes: Vec<u8>,
        position: Option<u64>,
    ) -> OpOutcome {
        use std::io::{Seek, SeekFrom};
        let file = files
            .lock()
            .expect("file registry lock")
            .files
            .remove(&handle);
        let Some(mut file) = file else {
            return node_fail_ebadf("write");
        };
        // Node's contract is callback-after-syscall.
        //
        // The tokio version of this needed an explicit flush() to get that:
        // `write_all` on a tokio::fs::File resolves once the bytes reach
        // tokio's INTERNAL buffer, with the real write(2) running later on the
        // blocking pool (flushed on drop, unsynchronized). Without the flush,
        // WriteStream 'finish' raced the kernel write and a finish-handler read
        // saw a short file -- conformance case 21 flaked exactly that way on
        // Linux under load.
        //
        // std::fs::File is UNBUFFERED: write_all IS the syscall, and it has
        // already returned by the time the blocking task completes. The race
        // cannot be reintroduced by forgetting a flush, because there is no
        // buffer to forget about.
        let done = tokio::task::spawn_blocking(move || {
            // pwrite when a position is given: save, seek, write, restore --
            // the same rule `fs_read_chunk` and the sync family follow.
            let r = match position {
                None => super::write_all_checked(&mut file, &bytes),
                Some(p) => {
                    let saved = file.stream_position();
                    match file.seek(SeekFrom::Start(p)) {
                        Ok(_) => {
                            let r = super::write_all_checked(&mut file, &bytes);
                            if let Ok(prev) = saved {
                                let _ = file.seek(SeekFrom::Start(prev));
                            }
                            r
                        }
                        Err(e) => Err(e),
                    }
                }
            };
            (file, r)
        })
        .await;
        let (file, written) = match done {
            Ok(t) => t,
            Err(e) => {
                return node_fail_fd(std::io::Error::other(e.to_string()), "write");
            }
        };
        // Reinstated before branching: a failed write must not retire the
        // descriptor. See the matching note in `fs_read_chunk`.
        reinsert_file(&files, handle, file);
        match written {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail_fd(e, "write"),
        }
    }

    /// Read one chunk from a streaming body. Bytes = a chunk, Done = EOF
    /// (handle dropped). Remove-read-reinsert keeps the lock short; the JS
    /// ReadableStream lock guarantees a single reader per handle. A cancel
    /// that landed while the read was in flight drops the body instead of
    /// reinserting it.
    pub use crate::http_client::body::read as fetch_body_read;

    #[cfg(test)]
    mod statfs_wire_tests {
        use super::statfs_fields_json;

        /// The decimal-string wire format is the whole reason
        /// `statfs(path, {bigint: true})` can be trusted. As JSON NUMBERS
        /// these would be doubles: 2^53+1 and u64::MAX both round on the way
        /// through, and the BigInt built from a rounded value is silently
        /// wrong -- precisely the case bigint mode exists to serve.
        ///
        /// No filesystem available to a test reports block counts that large,
        /// so the serializer is the only place this is checkable at all.
        #[test]
        fn fields_past_2_pow_53_survive_exactly() {
            let big = (1u64 << 53) + 1; // first integer a f64 cannot represent
            let json = statfs_fields_json(u64::MAX, 4096, big, big - 1, 0, u64::MAX - 1, 7);

            let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
            let field = |name: &str| parsed[name].as_str().expect("string").parse::<u64>();
            assert_eq!(field("type").unwrap(), u64::MAX);
            assert_eq!(field("blocks").unwrap(), big);
            assert_eq!(field("bfree").unwrap(), big - 1);
            assert_eq!(field("files").unwrap(), u64::MAX - 1);

            // The rounding this format exists to avoid, demonstrated.
            assert_eq!(big as f64 as u64, big - 1);
        }

        /// Pins the payload contract the JS `StatFs` constructor reads by
        /// name: all seven fields present, none emitted as a bare number.
        #[test]
        fn payload_carries_all_seven_fields_as_strings() {
            assert_eq!(
                statfs_fields_json(0, 1, 2, 3, 4, 5, 6),
                r#"{"type":"0","bsize":"1","blocks":"2","bfree":"3","bavail":"4","files":"5","ffree":"6"}"#
            );
        }
    }
}

/// Tests for the libc-to-rustix substitution's two silent-regression risks:
/// the `-1` uid/gid sentinel and the errno round trip. Both are `cfg(unix)`,
/// so they only run on the Linux and macOS legs.
#[cfg(all(test, unix))]
mod rustix_bridge_tests {
    use super::{chown_gid, chown_uid, io_from_errno};

    /// The whole point of `chown_uid`/`chown_gid`. `Uid::from_raw`'s own guard
    /// against `!0` is a `debug_assert!`, which is compiled out in release --
    /// so if these mapped `u32::MAX` to `Some(..)`, a release build would ask
    /// the kernel to set uid 4294967295 where node means "leave it alone", and
    /// every `chown(path, -1, gid)` would start failing with EINVAL/EPERM.
    #[test]
    fn minus_one_uid_and_gid_are_the_leave_alone_sentinel() {
        assert!(chown_uid(u32::MAX).is_none());
        assert!(chown_gid(u32::MAX).is_none());
    }

    /// ...and nothing else is. 0 (root) in particular must survive, since it
    /// is both a real id and the most likely value to be special-cased wrong.
    #[test]
    fn real_ids_including_root_survive_the_mapping() {
        for id in [0u32, 1, 1000, 65534, u32::MAX - 1] {
            assert_eq!(chown_uid(id).map(|u| u.as_raw()), Some(id));
            assert_eq!(chown_gid(id).map(|g| g.as_raw()), Some(id));
        }
    }

    /// The errno trap, half one: the NUMBER. On Linux rustix issues raw
    /// syscalls and never writes libc's `errno`, so
    /// `std::io::Error::last_os_error()` is meaningless after a rustix call --
    /// the value survives only through this helper. `node_errno` is
    /// `-raw_os_error()` on unix, so a lost number means `err.errno` goes
    /// `undefined` on every fs rejection.
    #[test]
    fn errno_number_round_trips() {
        let cases = [
            (rustix::io::Errno::NOENT, libc::ENOENT),
            (rustix::io::Errno::ACCESS, libc::EACCES),
            (rustix::io::Errno::PERM, libc::EPERM),
            (rustix::io::Errno::BADF, libc::EBADF),
            (rustix::io::Errno::NOTDIR, libc::ENOTDIR),
            (rustix::io::Errno::ISDIR, libc::EISDIR),
            (rustix::io::Errno::NOTEMPTY, libc::ENOTEMPTY),
            (rustix::io::Errno::EXIST, libc::EEXIST),
            (rustix::io::Errno::INVAL, libc::EINVAL),
            (rustix::io::Errno::PIPE, libc::EPIPE),
            (rustix::io::Errno::LOOP, libc::ELOOP),
        ];
        for (errno, raw) in cases {
            let e = io_from_errno(errno);
            assert_eq!(e.raw_os_error(), Some(raw), "raw_os_error for {raw}");
            assert_eq!(
                super::node_errno("", &e),
                Some(-raw),
                "node_errno for {raw}"
            );
        }
    }

    /// The errno trap, half two: the CODE STRING. `node_error_code` reads
    /// `ErrorKind`, which std derives from `raw_os_error()` -- so it only
    /// survives the move if the number does. Restricted to the errnos std maps
    /// to a STABLE `ErrorKind`, plus EPERM, which std folds into
    /// `PermissionDenied` alongside EACCES and `node_error_code` therefore
    /// reads from the raw number (it used to report `EACCES` for both). EBADF
    /// is not here: no stable kind, and the fd paths pass their code in
    /// explicitly rather than deriving it.
    #[test]
    fn errno_code_string_round_trips() {
        let cases = [
            (rustix::io::Errno::NOENT, "ENOENT"),
            (rustix::io::Errno::ACCESS, "EACCES"),
            (rustix::io::Errno::PERM, "EPERM"),
            (rustix::io::Errno::EXIST, "EEXIST"),
            (rustix::io::Errno::NOTDIR, "ENOTDIR"),
            (rustix::io::Errno::ISDIR, "EISDIR"),
            (rustix::io::Errno::NOTEMPTY, "ENOTEMPTY"),
            (rustix::io::Errno::INVAL, "EINVAL"),
            (rustix::io::Errno::PIPE, "EPIPE"),
        ];
        for (errno, code) in cases {
            assert_eq!(super::node_error_code(&io_from_errno(errno)), code);
        }
    }

    /// The positive-vs-negative half of the same trap: rustix's `linux_raw`
    /// backend stores kernel error values NEGATED internally. If
    /// `raw_os_error()` ever handed back that negated form, every code above
    /// would silently degrade to `EIO` (no `ErrorKind` matches a negative OS
    /// error) while still looking like a working error path.
    #[test]
    fn rustix_raw_os_error_is_positive() {
        assert!(rustix::io::Errno::NOENT.raw_os_error() > 0);
        assert_eq!(rustix::io::Errno::NOENT.raw_os_error(), libc::ENOENT);
    }
}

/// The raw-code error table and the per-operation rules on top of it. The
/// expected values are node's, measured on Windows 11 26200 with node v22.22.2
/// (libuv 1.51.0) where a probe could produce the code, and read off libuv's
/// src/win/error.c for the rest.
#[cfg(test)]
mod os_error_code_tests {
    use super::*;

    /// Every message oam writes must be libuv's, which js/node_compat.js keeps
    /// a second copy of (`UV_ERROR_MESSAGES`) for util.getSystemErrorMessage.
    /// Compared entry for entry, and counted, so neither copy can drift.
    #[test]
    fn uv_strerror_matches_the_js_table() {
        let js = include_str!("../../../js/node_compat.js");
        let start = js
            .find("const UV_ERROR_MESSAGES = {")
            .expect("UV_ERROR_MESSAGES in node_compat.js");
        let body = &js[start..];
        let body = &body[body.find('{').unwrap() + 1..body.find("};").unwrap()];
        let mut seen = 0;
        for line in body.lines() {
            let line = line.trim();
            if line.starts_with("//") {
                continue;
            }
            let mut rest = line;
            while let Some(colon) = rest.find(": \"") {
                let key = rest[..colon].trim().trim_start_matches(',').trim();
                let after = &rest[colon + 3..];
                let end = after.find('"').expect("closing quote");
                let text = &after[..end];
                assert_eq!(
                    uv_strerror(key),
                    Some(text),
                    "uv_strerror({key}) must match UV_ERROR_MESSAGES"
                );
                seen += 1;
                rest = &after[end + 1..];
            }
        }
        assert!(
            seen > 80,
            "parsed only {seen} entries from UV_ERROR_MESSAGES"
        );
        // Every JS entry matched a Rust one; equal counts leave no Rust-only
        // entry either.
        assert_eq!(
            super::UV_ERROR_MESSAGES.len(),
            seen,
            "the Rust table has entries the JS table lacks"
        );
        assert_eq!(uv_strerror("ENOTACODE"), None);
    }

    /// A raw-coded error gets libuv's words; an error with no raw code keeps
    /// its own. The TLS read path tells a peer's abrupt close from a real
    /// failure by rustls's text, so losing it is a behaviour change.
    #[test]
    fn a_message_keeps_library_text_when_there_is_no_os_code() {
        let tls = std::io::Error::other("peer closed connection without sending TLS close_notify");
        let code = node_error_code(&tls);
        assert_eq!(code, "EIO");
        assert!(node_error_message_fd(code, "read", &tls).contains("close_notify"));
        let missing = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert_eq!(
            node_error_message("ENOENT", "open", "p", &missing),
            "ENOENT: no such file or directory, open 'p'"
        );
    }

    /// A raw errno std has no kind for still gets its own name, and an EIO
    /// that is only a fallback keeps the OS's words.
    #[cfg(unix)]
    #[test]
    fn unix_errnos_are_named_by_number() {
        for (errno, code) in [
            (libc::EPERM, "EPERM"),
            (libc::EMFILE, "EMFILE"),
            (libc::ENFILE, "ENFILE"),
            (libc::EXDEV, "EXDEV"),
            (libc::ENOSPC, "ENOSPC"),
            (libc::EACCES, "EACCES"),
        ] {
            assert_eq!(
                node_error_code(&std::io::Error::from_raw_os_error(errno)),
                code
            );
        }
        let exdev = std::io::Error::from_raw_os_error(libc::EXDEV);
        assert_eq!(
            node_error_message("EXDEV", "rename", "p", &exdev),
            "EXDEV: cross-device link not permitted, rename 'p'"
        );
    }

    #[cfg(windows)]
    fn raw(code: u32) -> std::io::Error {
        std::io::Error::from_raw_os_error(code as i32)
    }

    #[cfg(windows)]
    #[test]
    fn windows_codes_follow_libuv() {
        use windows_sys::Win32::Foundation::*;
        use windows_sys::Win32::Networking::WinSock::*;
        let cases: &[(u32, &str)] = &[
            (ERROR_ACCESS_DENIED, "EPERM"),
            (ERROR_PRIVILEGE_NOT_HELD, "EPERM"),
            (ERROR_SHARING_VIOLATION, "EBUSY"),
            (ERROR_LOCK_VIOLATION, "EBUSY"),
            (ERROR_PIPE_BUSY, "EBUSY"),
            (ERROR_BAD_EXE_FORMAT, "EFTYPE"),
            (ERROR_EXE_MACHINE_TYPE_MISMATCH, "UNKNOWN"),
            (ERROR_ELEVATION_REQUIRED, "EACCES"),
            (ERROR_CANT_ACCESS_FILE, "EACCES"),
            (ERROR_FILE_NOT_FOUND, "ENOENT"),
            (ERROR_PATH_NOT_FOUND, "ENOENT"),
            (ERROR_INVALID_NAME, "ENOENT"),
            (ERROR_FILENAME_EXCED_RANGE, "ENAMETOOLONG"),
            (ERROR_DISK_FULL, "ENOSPC"),
            (ERROR_NOT_SAME_DEVICE, "EXDEV"),
            (ERROR_WRITE_PROTECT, "EROFS"),
            (ERROR_TOO_MANY_OPEN_FILES, "EMFILE"),
            (ERROR_DIR_NOT_EMPTY, "ENOTEMPTY"),
            (ERROR_ALREADY_EXISTS, "EEXIST"),
            (ERROR_INVALID_HANDLE, "EBADF"),
            (ERROR_OPERATION_ABORTED, "ECANCELED"),
            (ERROR_INVALID_FUNCTION, "EISDIR"),
            // Generic here; readdir's ENOTDIR is a per-operation rule. From
            // CreateProcessW it is a bad working directory, which node reports
            // as ENOENT.
            (ERROR_DIRECTORY, "ENOENT"),
            // oam's deliberate departures from the generic table.
            (ERROR_BROKEN_PIPE, "EPIPE"),
            (ERROR_NO_DATA, "EPIPE"),
        ];
        for &(code, expected) in cases {
            assert_eq!(node_error_code(&raw(code)), expected, "Win32 error {code}");
        }
        let winsock: &[(i32, &str)] = &[
            (WSAEACCES, "EACCES"),
            (WSAECONNRESET, "ECONNRESET"),
            (WSAECONNREFUSED, "ECONNREFUSED"),
            (WSAEWOULDBLOCK, "EAGAIN"),
            (WSAESHUTDOWN, "EPIPE"),
            (WSAHOST_NOT_FOUND, "ENOTFOUND"),
            (WSANO_DATA, "ENOTFOUND"),
            (WSATRY_AGAIN, "EAI_AGAIN"),
        ];
        for &(code, expected) in winsock {
            let e = std::io::Error::from_raw_os_error(code);
            assert_eq!(node_error_code(&e), expected, "Winsock error {code}");
        }
        // An error with no raw code still maps by kind.
        let kind_only = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(node_error_code(&kind_only), "EACCES");
    }

    #[cfg(windows)]
    #[test]
    fn windows_access_denied_is_eperm_everywhere_but_a_wrong_mode_descriptor() {
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        let denied = raw(ERROR_ACCESS_DENIED);
        assert_eq!(node_errno("EPERM", &denied), Some(-4048));
        assert_eq!(
            node_error_message("EPERM", "open", "p", &denied),
            "EPERM: operation not permitted, open 'p'"
        );
        // libuv's fs__read / fs__write turn it into EBADF on a descriptor.
        assert_eq!(fd_error_code(&denied), "EBADF");
    }

    #[cfg(windows)]
    #[test]
    fn windows_filesystem_rules_depend_on_the_operation() {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_DIRECTORY, ERROR_INVALID_NAME, ERROR_NOT_A_REPARSE_POINT,
        };
        let dir = std::env::temp_dir().join(format!("oam-fs-error-at-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let dir_s = dir.to_str().unwrap();
        let file_s = file.to_str().unwrap();
        let denied = raw(ERROR_ACCESS_DENIED);
        let at = |site, syscall, path| fs_error_at(site, syscall, path, &denied);
        let expect = |code, syscall, has_path| FsError {
            code,
            syscall,
            has_path,
        };

        assert_eq!(
            at(FsSite::Open("w"), "open", dir_s),
            expect("EISDIR", "open", true)
        );
        assert_eq!(
            at(FsSite::Open("w+"), "open", dir_s),
            expect("EISDIR", "open", true)
        );
        assert_eq!(
            at(FsSite::Open("wx"), "open", dir_s),
            expect("EEXIST", "open", true)
        );
        assert_eq!(
            at(FsSite::WriteFile, "open", dir_s),
            expect("EISDIR", "open", true)
        );
        assert_eq!(
            at(FsSite::ReadFile, "open", dir_s),
            expect("EISDIR", "read", false)
        );
        assert_eq!(
            at(FsSite::AppendFile, "open", dir_s),
            expect("EISDIR", "write", false)
        );
        // node opens a directory for reading; oam cannot, and says EPERM.
        assert_eq!(
            at(FsSite::Open("r"), "open", dir_s),
            expect("EPERM", "open", true)
        );
        // The same denial on a FILE is a plain EPERM for every operation.
        assert_eq!(
            at(FsSite::ReadFile, "open", file_s),
            expect("EPERM", "open", true)
        );
        assert_eq!(
            at(FsSite::Open("w"), "open", file_s),
            expect("EPERM", "open", true)
        );

        for code in [ERROR_INVALID_NAME, ERROR_DIRECTORY] {
            let e = raw(code);
            assert_eq!(
                fs_error_at(FsSite::Mkdir { recursive: false }, "mkdir", "a*b", &e),
                expect("EINVAL", "mkdir", true)
            );
        }
        // node's recursive mkdir re-stats and reports ENOENT instead.
        assert_eq!(
            fs_error_at(
                FsSite::Mkdir { recursive: true },
                "mkdir",
                "a*b",
                &raw(ERROR_INVALID_NAME)
            ),
            expect("ENOENT", "mkdir", true)
        );
        // readdir of a file is ENOTDIR; the same code elsewhere is ENOENT.
        assert_eq!(
            fs_error_at(FsSite::Scandir, "scandir", file_s, &raw(ERROR_DIRECTORY)),
            expect("ENOTDIR", "scandir", true)
        );
        assert_eq!(
            fs_error_at(FsSite::Open("r"), "open", file_s, &raw(ERROR_DIRECTORY)),
            expect("ENOENT", "open", true)
        );
        // ...but only mkdir: every other operation keeps the table's answer.
        assert_eq!(
            fs_error_at(FsSite::Open("r"), "open", "a*b", &raw(ERROR_INVALID_NAME)),
            expect("ENOENT", "open", true)
        );
        assert_eq!(
            fs_error_at(
                FsSite::Readlink,
                "readlink",
                file_s,
                &raw(ERROR_NOT_A_REPARSE_POINT)
            ),
            expect("EINVAL", "readlink", true)
        );

        let failure = at(FsSite::ReadFile, "open", dir_s);
        assert_eq!(
            fs_error_message(failure, dir_s, &denied),
            "EISDIR: illegal operation on a directory, read"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ops_complete_and_inflight_tracks() {
        let mut core = CoreRuntime::new().unwrap();
        assert!(!core.has_inflight());
        let id = core.spawn_op(ops::sleep(5));
        assert!(core.has_inflight());
        let completion = core
            .recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("op completes");
        assert_eq!(completion.id, id);
        assert!(matches!(completion.outcome, OpOutcome::Done));
        assert!(!core.has_inflight());
    }

    #[test]
    fn set_stdin_ref_retires_and_restores_the_live_read() {
        let mut core = CoreRuntime::new().unwrap();
        let id = core.spawn_stdin_op(ops::sleep(60_000));
        assert!(core.has_inflight(), "a stdin read pins the loop by default");

        // stdin.unref(): the read is still blocked in the OS, but the loop is
        // free to exit.
        core.set_stdin_ref(false);
        assert!(!core.has_inflight());
        // Idempotent -- a second unref must not double-count.
        core.set_stdin_ref(false);
        assert!(!core.has_inflight());

        // stdin.ref() puts it back.
        core.set_stdin_ref(true);
        assert!(core.has_inflight());
        core.set_stdin_ref(true);
        assert!(core.has_inflight());

        // The next read inherits the ref-ness in force when it is issued.
        core.set_stdin_ref(false);
        let next = core.spawn_stdin_op(ops::sleep(60_000));
        assert_ne!(next, id);
        assert!(
            !core.has_inflight(),
            "issued while unref'd, so it does not count"
        );
    }

    #[test]
    fn set_stdin_ref_after_the_read_settled_does_not_corrupt_inflight() {
        let mut core = CoreRuntime::new().unwrap();
        core.spawn_stdin_op(ops::sleep(5));
        let completion = core
            .recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("the stdin read settles");
        assert!(!core.has_inflight());
        // A destroy() landing after the read came back: the id is stale, so
        // both directions must be no-ops rather than underflow `inflight` or
        // resurrect a settled op.
        core.set_stdin_ref(false);
        assert!(!core.has_inflight());
        core.set_stdin_ref(true);
        assert!(!core.has_inflight());
        // An unrelated op still settles cleanly afterwards.
        let other = core.spawn_op(ops::sleep(5));
        assert!(core.has_inflight());
        let settled = core
            .recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("op completes");
        assert_eq!(settled.id, other);
        assert_ne!(settled.id, completion.id);
        assert!(!core.has_inflight());
    }

    #[test]
    fn handle_ops_follow_their_key_and_keys_stay_apart() {
        let mut core = CoreRuntime::new().unwrap();
        let sock = HandleKey::Tcp(7);
        let read = core.spawn_handle_op(sock, ops::sleep(60_000));
        let write = core.spawn_handle_op(sock, ops::sleep(60_000));
        assert!(
            core.has_inflight(),
            "a socket's ops pin the loop by default"
        );

        // socket.unref(): every op in flight under the key stops counting.
        core.set_handle_ref(sock, false);
        assert!(!core.has_inflight());
        core.set_handle_ref(sock, false);
        assert!(!core.has_inflight(), "idempotent");
        // The next op inherits the ref-ness in force when it is issued.
        let next = core.spawn_handle_op(sock, ops::sleep(60_000));
        assert!(next != read && next != write);
        assert!(
            !core.has_inflight(),
            "issued while unref'd, so it does not count"
        );

        // Another handle -- same id, different kind -- is its own key.
        let other = HandleKey::Tls(7);
        core.spawn_handle_op(other, ops::sleep(60_000));
        assert!(
            core.has_inflight(),
            "unref'ing Tcp(7) said nothing about Tls(7)"
        );
        core.set_handle_ref(other, false);
        assert!(!core.has_inflight());
        core.set_handle_ref(HandleKey::TcpServer(7), false);
        assert!(
            !core.has_inflight(),
            "a key with nothing in flight is inert"
        );

        // socket.ref() puts all three of the socket's ops back.
        core.set_handle_ref(sock, true);
        assert!(core.has_inflight());
        core.set_handle_ref(other, true);
        core.set_handle_ref(sock, false);
        assert!(core.has_inflight(), "Tls(7) alone still pins the loop");
        core.set_handle_ref(other, false);
        assert!(!core.has_inflight());
    }

    #[test]
    fn forgetting_a_handle_drops_its_bookkeeping_but_not_its_ops() {
        let mut core = CoreRuntime::new().unwrap();
        let sock = HandleKey::Tcp(3);
        core.spawn_handle_op(sock, ops::sleep(60_000));
        core.forget_handle(sock);
        assert!(core.has_inflight(), "a still-running op keeps its ref-ness");
        assert!(core.handles.is_empty() && core.op_handles.is_empty());
        // A stale unref() after close reaches nothing that was forgotten,
        // and leaves no residue once set back.
        core.set_handle_ref(sock, false);
        assert!(core.has_inflight());
        core.set_handle_ref(sock, true);
        assert!(core.handles.is_empty());
    }

    #[test]
    fn a_settled_handle_op_leaves_no_residue() {
        let mut core = CoreRuntime::new().unwrap();
        let sock = HandleKey::Tcp(5);
        core.spawn_handle_op(sock, ops::sleep(5));
        assert_eq!(core.handles.len(), 1);
        core.recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("the op settles");
        assert!(!core.has_inflight());
        // A referenced key with nothing in flight is the default state: gone.
        assert!(core.handles.is_empty() && core.op_handles.is_empty());
        // Flipping it after the fact must not touch `inflight`.
        core.set_handle_ref(sock, false);
        core.set_handle_ref(sock, true);
        assert!(!core.has_inflight());
        assert!(core.handles.is_empty());

        // Unref'd, the key outlives its ops -- the state must survive the gap
        // between one parked read and the next -- and is dropped by
        // forget_handle (the close path).
        core.set_handle_ref(sock, false);
        core.spawn_handle_op(sock, ops::sleep(5));
        assert!(!core.has_inflight());
        core.recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("the op settles");
        assert_eq!(core.handles.len(), 1, "an unref'd key is remembered");
        core.spawn_handle_op(sock, ops::sleep(60_000));
        assert!(!core.has_inflight(), "still unref'd across the gap");
        core.forget_handle(sock);
        assert!(core.handles.is_empty() && core.op_handles.is_empty());
        assert!(
            !core.has_inflight(),
            "forgotten while retired: the op stays retired"
        );
    }

    #[test]
    fn recv_deadline_times_out_without_completion() {
        let mut core = CoreRuntime::new().unwrap();
        let _id = core.spawn_op(ops::sleep(60_000));
        let start = Instant::now();
        let completion = core.recv_deadline(Some(start + Duration::from_millis(30)));
        assert!(completion.is_none());
        assert!(core.has_inflight());
    }

    #[test]
    fn read_text_file_fails_cleanly_on_missing_path() {
        let mut core = CoreRuntime::new().unwrap();
        core.spawn_op(ops::read_text_file("/definitely/not/here.txt".into()));
        let completion = core
            .recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("op completes");
        assert!(matches!(completion.outcome, OpOutcome::Failed(_)));
    }

    // Regression: an inherited fd closed via close_inherited_fd must never be
    // re-adopted. INHERITED_AT_START is a set-once startup snapshot; before the
    // fix, a second closeSync(n) issued after the kernel had reused fd n would
    // re-adopt -- and then close -- a descriptor the runtime had come to own
    // (a stale-snapshot double-close-with-reuse). Consuming the number on close
    // is what shuts that window. This test is the only setter of
    // INHERITED_AT_START in the crate, so it owns the global.
    #[test]
    fn closed_inherited_fd_is_not_re_adopted() {
        let fd = 9u64; // inside the 3..=MAX_INHERITED_FD adoption window
        INHERITED_AT_START
            .set(std::collections::HashSet::from([fd]))
            .expect("only setter of INHERITED_AT_START in this crate");

        // Born holding it, not yet closed -> eligible for adoption.
        assert!(inherited_eligible(fd));

        // Closing the parent's original consumes the number; a reissued fd n can
        // no longer be adopted, even though the snapshot still lists it.
        consume_inherited_fd(fd);
        assert!(!inherited_eligible(fd));

        // adopt_inherited_fd rides the same gate, so it refuses without ever
        // probing or duping the (possibly reused) descriptor.
        let registry: SyncFileRegistry =
            std::sync::Arc::new(std::sync::Mutex::new(FileState::default()));
        assert!(!adopt_inherited_fd(&registry, fd));
    }
}

/// The op-outcome error contract as record/replay persists it: replay.rs
/// writes every outcome with serde_json and reads it back on replay, so a file
/// recorded before a field existed must still load (and falls back to the live
/// outcome only if it does not parse at all).
#[cfg(test)]
mod op_outcome_serde_tests {
    use super::*;

    fn sys(code: &str) -> NodeSysError {
        NodeSysError {
            code: code.to_string(),
            message: format!("connect {code} 127.0.0.1:8080"),
            errno: Some(-4078),
            syscall: Some("connect".to_string()),
            hostname: None,
            address: Some("127.0.0.1".to_string()),
            port: Some(8080),
        }
    }

    #[test]
    fn a_node_failed_recorded_before_the_peer_fields_still_loads() {
        // The exact shape an older oam wrote: no hostname / address / port.
        let old = r#"{"NodeFailed":{"code":"ENOENT","message":"ENOENT: no such file or directory, open 'x'","syscall":"open","path":"x","errno":-4058}}"#;
        match serde_json::from_str::<OpOutcome>(old).expect("old payload parses") {
            OpOutcome::NodeFailed {
                code,
                message,
                syscall,
                path,
                errno,
                hostname,
                address,
                port,
            } => {
                assert_eq!(code, "ENOENT");
                assert_eq!(message, "ENOENT: no such file or directory, open 'x'");
                assert_eq!(syscall.as_deref(), Some("open"));
                assert_eq!(path.as_deref(), Some("x"));
                assert_eq!(errno, Some(-4058));
                assert_eq!((hostname, address, port), (None, None, None));
            }
            other => panic!("expected NodeFailed, got {other:?}"),
        }
        // The minimal old form (node_failed: code + message only) too.
        let minimal = r#"{"NodeFailed":{"code":"ENOTFOUND","message":"m"}}"#;
        assert!(matches!(
            serde_json::from_str::<OpOutcome>(minimal).expect("minimal payload parses"),
            OpOutcome::NodeFailed {
                syscall: None,
                path: None,
                errno: None,
                hostname: None,
                address: None,
                port: None,
                ..
            }
        ));
    }

    #[test]
    fn absent_fields_are_not_written() {
        // A new recording of an fs-shaped failure is byte-identical to what an
        // older oam wrote, so an older oam can still replay it.
        let outcome = OpOutcome::node_failed_at("ENOENT", "m", "open", Some("x"), Some(-2));
        assert_eq!(
            serde_json::to_string(&outcome).unwrap(),
            r#"{"NodeFailed":{"code":"ENOENT","message":"m","syscall":"open","path":"x","errno":-2}}"#
        );
        let bare = NodeSysError {
            code: "EAI_AGAIN".to_string(),
            message: "getaddrinfo EAI_AGAIN".to_string(),
            errno: None,
            syscall: None,
            hostname: None,
            address: None,
            port: None,
        };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"code":"EAI_AGAIN","message":"getaddrinfo EAI_AGAIN"}"#
        );
    }

    #[test]
    fn sys_fills_node_failed_without_a_path() {
        match OpOutcome::sys(sys("ECONNREFUSED")) {
            OpOutcome::NodeFailed {
                code,
                message,
                syscall,
                path,
                errno,
                hostname,
                address,
                port,
            } => {
                assert_eq!(code, "ECONNREFUSED");
                assert_eq!(message, "connect ECONNREFUSED 127.0.0.1:8080");
                assert_eq!(syscall.as_deref(), Some("connect"));
                assert_eq!(path, None);
                assert_eq!(errno, Some(-4078));
                assert_eq!(hostname, None);
                assert_eq!(address.as_deref(), Some("127.0.0.1"));
                assert_eq!(port, Some(8080));
            }
            other => panic!("expected NodeFailed, got {other:?}"),
        }
    }

    #[test]
    fn peer_fields_and_aggregates_round_trip() {
        let single = OpOutcome::sys(sys("ECONNREFUSED"));
        let json = serde_json::to_string(&single).unwrap();
        match serde_json::from_str::<OpOutcome>(&json).unwrap() {
            OpOutcome::NodeFailed { address, port, .. } => {
                assert_eq!((address.as_deref(), port), (Some("127.0.0.1"), Some(8080)));
            }
            other => panic!("expected NodeFailed, got {other:?}"),
        }
        let aggregate = OpOutcome::NodeAggregateFailed {
            errors: vec![sys("ECONNREFUSED"), sys("ETIMEDOUT")],
        };
        let json = serde_json::to_string(&aggregate).unwrap();
        match serde_json::from_str::<OpOutcome>(&json).unwrap() {
            OpOutcome::NodeAggregateFailed { errors } => {
                assert_eq!(errors, vec![sys("ECONNREFUSED"), sys("ETIMEDOUT")]);
            }
            other => panic!("expected NodeAggregateFailed, got {other:?}"),
        }
        // A child missing every optional field (an older writer) loads too.
        let sparse =
            r#"{"NodeAggregateFailed":{"errors":[{"code":"ECONNREFUSED","message":"m"}]}}"#;
        match serde_json::from_str::<OpOutcome>(sparse).unwrap() {
            OpOutcome::NodeAggregateFailed { errors } => {
                assert_eq!(errors.len(), 1);
                assert_eq!(errors[0].errno, None);
                assert_eq!(errors[0].port, None);
            }
            other => panic!("expected NodeAggregateFailed, got {other:?}"),
        }
    }
}
