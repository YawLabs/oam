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

pub mod child;
pub mod cluster;
pub mod dns;
pub mod http_server;
pub mod inspector;
/// Inbound OS signal delivery (SIGTERM/SIGINT/SIGHUP). Unix uses
/// tokio::signal::unix; Windows uses SetConsoleCtrlHandler. Both feed the op
/// channel with an OpCompletion{ id: SIGNAL_OP_ID, .. } that the engine
/// dispatches to process's JS listeners.
pub mod signal;
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

pub type OpId = u64;

/// Sentinel op id for inbound OS-signal completions. `next_id` starts at 1, so
/// no real spawned op ever uses 0 — the engine's settle path uses this to
/// recognize a signal (which has no parked PromiseResolver and was never
/// counted in `inflight`) and route it to `process.emit(name)`.
pub const SIGNAL_OP_ID: OpId = 0;

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
    NodeFailed {
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        syscall: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        errno: Option<i32>,
    },
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
        }
    }
}

#[derive(Debug)]
pub struct OpCompletion {
    pub id: OpId,
    pub outcome: OpOutcome,
}

/// Live streaming response bodies, keyed by handle. A std (not tokio)
/// Mutex on purpose: readers REMOVE the response under a short lock, await
/// the chunk with no lock held, then reinsert — no guard ever crosses an
/// await. Single-reader discipline is guaranteed by ReadableStream's lock.
pub type BodyRegistry = std::sync::Arc<std::sync::Mutex<HashMap<u64, reqwest::Response>>>;

/// Cancel tombstones for the remove-await-reinsert race: fetchBodyCancel on
/// a handle whose read is IN FLIGHT (absent from the registry) records the
/// handle here; the returning read sees it and drops the response instead of
/// reinserting -- otherwise a cancelled body silently revives and holds its
/// connection open for the rest of the run.
pub type CancelledBodies = std::sync::Arc<std::sync::Mutex<std::collections::HashSet<u64>>>;
/// Outbound request-body channels: JS writes chunks, reqwest drains them.
/// The receiver is taken by `fetch` when the request goes out; the sender
/// stays here so later writes reach the in-flight request.
/// Slice 4 of docs/design/streaming-bodies.md.
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
    use std::os::fd::FromRawFd;
    let raw = i32::try_from(fd).ok()?;
    // F_GETFD is the cheapest "is this open" question; dup would also tell us,
    // but only by allocating a descriptor we would have to put back.
    // SAFETY: `fcntl(F_GETFD)` only reads the flags of the integer fd `raw`; no
    // pointers are involved.
    if unsafe { libc::fcntl(raw, libc::F_GETFD) } == -1 {
        return None;
    }
    // FD_CLOEXEC on the dup: an fd the parent handed US is not automatically
    // something our own children should receive.
    // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` duplicates the integer fd `raw`; no
    // pointers are involved and the returned fd is checked below.
    let dup = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
    if dup == -1 {
        return None;
    }
    // SAFETY: `dup` is a fresh, owned fd (checked non-negative just above); the
    // new File takes sole ownership and closes it on drop.
    Some(unsafe { std::fs::File::from_raw_fd(dup) })
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

pub struct CoreRuntime {
    /// Option so Drop can take it for shutdown_background (see below).
    tokio: Option<tokio::runtime::Runtime>,
    /// Shared HTTP client (connection pool). Owned per CoreRuntime so pooled
    /// connections never outlive the tokio runtime they were spawned on.
    http: reqwest::Client,
    tx: mpsc::Sender<OpCompletion>,
    rx: mpsc::Receiver<OpCompletion>,
    next_id: OpId,
    inflight: usize,
    /// Ops that must NOT keep the event loop alive. A passive watcher (e.g.
    /// the http response close-watcher) observes something that may never
    /// happen; counting it in `inflight` pins the process forever. Same
    /// rationale as SIGNAL_OP_ID, but per-op rather than a fixed id.
    unref_ops: HashSet<OpId>,
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
        // not aws-lc-rs). Err means a provider is already installed: fine.
        static TLS_PROVIDER: std::sync::Once = std::sync::Once::new();
        TLS_PROVIDER.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("oam-io")
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("oam/", env!("CARGO_PKG_VERSION")))
            // `localhost` IPv4-first. Measured: oam's first request to
            // localhost cost ~317ms against Node's ~6ms, because resolution
            // yields ::1 first, the common case is a server bound to
            // 127.0.0.1, and the connector waits out its happy-eyeballs
            // fallback delay before trying IPv4. ::1 stays in the list as a
            // fallback, so a server bound only to IPv6 loopback still works.
            .resolve_to_addrs(
                "localhost",
                &[
                    std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                    std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 0)),
                ],
            )
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let (tx, rx) = mpsc::channel();
        Ok(Self {
            tokio: Some(tokio),
            http,
            tx,
            rx,
            next_id: 1,
            inflight: 0,
            unref_ops: HashSet::new(),
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

    /// Cheap Arc clone for ops that need the pooled HTTP client.
    pub fn http_client(&self) -> reqwest::Client {
        self.http.clone()
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

    pub fn has_inflight(&self) -> bool {
        self.inflight > 0
    }

    pub fn try_recv(&mut self) -> Option<OpCompletion> {
        let completion = self.rx.try_recv().ok();
        // A signal completion (SIGNAL_OP_ID) was never counted in `inflight`
        // (a bare listener must not pin the loop), so it must not decrement —
        // decrementing here would underflow when inflight == 0.
        if let Some(ref c) = completion
            && c.id != SIGNAL_OP_ID
            && !self.unref_ops.remove(&c.id)
        {
            self.inflight -= 1;
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
        // See try_recv: a SIGNAL_OP_ID completion was never counted, so must
        // not decrement `inflight` (which would underflow at 0 — this is the
        // path the no-inflight event-loop idle arm now waits on for signals).
        if let Some(ref c) = completion
            && c.id != SIGNAL_OP_ID
            && !self.unref_ops.remove(&c.id)
        {
            self.inflight -= 1;
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
            // alive to serve the OS default) is re-armed, not recreated:
            // tokio's signal registration is a one-time global, so the task
            // owning the stream has to outlive the JS listeners.
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
    /// so its ctrl handler falls through; unix keeps it dormant, because
    /// tokio's handler cannot be uninstalled and the task is what reproduces
    /// the default.
    pub fn stop_signal(&mut self, name: &str) {
        signal::stop_signal(&mut self.signals, name);
    }
}

impl Drop for CoreRuntime {
    fn drop(&mut self) {
        // The run is over: nothing on the IO runtime may block process
        // exit. A plain Runtime::drop WAITS — an idle keep-alive
        // connection (reqwest pool, 90s idle timeout; a hyper server
        // conn) turned exit into a 90-second hang. shutdown_background
        // drops everything without waiting.
        if let Some(runtime) = self.tokio.take() {
            runtime.shutdown_background();
        }
    }
}

pub async fn stdin_read() -> OpOutcome {
    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 65536];
    match stdin.read(&mut buf).await {
        Ok(0) => OpOutcome::Done,
        Ok(n) => {
            buf.truncate(n);
            OpOutcome::Bytes(buf)
        }
        Err(e) => OpOutcome::Failed(format!("stdin read: {e}")),
    }
}

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

/// Remove every registered artifact. Idempotent -- the list is taken, so a
/// normal-path cleanup followed by an exit-path drain does no double work.
pub fn run_exit_cleanup() {
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
pub fn node_error_code(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    // Windows winsock / getaddrinfo codes. std maps most WSA* codes to a
    // matching ErrorKind, but some (notably WSAHOST_NOT_FOUND) land in
    // Uncategorized and would fall through to EIO. libuv/Node map these WSA*
    // codes to fixed errno strings; mirror that table-exactly here so the NET
    // error surface reports ECONNREFUSED/ETIMEDOUT/ENOTFOUND/... instead of EIO
    // (the sibling of the fs libuv-win table). Unix POSIX errno flows through
    // the ErrorKind match below, which std maps reliably; this table is
    // Windows-only and never matches fs raw_os_errors (small Win32 codes).
    #[cfg(windows)]
    {
        let winsock = error.raw_os_error().and_then(|os| match os {
            10013 => Some("EACCES"),        // WSAEACCES
            10048 => Some("EADDRINUSE"),    // WSAEADDRINUSE
            10049 => Some("EADDRNOTAVAIL"), // WSAEADDRNOTAVAIL
            10051 => Some("ENETUNREACH"),   // WSAENETUNREACH
            10053 => Some("ECONNABORTED"),  // WSAECONNABORTED
            10054 => Some("ECONNRESET"),    // WSAECONNRESET
            10057 => Some("ENOTCONN"),      // WSAENOTCONN
            10060 => Some("ETIMEDOUT"),     // WSAETIMEDOUT
            10061 => Some("ECONNREFUSED"),  // WSAECONNREFUSED
            10065 => Some("EHOSTUNREACH"),  // WSAEHOSTUNREACH
            11001 => Some("ENOTFOUND"),     // WSAHOST_NOT_FOUND (getaddrinfo)
            11002 => Some("EAI_AGAIN"),     // WSATRY_AGAIN (getaddrinfo)
            11004 => Some("ENOTFOUND"),     // WSANO_DATA (getaddrinfo)
            _ => None,
        });
        if let Some(code) = winsock {
            return code;
        }
        // Invalid-filename classes: Windows rejects paths with characters
        // like '"' as ERROR_INVALID_NAME, which std leaves Uncategorized
        // (-> EIO). libuv maps these to ENOENT. Deliberately EXCLUDES 267
        // ERROR_DIRECTORY -- std decodes that to ErrorKind::NotADirectory,
        // which the match below turns into ENOTDIR (node's code for
        // readdir-on-a-file); mapping it here would regress that.
        if let Some(123 | 161 | 206) = error.raw_os_error() {
            return "ENOENT";
        }
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
        // Network error kinds (std maps POSIX errno on Unix and the common
        // winsock codes on Windows to these reliably; on Windows the winsock
        // table above already caught them). Previously all fell through to EIO.
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

/// The human-readable half of a node system-error message, keyed by code.
fn node_error_reason(code: &str, error: &std::io::Error) -> String {
    match code {
        "ENOENT" => "no such file or directory".to_string(),
        "EACCES" => "permission denied".to_string(),
        "EEXIST" => "file already exists".to_string(),
        "ENOTEMPTY" => "directory not empty".to_string(),
        "ENOTDIR" => "not a directory".to_string(),
        "EISDIR" => "illegal operation on a directory".to_string(),
        "EINVAL" => "invalid argument".to_string(),
        // Reachable via fd_error_code: without this the OS text ("Access is
        // denied. (os error 5)") would leak into the message.
        "EBADF" => "bad file descriptor".to_string(),
        _ => error.to_string(),
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

    pub fn decompress(bytes: &[u8], format: Format) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        match format {
            Format::Gzip => {
                flate2::read::GzDecoder::new(bytes).read_to_end(&mut out)?;
            }
            Format::Deflate => {
                flate2::read::ZlibDecoder::new(bytes).read_to_end(&mut out)?;
            }
            Format::DeflateRaw => {
                flate2::read::DeflateDecoder::new(bytes).read_to_end(&mut out)?;
            }
        }
        Ok(out)
    }

    /// Node's unzip*: auto-detect gzip (1f 8b magic) vs zlib-wrapped.
    pub fn unzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        if bytes.starts_with(&[0x1f, 0x8b]) {
            decompress(bytes, Format::Gzip)
        } else {
            decompress(bytes, Format::Deflate)
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
            // SAFETY: an all-zero `statfs` is a valid uninitialized value.
            let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
            // SAFETY: `c_path` is a NUL-terminated C string that outlives the
            // call and `&mut buf` is the live struct above; its fields are only
            // read below after a 0 (success) return.
            if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
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
            // SAFETY: an all-zero `statvfs` is a valid uninitialized value.
            let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
            // SAFETY: as above, for the POSIX `statvfs` shape -- `c_path`
            // outlives the call and `&mut buf` is the live struct; read only
            // after a 0 (success) return.
            if unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
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
            Err(e) => node_fail(e, "open", &path),
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
                    Err((e, _)) => return node_fail(e, "open", &path),
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
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "open", &path),
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
            Err(e) => node_fail(e, "scandir", &path),
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
            Err(e) => node_fail(e, "mkdir", &path),
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
            Err(e) => node_fail(e, "readlink", &path),
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
            use std::os::unix::io::AsRawFd;
            // -1 means "leave this one alone". node hands it through as an
            // unsigned value, so the wrap back to the signed libc type is the
            // intended round trip rather than an accident.
            // SAFETY: `file.as_raw_fd()` is a live fd for the duration of the
            // call and the uid/gid are plain integers; no pointers are involved.
            let rc =
                unsafe { libc::fchown(file.as_raw_fd(), uid as libc::uid_t, gid as libc::gid_t) };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
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
    #[cfg(unix)]
    fn to_timespec(ms: f64) -> libc::timespec {
        let secs = (ms / 1000.0).floor();
        let nanos = ((ms - secs * 1000.0) * 1_000_000.0).round();
        libc::timespec {
            tv_sec: secs as libc::time_t,
            tv_nsec: nanos as _,
        }
    }

    fn futimes_file(file: &std::fs::File, atime_ms: f64, mtime_ms: f64) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let times = [to_timespec(atime_ms), to_timespec(mtime_ms)];
            // SAFETY: `file.as_raw_fd()` is a live fd and `times.as_ptr()` points
            // at the live two-element array above, which outlives the call.
            let rc = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
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
            // -1 in either field means "leave it alone"; node passes it through
            // unsigned, so the wrap back to the signed libc type is the round
            // trip we want, not an accident.
            // SAFETY: `c.as_ptr()` is a NUL-terminated C string that outlives the
            // call and the uid/gid are plain integers; no other memory is
            // touched.
            let rc = unsafe {
                if follow {
                    libc::chown(c.as_ptr(), uid as libc::uid_t, gid as libc::gid_t)
                } else {
                    libc::lchown(c.as_ptr(), uid as libc::uid_t, gid as libc::gid_t)
                }
            };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
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
            let times = [to_timespec(atime_ms), to_timespec(mtime_ms)];
            let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
            // SAFETY: `c.as_ptr()` is a NUL-terminated C string and
            // `times.as_ptr()` points at the live array above; both outlive the
            // call, and AT_FDCWD/flags are plain integers.
            let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
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
        // SAFETY: `c.as_ptr()` is a NUL-terminated C string that outlives the
        // call; AT_FDCWD, the mode, and AT_SYMLINK_NOFOLLOW are plain integers.
        let rc = unsafe {
            libc::fchmodat(
                libc::AT_FDCWD,
                c.as_ptr(),
                mode as libc::mode_t,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
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

    /// The wire shape `fetch` sends down from JS (serialized JSON).
    #[derive(serde::Deserialize)]
    pub struct FetchRequest {
        pub url: String,
        #[serde(default)]
        pub method: Option<String>,
        #[serde(default)]
        pub headers: Vec<(String, String)>,
        #[serde(default)]
        pub body: Option<String>,
        #[serde(default)]
        pub body_base64: Option<String>,
        /// Handle into OutboundBodies: stream the body from JS instead of
        /// sending a materialized one. Mutually exclusive with body /
        /// body_base64.
        #[serde(default)]
        pub body_stream: Option<u64>,
        /// Connection pin: dial this IP for the URL's host instead of
        /// resolving DNS, while keeping Host header + TLS SNI = the host.
        /// Set by globalThis.fetch when an undici dispatcher carries a
        /// connect.lookup hook (DNS-rebind / SSRF pinning).
        #[serde(default)]
        pub pin: Option<FetchPin>,
    }

    /// A DNS/connect pin: `host` (the URL hostname, kept for Host + SNI) ->
    /// `ip` (the literal address to dial).
    #[derive(serde::Deserialize)]
    pub struct FetchPin {
        pub host: String,
        pub ip: String,
    }

    /// Build a one-off reqwest client that resolves `pin.host` to `pin.ip`
    /// (port from the URL), keeping the shared client's config. Used only for
    /// pinned requests, so the normal fetch path keeps the pooled client.
    fn build_pinned_client(url: &str, pin: &FetchPin) -> Result<reqwest::Client, String> {
        let parsed = reqwest::Url::parse(url).map_err(|e| format!("bad url: {e}"))?;
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| "url has no port and no known default".to_string())?;
        let ip: std::net::IpAddr = pin
            .ip
            .parse()
            .map_err(|e| format!("pin ip '{}' is not an IP: {e}", pin.ip))?;
        let addr = std::net::SocketAddr::new(ip, port);
        reqwest::Client::builder()
            .user_agent(concat!("oam/", env!("CARGO_PKG_VERSION")))
            .resolve(&pin.host, addr)
            .build()
            .map_err(|e| format!("pinned http client: {e}"))
    }

    /// Streaming fetch: resolves at HEADERS time with the response shape
    /// plus a body handle. The body streams through fetch_body_read one
    /// chunk per op — `for await (const chunk of response.body)` sees
    /// tokens as the server flushes them (the SSE/AI-client path).
    pub async fn fetch(
        client: reqwest::Client,
        req: FetchRequest,
        bodies: super::BodyRegistry,
        ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
        outbound: super::OutboundBodies,
    ) -> OpOutcome {
        let method = req.method.as_deref().unwrap_or("GET");
        let method = match reqwest::Method::from_bytes(method.as_bytes()) {
            Ok(m) => m,
            Err(_) => return OpOutcome::Failed(format!("fetch: invalid method '{method}'")),
        };
        // Connection pin (DNS-rebind / SSRF protection): a one-off client that
        // dials the verified IP for this host. Normal requests keep the
        // pooled client untouched.
        let client = match &req.pin {
            Some(pin) => match build_pinned_client(&req.url, pin) {
                Ok(c) => c,
                Err(e) => return OpOutcome::Failed(format!("fetch: connect pin failed: {e}")),
            },
            None => client,
        };
        let mut builder = client.request(method, &req.url);
        for (name, value) in &req.headers {
            builder = builder.header(name, value);
        }
        if let Some(handle) = req.body_stream {
            // Streamed body: hand reqwest the receiving half. Note this
            // sends chunked (no Content-Length) unless the caller set one.
            let rx = outbound
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(&handle)
                .and_then(|slot| slot.1.take());
            let Some(rx) = rx else {
                return OpOutcome::Failed(format!("fetch: unknown body stream {handle}"));
            };
            // futures-util (already a dep) rather than tokio-stream: unfold
            // the receiver into a Stream of io::Result chunks.
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv()
                    .await
                    .map(|item| (item.map_err(std::io::Error::other), rx))
            });
            builder = builder.body(reqwest::Body::wrap_stream(stream));
        } else if let Some(body) = req.body {
            builder = builder.body(body);
        } else if let Some(b64) = req.body_base64 {
            use base64::Engine;
            match base64::engine::general_purpose::STANDARD.decode(&b64) {
                Ok(bytes) => {
                    builder = builder.body(bytes);
                }
                Err(_) => return OpOutcome::Failed("fetch: malformed base64 body".into()),
            }
        }
        let response = match builder.send().await {
            Ok(r) => r,
            Err(e) => return OpOutcome::Failed(format!("fetch failed: {e}")),
        };
        let status = response.status().as_u16();
        let status_text = response
            .status()
            .canonical_reason()
            .unwrap_or_default()
            .to_string();
        let url = response.url().to_string();
        // Compare PARSED urls: string comparison false-positives on
        // normalization (trailing slash, default port, percent-casing).
        let redirected = reqwest::Url::parse(&req.url)
            .map(|original| *response.url() != original)
            .unwrap_or(false);
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        bodies
            .lock()
            .expect("body registry lock")
            .insert(handle, response);
        let payload = serde_json::json!({
            "status": status,
            "statusText": status_text,
            "url": url,
            "redirected": redirected,
            "headers": headers,
            "bodyHandle": handle,
        });
        OpOutcome::Json(payload.to_string())
    }

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
    pub async fn zlib_transform(
        bytes: Vec<u8>,
        format: String,
        level: i32,
        compress: bool,
    ) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            if !compress && format == "unzip" {
                return super::zlib::unzip(&bytes);
            }
            let Some(parsed) = super::zlib::Format::parse(&format) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown zlib format '{format}'"),
                ));
            };
            if compress {
                super::zlib::compress(&bytes, parsed, level)
            } else {
                super::zlib::decompress(&bytes, parsed)
            }
        })
        .await;
        match result {
            Ok(Ok(out)) => OpOutcome::Bytes(out),
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
            Err(e) => node_fail(e, "open", &path),
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
    /// (handle dropped). Remove-await-reinsert keeps the lock short; the
    /// JS ReadableStream lock guarantees a single reader per handle. A
    /// cancel that landed while the read was in flight (tombstone in
    /// `cancelled`) drops the response instead of reinserting it.
    pub async fn fetch_body_read(
        bodies: super::BodyRegistry,
        cancelled: super::CancelledBodies,
        cancel_signal: super::BodyCancelSignal,
        handle: u64,
    ) -> OpOutcome {
        let response = bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&handle);
        let Some(mut response) = response else {
            return OpOutcome::Failed(format!("fetch: body handle {handle} is gone"));
        };
        // Preemptive cancel: a peer that stops sending without closing would
        // otherwise park this await forever (aborting a request could never
        // release the op, and the loop could never drain). Re-check the
        // tombstone after waking -- Notify is broadcast, so another handle's
        // cancel can wake us spuriously.
        let result = loop {
            tokio::select! {
                r = response.chunk() => break r,
                _ = cancel_signal.notified() => {
                    if cancelled
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&handle)
                    {
                        return OpOutcome::Done;
                    }
                }
            }
        };
        let was_cancelled = cancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&handle);
        if was_cancelled {
            // Drop the response (closes the connection); report EOF -- the
            // JS stream is already cancelled and discards the outcome.
            return OpOutcome::Done;
        }
        match result {
            Ok(Some(bytes)) => {
                bodies
                    .lock()
                    .expect("body registry lock")
                    .insert(handle, response);
                // Double-check AFTER reinserting: a cancel can land between
                // the tombstone check above and the insert (tombstone-miss,
                // registry-miss) -- without this, the cancelled body revives
                // in the registry and holds its connection for the run.
                if cancelled
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&handle)
                {
                    bodies.lock().expect("body registry lock").remove(&handle);
                }
                OpOutcome::Bytes(bytes.to_vec())
            }
            Ok(None) => OpOutcome::Done,
            Err(e) => OpOutcome::Failed(format!("fetch: body read failed: {e}")),
        }
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
