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
/// node:zlib's deflaters: gzip, zlib and raw deflate, with the dictionary
/// option and `params()`.
mod deflate;
pub mod dns;
/// `fs.watch` / `fs.promises.watch`: OS change notifications (inotify,
/// ReadDirectoryChangesW, FSEvents) through the `notify` crate.
pub mod fs_watch;
/// oam's own HTTP client transport for the `fetch` op (#143).
pub mod http_client;
/// node's acceptance rules for inbound HTTP/1 request heads, and its
/// `maxHeaderSize` count.
pub mod http_conn;
pub mod http_head;
pub mod http_server;
/// zlib-faithful inflate shared by fetch's body decoder and node:zlib (#166).
mod inflate;
pub mod inspector;
/// The outbound TCP connector net.connect and tls.connect share: node's
/// lookupAndConnectMultiple algorithm and its error shapes.
pub mod net_connect;
/// Windows named pipes and Unix domain sockets for node:net
/// (`net.connect({ path })`, `server.listen(path)`); their streams live in
/// the TCP registry.
mod pipe;
/// Process scheduling priority: `os.getPriority` / `os.setPriority`, and the
/// `OAM_PRIORITY` / `OAM_ECOQOS` startup knobs.
pub mod priority;
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
/// node:zlib's zstd engine over libzstd (node's binding's native half).
pub mod zstd;

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
/// libuv's stat, access, chmod and directory-listing questions of a Windows
/// path, where std asks others (device paths, link loops).
#[cfg(windows)]
mod win_fs;

pub type OpId = u64;

/// Sentinel op id for inbound OS-signal completions. `next_id` starts at 1, so
/// no real spawned op ever uses 0 — the engine's settle path uses this to
/// recognize a signal (which has no parked PromiseResolver and was never
/// counted in `inflight`) and route it to `process.emit(name)`.
pub const SIGNAL_OP_ID: OpId = 0;

/// Sentinel op id for a [`LoopWaker`] wake: V8 posted a foreground task (an
/// async WebAssembly compile finishing, a FinalizationRegistry cleanup, an
/// `Atomics.waitAsync` notify) for the isolate this channel serves. `next_id`
/// counts up from 1 and never reaches it. Like a signal it was never counted
/// in `inflight`, carries no resolver, and is not part of the recorded op
/// stream: the engine runs the queued tasks at the top of its next turn.
pub const PLATFORM_TASK_OP_ID: OpId = OpId::MAX;

/// Wakes an event loop blocked on its op channel, from any thread. Held by
/// the engine's V8 platform glue; a wake for a loop that has gone away is a
/// no-op.
#[derive(Clone)]
pub struct LoopWaker(mpsc::Sender<OpCompletion>);

impl LoopWaker {
    pub fn wake(&self) {
        let _ = self.0.send(OpCompletion {
            id: PLATFORM_TASK_OP_ID,
            outcome: OpOutcome::Done,
        });
    }
}

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
    /// The Win32 code behind an `UNKNOWN` (`unknown_win32_error`, #450), for
    /// the builders that still hold the `io::Error`; `None` elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub win_error: Option<u32>,
}

/// A `--permission` refusal an async op raised after its synchronous gate had
/// already passed -- a `fetch` whose redirect leads to a host the net grant
/// does not cover (`http_client::NetCheck`).
///
/// The engine rejects with the same error its synchronous gates throw
/// (`throw_permission_denied`): Node's `ERR_ACCESS_DENIED`, message `Access to
/// this API has been restricted. Use --allow-net to manage permissions.`
/// (naming the flag that grants the permission), carrying `permission` and
/// `resource`. oam_core
/// has no permission model of its own (the grant lives in the engine), so this
/// only carries the verdict the engine's check returned.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AccessDenial {
    /// The permission's name as the refusal reports it (`Net`).
    pub permission: String,
    /// What was refused, as the check saw it (a host).
    pub resource: String,
}

/// The socket an HTTP client request went out on, as undici describes it on
/// a `SocketError` (`util.getSocketInfo`, node v22.22.2's bundled undici):
/// the two ends in node's spelling and the bytes the socket has carried.
/// The keys serialise as undici's own (`localAddress`, `bytesWritten`, ...),
/// which is how the engine hands them to JS.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_read: Option<u64>,
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
    /// A stat (stat, lstat, fstat): the engine resolves with a Float64Array
    /// over a fresh buffer laid out as [`ops::StatRecord`] describes, which
    /// the JS side builds its Stats from. A new variant, so a replay file
    /// recorded before it existed still loads. Boxed: inline, its 280 bytes
    /// would make every outcome that large.
    Stat(Box<ops::StatRecord>),
    /// A directory's entries (readdir, and opendir's listing): the engine
    /// resolves with `[names, kinds]`, two arrays of strings. A new
    /// variant, so a replay file recorded before it existed still loads.
    Listing(ops::DirListing),
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
        /// The second path of a two-path fs call (rename, copyfile, link,
        /// symlink): node names it in the message (`'a' -> 'b'`) and as
        /// `err.dest`. A serde default, like the three above.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dest: Option<String>,
        /// The Win32 code behind an `UNKNOWN` (`unknown_win32_error`, #450):
        /// the engine hangs it on the error as a non-enumerable `winError`,
        /// as the sync natives do (#442). A serde default, like the four
        /// above, so a recording from before it loads and one without it is
        /// byte-identical to what an older oam wrote.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        win_error: Option<u32>,
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
    /// The peer closed the HTTP client connection a request was on -- an
    /// orderly close, no reset -- before the response head was in or while
    /// its body was: undici's `SocketError` (`UND_ERR_SOCKET`, `other side
    /// closed`), with the socket it describes. The engine rejects with an
    /// Error carrying `code` and `socket`, which the JS turns into the error
    /// its caller reports (fetch: the SocketError itself as the cause;
    /// http.request: node's `socket hang up` / `aborted`). A new variant, so a
    /// replay file recorded before it existed still loads.
    SocketClosed {
        message: String,
        socket: SocketFacts,
    },
    /// An inbound OS signal (payload is the Node signal name, e.g. "SIGTERM").
    /// Only ever carried on a completion whose id == SIGNAL_OP_ID; the engine
    /// maps it to `process.emit(name)` rather than resolving a promise. serde-
    /// derived so it survives worker IPC, but workers never produce it.
    Signal(String),
}

impl OpOutcome {
    /// undici's `SocketError` for a connection its peer closed: `other side
    /// closed`, the text undici's client reports for an end-of-stream.
    pub fn socket_closed(socket: SocketFacts) -> Self {
        OpOutcome::SocketClosed {
            message: "other side closed".to_string(),
            socket,
        }
    }

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
            dest: None,
            win_error: None,
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
            dest: None,
            win_error: err.win_error,
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
            dest: None,
            win_error: None,
        }
    }

    /// `node_failed_at` for a two-path call: `path` and `dest` as node reports
    /// them (see `node_error_message_dest`).
    pub fn node_failed_dest(
        code: impl Into<String>,
        message: impl Into<String>,
        syscall: &str,
        path: &str,
        dest: &str,
        errno: Option<i32>,
    ) -> Self {
        OpOutcome::NodeFailed {
            code: code.into(),
            message: message.into(),
            syscall: Some(syscall.to_string()),
            path: Some(path.to_string()),
            errno,
            hostname: None,
            address: None,
            port: None,
            dest: Some(dest.to_string()),
            win_error: None,
        }
    }

    /// `node_failed_at` for a failure whose `io::Error` is still in hand: the
    /// errno and the Win32 code behind an `UNKNOWN` (`unknown_win32_error`,
    /// #450) come from that one error, so they cannot disagree. `code` is the
    /// caller's -- `fd_error_code` may have rewritten it.
    pub fn node_failed_io(
        code: &str,
        message: impl Into<String>,
        syscall: &str,
        path: Option<&str>,
        error: &std::io::Error,
    ) -> Self {
        OpOutcome::node_failed_at(code, message, syscall, path, node_errno(code, error))
            .with_win_error(unknown_win32_error(code, error))
    }

    /// `node_failed_dest` with the `io::Error` still in hand (see
    /// `node_failed_io`).
    pub fn node_failed_dest_io(
        code: &str,
        message: impl Into<String>,
        syscall: &str,
        path: &str,
        dest: &str,
        error: &std::io::Error,
    ) -> Self {
        OpOutcome::node_failed_dest(code, message, syscall, path, dest, node_errno(code, error))
            .with_win_error(unknown_win32_error(code, error))
    }

    /// The Win32 code behind an `UNKNOWN` on a `NodeFailed` (#450), for a
    /// caller that already has it (`check_access`). Any other outcome is
    /// returned as it is.
    pub fn with_win_error(mut self, win32: Option<u32>) -> Self {
        if let OpOutcome::NodeFailed { win_error, .. } = &mut self {
            *win_error = win32;
        }
        self
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
                Option<tokio::sync::mpsc::Sender<OutboundItem>>,
                Option<tokio::sync::mpsc::Receiver<OutboundItem>>,
            ),
        >,
    >,
>;
/// One item down an outbound request-body channel: a frame of the body --
/// the bytes JS wrote, or the trailer section an http2 client stream's
/// `sendTrailers()` ends it with -- or an `Err` that aborts the request
/// (`fetchBodyChannelCancel`).
pub type OutboundItem = Result<hyper::body::Frame<bytes::Bytes>, String>;

/// The [`OutboundItem`] for bytes JS wrote (no copy: the bytes move).
pub fn outbound_data(bytes: Vec<u8>) -> OutboundItem {
    Ok(hyper::body::Frame::data(bytes::Bytes::from(bytes)))
}

/// The [`OutboundItem`] for a trailer section JS hands over as `[name, value]`
/// pairs (each value one byte per code point, as node's nghttp2 sends it);
/// `None` when a name or a value cannot go out as a field.
pub fn outbound_trailers(pairs: &[(String, String)]) -> Option<OutboundItem> {
    http_server::trailer_fields(pairs).map(|map| Ok(hyper::body::Frame::trailers(map)))
}
/// Wakes an in-flight `fetch_body_read`. The tombstone set above is
/// checked only AFTER `chunk()` resolves, so a server that simply stops
/// sending leaves the read parked forever and pins the event loop. This
/// notifier makes cancellation preemptive.
pub type BodyCancelSignal = std::sync::Arc<tokio::sync::Notify>;

/// One open descriptor, shared by every op in flight on it.
///
/// An `Arc` so an op can take its own reference under the registry lock and
/// drop the lock before any IO. The registry used to hold a bare `File` and
/// the async read / write REMOVED it for the length of the IO await, then put
/// it back -- so a second `fs.read` on the same descriptor, fired before the
/// first completed, found the slot empty and called back EBADF. node serves
/// every one of them (libuv runs them on its thread pool against the one
/// descriptor), and so does this: nothing takes the file out, and every op --
/// positional ones through `read_at` / `write_all_at`, which never move the
/// cursor on unix -- works through a shared `&File`.
///
/// Closing removes the registry's reference. An op already holding its own
/// finishes against the still-open handle, and the OS descriptor closes when
/// the last reference drops -- the same outcome node gives a read that its
/// thread had already started when the close arrived. No lock is held across
/// any IO, so a slow descriptor never stalls another.
pub type OpenFile = std::sync::Arc<std::fs::File>;

/// The descriptor table every fd-taking `fs` op resolves through.
#[derive(Default)]
pub struct FileState {
    pub files: HashMap<u64, OpenFile>,
}
pub type FileRegistry = std::sync::Arc<std::sync::Mutex<FileState>>;

/// The open file behind `fd`, adopting a descriptor the parent handed us (see
/// `adopt_inherited_fd`) on first use. The registry lock is held only for the
/// lookup: the caller does its IO on the returned reference with nothing held.
pub fn registered_file(registry: &FileRegistry, fd: u64) -> Option<OpenFile> {
    let lookup = || {
        registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .files
            .get(&fd)
            .cloned()
    };
    // One lock on the hot path; adoption only on a miss.
    lookup().or_else(|| {
        if adopt_inherited_fd(registry, fd) {
            lookup()
        } else {
            None
        }
    })
}

/// Close `fd` as node's close does; false when it is not open (EBADF).
///
/// An adopted descriptor (below OWN_FD_BASE) is a DUP of the parent's, so
/// dropping ours closes only our copy: the original is closed too, or the
/// peer of an inherited pipe never sees EOF -- which is precisely how a CDP
/// child says "no more messages" on fd 4. The one exception is 0-2 on
/// Windows, which libuv's `fs__close` leaves open (`if (fd > 2)
/// _close(fd)`): closing one there succeeds and changes nothing, and the
/// descriptor goes on working, as in node. On unix it is really closed, and
/// a later call on it is EBADF.
pub fn close_descriptor(registry: &FileRegistry, fd: u64) -> bool {
    adopt_inherited_fd(registry, fd);
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if cfg!(windows) && fd <= 2 {
        return guard.files.contains_key(&fd);
    }
    let removed = guard.files.remove(&fd);
    drop(guard);
    let Some(file) = removed else {
        return false;
    };
    drop(file);
    if fd < OWN_FD_BASE {
        close_inherited_fd(fd);
        if let Some(closed) = STDIO_CLOSED.get(fd as usize) {
            closed.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    true
}

/// Which of descriptors 0-2 `close_descriptor` has really closed -- unix
/// only, as Windows leaves them open. The runtime's own stdout and stderr
/// writes do not go through the descriptor table, so they ask here.
static STDIO_CLOSED: [std::sync::atomic::AtomicBool; 3] = [
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
];

/// The error a write to the process's stdout (1) or stderr (2) gets once the
/// program has closed that descriptor with `fs.closeSync` / `fs.close`: EBADF,
/// as node's write(2) gets on unix, where libuv really closes it. `None` while
/// it is open -- always, on Windows. One relaxed load on the write path.
pub fn closed_stdio_error(fd: u64) -> Option<std::io::Error> {
    let closed = STDIO_CLOSED.get(fd as usize)?;
    if !closed.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    #[cfg(unix)]
    let errno = libc::EBADF;
    #[cfg(not(unix))]
    let errno = 6; // ERROR_INVALID_HANDLE; unreachable, nothing sets the flag
    Some(std::io::Error::from_raw_os_error(errno))
}

/// A read or write position as node's binding hands it to libuv (its
/// GetOffset: any safe integer, else -1), resolved to what the OS is asked
/// for: `Some(offset)` for a positional op, `None` for the cursor.
///
/// -1 is the cursor everywhere. Any other negative is the cursor on unix,
/// where libuv's uv__fs_read / uv__fs_write take `off < 0` to mean read(2) /
/// write(2). On Windows libuv's fs__read / fs__write hand every offset but -1
/// to the OS as an OVERLAPPED offset, two's complement and all, around a
/// saved and restored file pointer -- which is what `read_at` / `write_all_at`
/// do with the `Some` returned here -- and the OS decides (measured on node
/// v22.22.2): -2 is its FILE_USE_FILE_POINTER_POSITION, so the op happens at
/// the cursor and the cursor does not move; a handle opened for append writes
/// at the end; anything else fails ERROR_INVALID_PARAMETER, EINVAL.
pub fn file_offset(position: Option<i64>) -> Option<u64> {
    match position {
        Some(p) if p >= 0 || (p != -1 && cfg!(windows)) => Some(p as u64),
        _ => None,
    }
}

/// Read into `buf` at `position` -- `pread(2)` -- or from the cursor (see
/// `file_offset` for a negative one). A positional read does not move the
/// cursor, which is what node's positional `fs.read` family means.
///
/// Unix has a real `pread`, so concurrent positional reads on one descriptor
/// never see each other. Windows has no read that leaves the file pointer
/// alone (a ReadFile with an OVERLAPPED offset on a synchronous handle moves
/// it), so this does what libuv's `fs__read` does there: note the pointer,
/// read at the offset, put the pointer back.
pub fn read_at(
    file: &std::fs::File,
    buf: &mut [u8],
    position: Option<i64>,
) -> std::io::Result<usize> {
    use std::io::Read;
    let Some(p) = file_offset(position) else {
        return (&*file).read(buf);
    };
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buf, p)
    }
    #[cfg(windows)]
    {
        let saved = stream_position_of(file);
        let result = std::os::windows::fs::FileExt::seek_read(file, buf, p);
        restore_position(file, saved);
        result
    }
    #[cfg(not(any(unix, windows)))]
    {
        use std::io::{Seek, SeekFrom};
        let saved = stream_position_of(file);
        (&*file).seek(SeekFrom::Start(p))?;
        let result = (&*file).read(buf);
        restore_position(file, saved);
        result
    }
}

/// Write all of `bytes` at `position` -- `pwrite(2)` -- or at the cursor (see
/// `file_offset` for a negative one), with `write_all_checked`'s rule that an empty write still
/// reaches the descriptor. Same platform split as `read_at`. A descriptor
/// opened for APPEND writes at the end whatever the position, on every
/// platform node runs on; that is the OS's behaviour and node's.
pub fn write_all_at(
    file: &std::fs::File,
    bytes: &[u8],
    position: Option<i64>,
) -> std::io::Result<()> {
    let Some(p) = file_offset(position) else {
        return write_all_checked(file, bytes);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        if bytes.is_empty() {
            return file.write_at(bytes, p).map(|_| ());
        }
        file.write_all_at(bytes, p)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let saved = stream_position_of(file);
        let result = (|| {
            if bytes.is_empty() {
                return file.seek_write(bytes, p).map(|_| ());
            }
            let mut done = 0usize;
            while done < bytes.len() {
                // Wrapping, as libuv's int64 `offset + bytes` does for a
                // negative offset (see file_offset).
                match file.seek_write(&bytes[done..], p.wrapping_add(done as u64)) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => done += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })();
        restore_position(file, saved);
        result
    }
    #[cfg(not(any(unix, windows)))]
    {
        use std::io::{Seek, SeekFrom};
        let saved = stream_position_of(file);
        (&*file).seek(SeekFrom::Start(p))?;
        let result = write_all_checked(file, bytes);
        restore_position(file, saved);
        result
    }
}

#[cfg(not(unix))]
fn stream_position_of(file: &std::fs::File) -> Option<u64> {
    use std::io::Seek;
    (&*file).stream_position().ok()
}

/// Put the cursor back where `stream_position_of` found it. A descriptor that
/// cannot seek (a pipe) reported no position, and there is nothing to restore.
#[cfg(not(unix))]
fn restore_position(file: &std::fs::File, saved: Option<u64>) {
    use std::io::{Seek, SeekFrom};
    if let Some(prev) = saved {
        let _ = (&*file).seek(SeekFrom::Start(prev));
    }
}

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
/// 0/1/2 are adopted the same way (see `inherited_eligible`), so the window
/// is 0..OWN_FD_BASE.
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
    // F_GETFD is the cheapest "is this open" question; see fd_is_open.
    if !fd_is_open(raw) {
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
///
/// 0, 1 and 2 need no snapshot: they are the process's stdin, stdout and
/// stderr, which node's fd calls operate on (`fstatSync(0)`, `readSync(0)`,
/// `fsyncSync(1)`), and oam never opens anything of its own there.
fn inherited_eligible(fd: u64) -> bool {
    let born_holding = fd <= 2
        || ((3..=MAX_INHERITED_FD).contains(&fd)
            && INHERITED_AT_START
                .get()
                .is_some_and(|set| set.contains(&fd)));
    born_holding
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
        fd_is_open(raw)
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

/// Is the integer descriptor `raw` open? The unix twin of `crt_fd::osfhandle`'s
/// -1 test.
///
/// Safe: `fcntl(F_GETFD)` takes only an integer and reads that descriptor's
/// flags, changing nothing. It is fully defined on any `i32` -- a closed or
/// never-opened descriptor answers -1/EBADF -- so no caller obligation is left.
/// It has to be the raw call: it is the probe that establishes openness, which
/// a `BorrowedFd` would already presuppose.
#[cfg(unix)]
fn fd_is_open(raw: i32) -> bool {
    // SAFETY: `fcntl(F_GETFD)` only reads the flags of the integer fd `raw`; no
    // pointers are involved, and an invalid `raw` is reported as -1/EBADF
    // rather than being undefined.
    unsafe { libc::fcntl(raw, libc::F_GETFD) != -1 }
}

/// The snapshot's fds that `disable_stdio_inheritance` should act on: inherited
/// at start, not since consumed by `close_inherited_fd`, and at or above fd 3.
///
/// Split out from the platform I/O so the selection can be tested directly. The
/// `fd >= 3` filter is restated here even though the snapshot starts at 3, so
/// the unix rule (NEVER 0-2 -- see `disable_stdio_inheritance`) can never be
/// broken by a change to the snapshot.
fn inheritable_fds_to_disable(
    snapshot: &std::collections::HashSet<u64>,
    consumed: &std::collections::HashSet<u64>,
) -> Vec<i32> {
    snapshot
        .iter()
        .filter(|fd| !consumed.contains(fd))
        .filter_map(|&fd| i32::try_from(fd).ok())
        .filter(|&fd| fd >= 3)
        .collect()
}

/// Make the descriptors we were born holding non-inheritable, as node's
/// startup does with uv_disable_stdio_inheritance. Call ONCE, right after
/// `snapshot_inherited_fds` (whose record it reads) and before any child is
/// spawned.
///
/// Without it every child oam starts takes a copy of whatever its parent
/// handed oam, whatever that child's `stdio` says: a detached Windows child
/// with `stdio: 'ignore'` kept oam's stdout pipe, and any child kept an
/// extra-fd pipe (`stdio: [..., 'pipe']`), so the parent's read of that pipe
/// ended when the last such child exited instead of when oam did. Only the
/// inheritance flag changes. oam's own use is unaffected: its writes to 0-2
/// look the handle up each time, and an adopted fd is a dup made at first use
/// (`dup_inherited`), which never depended on the original's flag. A child
/// that is GIVEN one of these descriptors still gets it: `stdio: 'inherit'`
/// and `stdio: [..., n]` both hand the child a fresh inheritable duplicate
/// (Rust std, child_win), or dup2 it into place (child_unix), which clears
/// FD_CLOEXEC on the target.
///
/// - Windows: clears HANDLE_FLAG_INHERIT on the three std handles
///   (GetStdHandle), on the OS handles behind CRT fds 0-2, and on the OS
///   handle behind every CRT fd in the snapshot -- together, every CRT fd
///   from 0 to `MAX_INHERITED_FD` the parent passed in
///   STARTUPINFO.lpReserved2, which libuv walks entry by entry. CRT fds 0-2
///   are covered separately because a parent may pass a different handle
///   there than in STARTUPINFO's hStd* fields.
/// - unix: sets FD_CLOEXEC on every fd in the snapshot, which holds fds 3 and
///   up only. NEVER 0-2, unlike libuv: libuv's spawn always dup2s a child's
///   stdio into place, but Rust std's `Stdio::inherit` leaves fds 0-2 as they
///   are, so a close-on-exec flag there would close every `stdio: 'inherit'`
///   child's stdio at exec.
///
/// Coverage is the snapshot's window, fds 3 through `MAX_INHERITED_FD` (63):
/// a descriptor a parent passes at 64 or above stays inheritable (see
/// docs/node-divergences.md #19). An fd already closed through
/// `close_inherited_fd` is skipped, so a number the OS has since reissued to
/// one of oam's own descriptors is never touched.
pub fn disable_stdio_inheritance() {
    debug_assert!(
        INHERITED_AT_START.get().is_some(),
        "disable_stdio_inheritance needs snapshot_inherited_fds to have run first"
    );
    let consumed = INHERITED_CONSUMED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let inherited: Vec<i32> = INHERITED_AT_START
        .get()
        .map(|snapshot| inheritable_fds_to_disable(snapshot, &consumed))
        .unwrap_or_default();

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{
            HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        // The OS handles behind CRT fds 0-2 and the inherited CRT fds 3 and
        // up, looked up with the safe probe. A real handle is positive; -1 is
        // "not open" and -2 is the CRT's "std fd with no stream", neither of
        // which is ours to touch. 0-2 usually repeat a GetStdHandle handle
        // below; clearing a flag twice is harmless.
        let crt_handles: Vec<HANDLE> = (0..=2)
            .chain(inherited.iter().copied())
            .map(crt_fd::osfhandle)
            .filter(|&h| h > 0)
            .map(|h| h as HANDLE)
            .collect();
        // SAFETY: GetStdHandle takes a constant and returns this process's
        // std handle (or null / INVALID_HANDLE_VALUE, both skipped).
        // SetHandleInformation with mask HANDLE_FLAG_INHERIT and flags 0 only
        // clears the inheritance bit of a handle this process holds: the std
        // handles just fetched, and the CRT-table handles of fds 0-2 and of
        // the inherited fds probed just above (fds oam has closed are
        // filtered out before the probe) -- and at
        // worst a stale handle value makes the call fail with
        // ERROR_INVALID_HANDLE. No pointer is passed or
        // dereferenced. A failure is ignored: it leaves that handle
        // inheritable, which is the behaviour before this call.
        unsafe {
            for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let h = GetStdHandle(id);
                if !h.is_null() && h != INVALID_HANDLE_VALUE {
                    SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
                }
            }
            for h in crt_handles {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }

    #[cfg(unix)]
    for raw in inherited {
        // SAFETY: fcntl F_GETFD / F_SETFD take only the integer fd `raw` and an
        // integer flag word; no pointer is involved, and a descriptor closed
        // since the snapshot answers -1/EBADF, which is checked. `raw` is a
        // descriptor the parent handed us (fd >= 3, in the startup snapshot,
        // not since closed by oam); an adopted fd is a separate dup, so this
        // is the parent's original. Setting its close-on-exec flag breaks no
        // owner's invariant -- Rust std sets FD_CLOEXEC on every descriptor it
        // creates -- it only stops children inheriting it, as node's startup
        // does. A failed F_SETFD leaves it inheritable, the behaviour before
        // this call.
        unsafe {
            let flags = libc::fcntl(raw, libc::F_GETFD);
            if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    let _ = inherited;
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
            guard.files.insert(fd, std::sync::Arc::new(file));
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
/// - Compress/Decompress: gzip/deflate/deflateRaw (NodeDeflate encoders,
///   NodeInflate decoders), truly incremental.
/// - BrotliCompress/BrotliDecompress: pure-Rust brotli via the `brotli` crate.
/// - HandleCompress/HandleDecompress: node's low-level zlib handle.
pub enum ZlibStream {
    Compress(zlib::StreamCompressor),
    Decompress(zlib::StreamDecompressor),
    // Brotli state is large (~5 KB for the compressor); Box keeps the enum
    // discriminant compact so the gzip/deflate variants (the hot path) don't
    // pay for brotli's footprint in the HashMap registry. Heap indirection
    // is paid once per brotli stream, never on the per-chunk write path.
    BrotliCompress(Box<BrotliCompressor>),
    BrotliDecompress(Box<BrotliDecompressor>),
    HandleCompress(Box<zlib::NodeDeflate>),
    HandleDecompress(Box<zlib::NodeInflate>),
    /// node's zstd handle (`zstd::ZstdHandle`). Shared, not owned: an async
    /// write holds its own reference and the handle's lock for the length of
    /// the libzstd call, so the registry lock is held only to look it up and
    /// a close that lands mid-write frees the context when that write ends.
    Zstd(std::sync::Arc<std::sync::Mutex<zstd::ZstdHandle>>),
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
    /// An `fs.watch` watcher: its parked `fsWatchNext` (see
    /// [`crate::fs_watch`]).
    FsWatch(u64),
    /// A worker's `parentPort`: its parked receive. node unrefs the port
    /// when its last 'message' listener goes, and the worker may then exit
    /// with the receive still waiting (#401).
    ParentPort,
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
    /// `http.request`'s "this request has a connection" signals
    /// (`http_client::sent`); dropped with the run.
    sent_signals: http_client::sent::SentSignals,
    /// Fetches with no response head yet that JS can cancel
    /// (`http_client::send::FetchCancel`).
    fetch_cancels: http_client::send::FetchCancels,
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
    /// `fs.watch` watchers; dropped (and the OS watches stopped) with the run.
    fs_watches: fs_watch::FsWatchRegistry,
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
            sent_signals: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            fetch_cancels: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
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
            fs_watches: fs_watch::FsWatchRegistry::default(),
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
        let (tx, rx) = tokio::sync::mpsc::channel::<OutboundItem>(8);
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

    /// Open a sent signal and return its handle (`fetchSentOpen`).
    pub fn new_sent_signal(&self) -> u64 {
        let handle = self
            .next_body
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        http_client::sent::open(&self.sent_signals, handle);
        handle
    }

    /// `http.request`'s sent signals (Arc clone).
    pub fn sent_signals(&self) -> http_client::sent::SentSignals {
        self.sent_signals.clone()
    }

    /// In-flight fetches JS can cancel before their response head (Arc
    /// clone).
    pub fn fetch_cancels(&self) -> http_client::send::FetchCancels {
        self.fetch_cancels.clone()
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

    /// `fs.watch` watchers (Arc clone; die with the run).
    pub fn fs_watches(&self) -> fs_watch::FsWatchRegistry {
        self.fs_watches.clone()
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

    /// A handle that wakes this runtime's op channel (see [`LoopWaker`]).
    pub fn loop_waker(&self) -> LoopWaker {
        LoopWaker(self.tx.clone())
    }

    /// Bookkeeping shared by `try_recv` and `recv_deadline`: a settled op
    /// stops counting, and leaves its handle's in-flight set (its id must
    /// never be re-counted by `set_handle_ref` again).
    fn note_settled(&mut self, completion: &OpCompletion) {
        if completion.id == SIGNAL_OP_ID || completion.id == PLATFORM_TASK_OP_ID {
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

/// The Win32 code behind a system error node can only call `UNKNOWN`: the raw
/// code (`GetLastError` / `WSAGetLastError`, as winerror.h writes it) when
/// libuv's table has no row for it; `None` for every named code, and off
/// Windows.
///
/// `UNKNOWN` says only that libuv has no name for the code, and the code is
/// then the one clue to a failure nobody can reproduce (#442: a console write
/// that died `UNKNOWN: unknown error, write`). The engine hangs it on the
/// error as a non-enumerable `winError` (`keep_win32_code`,
/// docs/node-divergences.md #59), so node's shape -- `errno`, `code`,
/// `syscall`, `message`, `Object.keys` -- is as it was. `code` is the caller's,
/// not derived here again: `fd_error_code` rewrites a PermissionDenied to
/// EBADF, and that error carries no code worth keeping.
///
/// Two raw values are not Win32 codes and are refused: 0, NO_ERROR, which
/// `windows_error_code` also calls UNKNOWN but which names no failure; and
/// `UV_UNKNOWN` itself, oam's stand-in for libuv's own UV_UNKNOWN
/// (`as_the_crt_takes_it`, the `\\.\mailslot` open), which reported as a Win32
/// code would be a libuv errno in disguise. Everything else is the DWORD the
/// OS set, the HRESULT-shaped ones std stores negative included.
pub fn unknown_win32_error(code: &str, error: &std::io::Error) -> Option<u32> {
    #[cfg(windows)]
    {
        if code != "UNKNOWN" {
            return None;
        }
        match error.raw_os_error()? {
            0 | UV_UNKNOWN => None,
            raw => Some(raw as u32),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (code, error);
        None
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
/// Where libuv changes the answer for one operation only -- mkdir's
/// `EINVAL`, readlink's -- that is `fs_error_at`'s job (and for a listing
/// `ops::readdir_to_json`'s), not this table's: the same code means
/// something else at another call site
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
pub enum FsSite {
    /// `fs.open` with these flags.
    Open(OpenFlags),
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
/// For a whole-file operation (`readFile`, `writeFile`, `appendFile`) this is
/// the OPEN half only; `whole_file_error` reports the read or write half.
/// `readFile` of a DIRECTORY fails on the read everywhere in node (`EISDIR:
/// illegal operation on a directory, read`, no path): open admits the
/// directory -- open(2) on Linux and macOS, CreateFileW with
/// FILE_FLAG_BACKUP_SEMANTICS on Windows (`opens_as_libuv`) -- and the read
/// fails, which is the read half's own answer.
///
/// Every rule here is Windows-only, and each is a place where the generic
/// table alone gives node's code for the wrong operation:
///
/// - A truncating open (`w`, `w+`, `writeFile`) of a DIRECTORY fails
///   ERROR_FILE_EXISTS -- CREATE_ALWAYS (`OpenFlags::open`) collides with the
///   name before the directory's ACL or sharing is asked -- which the table
///   reads as `EEXIST` and fs__open turns into `EISDIR` for any open that
///   creates and is not exclusive. A root is refused ERROR_ACCESS_DENIED,
///   the table's `EPERM`, with no rule.
/// - fs__mkdir reports ERROR_INVALID_NAME and ERROR_DIRECTORY as `EINVAL`. Not
///   for a recursive mkdir: node's MKDirp re-stats the path after the failure
///   and reports `ENOENT`.
/// - fs__readlink reports ERROR_NOT_A_REPARSE_POINT as `EINVAL`.
///
/// `readdir` and `opendir` have rules too, which need the path looked at
/// again and so live with the listing: `ops::readdir_to_json`.
///
/// Every other open of a directory needs no rule. It opens, a root included,
/// and the read or write on it is what fails; `wx` and `ax` fail
/// ERROR_FILE_EXISTS, the table's `EEXIST`; and an open of a root for writing
/// is refused ERROR_ACCESS_DENIED, the table's `EPERM`, as node's is.
pub fn fs_error_at(
    site: FsSite,
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
            ERROR_DIRECTORY, ERROR_FILE_EXISTS, ERROR_INVALID_NAME, ERROR_NOT_A_REPARSE_POINT,
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
            FsSite::Readlink if raw == Some(ERROR_NOT_A_REPARSE_POINT) => {
                return at("EINVAL", syscall, true);
            }
            _ => {}
        }
        let truncates = match site {
            FsSite::WriteFile => true,
            FsSite::Open(flags) => flags.creates_or_opens(),
            _ => false,
        };
        if truncates && raw == Some(ERROR_FILE_EXISTS) {
            return at("EISDIR", syscall, true);
        }
    }
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

/// Which half of a whole-file operation failed.
///
/// node's `readFile`, `writeFile` and `appendFile` (sync, callback and
/// promise) open the path and then read or write the descriptor, and report
/// the two halves differently: a failed open names syscall `open` and the path
/// (`ENOENT: no such file or directory, open 'p'`), a failed read or write names
/// syscall `read` / `write` and no path at all (`EBUSY: resource busy or
/// locked, read` for a region another process has locked, `EISDIR ..., read`
/// for a directory opened on Linux and macOS, `ENOSPC ..., write`). std's
/// `fs::read` / `fs::write` return one `io::Error` for both halves, so every
/// failure got the open's label -- and `process.loadEnvFile`, which reports any
/// failed open as ENOENT, turned a read failure on an existing file into a
/// false ENOENT.
#[derive(Debug)]
pub enum WholeFileError {
    /// Opening the path failed.
    Open(std::io::Error),
    /// The read or write on the opened file failed.
    Transfer(std::io::Error),
}

impl WholeFileError {
    pub fn io(&self) -> &std::io::Error {
        match self {
            WholeFileError::Open(e) | WholeFileError::Transfer(e) => e,
        }
    }
}

/// What libuv's fs__open asks of CreateFileW for EVERY path and std does not:
/// FILE_FLAG_BACKUP_SEMANTICS, without which Windows opens no directory at
/// all. With it a directory opens as it does under open(2), so node hands out
/// a descriptor for one (`openSync(dir, "r")`, `"r+"`, `"a"`), `fstat` of it
/// is a directory, and a read or write on it is what fails:
/// ERROR_INVALID_FUNCTION, the table's `EISDIR`. The flag changes nothing for
/// a regular file.
///
/// node's opens are `OpenFlags::open`, which asks for the flag itself. This
/// is for the opens still made through std: `readFile`'s, and the second look
/// a failed listing takes (`ops::readdir_to_json`). Elsewhere the flag does
/// not exist and this is a no-op.
pub fn opens_as_libuv(options: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    options
}

/// Whether `path` is the root of a share: a UNC prefix with nothing under
/// it. Not a device (`\\.\CON`), which is a prefix with nothing under it
/// too, and which node opens or refuses as CreateFileW does.
#[cfg(windows)]
fn is_share_root(path: &std::path::Path) -> bool {
    use std::path::{Component, Prefix};
    let mut parts = path.components();
    matches!(
        parts.next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    ) && parts.all(|part| matches!(part, Component::RootDir))
}

/// `options.open(path)` with libuv's two Windows refusals. Before the open,
/// the one path FILE_FLAG_BACKUP_SEMANTICS opens and node does not: the root
/// of a share that is no disk (`\\srv\IPC$`). node fails every open of it
/// `EINVAL` (v22.22.2, measured), which is also what a stat of it answers in
/// both runtimes. With the flag CreateFileW hands back a handle for it, so
/// `openSync` returned a descriptor and an `appendFile` of no data succeeded.
/// Only a root is asked, so no other open pays for the stat. After the open,
/// what the C runtime refuses (`as_the_crt_takes_it`), as for
/// `OpenFlags::open_with_mode`'s handles: `\\.\UNC\srv\IPC$` is `EINVAL`,
/// `\\.\mailslot` `UNKNOWN`.
pub fn open_path(
    options: &std::fs::OpenOptions,
    path: impl AsRef<std::path::Path>,
) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    refuse_a_root_that_is_no_disk(path.as_ref())?;
    #[cfg(windows)]
    return options.open(path).and_then(as_the_crt_takes_it);
    #[cfg(not(windows))]
    options.open(path)
}

/// What libuv does with every handle its open gets (src/win/fs.c fs__open,
/// v1.51.0): hands it to the CRT's `_open_osfhandle`, which refuses one whose
/// type GetFileType cannot name. libuv reports that refusal as the thread's
/// last error -- ERROR_INVALID_PARAMETER for the root of a share that is no
/// disk however it is spelled (`\\.\UNC\srv\IPC$`), node's `EINVAL` -- or,
/// where GetFileType set none, `UV_UNKNOWN` itself: the mailslot namespace
/// (`\\.\mailslot`), node's `UNKNOWN` (errno -4094). Measured against
/// v22.22.2 (#330).
#[cfg(windows)]
fn as_the_crt_takes_it(file: std::fs::File) -> std::io::Result<std::fs::File> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_TYPE_UNKNOWN;
    if crate::child_win::file_type(file.as_raw_handle()) != FILE_TYPE_UNKNOWN {
        return Ok(file);
    }
    // GetFileType sets the last error on this path itself, NO_ERROR included.
    let why = std::io::Error::last_os_error();
    Err(match why.raw_os_error() {
        Some(0) | None => std::io::Error::from_raw_os_error(UV_UNKNOWN),
        Some(_) => why,
    })
}

/// libuv's UV_UNKNOWN, which it stores as a Win32 error where it has none;
/// the error table has no entry for it, so it reads back as `UNKNOWN`.
#[cfg(windows)]
const UV_UNKNOWN: i32 = -4094;

/// `open_path`'s refusal, for the two opens that share it.
#[cfg(windows)]
fn refuse_a_root_that_is_no_disk(path: &std::path::Path) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    if is_share_root(path) {
        match std::fs::metadata(path) {
            Err(e) if e.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() => {
                return Err(e);
            }
            _ => {}
        }
    }
    Ok(())
}

/// The platform's value for each open flag node knows. node's
/// `fs.constants.O_*` are these numbers -- open(2)'s on unix, the CRT's and
/// libuv's on Windows -- and a numeric `flags` is handed to the open as it is,
/// so they have to be the real ones: `open_flag_table` is where JS gets them.
#[cfg(unix)]
mod o_flag {
    use rustix::fs::OFlags;
    pub const RDONLY: i32 = OFlags::RDONLY.bits() as i32;
    pub const WRONLY: i32 = OFlags::WRONLY.bits() as i32;
    pub const RDWR: i32 = OFlags::RDWR.bits() as i32;
    pub const CREAT: i32 = OFlags::CREATE.bits() as i32;
    pub const EXCL: i32 = OFlags::EXCL.bits() as i32;
    pub const NOCTTY: i32 = OFlags::NOCTTY.bits() as i32;
    pub const TRUNC: i32 = OFlags::TRUNC.bits() as i32;
    pub const APPEND: i32 = OFlags::APPEND.bits() as i32;
    pub const DIRECTORY: i32 = OFlags::DIRECTORY.bits() as i32;
    pub const NOFOLLOW: i32 = OFlags::NOFOLLOW.bits() as i32;
    pub const SYNC: i32 = OFlags::SYNC.bits() as i32;
    // Not rustix's DSYNC on Linux: its raw backend spells that as O_SYNC's
    // bits (it asks the kernel for the stronger guarantee), and node's
    // `fs.constants.O_DSYNC` is the kernel's own O_DSYNC -- 0o10000 on the
    // architectures oam is built for (x86_64, aarch64).
    #[cfg(target_os = "linux")]
    pub const DSYNC: i32 = 0o10000;
    #[cfg(not(target_os = "linux"))]
    pub const DSYNC: i32 = OFlags::DSYNC.bits() as i32;
    pub const NONBLOCK: i32 = OFlags::NONBLOCK.bits() as i32;
    /// The flags an open may carry and still only read: see
    /// `OpenFlags::writes`.
    #[cfg(target_os = "linux")]
    pub const HARMLESS: i32 =
        NOCTTY | DIRECTORY | NOFOLLOW | SYNC | DSYNC | NONBLOCK | NOATIME | DIRECT;
    #[cfg(not(target_os = "linux"))]
    pub const HARMLESS: i32 = NOCTTY | DIRECTORY | NOFOLLOW | SYNC | DSYNC | NONBLOCK;
    #[cfg(target_os = "linux")]
    pub const NOATIME: i32 = OFlags::NOATIME.bits() as i32;
    // Apple's O_SYMLINK (sys/fcntl.h), which the rustix this is built against
    // has no name for.
    #[cfg(target_os = "macos")]
    pub const SYMLINK: i32 = 0x0020_0000;
    #[cfg(target_os = "linux")]
    pub const DIRECT: i32 = OFlags::DIRECT.bits() as i32;
}
/// libuv's values (include/uv/win.h): the CRT's `_O_*` where it has one, and
/// libuv's own high bits for what the CRT has no flag for.
#[cfg(windows)]
mod o_flag {
    pub const RDONLY: i32 = 0x0000;
    pub const WRONLY: i32 = 0x0001;
    pub const RDWR: i32 = 0x0002;
    pub const APPEND: i32 = 0x0008;
    pub const RANDOM: i32 = 0x0010;
    pub const SEQUENTIAL: i32 = 0x0020;
    pub const TEMPORARY: i32 = 0x0040;
    pub const CREAT: i32 = 0x0100;
    pub const TRUNC: i32 = 0x0200;
    pub const EXCL: i32 = 0x0400;
    pub const SHORT_LIVED: i32 = 0x1000;
    pub const DIRECT: i32 = 0x0200_0000;
    pub const DSYNC: i32 = 0x0400_0000;
    pub const SYNC: i32 = 0x0800_0000;
    pub const EXLOCK: i32 = 0x1000_0000;
    pub const FILEMAP: i32 = 0x2000_0000;
    /// The flags an open may carry and still only read: see
    /// `OpenFlags::writes`. Not TEMPORARY, which deletes the file when the
    /// descriptor closes; and not FILEMAP, which asks for a writable
    /// mapping of the file in libuv and is left off the list for that.
    pub const HARMLESS: i32 = RANDOM | SEQUENTIAL | SHORT_LIVED | DIRECT | DSYNC | SYNC | EXLOCK;
}

/// `process.umask`: set the process's file mode creation mask to `set` and
/// say what it was, or with `None` only say what it is.
///
/// On unix it is umask(2), so the kernel takes it off the mode of every
/// file and directory this process creates, and a child inherits it. The
/// value is remembered here once known, because umask(2) can only be read
/// by setting it: asking through it each time, as node does, would leave
/// the mask at zero for a moment in which another thread's open creates a
/// file wider than was asked. Linux can say without that (/proc); elsewhere
/// the first read sets and restores it once. A mask changed behind this --
/// by a native addon calling umask(2) itself -- is not seen until the next
/// set.
///
/// The first value this learns is the mask the process started with, and
/// is noted as that (`oam_diagnostics::own_files`): what oam writes for
/// itself keeps the modes that mask gives, whatever is set here later.
#[cfg(unix)]
pub fn process_umask(set: Option<u32>) -> u32 {
    use rustix::fs::Mode;
    // What the kernel keeps of a mask, and so what it says the mask was:
    // the permission bits on Linux (S_IRWXUGO), those and the set-id and
    // sticky bits on macOS and the BSDs (ALLPERMS).
    const KEPT: u32 = if cfg!(any(target_os = "linux", target_os = "android")) {
        0o777
    } else {
        0o7777
    };
    // mode_t is 16 bits on macOS and 32 on Linux.
    #[allow(clippy::useless_conversion)]
    let bits = |mode: Mode| u32::from(mode.as_raw_mode()) & KEPT;
    static KNOWN: std::sync::Mutex<Option<u32>> = std::sync::Mutex::new(None);
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    match set {
        Some(mask) => {
            let mask = mask & KEPT;
            // Said first: from here what oam writes for itself sets its
            // modes itself. (A write of its own already past that check
            // when the mask changes under it is the one thing not covered.)
            oam_diagnostics::own_files::note_umask_set();
            let was = bits(rustix::process::umask(Mode::from_raw_mode(mask as _)));
            oam_diagnostics::own_files::note_inherited_umask(known.unwrap_or(was));
            *known = Some(mask);
            was
        }
        None => *known.get_or_insert_with(|| {
            let mask = umask_from_proc().unwrap_or_else(|| {
                let was = rustix::process::umask(Mode::empty());
                rustix::process::umask(was);
                bits(was)
            });
            oam_diagnostics::own_files::note_inherited_umask(mask);
            mask
        }),
    }
}

/// Learn the mask the process started with, before any program code can
/// change it: the CLI calls this first thing (see `process_umask`). Nothing
/// to learn on Windows, where the mask is this process's own from zero.
pub fn snapshot_inherited_umask() {
    #[cfg(unix)]
    process_umask(None);
}

/// The `Umask:` line of /proc/self/status (Linux 4.7 and later): the mask
/// read without touching it.
#[cfg(unix)]
fn umask_from_proc() -> Option<u32> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find_map(|l| l.strip_prefix("Umask:"))?;
    u32::from_str_radix(line.trim(), 8).ok()
}

/// The mask on Windows, where there is no such thing below the C runtime:
/// what node's `_umask` keeps is the owner's read and write bits, and the
/// one use libuv makes of it is in its open (`OpenFlags::open_with_mode`).
#[cfg(windows)]
static WINDOWS_UMASK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(windows)]
pub fn process_umask(set: Option<u32>) -> u32 {
    use std::sync::atomic::Ordering;
    match set {
        Some(mask) => WINDOWS_UMASK.swap(mask & 0o600, Ordering::AcqRel),
        None => WINDOWS_UMASK.load(Ordering::Acquire),
    }
}

#[cfg(not(any(unix, windows)))]
pub fn process_umask(_set: Option<u32>) -> u32 {
    0
}

/// Whether a Windows open that may create its file asks for a read-only
/// one: libuv's fs__open rule, the mode less the umask having no
/// owner-write bit.
#[cfg(any(windows, test))]
fn creates_read_only(mode: u32, umask: u32) -> bool {
    (mode & !umask) & 0o200 == 0
}

/// `fs.constants`' open flags on this platform: the names node has here, in
/// node's order, with the platform's values (v22.22.2; Windows and Linux
/// measured, macOS from node's source).
pub fn open_flag_table() -> Vec<(&'static str, i32)> {
    let mut table = vec![
        ("O_RDONLY", o_flag::RDONLY),
        ("O_WRONLY", o_flag::WRONLY),
        ("O_RDWR", o_flag::RDWR),
        ("O_CREAT", o_flag::CREAT),
        ("O_EXCL", o_flag::EXCL),
    ];
    #[cfg(windows)]
    table.extend([
        ("UV_FS_O_FILEMAP", o_flag::FILEMAP),
        ("O_TRUNC", o_flag::TRUNC),
        ("O_APPEND", o_flag::APPEND),
    ]);
    #[cfg(unix)]
    {
        table.extend([
            ("UV_FS_O_FILEMAP", 0),
            ("O_NOCTTY", o_flag::NOCTTY),
            ("O_TRUNC", o_flag::TRUNC),
            ("O_APPEND", o_flag::APPEND),
            ("O_DIRECTORY", o_flag::DIRECTORY),
        ]);
        #[cfg(target_os = "linux")]
        table.push(("O_NOATIME", o_flag::NOATIME));
        table.extend([
            ("O_NOFOLLOW", o_flag::NOFOLLOW),
            ("O_SYNC", o_flag::SYNC),
            ("O_DSYNC", o_flag::DSYNC),
        ]);
        #[cfg(target_os = "linux")]
        table.push(("O_DIRECT", o_flag::DIRECT));
        #[cfg(target_os = "macos")]
        table.push(("O_SYMLINK", o_flag::SYMLINK));
        table.push(("O_NONBLOCK", o_flag::NONBLOCK));
    }
    table
}

/// The flags of one open: the platform's O_* bits, as node hands them to
/// libuv. A flag STRING is only node's shorthand for some of them
/// (`stringToFlags`; `open_options_for` here), and a number is the bits
/// themselves -- so the open is made from the bits, whichever it was given.
///
/// The natives used to take the string, and a number was turned into the
/// nearest one. No string spells "write, do not truncate, do not append", so
/// `O_WRONLY`, `O_WRONLY | O_CREAT` and `O_RDWR | O_CREAT` opened as `"w"` /
/// `"w+"` and emptied the file (#328).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenFlags(pub i32);

impl OpenFlags {
    /// Whether the open needs the write grant: anything but an open that
    /// can ONLY read. That is decided by what is allowed, not by what is
    /// known to write: the access mode must be read-only and every other
    /// bit one of a short list that changes nothing (`o_flag::HARMLESS`).
    /// Any other bit -- O_CREAT, O_TRUNC, O_APPEND, Windows's delete-on-close
    /// O_TEMPORARY, a flag this table has never heard of -- needs the write
    /// grant, since the bits now reach the open as they are. node's rule
    /// (CheckOpenPermissions) names the writing flags instead; this one is a
    /// superset of it.
    pub fn writes(self) -> bool {
        self.0 & !o_flag::HARMLESS != o_flag::RDONLY
    }

    /// Whether the open needs the read grant: any access mode but
    /// write-only, as node's rule has it (CheckOpenPermissions). An open is
    /// asked for BOTH grants when it reads and writes -- `r+` hands back a
    /// descriptor that can read the file.
    pub fn reads(self) -> bool {
        self.0 & (o_flag::WRONLY | o_flag::RDWR) != o_flag::WRONLY
    }

    /// Creates a file that is missing and accepts one that is there: the
    /// opens libuv reports a name collision for as `EISDIR` (`fs_error_at`).
    pub fn creates_or_opens(self) -> bool {
        self.0 & o_flag::CREAT != 0 && self.0 & o_flag::EXCL == 0
    }

    /// The open with node's default `mode`, 0o666: what a file it creates
    /// gets when the caller names none.
    pub fn open(self, path: impl AsRef<std::path::Path>) -> std::io::Result<std::fs::File> {
        self.open_with_mode(path, 0o666)
    }

    /// open(2) with exactly these bits (and libuv's O_CLOEXEC) and `mode`,
    /// which is the whole of what node's open is on unix. The kernel uses
    /// the mode only for a file this open creates, less the umask.
    #[cfg(unix)]
    pub fn open_with_mode(
        self,
        path: impl AsRef<std::path::Path>,
        mode: u32,
    ) -> std::io::Result<std::fs::File> {
        use rustix::fs::{Mode, OFlags};
        let flags = OFlags::from_bits_retain(self.0 as _) | OFlags::CLOEXEC;
        // Retried while a signal interrupts it, as std's open and libuv's are.
        let fd = rustix::io::retry_on_intr(|| {
            rustix::fs::open(path.as_ref(), flags, Mode::from_raw_mode(mode as _))
        })
        .map_err(io_from_errno)?;
        Ok(std::fs::File::from(fd))
    }

    /// libuv's fs__open (src/win/fs.c, v1.51.0): ONE CreateFileW whose
    /// access, creation disposition and attributes are worked out from the
    /// bits the way libuv works them out. std's OpenOptions cannot make the
    /// call -- it has no creation disposition to set, and its truncating open
    /// is an open and then a truncation -- and node's answers are this
    /// call's (v22.22.2, measured; conformance cases 394 and 398):
    ///
    /// - `O_CREAT | O_TRUNC` is CREATE_ALWAYS, which REPLACES the file: a
    ///   named pipe opens (#320), a hidden or system file is refused (#321),
    ///   the file's streams go and its attributes are reset;
    /// - `O_TRUNC` without `O_CREAT` is TRUNCATE_EXISTING, which CreateFileW
    ///   refuses for an access mask without the GENERIC_WRITE bit itself:
    ///   node's `EINVAL`;
    /// - `O_APPEND` swaps FILE_WRITE_DATA for FILE_APPEND_DATA, whatever the
    ///   access mode, so even `O_RDONLY | O_APPEND` writes at the end;
    /// - FILE_FLAG_BACKUP_SEMANTICS on every open, without which Windows
    ///   opens no directory;
    /// - a combination libuv has no case for (both access bits; both of
    ///   sequential and random) is `EINVAL` before any call.
    ///
    /// Two things are not libuv's:
    ///
    /// - the open's `mode`: libuv adds FILE_ATTRIBUTE_READONLY for a creating
    ///   open whose mode lacks the owner's write bit (#324);
    /// - UV_FS_O_FILEMAP opens as libuv opens it -- a write-only open asks
    ///   read access too, and a handle without standard information (a
    ///   console, the pipe namespace) fails the open -- but no mapping is
    ///   made: libuv does its reads and writes through one, emulating append
    ///   and the access mode there, and opens an append-mode file read-write
    ///   without the append swap; here it is an ordinary handle, which
    ///   differs only together with O_APPEND or O_DIRECT
    ///   (docs/node-divergences.md).
    #[cfg(windows)]
    pub fn open_with_mode(
        self,
        path: impl AsRef<std::path::Path>,
        mode: u32,
    ) -> std::io::Result<std::fs::File> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CREATE_ALWAYS, CREATE_NEW, CreateFileW, DELETE, FILE_APPEND_DATA,
            FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_TEMPORARY,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_NO_BUFFERING,
            FILE_FLAG_RANDOM_ACCESS, FILE_FLAG_SEQUENTIAL_SCAN, FILE_FLAG_WRITE_THROUGH,
            FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FILE_WRITE_DATA, OPEN_ALWAYS, OPEN_EXISTING, TRUNCATE_EXISTING,
        };
        let path = path.as_ref();
        let invalid = || std::io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as i32);
        refuse_a_root_that_is_no_disk(path)?;
        let bits = self.0;
        let has = |flag: i32| bits & flag != 0;

        let mut access = match bits & (o_flag::WRONLY | o_flag::RDWR) {
            o_flag::RDONLY => FILE_GENERIC_READ,
            // A mapping is always read: libuv opens a write-only
            // UV_FS_O_FILEMAP file read-write (a console then refuses the
            // open EINVAL, as it refuses any read-write open).
            o_flag::WRONLY if has(o_flag::FILEMAP) => FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            o_flag::WRONLY => FILE_GENERIC_WRITE,
            o_flag::RDWR => FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            _ => return Err(invalid()),
        };
        if has(o_flag::APPEND) {
            access &= !FILE_WRITE_DATA;
            access |= FILE_APPEND_DATA;
        }
        let share = if has(o_flag::EXLOCK) {
            0
        } else {
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        };
        let disposition = match (has(o_flag::CREAT), has(o_flag::EXCL), has(o_flag::TRUNC)) {
            (false, _, false) => OPEN_EXISTING,
            (true, false, false) => OPEN_ALWAYS,
            (true, true, _) => CREATE_NEW,
            (false, _, true) => TRUNCATE_EXISTING,
            (true, false, true) => CREATE_ALWAYS,
        };
        let mut attributes = FILE_ATTRIBUTE_NORMAL;
        // The one thing a mode says on Windows: an open that may create its
        // file, with no owner-write bit left once the umask is taken off,
        // asks for a read-only one (libuv's fs__open). CreateFileW applies
        // the attribute to a file it creates or (for CREATE_ALWAYS)
        // replaces, and ignores it for one it merely opens.
        if has(o_flag::CREAT) && creates_read_only(mode, process_umask(None)) {
            attributes |= FILE_ATTRIBUTE_READONLY;
        }
        if has(o_flag::TEMPORARY) {
            attributes |= FILE_FLAG_DELETE_ON_CLOSE | FILE_ATTRIBUTE_TEMPORARY;
            access |= DELETE;
        }
        if has(o_flag::SHORT_LIVED) {
            attributes |= FILE_ATTRIBUTE_TEMPORARY;
        }
        match (has(o_flag::SEQUENTIAL), has(o_flag::RANDOM)) {
            (false, false) => {}
            (true, false) => attributes |= FILE_FLAG_SEQUENTIAL_SCAN,
            (false, true) => attributes |= FILE_FLAG_RANDOM_ACCESS,
            (true, true) => return Err(invalid()),
        }
        if has(o_flag::DIRECT) {
            // Unbuffered I/O and an append-only handle do not go together:
            // libuv gives up the append where the handle can also write at
            // an offset, and refuses the open where it cannot.
            if access & FILE_APPEND_DATA != 0 {
                if access & FILE_WRITE_DATA != 0 {
                    access &= !FILE_APPEND_DATA;
                } else {
                    return Err(invalid());
                }
            }
            attributes |= FILE_FLAG_NO_BUFFERING;
        }
        match (has(o_flag::DSYNC), has(o_flag::SYNC)) {
            (false, false) => {}
            (true, true) => return Err(invalid()),
            _ => attributes |= FILE_FLAG_WRITE_THROUGH,
        }
        attributes |= FILE_FLAG_BACKUP_SEMANTICS;

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        // CreateFileW reads a name up to its first NUL, and would open the file
        // named by what comes before one; std refuses such a path the same way.
        if wide.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "file name contained an unexpected NUL byte",
            ));
        }
        wide.push(0);
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call;
        // the security attributes and the template handle are null, which
        // CreateFileW documents as optional, and every other argument is a
        // plain value.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                share,
                std::ptr::null(),
                disposition,
                attributes,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `handle` is the open handle CreateFileW just returned (it is
        // not INVALID_HANDLE_VALUE), nothing else has seen it, and the File takes
        // the one ownership of it. It was not opened FILE_FLAG_OVERLAPPED, which
        // is what File's synchronous reads and writes need.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        let file = as_the_crt_takes_it(file)?;
        // libuv sizes a UV_FS_O_FILEMAP file for its mapping by asking its
        // standard information, and fails the open with that call's error:
        // a console has none (ERROR_INVALID_FUNCTION, EISDIR), the pipe
        // namespace refuses it (ERROR_INVALID_PARAMETER, EINVAL).
        if has(o_flag::FILEMAP) {
            ops::file_info::<windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO>(&file)?;
        }
        Ok(file)
    }

    #[cfg(not(any(unix, windows)))]
    pub fn open_with_mode(
        self,
        path: impl AsRef<std::path::Path>,
        _mode: u32,
    ) -> std::io::Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        options.open(path)
    }
}

/// `fs.readFile` of a path: `std::fs::read`, with the open and the read kept
/// apart. Same syscalls on success: open, one fstat for the size hint, one
/// read of the whole file and one short read that finds EOF.
pub fn read_whole_file(path: &str) -> Result<Vec<u8>, WholeFileError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    let file = open_path(opens_as_libuv(&mut options), path).map_err(WholeFileError::Open)?;
    let size = file.metadata().ok().map(|m| m.len());
    read_opened(&file, size)
}

/// Read everything from an already-open file, `size` being the caller's
/// fstat answer. Every failure here is the read half's.
///
/// This is `std::fs::read`'s hinted read loop: reserve exactly `size`, read
/// into all of that room in one call, and once it is full make one 32-byte
/// probe read that finds EOF (or, when the file grew, more bytes, after which
/// the buffer grows and the loop goes on). A regular file comes in with two
/// read calls whatever its size. Neither of std's `read_to_end`s gives that
/// here: the generic one has no size hint, so it caps its first read at 8 KiB
/// and doubles from there (nine reads for 1 MiB), and `File`'s re-derives the
/// hint with an fstat and a seek. The one difference from std: the room is
/// zero-filled once before the read, because std reads into uninitialised
/// memory with `unsafe` that oam_core does not use.
fn read_opened(
    mut reader: impl std::io::Read,
    size: Option<u64>,
) -> Result<Vec<u8>, WholeFileError> {
    const PROBE: usize = 32;
    const GROW: usize = 8 * 1024;
    // std::fs::read's own answer when the buffer cannot be had.
    let out_of_memory = |e: std::collections::TryReserveError| {
        WholeFileError::Transfer(std::io::Error::new(std::io::ErrorKind::OutOfMemory, e))
    };
    let mut bytes = Vec::new();
    if let Some(size) = size.and_then(|n| usize::try_from(n).ok()) {
        bytes.try_reserve_exact(size).map_err(out_of_memory)?;
    }
    // bytes[..filled] is the file so far; bytes[filled..] is zeroed room.
    let mut filled = 0;
    loop {
        if filled == bytes.capacity() {
            // Full: the size hint was right (or absent). Probe before growing.
            let mut probe = [0u8; PROBE];
            let n = match reader.read(&mut probe) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(WholeFileError::Transfer(e)),
            };
            bytes.truncate(filled);
            bytes.try_reserve(GROW.max(n)).map_err(out_of_memory)?;
            bytes.extend_from_slice(&probe[..n]);
            filled += n;
            continue;
        }
        // Zero only room no earlier pass zeroed, so a short read costs nothing.
        bytes.resize(bytes.capacity(), 0);
        match reader.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(WholeFileError::Transfer(e)),
        }
    }
    bytes.truncate(filled);
    Ok(bytes)
}

/// `fs.writeFile` (`append == false`: `std::fs::write`'s create + truncate)
/// or `fs.appendFile` (create + append) of a path, with the open and the write
/// kept apart. `mode` is the open's (`OpenFlags::open_with_mode`): what the
/// file gets if this creates it.
pub fn write_whole_file(
    path: &str,
    data: &[u8],
    append: bool,
    mode: u32,
) -> Result<(), WholeFileError> {
    let file = open_options_for(if append { "a" } else { "w" })
        .open_with_mode(path, mode)
        .map_err(WholeFileError::Open)?;
    write_opened(&file, data)
}

/// Write all of `data` to an already-open file: the write half.
fn write_opened(mut writer: impl std::io::Write, data: &[u8]) -> Result<(), WholeFileError> {
    writer.write_all(data).map_err(WholeFileError::Transfer)
}

/// What node reports for a failed whole-file operation at `site`
/// (`FsSite::ReadFile`, `WriteFile` or `AppendFile`): an open failure goes
/// through `fs_error_at` as syscall `open` with the path; a read or write
/// failure is the descriptor's, as libuv's fs__read / fs__write report it --
/// `fd_error_code`, syscall `read` (readFile) or `write`, no path.
pub fn whole_file_error(site: FsSite, path: &str, error: &WholeFileError) -> FsError {
    match error {
        WholeFileError::Open(e) => fs_error_at(site, "open", path, e),
        WholeFileError::Transfer(e) => FsError {
            code: fd_error_code(e),
            syscall: if site == FsSite::ReadFile {
                "read"
            } else {
                "write"
            },
            has_path: false,
        },
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

/// Node-style error message for a TWO-path operation (rename, copyfile,
/// link, symlink): "ENOENT: no such file or directory, rename 'a' -> 'b'".
/// The error carries the second path as `dest` too.
pub fn node_error_message_dest(
    code: &str,
    syscall: &str,
    path: &str,
    dest: &str,
    error: &std::io::Error,
) -> String {
    let reason = node_error_reason(code, error);
    format!("{code}: {reason}, {syscall} '{path}' -> '{dest}'")
}

/// The path a filesystem error names, for a path as the caller passed it:
/// `fs_shown_path(&fs_os_path(path))`.
///
/// On Windows node's binding resolves every path before libuv sees it
/// (`ToNamespacedPath`: `PathResolve`, then the `\\?\` long-path prefix),
/// and the error reports that path with the prefix taken back off
/// (`StringFromPath`): `fs.statSync("x")` fails with `stat 'C:\cwd\x'`,
/// `mkdirSync("a/b")` with `mkdir 'C:\cwd\a\b'`. An empty path is left alone
/// (`fs.openSync("")` fails `open ''`), and so is one that resolves to two
/// bytes or fewer, as `ToNamespacedPath` leaves those. Elsewhere the path
/// is reported as passed.
///
/// Only for a path as the caller passed it. A path that is already the OS
/// path (an `FsPath`) is shown with `fs_shown_path` alone: resolving it again
/// is not the same thing (`\\?\C:\` resolves to the volume `\\?\C:`, so
/// `mkdirSync("C:\\")` would report `'C:'` where node reports `'C:\'`).
///
/// Not for `mkdtemp`, whose template node passes to libuv unresolved, or a
/// symlink's target, which is stored as written.
pub fn fs_error_path(path: &str) -> std::borrow::Cow<'_, str> {
    match fs_os_path(path) {
        std::borrow::Cow::Borrowed(os) => fs_shown_path(os),
        std::borrow::Cow::Owned(os) => std::borrow::Cow::Owned(fs_shown_path(&os).into_owned()),
    }
}

/// The path node's fs binding hands libuv for `path` on this platform: on
/// Windows `fs_os_path_with` with the process's real current directory and
/// the `=X:` per-drive directories, as `ToNamespacedPath` reads them (node's
/// C++ never sees a patched `process.cwd`); elsewhere `path` itself, borrowed.
pub fn fs_os_path(path: &str) -> std::borrow::Cow<'_, str> {
    #[cfg(windows)]
    {
        fs_os_path_with(path, real_cwd, real_drive_cwd, Win32ResolveMode::Cpp)
    }
    #[cfg(not(windows))]
    std::borrow::Cow::Borrowed(path)
}

/// node's `ToNamespacedPath` (src/path.cc, v22.22.2), the same steps as
/// lib/path.js `win32.toNamespacedPath`, on any host: an empty path is left
/// alone; otherwise the path is resolved (`win32_resolve_mode`), and a result
/// of two bytes or fewer leaves the path as passed (`Q:`, `Q:.`); a
/// UNC result (`\\srv\sh\x`) becomes `\\?\UNC\srv\sh\x`, a drive-absolute one
/// (`C:\x`) `\\?\C:\x`, and any other result (a `\\?\` or `\\.\` path, a
/// drive-relative `Z:a` the resolve could not anchor) is used as resolved.
///
/// `cwd` is called only when the resolve needs the current directory, and
/// `drive_cwd(device)` only for a drive-relative path. `mode` is
/// `Win32ResolveMode::Cpp` for what the fs binding does. Measured on node
/// v22.22.2 with the cwd `C:\Windows`: `x` gives `\\?\C:\Windows\x`, `C:\`
/// gives `\\?\C:\`, `\\srv\sh` gives `\\?\UNC\srv\sh\`, and `\\?\C:\` gives
/// `\\?\C:` (the volume, not its root directory), so this is not idempotent
/// on a root and an OS path must never be passed through it again.
pub fn fs_os_path_with(
    path: &str,
    cwd: impl Fn() -> String,
    drive_cwd: impl Fn(&str) -> Option<String>,
    mode: Win32ResolveMode,
) -> std::borrow::Cow<'_, str> {
    if path.is_empty() {
        return std::borrow::Cow::Borrowed(path);
    }
    let resolved = win32_resolve_mode(path, cwd, drive_cwd, mode);
    // The C++ compares the UTF-8 byte length, not UTF-16 units.
    let bytes = resolved.as_bytes();
    if bytes.len() <= 2 {
        return std::borrow::Cow::Borrowed(path);
    }
    if bytes[0] == b'\\' {
        if bytes[1] == b'\\' && bytes[2] != b'?' && bytes[2] != b'.' {
            return std::borrow::Cow::Owned(format!(r"\\?\UNC\{}", &resolved[2..]));
        }
    } else if bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        return std::borrow::Cow::Owned(format!(r"\\?\{resolved}"));
    }
    std::borrow::Cow::Owned(resolved)
}

/// The path an error shows for an OS path (one from `fs_os_path`), on this
/// platform: on Windows `win32_shown_path`, elsewhere `os` itself, borrowed.
pub fn fs_shown_path(os: &str) -> std::borrow::Cow<'_, str> {
    #[cfg(windows)]
    {
        win32_shown_path(os)
    }
    #[cfg(not(windows))]
    std::borrow::Cow::Borrowed(os)
}

/// node's `StringFromPath` (src/node_file.cc, v22.22.2) on any host: the
/// prefix `ToNamespacedPath` added is taken off the exact string handed to
/// libuv, and nothing else is done to it. `\\?\UNC\srv\sh\x` shows as
/// `\\srv\sh\x`; any other `\\?\` is dropped (`\\?\C:\x` shows `C:\x`,
/// `\\?\GLOBALROOT\x` shows `GLOBALROOT\x`, `\\?\` shows the empty string);
/// `\\.\` paths and the rest show as they are. `UNC` is matched case-sensitively:
/// node shows `\\?\unc\srv\sh` as `unc\srv\sh`.
pub fn win32_shown_path(os: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = os.strip_prefix(r"\\?\UNC\") {
        std::borrow::Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = os.strip_prefix(r"\\?\") {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Borrowed(os)
    }
}

/// A filesystem op's path once it is the path node's binding would hand
/// libuv (`fs_os_path`). Holding one instead of a `String` keeps the two
/// spellings apart: the op opens `os()`, and its errors show `shown()`, the
/// OS path with the prefix taken off -- never the OS path resolved again,
/// which is not the same path on a root (see `fs_os_path_with`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsPath {
    os: String,
}

impl FsPath {
    /// The OS path for `path` as the caller passed it. Off Windows this is
    /// `path` itself, moved in.
    pub fn new(path: String) -> Self {
        let os = match fs_os_path(&path) {
            std::borrow::Cow::Owned(os) => Some(os),
            std::borrow::Cow::Borrowed(_) => None,
        };
        Self {
            os: os.unwrap_or(path),
        }
    }

    /// The path to hand the OS.
    pub fn os(&self) -> &str {
        &self.os
    }

    /// A path that is already an OS path: a piece of one `os` gave.
    pub fn from_os(os: String) -> Self {
        Self { os }
    }

    /// The OS path node's `readdir` hands libuv's scandir. On Windows that
    /// is the one path a trailing separator survives on: `readdirSync("f\\")`
    /// fails `ENOENT: ..., scandir 'C:\cwd\f\'` where `f` is a file, though
    /// `statSync("f\\")` and `openSync("f\\")` answer for `f` (v22.22.2,
    /// measured: `f\`, `f/` and `f\\` keep one separator, `f\.` keeps
    /// none). `opendir` does not keep it. The code is the open's, so it is
    /// not `ENOENT` everywhere: under a directory itself reached through a
    /// junction or symlink the same open succeeds, and the answer is `ENOTDIR`
    /// in both runtimes.
    pub fn for_scandir(path: String) -> Self {
        let trailing = cfg!(windows) && path.ends_with(['\\', '/']);
        let mut found = Self::new(path);
        if trailing && !found.os.is_empty() && !found.os.ends_with('\\') {
            found.os.push('\\');
        }
        found
    }

    /// The path an error names: `fs_shown_path` of the OS path.
    pub fn shown(&self) -> std::borrow::Cow<'_, str> {
        fs_shown_path(&self.os)
    }

    /// The OS path, owned.
    pub fn into_os(self) -> String {
        self.os
    }
}

impl AsRef<std::path::Path> for FsPath {
    fn as_ref(&self) -> &std::path::Path {
        std::path::Path::new(&self.os)
    }
}

/// The process's real current directory as node's C++ reads it, for the
/// Windows resolves (`fs_os_path`): never a patched `process.cwd`.
#[cfg(windows)]
fn real_cwd() -> String {
    std::env::current_dir()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The per-drive current directory Windows keeps in `=X:`.
#[cfg(windows)]
fn real_drive_cwd(device: &str) -> Option<String> {
    std::env::var(format!("={device}")).ok()
}

/// The target string a symlink stores, as node's fs hands it to the binding
/// for a file or directory link (`preprocessSymlinkDestination` in
/// lib/internal/fs/utils.js, v22.22.2): on Windows an absolute target
/// (`path.win32.isAbsolute`: a separator first, or `X:` and a separator) is
/// namespaced as `path.toNamespacedPath` does (`\\?\C:\x`, `\\?\UNC\srv\sh\x`;
/// a rooted `\x` lands on the cwd's drive), and a relative one, which stays
/// relative to the link, only has `/` turned into `\` -- `..` and `.` are
/// kept. Elsewhere the target as given. A symlink error shows this string
/// strip-only (`fs_shown_path`), as node's does.
pub fn symlink_target_os(target: &str) -> std::borrow::Cow<'_, str> {
    #[cfg(windows)]
    {
        win32_symlink_target(target, real_cwd, real_drive_cwd)
    }
    #[cfg(not(windows))]
    std::borrow::Cow::Borrowed(target)
}

/// `symlink_target_os` on Windows, with the cwd and `=X:` lookups given.
pub fn win32_symlink_target(
    target: &str,
    cwd: impl Fn() -> String,
    drive_cwd: impl Fn(&str) -> Option<String>,
) -> std::borrow::Cow<'_, str> {
    let b = target.as_bytes();
    let sep = |c: u8| c == b'/' || c == b'\\';
    let absolute = !b.is_empty()
        && (sep(b[0]) || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && sep(b[2])));
    if absolute {
        fs_os_path_with(target, cwd, drive_cwd, Win32ResolveMode::Js)
    } else if target.contains('/') {
        std::borrow::Cow::Owned(target.replace('/', "\\"))
    } else {
        std::borrow::Cow::Borrowed(target)
    }
}

/// `fs.constants.COPYFILE_EXCL`: fail if the destination exists.
pub const COPYFILE_EXCL: u32 = 1;
/// `fs.constants.COPYFILE_FICLONE_FORCE`: clone or fail.
pub const COPYFILE_FICLONE_FORCE: u32 = 4;

/// `fs.copyFile` with node's `mode` bits (already checked to be 0..7), as
/// libuv's uv_fs_copyfile treats them:
///
/// - `COPYFILE_FICLONE_FORCE` on Windows is `ENOSYS` before anything is
///   touched: libuv's Windows copy has no clone. Elsewhere it copies, as
///   `COPYFILE_FICLONE` (try a clone, fall back) always does here.
/// - `COPYFILE_EXCL` fails `EEXIST` when the destination exists, as
///   CopyFileW with fail-if-exists and libuv's Unix `O_EXCL` open do. The
///   source is looked at first, so a missing source is `ENOENT` even when
///   the destination exists; the destination is then claimed with a
///   create-new open, so a destination made meanwhile is not overwritten,
///   and the claim is dropped again if the copy fails.
pub fn copy_file(from: &FsPath, to: &FsPath, mode: u32) -> std::io::Result<()> {
    #[cfg(windows)]
    if mode & COPYFILE_FICLONE_FORCE != 0 {
        // No raw OS code to carry: the kind gives ENOSYS, the text uv_strerror's.
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "function not implemented",
        ));
    }
    if mode & COPYFILE_EXCL == 0 {
        return std::fs::copy(from, to).map(|_| ());
    }
    std::fs::metadata(from)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    std::fs::copy(from, to).map(|_| ()).inspect_err(|_| {
        let _ = std::fs::remove_file(to);
    })
}

/// The link type a `symlink` call asked for, as node's JS hands it over:
/// `Probe` when it gave none (on Windows node then stats the target to
/// choose), otherwise `file`, `dir` or `junction`. Off Windows the type
/// changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymlinkType {
    Probe,
    File,
    Dir,
    Junction,
}

impl SymlinkType {
    /// The type from the op's argument: `None` is `Probe`, and anything
    /// that is not `dir` or `junction` is a file link (node's JS has already
    /// refused any other string).
    pub fn from_arg(arg: Option<&str>) -> Self {
        match arg {
            None => SymlinkType::Probe,
            Some("dir") => SymlinkType::Dir,
            Some("junction") => SymlinkType::Junction,
            Some(_) => SymlinkType::File,
        }
    }
}

/// The target string a link of `kind` stores (`preprocessSymlinkDestination`):
/// a junction's target is resolved against the link's parent and namespaced
/// (`toNamespacedPath(path.resolve(link, '..', target))`), any other's is
/// `symlink_target_os`. oam has no junction call of its own and makes a
/// junction as a directory symlink to that target.
pub fn symlink_stored_target(link: &str, target: &str, kind: SymlinkType) -> String {
    #[cfg(windows)]
    if kind == SymlinkType::Junction {
        return symlink_target_os(&symlink_probe_path(link, target)).into_owned();
    }
    let _ = (link, kind);
    symlink_target_os(target).into_owned()
}

/// The path node's Windows `fs.symlink` stats to choose a directory or a
/// file link when no type is given: `path.resolve(link, '..', target)`, the
/// target taken relative to the link's parent directory, not the cwd.
#[cfg(windows)]
pub fn symlink_probe_path(link: &str, target: &str) -> String {
    win32_symlink_probe(link, target, real_cwd, real_drive_cwd)
}

/// `symlink_probe_path` with the cwd and `=X:` lookups given.
pub fn win32_symlink_probe(
    link: &str,
    target: &str,
    cwd: impl Fn() -> String,
    drive_cwd: impl Fn(&str) -> Option<String>,
) -> String {
    win32_resolve_all_mode(&[link, "..", target], cwd, drive_cwd, Win32ResolveMode::Js)
}

/// The template node's binding hands libuv for `mkdtemp(prefix)` (src/
/// node_file.cc `Mkdtemp`, v22.22.2): the prefix as given -- NOT resolved,
/// not joined to any temp directory -- with the `XXXXXX` libuv replaces.
///
/// The binding appends the X's with `snprintf(out + len, len + 6, "%s",
/// "XXXXXX")`, whose size bound is the length of the WHOLE buffer, so an
/// empty prefix gets only five X's, which libuv refuses on Windows and glibc (macOS's
/// mkdtemp(3) fills them, see `MKDTEMP_FILLS_X_RUN`): node's
/// `mkdtempSync("")` fails `EINVAL: invalid argument, mkdtemp 'XXXXX'`. That
/// quirk is reproduced here rather than in `mkdtemp`, so the permission check
/// and the error name the same template node's do.
pub fn mkdtemp_template(prefix: &str) -> String {
    let x_count = if prefix.is_empty() { 5 } else { 6 };
    let mut template = String::with_capacity(prefix.len() + 6);
    template.push_str(prefix);
    template.push_str(&"XXXXXX"[..x_count]);
    template
}

/// What libuv's mkdtemp puts in place of the template's X's: characters of
/// [a-zA-Z0-9], the alphabet of libuv's Windows `fs__make_tmp`, glibc's
/// `__gen_tempname` and Darwin's `_gettemp` (`padchar`) alike.
const MKDTEMP_CHARS: &[u8; 62] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// How many base-62 characters one 64-bit draw spells: 62^10 < 2^64 < 62^11.
/// Six X's (every platform but macOS, and macOS for a prefix not ending in
/// X) still take exactly one draw per name.
const MKDTEMP_CHARS_PER_DRAW: usize = 10;

/// Whether this platform's mkdtemp replaces the template's WHOLE run of
/// trailing X's rather than exactly the last six. libuv's Unix
/// `uv__fs_mkdtemp` hands the template to the libc's mkdtemp(3) as given,
/// and Darwin's (`_gettemp`, Libc's FreeBSD-derived `mktemp.c`) fills
/// `while (trv >= path && *trv == 'X')` with no minimum: there node's
/// `mkdtempSync("aX")` (template `aXXXXXXX`) makes `a` plus seven random
/// characters, and the five-X template of an empty prefix is accepted.
/// node's fs docs say as much ("some platforms, notably the BSDs, can return
/// more than six random characters, and replace trailing X characters in
/// prefix"). glibc and libuv's Windows loop replace exactly six and refuse
/// fewer. Derived from source, not run on a Mac: conformance cases 302
/// (empty prefix) and 303 (a prefix of X's) measure it on darwin.
const MKDTEMP_FILLS_X_RUN: bool = cfg!(target_os = "macos");

/// How many names mkdtemp tries before giving up: the platform's `TMP_MAX`,
/// as libuv's Windows loop and glibc's both count -- 32767 in the MSVC CRT,
/// 62^3 in glibc. macOS's libc keeps trying past that; a run of 238328
/// collisions is not a case anyone reaches.
#[cfg(windows)]
const MKDTEMP_TRIES: u32 = 32767;
#[cfg(not(windows))]
const MKDTEMP_TRIES: u32 = 238_328;

/// node's `mkdtemp` (libuv `uv_fs_mkdtemp`), the one implementation the sync
/// op and the async op both run: `template` (from `mkdtemp_template`) must
/// end in `XXXXXX` or the call fails EINVAL (on macOS it need only be
/// non-empty); each try replaces those six characters (on macOS the whole
/// trailing run of X's, see `MKDTEMP_FILLS_X_RUN`) with fresh ones from the
/// OS CSPRNG -- 64-bit draws spelled in base 62, libuv's Windows scheme --
/// and creates that directory, trying again only when the name already
/// exists. The directory is made 0700 on Unix, as mkdtemp(3) makes it.
///
/// Ok is the created path: the template with its X's replaced, separators and
/// relativity exactly as given (node returns `sub/x-AbC123` for `sub/x-`).
/// Err carries the path libuv's request holds afterwards (`req->path`), which
/// is what node's ASYNC error names. On Windows (`fs__mktemp`) libuv writes
/// the name back only on success, so a failed CreateDirectoryW leaves the
/// template (`mkdtemp 'nope/x-XXXXXX'`), and a template without six X's, a
/// failed RtlGenRandom or running out of tries "clobbers" it to the empty
/// string (`mkdtemp ''`). On Unix mkdtemp(3) fills libuv's copy in place
/// before creating, so a failed create leaves the last name tried, and a
/// template it refuses is left untouched. node's SYNC error names
/// the template on every platform (`FSReqWrapSync::path_p` is the binding's
/// own buffer, which libuv never writes), so the sync op ignores this path.
/// Running out of tries fails EEXIST on both (glibc's answer, and on Windows
/// the ERROR_ALREADY_EXISTS still in GetLastError); at one collision in 62^6
/// per try, nothing gets that far.
pub fn mkdtemp(template: &str) -> Result<String, (std::io::Error, String)> {
    // libuv reports a failed RtlGenRandom as EIO; io::Error::other is what
    // node_error_code reads as EIO.
    mkdtemp_drawing(template, MKDTEMP_TRIES, MKDTEMP_FILLS_X_RUN, || {
        getrandom::u64().map_err(|e| std::io::Error::other(e.to_string()))
    })
}

/// `mkdtemp` over a given number of tries, either X rule (`fills_x_run`:
/// macOS's), and a given source of 64-bit draws, so the tests can force
/// collisions and run both rules on any host.
fn mkdtemp_drawing(
    template: &str,
    tries: u32,
    fills_x_run: bool,
    mut next_draw: impl FnMut() -> std::io::Result<u64>,
) -> Result<String, (std::io::Error, String)> {
    let x_run = template.bytes().rev().take_while(|&b| b == b'X').count();
    let refused = if fills_x_run {
        template.is_empty()
    } else {
        x_run < 6
    };
    if refused {
        let e = std::io::Error::from(std::io::ErrorKind::InvalidInput);
        return Err((e, mkdtemp_refused_path(template)));
    }
    let fill = if fills_x_run { x_run } else { 6 };
    // Darwin tries a template with no X's once: it has no permutation to
    // cycle through and fails EEXIST. (node's templates always end in X.)
    let tries = if fill == 0 { 1 } else { tries };
    // The X's are ASCII, so this is a char boundary.
    let stem = &template[..template.len() - fill];
    let mut path = String::with_capacity(template.len());
    let mut last_error = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    for _ in 0..tries {
        path.clear();
        path.push_str(stem);
        let mut draw = 0;
        for i in 0..fill {
            if i % MKDTEMP_CHARS_PER_DRAW == 0 {
                draw = next_draw().map_err(|e| (e, mkdtemp_refused_path(template)))?;
            }
            path.push(char::from(MKDTEMP_CHARS[(draw % 62) as usize]));
            draw /= 62;
        }
        match create_mkdtemp_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_error = e,
            Err(e) => return Err((e, mkdtemp_failed_path(template, &path))),
        }
    }
    let exhausted = if cfg!(windows) { String::new() } else { path };
    Err((last_error, exhausted))
}

/// A failed `mkdir`: the error, and the path it was for -- the one asked
/// for, or for a recursive mkdir the component that could not be made.
#[derive(Debug)]
pub struct MkdirFailure {
    pub error: std::io::Error,
    pub at: String,
}

/// mkdir(2) with `mode`, which the kernel takes the process's mask off. On
/// Windows there is no mode to give, as with libuv's.
fn mkdir_one(path: &str, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(mode).create(path)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::create_dir(path)
    }
}

/// `fs.mkdir` of `path` (an OS path: `FsPath::os`).
///
/// Recursive, it is node's MKDirpSync (src/node_file.cc, v22.22.2), kept
/// step for step because what it reports is a matter of its steps: it
/// tries the path itself, climbs a parent at a time while the answer is
/// ENOENT -- cutting the string at its last separator, so the pieces are
/// the caller's own spelling -- and comes back down. Every directory it
/// makes gets `mode`; one already there is left as it is. The answer is the
/// first directory THIS call made (`None` when all were there), which is
/// why it is not a walk to the first existing ancestor: a directory another
/// process makes in between is not one this call made. A component that is
/// a file is ENOTDIR, the path itself being one is EEXIST.
///
/// `asynchronous` is for the one place node's two walks part ways: a
/// component that is there but cannot be looked at (a link to nothing) is
/// the stat's own error from MKDirpSync and ENOTDIR from MKDirpAsync.
pub fn make_dir(
    path: &str,
    mode: u32,
    recursive: bool,
    asynchronous: bool,
) -> Result<Option<String>, MkdirFailure> {
    let fail = |error: std::io::Error, at: &str| MkdirFailure {
        error,
        at: at.to_string(),
    };
    if !recursive {
        return mkdir_one(path, mode)
            .map(|()| None)
            .map_err(|e| fail(e, path));
    }
    let mut first: Option<String> = None;
    let mut stack = vec![path.to_string()];
    // Every turn makes a directory, ends, or climbs, and a component is
    // tried twice at most -- on the way up and on the way down. The bound
    // is for a file system that will not have it so (a directory made that
    // then is not there to make another in), where node's walk never ends.
    let bound = 2 * path.matches(std::path::MAIN_SEPARATOR).count() + 16;
    let mut turns = 0usize;
    while let Some(next) = stack.pop() {
        turns += 1;
        let error = match mkdir_one(&next, mode) {
            Ok(()) => {
                first.get_or_insert(next);
                continue;
            }
            Err(error) => error,
        };
        // As libuv's mkdir reports it: a name Windows cannot have is EINVAL
        // there, not the ENOENT that would send this climbing.
        let code = fs_error_at(FsSite::Mkdir { recursive: false }, "mkdir", &next, &error).code;
        match code {
            "EACCES" | "ENOSPC" | "ENOTDIR" | "EPERM" => return Err(fail(error, &next)),
            "ENOENT" if turns < bound => {
                if let Some(cut) = next.rfind(std::path::MAIN_SEPARATOR) {
                    let parent = next[..cut].to_string();
                    stack.push(next);
                    stack.push(parent);
                    continue;
                }
                // Nothing above it to make: the stat below says why.
            }
            _ => {}
        }
        // A component of the path, with more of it still to make, rather
        // than the path itself.
        let component = code == "EEXIST" && !stack.is_empty();
        // With the OS's own number on unix, where the errno node reports is
        // read off the error.
        let in_the_way = |component: bool| {
            #[cfg(unix)]
            let error = io_from_errno(if component {
                rustix::io::Errno::NOTDIR
            } else {
                rustix::io::Errno::EXIST
            });
            #[cfg(not(unix))]
            let error = std::io::Error::from(if component {
                std::io::ErrorKind::NotADirectory
            } else {
                std::io::ErrorKind::AlreadyExists
            });
            error
        };
        // The stat node's (libuv's) would make: on Windows a path that goes
        // on past a looping link is ENOENT there, where std's says ELOOP.
        let is_dir = std::fs::metadata(&next).map(|meta| meta.is_dir());
        #[cfg(windows)]
        let is_dir = is_dir.or_else(|failed| {
            ops::as_libuv_stats(&next, false, failed).map(|stat| stat.kind() == ops::EntryKind::Dir)
        });
        match is_dir {
            // Already a directory: nothing to make here.
            Ok(true) => {}
            Ok(_) => return Err(fail(in_the_way(component), &next)),
            Err(_) if asynchronous && component => return Err(fail(in_the_way(true), &next)),
            Err(stat) => return Err(fail(stat, &next)),
        }
    }
    Ok(first)
}

#[cfg(unix)]
fn create_mkdtemp_dir(path: &str) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_mkdtemp_dir(path: &str) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

/// What libuv's request holds after `mkdtemp` refuses before creating
/// anything: Windows clobbers it to the empty string, Unix leaves the
/// template. See `mkdtemp`.
fn mkdtemp_refused_path(template: &str) -> String {
    if cfg!(windows) {
        String::new()
    } else {
        template.to_string()
    }
}

/// What libuv's request holds after a create fails other than EEXIST: the
/// template on Windows, the name tried on Unix. See `mkdtemp`.
fn mkdtemp_failed_path(template: &str, tried: &str) -> String {
    if cfg!(windows) {
        template.to_string()
    } else {
        tried.to_string()
    }
}

/// Which of node's two implementations of `path.win32.resolve` to follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Win32ResolveMode {
    /// lib/path.js `win32.resolve`, behind `path.resolve` and
    /// `path.toNamespacedPath`.
    Js,
    /// src/path.cc `PathResolve`, what node's fs binding runs before libuv.
    /// It differs in one rule. For a drive-relative path (`Z:a`) both read
    /// the drive's directory from `=Z:`, falling back to the current
    /// directory, and replace one on another drive with the drive's root
    /// `Z:\`; the JS takes "on another drive" to mean a `\` at index 2, the
    /// C++ a `/`. Measured on node v22.22.2 (cwd `C:\Windows`, a first
    /// `statSync('Z:a')` per drive): `=Z:` unset gives `Z:a` (the cwd is
    /// kept and then skipped as another drive's path, so the result stays
    /// drive-relative) where `path.win32.resolve` gives `Z:\a`;
    /// `=Z:=C:/elsewhere` gives `Z:\a` where the JS gives `Z:a`; `=Z:=Z:\d`
    /// gives `Z:\d\a` in both. The C++ also takes an empty fallback directory
    /// as the drive's root, where the JS skips it.
    Cpp,
}

/// node's `path.win32.resolve(path)` (lib/path.js): `cwd` is
/// `process.cwd()`, and `drive_cwd(device)` the per-drive current directory
/// Windows keeps in the `=C:` environment variables, for a drive-relative
/// path (`D:x`) on another drive. `win32_resolve_mode` with
/// `Win32ResolveMode::Js`.
///
/// Pure, so every rule is unit-tested on any host; only `fs_os_path` is
/// Windows-only.
pub fn win32_resolve(path: &str, cwd: &str, drive_cwd: impl Fn(&str) -> Option<String>) -> String {
    win32_resolve_mode(path, || cwd.to_string(), drive_cwd, Win32ResolveMode::Js)
}

/// `path.win32.resolve(path)` as lib/path.js (`Win32ResolveMode::Js`) or
/// src/path.cc `PathResolve` (`Win32ResolveMode::Cpp`) computes it. `cwd` is
/// called only when the path is not absolute with a device.
pub fn win32_resolve_mode(
    path: &str,
    cwd: impl Fn() -> String,
    drive_cwd: impl Fn(&str) -> Option<String>,
    mode: Win32ResolveMode,
) -> String {
    win32_resolve_all_mode(&[path], cwd, drive_cwd, mode)
}

/// `path.win32.resolve(...paths)` with several arguments, as
/// `win32_resolve_mode` resolves one: the arguments are taken right to left
/// until one is absolute with a device, then (`i === -1`) the cwd.
pub fn win32_resolve_all_mode(
    paths: &[&str],
    cwd: impl Fn() -> String,
    drive_cwd: impl Fn(&str) -> Option<String>,
    mode: Win32ResolveMode,
) -> String {
    let is_sep = |b: u8| b == b'/' || b == b'\\';
    let mut resolved_device = String::new();
    let mut resolved_tail = String::new();
    let mut resolved_absolute = false;
    // resolve's loop over its arguments, last first, then (`i === -1`) the
    // cwd -- or, once a device is known, that drive's own current directory.
    for step in 0..=paths.len() {
        let candidate = if step < paths.len() {
            paths[paths.len() - 1 - step].to_string()
        } else if resolved_device.is_empty() {
            cwd()
        } else {
            // `process.env['=Z:'] || process.cwd()`: an empty value is unset.
            let drive = drive_cwd(&resolved_device)
                .filter(|dir| !dir.is_empty())
                .unwrap_or_else(&cwd);
            // Not a directory on that drive: the drive's root instead. The
            // JS and the C++ test a different separator, see
            // `Win32ResolveMode::Cpp`.
            let other_drive = drive
                .get(..2)
                .is_none_or(|head| !head.eq_ignore_ascii_case(&resolved_device));
            let root = match mode {
                Win32ResolveMode::Js => other_drive && drive.as_bytes().get(2) == Some(&b'\\'),
                Win32ResolveMode::Cpp => {
                    drive.is_empty() || (other_drive && drive.as_bytes().get(2) == Some(&b'/'))
                }
            };
            if root {
                format!("{resolved_device}\\")
            } else {
                drive
            }
        };
        if candidate.is_empty() {
            continue;
        }
        let bytes = candidate.as_bytes();
        let len = bytes.len();
        let mut root_end = 0;
        let mut device = String::new();
        let mut is_absolute = false;
        if len == 1 {
            if is_sep(bytes[0]) {
                root_end = 1;
                is_absolute = true;
            }
        } else if is_sep(bytes[0]) {
            // A separator first: absolute, and possibly a UNC root.
            is_absolute = true;
            if is_sep(bytes[1]) {
                let mut j = 2;
                let mut last = j;
                while j < len && !is_sep(bytes[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    let first_part = &candidate[last..j];
                    last = j;
                    while j < len && is_sep(bytes[j]) {
                        j += 1;
                    }
                    if j < len && j != last {
                        last = j;
                        while j < len && !is_sep(bytes[j]) {
                            j += 1;
                        }
                        if j == len || j != last {
                            if first_part != "." && first_part != "?" {
                                device = format!(r"\\{first_part}\{}", &candidate[last..j]);
                                root_end = j;
                            } else {
                                // A device root (`\\.\PHYSICALDRIVE0`).
                                device = format!(r"\\{first_part}");
                                root_end = 4;
                            }
                        }
                    }
                }
            } else {
                root_end = 1;
            }
        } else if bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            device = candidate[..2].to_string();
            root_end = 2;
            if len > 2 && is_sep(bytes[2]) {
                is_absolute = true;
                root_end = 3;
            }
        }
        if !device.is_empty() {
            if resolved_device.is_empty() {
                resolved_device = device;
            } else if !device.eq_ignore_ascii_case(&resolved_device) {
                // A path on another device does not apply.
                continue;
            }
        }
        if resolved_absolute {
            if !resolved_device.is_empty() {
                break;
            }
        } else {
            resolved_tail = format!(r"{}\{resolved_tail}", &candidate[root_end..]);
            resolved_absolute = is_absolute;
            if is_absolute && !resolved_device.is_empty() {
                break;
            }
        }
    }
    let tail = normalize_win32_tail(&resolved_tail, !resolved_absolute);
    if resolved_absolute {
        format!(r"{resolved_device}\{tail}")
    } else {
        let joined = format!("{resolved_device}{tail}");
        if joined.is_empty() {
            ".".to_string()
        } else {
            joined
        }
    }
}

/// lib/path.js `normalizeString` with `\` as the separator: collapse `.` and
/// empty segments, apply `..` (above the root only when `allow_above_root`).
fn normalize_win32_tail(path: &str, allow_above_root: bool) -> String {
    let is_sep = |b: u8| b == b'/' || b == b'\\';
    let bytes = path.as_bytes();
    let mut res = String::new();
    let mut last_segment_length = 0usize;
    let mut last_slash: isize = -1;
    let mut dots: i32 = 0;
    let mut code = 0u8;
    // `res.length - 1 - res.lastIndexOf('\\')`, with lastIndexOf's -1.
    let segment_length = |res: &str| {
        (res.len() as isize - 1 - res.rfind('\\').map_or(-1, |at| at as isize)) as usize
    };
    for i in 0..=bytes.len() {
        if i < bytes.len() {
            code = bytes[i];
        } else if is_sep(code) {
            break;
        } else {
            code = b'/';
        }
        if is_sep(code) {
            if last_slash == i as isize - 1 || dots == 1 {
                // An empty or `.` segment.
            } else if dots == 2 {
                let ends_in_dotdot =
                    res.len() >= 2 && last_segment_length == 2 && res.ends_with("..");
                if !ends_in_dotdot {
                    if res.len() > 2 {
                        match res.rfind('\\') {
                            None => {
                                res.clear();
                                last_segment_length = 0;
                            }
                            Some(at) => {
                                res.truncate(at);
                                last_segment_length = segment_length(&res);
                            }
                        }
                        last_slash = i as isize;
                        dots = 0;
                        continue;
                    } else if !res.is_empty() {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i as isize;
                        dots = 0;
                        continue;
                    }
                }
                if allow_above_root {
                    res.push_str(if res.is_empty() { ".." } else { r"\.." });
                    last_segment_length = 2;
                }
            } else {
                if !res.is_empty() {
                    res.push('\\');
                }
                res.push_str(&path[(last_slash + 1) as usize..i]);
                last_segment_length = (i as isize - last_slash - 1) as usize;
            }
            last_slash = i as isize;
            dots = 0;
        } else if code == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
    }
    res
}

#[cfg(test)]
mod win32_resolve_tests {
    use super::win32_resolve;

    /// `path.win32.resolve(p)` on node v22.22.2 with `process.cwd()` returning
    /// `C:\work\dir` (and no `=D:`-style drive directories).
    const NODE: &[(&str, &str)] = &[
        ("x", "C:\\work\\dir\\x"),
        ("does-not-exist.txt", "C:\\work\\dir\\does-not-exist.txt"),
        ("nope/sub", "C:\\work\\dir\\nope\\sub"),
        ("nope\\a\\b\\", "C:\\work\\dir\\nope\\a\\b"),
        (".", "C:\\work\\dir"),
        ("..", "C:\\work"),
        ("../..", "C:\\"),
        ("../../../../x", "C:\\x"),
        ("sub/../x", "C:\\work\\dir\\x"),
        ("./a/./b/../c", "C:\\work\\dir\\a\\c"),
        ("a//b\\\\c", "C:\\work\\dir\\a\\b\\c"),
        ("C:\\abs\\path", "C:\\abs\\path"),
        ("C:/abs/fwd", "C:\\abs\\fwd"),
        ("c:\\Lower\\Case", "c:\\Lower\\Case"),
        ("C:", "C:\\work\\dir"),
        ("C:rel", "C:\\work\\dir\\rel"),
        ("D:", "D:\\"),
        ("D:rel\\x", "D:\\rel\\x"),
        ("E:rel", "E:\\rel"),
        ("\\rooted", "C:\\rooted"),
        ("/rooted/fwd", "C:\\rooted\\fwd"),
        ("\\", "C:\\"),
        ("\\\\server\\share\\dir", "\\\\server\\share\\dir"),
        ("\\\\server\\share", "\\\\server\\share\\"),
        ("//server/share/x/../y", "\\\\server\\share\\y"),
        ("\\\\.\\pipe\\name", "\\\\.\\pipe\\name"),
        ("\\\\?\\C:\\long", "\\\\?\\C:\\long"),
        ("C:\\a\\..\\..\\..", "C:\\"),
        ("...", "C:\\work\\dir\\..."),
        ("a/...", "C:\\work\\dir\\a\\..."),
        ("a/../..", "C:\\work"),
        ("C:\\x\\.", "C:\\x"),
        ("C:\\x\\", "C:\\x"),
    ];

    #[test]
    fn matches_node_path_win32_resolve() {
        for (input, want) in NODE {
            assert_eq!(
                win32_resolve(input, r"C:\work\dir", |_| None),
                *want,
                "{input:?}"
            );
        }
    }

    #[test]
    fn a_drive_relative_path_uses_that_drives_directory() {
        let drive = |device: &str| (device == "D:").then(|| r"D:\dcwd\sub".to_string());
        assert_eq!(
            win32_resolve("D:x", r"C:\work\dir", drive),
            r"D:\dcwd\sub\x"
        );
        // A directory recorded for the drive that is on another drive is not
        // used: the drive's root is.
        let wrong = |_: &str| Some(r"C:\elsewhere".to_string());
        assert_eq!(win32_resolve("D:x", r"C:\work\dir", wrong), r"D:\x");
    }
}

#[cfg(test)]
mod fs_os_path_tests {
    use super::{
        FsPath, Win32ResolveMode, fs_error_path, fs_os_path, fs_os_path_with, fs_shown_path,
        strip_unc_prefix, win32_resolve_all_mode, win32_resolve_mode, win32_shown_path,
        win32_symlink_probe, win32_symlink_target,
    };
    use std::borrow::Cow;

    const CWD: &str = r"C:\Windows";

    /// `fs_os_path_with` in the binding's mode, cwd `C:\Windows`, no `=X:`
    /// drive directories.
    fn os(path: &str) -> String {
        fs_os_path_with(path, || CWD.to_string(), |_| None, Win32ResolveMode::Cpp).into_owned()
    }

    /// (input, OS path, node's `err.path`). The third column is measured:
    /// node v22.22.2 on Windows 11, cwd `C:\Windows`, `fs.statSync(input)`
    /// (`fs.mkdirSync` for the roots, which exist), each row's `err.path`
    /// pasted as printed. The second column is what `ToNamespacedPath` hands
    /// libuv: lib/path.js `toNamespacedPath` printed the same string for
    /// every row but the two drive-relative ones on a drive with no `=Z:`,
    /// where the C++ resolve differs (`Win32ResolveMode::Cpp`).
    const NODE: &[(&str, &str, &str)] = &[
        // Relative to the cwd.
        ("zz-none", r"\\?\C:\Windows\zz-none", r"C:\Windows\zz-none"),
        (
            "sub/zz-none/",
            r"\\?\C:\Windows\sub\zz-none",
            r"C:\Windows\sub\zz-none",
        ),
        (
            r".\zz-none\.\x",
            r"\\?\C:\Windows\zz-none\x",
            r"C:\Windows\zz-none\x",
        ),
        // Drive-absolute, any separator, the case kept.
        (r"C:\zz-none", r"\\?\C:\zz-none", r"C:\zz-none"),
        ("C:/zz-none/x", r"\\?\C:\zz-none\x", r"C:\zz-none\x"),
        (r"c:\zz-none", r"\\?\c:\zz-none", r"c:\zz-none"),
        // Rooted on the cwd's drive.
        ("/zz-none", r"\\?\C:\zz-none", r"C:\zz-none"),
        (r"\zz-none", r"\\?\C:\zz-none", r"C:\zz-none"),
        // Drive roots: the OS path keeps its trailing `\`.
        (r"C:\", r"\\?\C:\", r"C:\"),
        ("C:/", r"\\?\C:\", r"C:\"),
        (r"\", r"\\?\C:\", r"C:\"),
        ("/", r"\\?\C:\", r"C:\"),
        // `..` past the root stops at it.
        (r"C:\..\..\zz-none", r"\\?\C:\zz-none", r"C:\zz-none"),
        (r"C:\..\..", r"\\?\C:\", r"C:\"),
        (r"..\..\..\..\..\zz-none", r"\\?\C:\zz-none", r"C:\zz-none"),
        // Drive-relative: on the cwd's drive, and on a drive with no `=Z:`
        // (the first call per drive in a process; see Win32ResolveMode::Cpp).
        (
            "C:zz-none",
            r"\\?\C:\Windows\zz-none",
            r"C:\Windows\zz-none",
        ),
        ("Z:zz-none", "Z:zz-none", "Z:zz-none"),
        (r"Z:a\..\b", "Z:b", "Z:b"),
        ("Q:", "Q:", "Q:"),
        ("Q:.", "Q:.", "Q:."),
        (r"Z:\zz-none", r"\\?\Z:\zz-none", r"Z:\zz-none"),
        // Already namespaced: resolved again, not prefixed again.
        (r"\\?\C:\zz-none", r"\\?\C:\zz-none", r"C:\zz-none"),
        (r"\\?\C:\zz-none\..\nope", r"\\?\C:\nope", r"C:\nope"),
        (r"\\?\C:\", r"\\?\C:", "C:"),
        (r"\\?\C:", r"\\?\C:", "C:"),
        ("//?/C:/zz-none", r"\\?\C:\zz-none", r"C:\zz-none"),
        (r"\\?\C:\a\..\..\..\zz", r"\\?\zz", "zz"),
        (r"\\?\C:\a\..\..", r"\\?\", ""),
        (r"\\?\", r"\\?\C:\?", r"C:\?"),
        (r"\\?\zz-none", r"\\?\zz-none", "zz-none"),
        (
            r"\\?\GLOBALROOT\zz-none",
            r"\\?\GLOBALROOT\zz-none",
            r"GLOBALROOT\zz-none",
        ),
        (
            r"\\?\UNC\localhost\zz-noshare\x",
            r"\\?\UNC\localhost\zz-noshare\x",
            r"\\localhost\zz-noshare\x",
        ),
        (
            r"\\?\UNC\localhost\zz-noshare\",
            r"\\?\UNC\localhost\zz-noshare",
            r"\\localhost\zz-noshare",
        ),
        (
            "//?/UNC/localhost/zz-noshare/x",
            r"\\?\UNC\localhost\zz-noshare\x",
            r"\\localhost\zz-noshare\x",
        ),
        (
            r"\\?\unc\localhost\zz-noshare\x",
            r"\\?\unc\localhost\zz-noshare\x",
            r"unc\localhost\zz-noshare\x",
        ),
        (r"\\?\UNC\zz-srv", r"\\?\UNC\zz-srv", r"\\zz-srv"),
        // Device paths: resolved, never prefixed, shown as they are.
        (r"\\.\C:\zz-none", r"\\.\C:\zz-none", r"\\.\C:\zz-none"),
        (r"\\.\zz-nodevice", r"\\.\zz-nodevice", r"\\.\zz-nodevice"),
        (r"\\.\C:\..\..\zz", r"\\.\zz", r"\\.\zz"),
        // UNC, share roots included: the share root keeps a trailing `\`.
        (
            r"\\localhost\zz-noshare",
            r"\\?\UNC\localhost\zz-noshare\",
            r"\\localhost\zz-noshare\",
        ),
        (
            r"\\localhost\zz-noshare\",
            r"\\?\UNC\localhost\zz-noshare\",
            r"\\localhost\zz-noshare\",
        ),
        (
            r"\\localhost\zz-noshare\..",
            r"\\?\UNC\localhost\zz-noshare\",
            r"\\localhost\zz-noshare\",
        ),
        (
            r"\\localhost\zz-noshare\..\..\x",
            r"\\?\UNC\localhost\zz-noshare\x",
            r"\\localhost\zz-noshare\x",
        ),
        (
            "//localhost/zz-noshare/x/",
            r"\\?\UNC\localhost\zz-noshare\x",
            r"\\localhost\zz-noshare\x",
        ),
        // A server with no share is not a UNC root: rooted on the cwd's drive.
        (r"\\srv", r"\\?\C:\srv", r"C:\srv"),
        // Device names are ordinary names in a `\\?\` path.
        ("NUL", r"\\?\C:\Windows\NUL", r"C:\Windows\NUL"),
        ("CON", r"\\?\C:\Windows\CON", r"C:\Windows\CON"),
        (
            r"zz-none\COM1.txt",
            r"\\?\C:\Windows\zz-none\COM1.txt",
            r"C:\Windows\zz-none\COM1.txt",
        ),
        // Trailing dots and spaces are kept.
        (
            "zz-none.",
            r"\\?\C:\Windows\zz-none.",
            r"C:\Windows\zz-none.",
        ),
        (
            "zz-none ",
            r"\\?\C:\Windows\zz-none ",
            r"C:\Windows\zz-none ",
        ),
        (
            r"zz-none. .\x",
            r"\\?\C:\Windows\zz-none. .\x",
            r"C:\Windows\zz-none. .\x",
        ),
        (r"\\?\C:\zz-none.", r"\\?\C:\zz-none.", r"C:\zz-none."),
        (r"\\?\C:\zz-none \x", r"\\?\C:\zz-none \x", r"C:\zz-none \x"),
    ];

    #[test]
    fn os_path_and_shown_path_match_node() {
        for (input, want_os, err_path) in NODE {
            let got = os(input);
            assert_eq!(got, *want_os, "fs_os_path_with({input:?})");
            assert_eq!(win32_shown_path(&got), *err_path, "shown(os({input:?}))");
        }
    }

    /// Rows whose `stat` succeeded on that box, so node printed no err.path;
    /// lib/path.js `toNamespacedPath` printed the OS path.
    #[test]
    fn os_path_of_existing_paths_matches_to_namespaced_path() {
        for (input, want_os) in [
            ("C:", r"\\?\C:\Windows"),
            (r"\\.\", r"\\?\C:\"),
            (r"C:\zz-none\..\..\..", r"\\?\C:\"),
        ] {
            assert_eq!(os(input), want_os, "{input:?}");
        }
    }

    #[test]
    fn long_paths_are_prefixed_whole() {
        let long = "a".repeat(100);
        let tail = [long.as_str(); 4].join(r"\");
        let abs = format!(r"C:\zz-none\{tail}");
        assert_eq!(os(&abs), format!(r"\\?\{abs}"));
        assert_eq!(win32_shown_path(&os(&abs)), abs);
        let rel = format!(r"zz-none\{tail}");
        assert_eq!(os(&rel), format!(r"\\?\C:\Windows\{rel}"));
        assert_eq!(win32_shown_path(&os(&rel)), format!(r"C:\Windows\{rel}"));
    }

    /// Resolving an OS path again gives the same OS path except on a root:
    /// `\\?\C:\` names the drive's root directory and its resolve `\\?\C:`
    /// the volume; `\\?\UNC\srv\sh\` loses its trailing `\` the same way;
    /// and `\\?\` resolves to `C:\?`. This is why an error shows an OS path
    /// strip-only.
    #[test]
    fn os_path_is_idempotent_except_on_roots() {
        let roots: &[&str] = &[
            r"C:\",
            "C:/",
            r"\",
            "/",
            r"C:\..\..",
            r"\\?\C:\a\..\..",
            r"\\localhost\zz-noshare",
            r"\\localhost\zz-noshare\",
            r"\\localhost\zz-noshare\..",
        ];
        for (input, _, _) in NODE {
            let once = os(input);
            let twice = os(&once);
            if roots.contains(input) {
                assert_ne!(twice, once, "{input:?} is a root and is not idempotent");
            } else {
                assert_eq!(twice, once, "os(os({input:?}))");
            }
        }
        assert_eq!(os(r"\\?\C:\"), r"\\?\C:");
        assert_eq!(os(r"\\?\UNC\srv\sh\"), r"\\?\UNC\srv\sh");
    }

    /// The drive-directory rule of the two resolves, measured with `=Z:` set
    /// in a child's environment (cwd `C:\Windows`): `statSync('Z:a')` for the
    /// C++, `path.win32.resolve('Z:a')` for the JS.
    #[test]
    fn the_cpp_and_js_resolves_differ_only_in_the_drive_directory_check() {
        let cwd = || CWD.to_string();
        let cases: &[(Option<&str>, &str, &str)] = &[
            (None, "Z:a", r"Z:\a"),
            (Some(r"Z:\dcwd"), r"Z:\dcwd\a", r"Z:\dcwd\a"),
            (Some(r"C:\elsewhere"), "Z:a", r"Z:\a"),
            (Some("C:/elsewhere"), r"Z:\a", "Z:a"),
            (Some("Z:/fwd"), r"Z:\fwd\a", r"Z:\fwd\a"),
            (Some(r"z:\low"), r"Z:\low\a", r"Z:\low\a"),
            (Some(""), "Z:a", r"Z:\a"),
        ];
        for (dir, cpp, js) in cases {
            let drive = |_: &str| dir.map(str::to_string);
            assert_eq!(
                win32_resolve_mode("Z:a", cwd, drive, Win32ResolveMode::Cpp),
                *cpp,
                "Cpp, =Z: {dir:?}"
            );
            assert_eq!(
                win32_resolve_mode("Z:a", cwd, drive, Win32ResolveMode::Js),
                *js,
                "Js, =Z: {dir:?}"
            );
        }
        // Once Windows has set `=Z:` to `Z:\` (it does on the first
        // drive-relative call), node's later calls resolve: `Z:\zz-none`.
        let set = |_: &str| Some(r"Z:\".to_string());
        let os_set = |p: &str| fs_os_path_with(p, cwd, set, Win32ResolveMode::Cpp).into_owned();
        assert_eq!(os_set("Z:zz-none"), r"\\?\Z:\zz-none");
        assert_eq!(win32_shown_path(&os_set("Z:")), r"Z:\");
        assert_eq!(win32_shown_path(&os_set(r"Z:a\..")), r"Z:\");
        // The C++ takes an empty current directory as the drive's root.
        let none = || String::new();
        assert_eq!(
            win32_resolve_mode("Z:a", none, |_| None, Win32ResolveMode::Cpp),
            r"Z:\a"
        );
        assert_eq!(
            win32_resolve_mode("Z:a", none, |_| None, Win32ResolveMode::Js),
            "Z:a"
        );
    }

    /// node v22.22.2's `symlinkSync(target, existing)` fails EEXIST naming
    /// the target as its binding stored it, strip-only (stage2-sym.cjs, cwd
    /// `W`'s parent): an absolute target namespaced, a relative one with `/`
    /// turned into `\` and nothing else done to it.
    #[test]
    fn a_symlink_target_is_stored_as_node_preprocesses_it() {
        for (target, stored, shown) in [
            (
                "C:/w/sub/file.txt",
                r"\\?\C:\w\sub\file.txt",
                r"C:\w\sub\file.txt",
            ),
            (
                r"C:\w\sub\file.txt",
                r"\\?\C:\w\sub\file.txt",
                r"C:\w\sub\file.txt",
            ),
            ("sub/file.txt", r"sub\file.txt", r"sub\file.txt"),
            (r"\w\sub", r"\\?\C:\w\sub", r"C:\w\sub"),
            (r"\\?\C:\w\sub", r"\\?\C:\w\sub", r"C:\w\sub"),
            (r"C:\", r"\\?\C:\", r"C:\"),
            ("../x/./y", r"..\x\.\y", r"..\x\.\y"),
            (r"\\srv\sh\x", r"\\?\UNC\srv\sh\x", r"\\srv\sh\x"),
            ("C:x", "C:x", "C:x"),
            ("plain", "plain", "plain"),
            ("", "", ""),
        ] {
            let got = win32_symlink_target(target, || CWD.to_string(), |_| None);
            assert_eq!(got, stored, "{target:?}");
            assert_eq!(win32_shown_path(&got), shown, "{target:?}");
        }
        // A relative target with no `/` is not copied.
        assert!(matches!(
            win32_symlink_target(r"a\b", || unreachable!(), |_| None),
            Cow::Borrowed(_)
        ));
    }

    /// node's Windows symlink stats `path.resolve(link, '..', target)` to
    /// choose a directory or a file link: the target relative to the link's
    /// parent. stage2-sym.cjs: `symlinkSync('dir', 'W/sub/l1')` with
    /// `W/sub/dir` a directory makes a directory link even though the cwd has
    /// no `dir`.
    #[test]
    fn the_symlink_probe_is_relative_to_the_links_parent() {
        let probe = |link: &str, target: &str| {
            win32_symlink_probe(link, target, || CWD.to_string(), |_| None)
        };
        assert_eq!(probe(r"C:\w\sub\l1", "dir"), r"C:\w\sub\dir");
        assert_eq!(probe(r"C:\w\l2", "sub/dir"), r"C:\w\sub\dir");
        assert_eq!(probe(r"C:\w\l3", r"C:\abs\dir"), r"C:\abs\dir");
        assert_eq!(probe(r"C:\w\l4", r"\rooted"), r"C:\rooted");
        assert_eq!(probe(r"C:\w\l5", r"..\up"), r"C:\up");
        assert_eq!(probe("rel-link", "t"), r"C:\Windows\t");
        assert_eq!(probe(r"\\srv\sh\l", "t"), r"\\srv\sh\t");
        // A drive-relative target on another drive: the `..` still applies,
        // to that drive's directory (here its root, `=D:` being unset).
        assert_eq!(probe(r"C:\w\l", "D:x"), r"D:\x");
        // With one argument the resolve is `win32_resolve_mode`'s.
        for path in ["x", r"C:\a\..\b", r"\\srv\sh", "D:y", ""] {
            assert_eq!(
                win32_resolve_all_mode(&[path], || CWD.to_string(), |_| None, Win32ResolveMode::Js),
                win32_resolve_mode(path, || CWD.to_string(), |_| None, Win32ResolveMode::Js),
                "{path:?}"
            );
        }
    }

    #[test]
    fn cwd_is_read_only_when_needed() {
        let cwd = || -> String { panic!("cwd read for an absolute path") };
        for p in [r"C:\x", r"\\srv\sh\x", r"\\?\C:\x", r"\\.\pipe\x"] {
            fs_os_path_with(p, cwd, |_| None, Win32ResolveMode::Cpp);
        }
        let empty = fs_os_path_with("", cwd, |_| None, Win32ResolveMode::Cpp);
        assert!(matches!(empty, Cow::Borrowed("")));
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_os_path_is_namespaced_and_shown_strip_only() {
        let root = FsPath::new(r"C:\".to_string());
        assert_eq!(root.os(), r"\\?\C:\");
        assert_eq!(root.shown(), r"C:\");
        let unc = FsPath::new(r"\\srv\sh".to_string());
        assert_eq!(unc.os(), r"\\?\UNC\srv\sh\");
        assert_eq!(unc.shown(), r"\\srv\sh\");
        let rel = FsPath::new("zz-none".to_string());
        assert!(rel.os().starts_with(r"\\?\") && rel.os().ends_with(r"\zz-none"));
        assert_eq!(fs_error_path(r"C:\"), r"C:\");
        assert_eq!(fs_error_path(r"\\?\C:\"), "C:");
        assert_eq!(fs_error_path(""), "");
        assert!(matches!(fs_os_path(""), Cow::Borrowed("")));
        assert_eq!(fs_shown_path(r"\\?\UNC\srv\sh\x"), r"\\srv\sh\x");
        assert_eq!(
            strip_unc_prefix(std::path::Path::new(r"\\?\UNC\srv\sh\x")),
            r"\\srv\sh\x"
        );
        assert_eq!(strip_unc_prefix(std::path::Path::new(r"\\?\C:\x")), r"C:\x");
    }

    #[cfg(not(windows))]
    #[test]
    fn off_windows_paths_are_borrowed_as_given() {
        for p in ["", "x", "/abs/x", r"\\?\C:\x", r"C:\"] {
            assert!(matches!(fs_os_path(p), Cow::Borrowed(b) if b == p), "{p:?}");
            assert!(
                matches!(fs_shown_path(p), Cow::Borrowed(b) if b == p),
                "{p:?}"
            );
            assert!(
                matches!(fs_error_path(p), Cow::Borrowed(b) if b == p),
                "{p:?}"
            );
            let fs_path = FsPath::new(p.to_string());
            assert_eq!(fs_path.os(), p);
            assert_eq!(fs_path.shown(), p);
            assert_eq!(strip_unc_prefix(std::path::Path::new(p)), p);
        }
    }
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
/// node's `stringToFlags` (lib/internal/fs/utils.js): the bits one of node's
/// flag strings stands for. The JS layer makes the same translation before a
/// flag reaches a native; this one serves the opens made on this side
/// (`writeFile`, `appendFile`) and the tests. A string node does
/// not know is read-only here; node itself refuses it, in JS.
///
/// The synchronous spellings (`rs`, `as+`, ...) add O_SYNC, where the platform
/// has it: node's `fs.constants` has no O_SYNC on Windows, so there they are
/// their plain twins.
pub fn open_options_for(flags: &str) -> OpenFlags {
    use o_flag::{APPEND, CREAT, EXCL, RDONLY, RDWR, TRUNC, WRONLY};
    #[cfg(unix)]
    const SYNC: i32 = o_flag::SYNC;
    #[cfg(not(unix))]
    const SYNC: i32 = 0;
    OpenFlags(match flags {
        "r" => RDONLY,
        "rs" | "sr" => RDONLY | SYNC,
        "r+" => RDWR,
        "rs+" | "sr+" => RDWR | SYNC,
        "w" => TRUNC | CREAT | WRONLY,
        "wx" | "xw" => TRUNC | CREAT | WRONLY | EXCL,
        "w+" => TRUNC | CREAT | RDWR,
        "wx+" | "xw+" => TRUNC | CREAT | RDWR | EXCL,
        "a" => APPEND | CREAT | WRONLY,
        "ax" | "xa" => APPEND | CREAT | WRONLY | EXCL,
        "as" | "sa" => APPEND | CREAT | WRONLY | SYNC,
        "a+" => APPEND | CREAT | RDWR,
        "ax+" | "xa+" => APPEND | CREAT | RDWR | EXCL,
        "as+" | "sa+" => APPEND | CREAT | RDWR | SYNC,
        _ => RDONLY,
    })
}

/// `write_all`, except that an EMPTY buffer still issues one write syscall.
///
/// `std`'s `write_all` loops `while !buf.is_empty()`, so a zero-byte write
/// never reaches the OS and never validates the descriptor. Node's does:
/// `writevSync(fd, [Buffer.alloc(0)])` reports EBADF on a closed or wrong-mode
/// fd rather than quietly returning 0. Only an EMPTY LIST skips the descriptor
/// entirely, and that case never gets here.
pub fn write_all_checked(file: &std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = file;
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
/// call. X_OK is approximated as existence (wave 1).
/// Node's `fs.access`. On failure returns (code, message, errno, and the Win32
/// code behind an `UNKNOWN` -- `unknown_win32_error` -- or `None`).
///
/// The errno rides along because both callers need it to build node's system-
/// error shape and only this function still holds the `io::Error` it came
/// from. Without it `fs.access`/`fs.promises.access` rejected with no `errno`
/// at all (and the async form with no `syscall` or `path` either), where every
/// sibling fs op carries all four. The Win32 code rides along for the same
/// reason (#442); the sync caller keeps it, the async one does not yet.
///
/// The message names the path as `path.shown()` reports it, and so must the
/// caller's `path` property.
///
/// On Windows it is libuv's fs__access: the path's own attributes
/// (GetFileAttributesW, which does not follow a link -- a dangling one is
/// accessible), and W_OK refused only for a read-only FILE (directories
/// cannot be read-only on Windows, libuv says).
pub fn check_access(
    path: &FsPath,
    mode: i32,
) -> Result<(), (String, String, Option<i32>, Option<u32>)> {
    let shown = path.shown();
    let failed = |e: std::io::Error| {
        let code = node_error_code(&e);
        (
            code.to_string(),
            node_error_message(code, "access", &shown, &e),
            node_errno(code, &e),
            unknown_win32_error(code, &e),
        )
    };
    #[cfg(windows)]
    let read_only = {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
        };
        let attributes = win_fs::file_attributes(path.os()).map_err(failed)?;
        attributes & FILE_ATTRIBUTE_READONLY != 0 && attributes & FILE_ATTRIBUTE_DIRECTORY == 0
    };
    #[cfg(not(windows))]
    let read_only = std::fs::metadata(path)
        .map_err(failed)?
        .permissions()
        .readonly();
    if mode & 2 != 0 && read_only {
        let code = if cfg!(windows) { "EPERM" } else { "EACCES" };
        return Err((
            code.to_string(),
            format!("{code}: operation not permitted, access '{shown}'"),
            // Synthesised (no io::Error behind it): the libuv numbers node
            // reports for these two.
            Some(if cfg!(windows) { -4048 } else { -13 }),
            None,
        ));
    }
    Ok(())
}

/// node's `fs.chmod` of a path. On Windows it is libuv's (the CRT's
/// _wchmod): only the read-only attribute, set or cleared by the owner's
/// write bit, on the path's own attributes -- a link's, not its target's
/// (`win_fs::chmod`).
pub fn chmod_path(path: &FsPath, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        win_fs::chmod(path.os(), mode)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_readonly(mode & 0o200 == 0);
        std::fs::set_permissions(path, perms)
    }
}

/// unlink as libuv's: a file, or on Windows a directory symlink or junction
/// -- a link, which fs__unlink removes as one -- but never a directory. std's
/// remove_file refuses a directory link EPERM, and node's fs.cp replaces one
/// (#390).
pub fn unlink_path(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink_dir()) {
            return ops::unlink_directory_link(path);
        }
    }
    std::fs::remove_file(path)
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

/// std::fs::canonicalize (and a symlink target read back) gives `\\?\`
/// paths on Windows, which node shows without the prefix: the path as
/// `fs_shown_path` shows it, so a UNC result `\\?\UNC\srv\sh\x` reads
/// `\\srv\sh\x` (not `UNC\srv\sh\x`) and `\\?\C:\x` reads `C:\x`. Off
/// Windows the path is returned as it is.
pub fn strip_unc_prefix(path: &std::path::Path) -> String {
    fs_shown_path(&path.to_string_lossy()).into_owned()
}

/// node:zlib backend (flate2 encoders, [`inflate::NodeInflate`] decoders,
/// brotli). Sync fns serve the *Sync natives directly; the async ops below
/// wrap them in spawn_blocking -- compression is CPU work and must not sit on
/// the isolate thread for the callback forms.
///
/// Incremental streaming (StreamCompressor / StreamDecompressor /
/// BrotliCompressor / BrotliDecompressor) backs the JS Transform classes.
/// Each JS Transform stream creates one handle in the ZlibRegistry;
/// _transform feeds chunks via zlibStreamWrite and _flush finalizes via
/// zlibStreamFlush.
pub mod zlib {
    pub use crate::deflate::NodeDeflate;
    pub use crate::inflate::{NodeInflate, Wrap, ZlibError};

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

    /// All of `bytes` deflated in `format` at node's `level` (-1 is zlib's
    /// default, 6).
    pub fn compress(bytes: &[u8], format: Format, level: i32) -> std::io::Result<Vec<u8>> {
        compress_capped(bytes, format, level, None, None)
    }

    /// The message of the `io::Error` `decompress_capped` returns when the
    /// output would exceed `max_output`. The op layer matches on it to raise
    /// node's `RangeError [ERR_BUFFER_TOO_LARGE]`; nothing else produces it.
    pub const OUTPUT_TOO_LARGE: &str = "zlib output exceeds maxOutputLength";

    /// zlib's `Z_FINISH`: node's default `finishFlush`, the one flush under
    /// which an inflate that stops inside the stream is an error.
    pub const Z_FINISH: i32 = 4;

    pub fn decompress(bytes: &[u8], format: Format) -> std::io::Result<Vec<u8>> {
        decompress_capped(bytes, format, None, Z_FINISH, None)
    }

    /// Decompress, giving up as soon as the output passes `max_output` bytes.
    ///
    /// node's `maxOutputLength` (the one-shot zlib APIs). The cap is enforced
    /// while inflating, not on the finished buffer: a 200 KB gzip of 200 MiB
    /// of spaces must fail after ~`max_output` bytes of work, not after the
    /// whole 200 MiB has been allocated -- that allocation is the OOM the
    /// option exists to prevent.
    ///
    /// A decode failure is an `io::Error` wrapping a [`ZlibError`] (see
    /// [`zlib_error`]): node's code, errno and message for it.
    ///
    /// `finish_flush` is node's `finishFlush` option (see [`inflate_all`]),
    /// `dictionary` its `dictionary` (see [`NodeInflate::with_dictionary`]).
    pub fn decompress_capped(
        bytes: &[u8],
        format: Format,
        max_output: Option<usize>,
        finish_flush: i32,
        dictionary: Option<&[u8]>,
    ) -> std::io::Result<Vec<u8>> {
        let wrap = match format {
            Format::Gzip => Wrap::Gzip,
            Format::Deflate => Wrap::Zlib,
            Format::DeflateRaw => Wrap::Raw,
        };
        inflate_all(bytes, wrap, max_output, finish_flush, dictionary)
    }

    /// Inflate the whole of `bytes` under node's one-shot finishing flush,
    /// `finish_flush` (the `finishFlush` option, Z_FINISH by default). Under
    /// Z_FINISH, input that ends inside the stream is Z_BUF_ERROR "unexpected
    /// end of file"; under any other flush it is not an error and the result
    /// is what decoded, as node_zlib.cc's CheckError reports Z_BUF_ERROR only
    /// under Z_FINISH -- the lenient decode HTTP clients ask for with
    /// Z_SYNC_FLUSH. What follows a complete stream is dropped (for gzip:
    /// unless it is another member). Output is produced
    /// [`inflate::STEP_OUT`] at a time, so memory stays within `max_output`
    /// plus one step whatever the input inflates to.
    fn inflate_all(
        bytes: &[u8],
        wrap: Wrap,
        max_output: Option<usize>,
        finish_flush: i32,
        dictionary: Option<&[u8]>,
    ) -> std::io::Result<Vec<u8>> {
        let mut dec = NodeInflate::with_dictionary(wrap, dictionary);
        let mut out = Vec::new();
        let mut buf = vec![0u8; crate::inflate::STEP_OUT];
        let mut input = bytes;
        loop {
            let (used, produced) = dec.step(input, &mut buf).map_err(std::io::Error::other)?;
            input = &input[used..];
            if used == 0 && produced == 0 {
                break;
            }
            if max_output.is_some_and(|cap| out.len() + produced > cap) {
                return Err(std::io::Error::other(OUTPUT_TOO_LARGE));
            }
            out.extend_from_slice(&buf[..produced]);
        }
        if finish_flush == Z_FINISH {
            dec.finish().map_err(std::io::Error::other)?;
        }
        Ok(out)
    }

    /// The node:zlib failure an inflate `io::Error` carries, if it is one.
    pub fn zlib_error(error: &std::io::Error) -> Option<ZlibError> {
        error.get_ref()?.downcast_ref::<ZlibError>().copied()
    }

    // Kept next to `inflate_all` (the code it guards) rather than at the module
    // end past the streaming and brotli code.
    #[cfg(test)]
    #[allow(clippy::items_after_test_module)]
    mod capped_tests {
        use super::{
            Format, OUTPUT_TOO_LARGE, Z_FINISH, compress, decompress_capped, unzip, unzip_capped,
            zlib_error,
        };

        #[test]
        fn decompress_capped_stops_a_gzip_bomb_at_the_cap() {
            // 16 MiB of spaces gzips to a few KB. Under a 1 KiB cap the real
            // decoder path returns the cap error having buffered ~1 KiB, not
            // 16 MiB; and the cap boundary is exact.
            let size = 16 * 1024 * 1024;
            let bomb = compress(&vec![b' '; size], Format::Gzip, 6).unwrap();
            let err =
                decompress_capped(&bomb, Format::Gzip, Some(1024), Z_FINISH, None).unwrap_err();
            assert_eq!(err.to_string(), OUTPUT_TOO_LARGE);
            assert!(decompress_capped(&bomb, Format::Gzip, Some(size), Z_FINISH, None).is_ok());
            assert!(
                decompress_capped(&bomb, Format::Gzip, Some(size - 1), Z_FINISH, None).is_err()
            );
        }

        #[test]
        fn one_shot_inflate_reads_every_gzip_member_and_codes_its_errors() {
            // #166: flate2's read::GzDecoder stopped after the first member,
            // and every failure was an uncoded io::Error.
            let mut two = compress(b"hello", Format::Gzip, 6).unwrap();
            two.extend(compress(b"world", Format::Gzip, 6).unwrap());
            assert_eq!(
                decompress_capped(&two, Format::Gzip, None, Z_FINISH, None).unwrap(),
                b"helloworld"
            );
            assert_eq!(unzip(&two).unwrap(), b"helloworld");
            let err = decompress_capped(b"not gzip at all", Format::Gzip, None, Z_FINISH, None)
                .unwrap_err();
            let coded = zlib_error(&err).expect("a coded zlib error");
            assert_eq!(
                (coded.code, coded.errno, coded.message),
                ("Z_DATA_ERROR", -3, "incorrect header check")
            );
            let packed = compress(b"hello world hello world", Format::Deflate, 6).unwrap();
            let err =
                decompress_capped(&packed[..8], Format::Deflate, None, Z_FINISH, None).unwrap_err();
            let coded = zlib_error(&err).expect("a coded zlib error");
            assert_eq!((coded.code, coded.errno), ("Z_BUF_ERROR", -5));
        }

        #[test]
        fn a_finishing_flush_other_than_z_finish_returns_what_decoded() {
            // Node's `finishFlush: Z_SYNC_FLUSH` (axios, node-fetch): a stream
            // cut short is not an error, the result is what decoded. Every
            // other flush value is as lenient; only Z_FINISH checks the end.
            let plain: Vec<u8> = (0..4000u32).flat_map(|i| i.to_le_bytes()).collect();
            for format in [Format::Gzip, Format::Deflate, Format::DeflateRaw] {
                let packed = compress(&plain, format, 6).unwrap();
                let cut = &packed[..packed.len() / 2];
                for flush in [0, 1, 2, 3, 5] {
                    let out = decompress_capped(cut, format, None, flush, None).unwrap();
                    assert!(
                        !out.is_empty() && plain.starts_with(&out),
                        "{format:?} {flush}"
                    );
                    let whole = decompress_capped(&packed, format, None, flush, None).unwrap();
                    assert_eq!(whole, plain, "{format:?} {flush}");
                }
                let err = decompress_capped(cut, format, None, Z_FINISH, None).unwrap_err();
                assert_eq!(zlib_error(&err).map(|e| e.code), Some("Z_BUF_ERROR"));
            }
            // A decode error is still one under a lenient flush.
            let junk = [0xffu8, 0x00, 0x01, 0x02, 0x03, 0x04];
            assert!(decompress_capped(&junk, Format::Gzip, None, 2, None).is_err());
            assert!(decompress_capped(&junk, Format::Deflate, None, 2, None).is_err());
            let gz = compress(&plain, Format::Gzip, 6).unwrap();
            assert_eq!(
                unzip_capped(&gz[..gz.len() - 4], None, 2, None).unwrap(),
                plain
            );
            assert!(unzip_capped(&[], None, 2, None).unwrap().is_empty());
            assert!(unzip_capped(&[], None, Z_FINISH, None).is_err());
        }
    }

    /// `compress` with node's `maxOutputLength`, which node applies to the
    /// encoders as well, and its `dictionary` (see [`NodeDeflate`]). The cap
    /// is checked on the finished buffer: compressed output is bounded by the
    /// input, so there is no bomb to stop early.
    pub fn compress_capped(
        bytes: &[u8],
        format: Format,
        level: i32,
        max_output: Option<usize>,
        dictionary: Option<&[u8]>,
    ) -> std::io::Result<Vec<u8>> {
        let out = NodeDeflate::new(format, level, dictionary).finish_vec(bytes);
        match max_output {
            Some(cap) if out.len() > cap => Err(std::io::Error::other(OUTPUT_TOO_LARGE)),
            _ => Ok(out),
        }
    }

    /// Node's unzip*: auto-detect gzip (1f 8b magic) vs zlib-wrapped.
    pub fn unzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        unzip_capped(bytes, None, Z_FINISH, None)
    }

    /// `unzip` with node's `maxOutputLength`, `finishFlush` and
    /// `dictionary`; see `decompress_capped`.
    pub fn unzip_capped(
        bytes: &[u8],
        max_output: Option<usize>,
        finish_flush: i32,
        dictionary: Option<&[u8]>,
    ) -> std::io::Result<Vec<u8>> {
        inflate_all(bytes, Wrap::Auto, max_output, finish_flush, dictionary)
    }

    // ----------------------------------------------------------------
    // Incremental streaming: gzip / deflate / deflateRaw
    //
    // Compression is NodeDeflate (crate::deflate), miniz's compressor
    // driven directly: each chunk comes back as the bytes it completed, so
    // nothing buffers the full input, and params() can change the level
    // between chunks.
    //
    // Decompression is NodeInflate (crate::inflate): each chunk runs through
    // the inflate state machine and comes back as that chunk's output, with
    // ~50 KB of state per stream (the 32 KiB window, one 16 KiB step and
    // miniz's decoder).
    //
    // The "unzip" auto-detect variant resolves the format from the first
    // two bytes of the STREAM, however the writes carve it up.
    //
    // Send requirement: NodeDeflate and NodeInflate are Send, and our
    // wrappers hold no thread-local state.
    // ----------------------------------------------------------------

    /// An incremental deflater for gzip, deflate or deflateRaw. Created via
    /// `StreamCompressor::new`; consumes chunks via `write_chunk`;
    /// finalizes via `finish` (emits the trailing CRC / checksum bytes the
    /// format requires).
    pub struct StreamCompressor {
        inner: NodeDeflate,
    }

    impl StreamCompressor {
        /// `dictionary` is node's option (see [`NodeDeflate`]).
        pub fn new(format: Format, level: i32, dictionary: Option<&[u8]>) -> Self {
            Self {
                inner: NodeDeflate::new(format, level, dictionary),
            }
        }

        /// Feed a chunk. Returns whatever bytes the encoder produced
        /// immediately (may be empty -- the encoder buffers internally
        /// until it has a full deflate block ready).
        #[inline]
        pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
            Ok(self.inner.deflate_vec(chunk, 0))
        }

        /// node's `params()`: what the encoder holds, under a sync flush,
        /// then the new level, if any, for what follows.
        pub fn params(&mut self, level: Option<i32>) -> Vec<u8> {
            self.inner.params(level)
        }

        /// Flush and finalize. Consumes self; returns the tail bytes
        /// (including the gzip/zlib trailer). After this the stream handle
        /// is dropped -- close is implicit.
        pub fn finish(mut self) -> std::io::Result<Vec<u8>> {
            Ok(self.inner.finish_vec(&[]))
        }
    }

    // `StreamCompressor` is `Send` by auto-derivation: `NodeDeflate` holds only
    // miniz's compressor and owned buffers, all `Send`, and the wrapper adds
    // no thread-affine state. Deliberately NOT a manual
    // `unsafe impl Send` -- that would suppress the compiler's own auto-trait
    // check and silently keep asserting `Send` if the inner types ever stopped
    // being it.

    // ----------------------------------------------------------------
    // Truly incremental decompressor -- slice A.
    //
    // Each write_chunk call runs the inflate state machine (NodeInflate)
    // over the chunk at once and returns whatever bytes it decoded. The full
    // compressed stream never needs to live in memory simultaneously.
    //
    // Unzip decides gzip or zlib on the stream's first two bytes however the
    // writes split them (#195): NodeInflate reads them as one header, as
    // zlib's inflate does (NEEDBITS(16)).
    // ----------------------------------------------------------------

    /// Truly incremental decompressor: ~50 KB of state per stream regardless
    /// of input size. Errors are node's (an `io::Error` wrapping a
    /// [`ZlibError`]); see [`NodeInflate`] for what each format accepts.
    pub struct StreamDecompressor {
        inner: Box<NodeInflate>,
    }

    impl StreamDecompressor {
        /// An inflate stream reading `wrap`, with node's `dictionary` option;
        /// see [`NodeInflate::with_dictionary`].
        pub fn new(wrap: Wrap, dictionary: Option<&[u8]>) -> Self {
            Self {
                inner: Box::new(NodeInflate::with_dictionary(wrap, dictionary)),
            }
        }
        pub fn new_unzip() -> Self {
            Self::new(Wrap::Auto, None)
        }

        /// Feed one chunk of compressed data. Returns the decompressed bytes
        /// it made decodable (a deflate block that spans chunks finishes
        /// arriving on later calls). Input after the end of the stream is
        /// dropped, as node drops it; for gzip, a following member is
        /// decoded as part of the same stream.
        #[inline]
        pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
            let mut out = Vec::new();
            let mut buf = vec![0u8; crate::inflate::STEP_OUT];
            let mut input = chunk;
            loop {
                let (used, produced) = self
                    .inner
                    .step(input, &mut buf)
                    .map_err(std::io::Error::other)?;
                input = &input[used..];
                if used == 0 && produced == 0 {
                    return Ok(out);
                }
                out.extend_from_slice(&buf[..produced]);
            }
        }

        /// Finalize under the default finishing flush, Z_FINISH; see
        /// [`Self::finish_with`].
        pub fn finish(self) -> std::io::Result<Vec<u8>> {
            self.finish_with(Z_FINISH)
        }

        /// Finalize at `end()` under node's `finishFlush`. Every write's
        /// output was already returned, so this only checks, under Z_FINISH,
        /// that the stream is complete -- input that ended inside it is
        /// Z_BUF_ERROR "unexpected end of file", an empty stream included.
        /// Under any other flush the stream ends with what decoded.
        pub fn finish_with(self, finish_flush: i32) -> std::io::Result<Vec<u8>> {
            if finish_flush == Z_FINISH {
                self.inner.finish().map_err(std::io::Error::other)?;
            }
            Ok(Vec::new())
        }
    }

    // `StreamDecompressor` is `Send` by auto-derivation: `NodeInflate` holds
    // only owned buffers and miniz/flate2 state, all `Send`. No manual
    // `unsafe impl` -- see the note on `StreamCompressor` above.

    // Compile-time proof of both notes. A manual `unsafe impl Send` asserts
    // `Send` forever; this instead FAILS THE BUILD the day an inner type stops
    // being `Send`, which is the behaviour we actually want.
    const _: () = {
        const fn assert_send<T: Send>() {}
        assert_send::<StreamCompressor>();
        assert_send::<StreamDecompressor>();
    };

    #[cfg(test)]
    mod unzip_stream_tests {
        use super::{Format, StreamDecompressor, compress};

        /// Feed `bytes` to a fresh unzip stream `size` bytes per write.
        fn unzip_in_chunks(bytes: &[u8], size: usize) -> std::io::Result<Vec<u8>> {
            let mut dec = StreamDecompressor::new_unzip();
            let mut out = Vec::new();
            for chunk in bytes.chunks(size) {
                out.extend(dec.write_chunk(chunk)?);
            }
            out.extend(dec.finish()?);
            Ok(out)
        }

        #[test]
        fn unzip_detects_the_format_however_the_first_bytes_are_split() {
            // #195: the format was chosen from the first WRITE, and a one-byte
            // write cannot carry both gzip magic bytes, so a gzip stream fed a
            // byte at a time was decoded as zlib and failed.
            let plain = vec![b'x'; 5000];
            for format in [Format::Gzip, Format::Deflate] {
                let packed = compress(&plain, format, 6).unwrap();
                for size in [1, 2, 3, 16, packed.len()] {
                    let out = unzip_in_chunks(&packed, size)
                        .unwrap_or_else(|e| panic!("{format:?} in {size}-byte writes: {e}"));
                    assert_eq!(out, plain, "{format:?} in {size}-byte writes");
                }
            }
        }

        #[test]
        fn unzip_skips_empty_writes_while_undecided() {
            let plain = b"hello".to_vec();
            let packed = compress(&plain, Format::Gzip, 6).unwrap();
            let mut dec = StreamDecompressor::new_unzip();
            let mut out = Vec::new();
            out.extend(dec.write_chunk(&[]).unwrap());
            out.extend(dec.write_chunk(&packed[..1]).unwrap());
            out.extend(dec.write_chunk(&[]).unwrap());
            out.extend(dec.write_chunk(&packed[1..]).unwrap());
            out.extend(dec.finish().unwrap());
            assert_eq!(out, plain);
        }

        #[test]
        fn unzip_with_no_input_is_unexpected_end_of_file() {
            // Node (v22.22.2): createUnzip().end() with no data, or only empty
            // writes, errors Z_BUF_ERROR "unexpected end of file" -- as every
            // inflate stream does.
            let err = unzip_in_chunks(&[], 1).unwrap_err();
            assert_eq!(
                super::zlib_error(&err),
                Some(super::ZlibError::UNEXPECTED_EOF)
            );
        }

        #[test]
        fn unzip_ended_one_byte_in_does_not_drop_the_byte_silently() {
            // The held byte must reach a decoder at end of stream. A lone gzip
            // magic byte is a truncated gzip stream, which the gzip decoder
            // refuses; before the fix this case could not arise (the byte was
            // written to a zlib decoder at once), and holding it must not turn
            // it into a clean empty result.
            assert!(unzip_in_chunks(&[0x1f], 1).is_err());
        }
    }
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
    /// Vec, or `Err(Vec)` when the stream has not ended. Corrupt data fails
    /// the write that carries it, so a stream that wrote cleanly and does
    /// not end here stopped short: node's Z_BUF_ERROR "unexpected end of
    /// file" (node_zlib.cc's brotli `CheckError` under
    /// BROTLI_OPERATION_FINISH), an empty input included.
    pub fn finish(self) -> std::io::Result<Vec<u8>> {
        self.inner.into_inner().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                zlib::ZlibError::UNEXPECTED_EOF,
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

// Kept next to the brotli code it guards rather than at the file's end.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod brotli_tests {
    use super::{BrotliCompressor, BrotliDecompressor, zlib};

    #[test]
    fn a_stream_that_stops_short_is_nodes_unexpected_end_of_file() {
        let mut enc = BrotliCompressor::new();
        let mut packed = enc.write_chunk(b"hello hello hello").unwrap();
        packed.extend(enc.finish().unwrap());
        // Whole: the data. Cut short, or empty: Z_BUF_ERROR, as node's.
        let mut dec = BrotliDecompressor::new();
        let mut out = dec.write_chunk(&packed).unwrap();
        out.extend(dec.finish().unwrap());
        assert_eq!(out, b"hello hello hello");
        for cut in [0, 1, 5, packed.len() - 1] {
            let mut dec = BrotliDecompressor::new();
            dec.write_chunk(&packed[..cut]).unwrap();
            let err = dec.finish().unwrap_err();
            assert_eq!(
                zlib::zlib_error(&err),
                Some(zlib::ZlibError::UNEXPECTED_EOF),
                "cut at {cut}"
            );
        }
    }
}

/// Built-in op implementations. Plain futures; the engine decides how their
/// outcomes surface in JS.
pub mod ops {
    use super::OpOutcome;
    use std::time::Duration;

    pub async fn sleep(ms: u64) -> OpOutcome {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        OpOutcome::Done
    }

    /// A failed path operation, naming the path as node does: the OS path
    /// with the prefix taken off (`FsPath::shown`).
    fn node_fail(error: std::io::Error, syscall: &str, path: &super::FsPath) -> OpOutcome {
        node_fail_as_passed(error, syscall, &path.shown())
    }

    /// `node_fail` naming `path` exactly as given: for mkdtemp, whose
    /// template node does not resolve.
    fn node_fail_as_passed(error: std::io::Error, syscall: &str, path: &str) -> OpOutcome {
        let code = super::node_error_code(&error);
        OpOutcome::node_failed_io(
            code,
            super::node_error_message(code, syscall, path, &error),
            syscall,
            Some(path),
            &error,
        )
    }

    /// A failed two-path operation: node names both, `'path' -> 'dest'`, and
    /// sets `dest`. The caller passes them as they are to be shown.
    fn node_fail_dest(error: std::io::Error, syscall: &str, path: &str, dest: &str) -> OpOutcome {
        let code = super::node_error_code(&error);
        OpOutcome::node_failed_dest_io(
            code,
            super::node_error_message_dest(code, syscall, path, dest, &error),
            syscall,
            path,
            dest,
            &error,
        )
    }

    /// A failed whole-file operation (see `whole_file_error`): the open half
    /// names the path, the read or write half does not.
    fn node_fail_whole_file(
        site: super::FsSite,
        error: super::WholeFileError,
        path: &super::FsPath,
    ) -> OpOutcome {
        let failure = super::whole_file_error(site, path.os(), &error);
        let shown = path.shown();
        OpOutcome::node_failed_io(
            failure.code,
            super::fs_error_message(failure, &shown, error.io()),
            failure.syscall,
            failure.has_path.then_some(&*shown),
            error.io(),
        )
    }

    /// `node_fail` for an operation with call-site error rules (see
    /// `fs_error_at`).
    fn node_fail_at(
        site: super::FsSite,
        error: std::io::Error,
        syscall: &'static str,
        path: &super::FsPath,
    ) -> OpOutcome {
        let failure = super::fs_error_at(site, syscall, path.os(), &error);
        let shown = path.shown();
        OpOutcome::node_failed_io(
            failure.code,
            super::fs_error_message(failure, &shown, &error),
            failure.syscall,
            failure.has_path.then_some(&*shown),
            &error,
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
    /// A failed fd operation other than a read or write (fstat, fsync,
    /// ftruncate, fchmod, fchown, futimes): node's code for the error as it is,
    /// and no path -- node's message ends at the syscall. These used to go
    /// through `node_fail` with an empty path, which put a `path: ''` on the
    /// error and `, fsync ''` at the end of its message.
    fn node_fail_fd_op(error: std::io::Error, syscall: &str) -> OpOutcome {
        let code = super::node_error_code(&error);
        OpOutcome::node_failed_io(
            code,
            super::node_error_message_fd(code, syscall, &error),
            syscall,
            None,
            &error,
        )
    }

    fn node_fail_fd(error: std::io::Error, syscall: &str) -> OpOutcome {
        let code = super::fd_error_code(&error);
        OpOutcome::node_failed_io(
            code,
            super::node_error_message_fd(code, syscall, &error),
            syscall,
            None,
            &error,
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
        /// Inode-change time, or None if it could not be read.
        ctime: Option<Timespec>,
        /// A Windows reparse point's tag, read for an lstat of one (a
        /// symlink is IO_REPARSE_TAG_SYMLINK, a junction
        /// IO_REPARSE_TAG_MOUNT_POINT); None everywhere else.
        reparse_tag: Option<u32>,
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
            ctime: Some(Timespec {
                sec: meta.ctime(),
                nsec: meta.ctime_nsec(),
            }),
            reparse_tag: None,
        })
    }

    /// A stat time as libuv's `uv_timespec_t` holds it, from which node
    /// derives both forms (lib/internal/fs/utils.js, v22.22.2): `*Ms` is
    /// `sec * 1e3 + nsec / 1e6` in doubles, a BigIntStats' `*Ns` is
    /// `sec * 1e9 + nsec` exactly (#392). The times used to be sent as whole
    /// milliseconds (`as_millis`), which dropped the fraction node reports
    /// (1792020645055.304 came out 1792020645055), and a bigint form needs
    /// the nanoseconds anyway.
    #[derive(Clone, Copy, Default)]
    struct Timespec {
        sec: i64,
        nsec: i64,
    }

    impl Timespec {
        /// A `SystemTime` (std's view of the same field), as libuv's
        /// conversion of it would read. A time std cannot read is 0, as
        /// before.
        fn of(time: std::io::Result<std::time::SystemTime>) -> Self {
            let Ok(time) = time else {
                return Self::default();
            };
            // Nanoseconds since the epoch, signed: a time before it is a
            // negative offset rather than an error.
            let nanos: i128 = match time.duration_since(std::time::UNIX_EPOCH) {
                Ok(after) => after.as_nanos() as i128,
                Err(before) => -(before.duration().as_nanos() as i128),
            };
            #[cfg(windows)]
            {
                // std's SystemTime on Windows IS the FILETIME, so this is
                // exact in 100 ns ticks.
                Self::from_unix_ticks((nanos / 100) as i64)
            }
            #[cfg(not(windows))]
            {
                // The kernel's normalized timespec: nsec in [0, 1e9).
                Self {
                    sec: nanos.div_euclid(1_000_000_000) as i64,
                    nsec: nanos.rem_euclid(1_000_000_000) as i64,
                }
            }
        }

        /// A FILETIME (100 ns ticks since 1601-01-01).
        #[cfg(windows)]
        fn from_filetime(ticks: i64) -> Self {
            const WIN_TO_UNIX_TICK_OFFSET: i64 = 116_444_736_000_000_000;
            Self::from_unix_ticks(ticks - WIN_TO_UNIX_TICK_OFFSET)
        }

        /// libuv's `uv__filetime_to_timespec` (deps/uv/src/win/fs.c): the
        /// seconds land in a C `long`, which is 32 bits on Windows -- so a
        /// time past 2106 wraps, which node reports as it is (`utimesSync(f,
        /// 2 ** 32 + 0.5, ...)` then reads back a `mtimeMs` of 500).
        #[cfg(windows)]
        fn from_unix_ticks(ticks: i64) -> Self {
            const TICKS_PER_SEC: i64 = 10_000_000;
            let mut sec = (ticks / TICKS_PER_SEC) as i32;
            let mut nsec = (ticks % TICKS_PER_SEC) * 100;
            if nsec < 0 {
                sec = sec.wrapping_sub(1);
                nsec += 1_000_000_000;
            }
            Self {
                sec: i64::from(sec),
                nsec,
            }
        }

        /// The seconds as node's FillStatsArray stores them (src/node_file-
        /// inl.h): on Windows through a cast to C `unsigned long`, 32 bits
        /// there, so a time before the epoch reads as one after 2038;
        /// elsewhere as they are, negative before the epoch. The same value
        /// lands in the Float64Array of a Stats and the BigInt64Array of a
        /// BigIntStats.
        fn node_sec(self) -> i64 {
            #[cfg(windows)]
            {
                i64::from(self.sec as u32)
            }
            #[cfg(not(windows))]
            {
                self.sec
            }
        }

        /// node's `msFromTimeSpec`: the `*Ms` field of a `Stats`.
        fn node_ms(self) -> f64 {
            self.node_sec() as f64 * 1000.0 + self.nsec as f64 / 1_000_000.0
        }
        // A BigIntStats' `*Ns` (node's `nsFromTimeSpecBigInt`) is made on
        // the JS side, from the `node_sec` and `nsec` a StatRecord carries.
    }

    /// The information classes [`file_info`] reads and [`set_file_info`]
    /// writes: each is the struct GetFileInformationByHandleEx fills, and
    /// SetFileInformationByHandle reads, for its `CLASS`, which is what makes
    /// the one call of each sound for every implementor. Crate-private, so no
    /// other pairing can be made.
    #[cfg(windows)]
    pub(crate) trait FileInfoClass: Default {
        const CLASS: windows_sys::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS;
    }
    #[cfg(windows)]
    impl FileInfoClass for windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO {
        const CLASS: windows_sys::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS =
            windows_sys::Win32::Storage::FileSystem::FileBasicInfo;
    }
    #[cfg(windows)]
    impl FileInfoClass for windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO {
        const CLASS: windows_sys::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS =
            windows_sys::Win32::Storage::FileSystem::FileStandardInfo;
    }
    #[cfg(windows)]
    impl FileInfoClass for windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TAG_INFO {
        const CLASS: windows_sys::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS =
            windows_sys::Win32::Storage::FileSystem::FileAttributeTagInfo;
    }
    #[cfg(windows)]
    impl FileInfoClass for windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO {
        const CLASS: windows_sys::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS =
            windows_sys::Win32::Storage::FileSystem::FileDispositionInfo;
    }

    /// Set a handle's information of class `T` ([`FileInfoClass`]).
    #[cfg(windows)]
    pub(crate) fn set_file_info<T: FileInfoClass>(
        file: &std::fs::File,
        info: &T,
    ) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;
        // SAFETY: `file` owns its handle for the whole call, and `info` is a
        // readable T of exactly the size passed -- the struct the API reads
        // for T::CLASS (FileInfoClass pairs them).
        let ok = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle().cast::<std::ffi::c_void>(),
                T::CLASS,
                (info as *const T).cast(),
                std::mem::size_of::<T>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// libuv's fs__unlink for a directory link on Windows, on one handle so
    /// that what is checked is what is deleted: the reparse point itself is
    /// opened; a directory that is not a symlink or junction is refused
    /// EPERM (ERROR_ACCESS_DENIED, as POSIX has unlink refuse a directory);
    /// a read-only link is made writable; and the handle is marked for
    /// deletion. Checking the path and then removing it by path again could
    /// delete a real directory put there in between.
    #[cfg(windows)]
    pub(crate) fn unlink_directory_link(path: &std::path::Path) -> std::io::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
            FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FILE_WRITE_ATTRIBUTES,
        };
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
        const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
        let file = std::fs::OpenOptions::new()
            .access_mode(DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let basic = file_info::<FILE_BASIC_INFO>(&file)?;
        if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            let link = basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                && file_info::<FILE_ATTRIBUTE_TAG_INFO>(&file).is_ok_and(|tag| {
                    matches!(
                        tag.ReparseTag,
                        IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT
                    )
                });
            if !link {
                return Err(std::io::Error::from_raw_os_error(
                    ERROR_ACCESS_DENIED as i32,
                ));
            }
        }
        if basic.FileAttributes & FILE_ATTRIBUTE_READONLY != 0 {
            // Times of 0 are left as they are.
            let writable = FILE_BASIC_INFO {
                FileAttributes: (basic.FileAttributes & !FILE_ATTRIBUTE_READONLY)
                    | FILE_ATTRIBUTE_ARCHIVE,
                ..FILE_BASIC_INFO::default()
            };
            set_file_info(&file, &writable)?;
        }
        let delete = FILE_DISPOSITION_INFO { DeleteFile: true };
        if let Err(failed) = set_file_info(&file, &delete) {
            if basic.FileAttributes & FILE_ATTRIBUTE_READONLY != 0 {
                let restored = FILE_BASIC_INFO {
                    FileAttributes: basic.FileAttributes,
                    ..FILE_BASIC_INFO::default()
                };
                let _ = set_file_info(&file, &restored);
            }
            return Err(failed);
        }
        Ok(())
    }

    /// A handle's information of class `T`, or why it has none: a device
    /// that keeps no file information (a console) fails
    /// ERROR_INVALID_FUNCTION.
    #[cfg(windows)]
    pub(crate) fn file_info<T: FileInfoClass>(file: &std::fs::File) -> std::io::Result<T> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
        let mut out = T::default();
        // SAFETY: `file` owns its handle for the whole call, and `out` is a
        // writable T of exactly the size passed -- the struct the API fills
        // for T::CLASS (FileInfoClass pairs them).
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle().cast::<std::ffi::c_void>(),
                T::CLASS,
                (&mut out as *mut T).cast(),
                std::mem::size_of::<T>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(out)
    }

    /// A handle's FILE_STANDARD_INFO ([`file_info`]).
    #[cfg(windows)]
    fn standard_info(
        file: &std::fs::File,
    ) -> std::io::Result<windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO> {
        file_info(file)
    }

    #[cfg(windows)]
    fn stat_extras(meta: &std::fs::Metadata, source: StatSource<'_>) -> Option<StatExtras> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
            FILE_BASIC_INFO, GetFileInformationByHandle,
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

        // `blocks` counts 512-byte units of ALLOCATED space, which is not
        // ceil(size / 512): a small file resident in the MFT allocates none,
        // and node duly reports 0 blocks for it.
        let blocks =
            standard_info(file).map_or(0, |standard| (standard.AllocationSize as u64) >> 9);

        let basic = file_info::<FILE_BASIC_INFO>(file).ok();
        let ctime = basic.map(|basic| Timespec::from_filetime(basic.ChangeTime));
        // An lstat of a reparse point (the handle was opened on the point
        // itself): which kind it is. fs.cpSync copies a junction as the
        // directory it leads to, a symlink as a link, as node's MSVC
        // std::filesystem tells them apart (#390).
        let reparse_tag = match (source, basic) {
            (StatSource::Path(_), Some(basic))
                if meta.is_symlink()
                    && basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 =>
            {
                file_info::<FILE_ATTRIBUTE_TAG_INFO>(file)
                    .ok()
                    .map(|tag| tag.ReparseTag)
            }
            _ => None,
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
            ctime,
            reparse_tag,
        })
    }

    /// Opens a handle suitable for stat: attributes only, and able to open a
    /// DIRECTORY (which needs BACKUP_SEMANTICS) or a symlink itself rather than
    /// its target (OPEN_REPARSE_POINT).
    #[cfg(windows)]
    pub(crate) fn open_for_stat(
        path: &str,
        keep_reparse_point: bool,
    ) -> std::io::Result<std::fs::File> {
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

    /// stat/lstat of a path, as the record the JS side builds its Stats from.
    ///
    /// Blocking; async callers must run it on a blocking thread.
    pub fn stat_path_record(path: &str, lstat: bool) -> std::io::Result<StatRecord> {
        #[cfg(windows)]
        return stat_path_record_std(path, lstat)
            .or_else(|failed| as_libuv_stats(path, lstat, failed));
        #[cfg(not(windows))]
        stat_path_record_std(path, lstat)
    }

    /// What libuv's stat makes of a path std could not stat
    /// (`win_fs::stat_verdict`): NUL's fixed shape, libuv's own error, or
    /// std's. Not asked when std found nothing there -- the answer libuv
    /// gives such a path as it is, and the one an existence probe meets
    /// most -- so a missing path costs no more than it did.
    #[cfg(windows)]
    pub(crate) fn as_libuv_stats(
        path: &str,
        lstat: bool,
        failed: std::io::Error,
    ) -> std::io::Result<StatRecord> {
        use crate::win_fs::{StatVerdict, stat_verdict};
        use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
        if failed.raw_os_error().is_some_and(|code| {
            code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PATH_NOT_FOUND as i32
        }) {
            return Err(failed);
        }
        match stat_verdict(path, lstat) {
            StatVerdict::NullDevice => Ok(null_device_record()),
            StatVerdict::Failed(error) => Err(error),
            StatVerdict::Unexplained => Err(failed),
        }
    }

    /// The stat libuv gives NUL, by path or by descriptor
    /// (`fs__stat_assign_statbuf_null`, node v22.22.2): a character device,
    /// mode 0o20666, nlink 1, blksize 4096, rdev FILE_DEVICE_NULL << 16,
    /// every other number and every time 0.
    #[cfg(windows)]
    fn null_device_record() -> StatRecord {
        let rdev = u64::from(crate::win_fs::FILE_DEVICE_NULL) << 16;
        // dev, nlink, uid, gid, rdev, blksize, ino, blocks.
        StatRecord::new(
            EntryKind::File,
            0o020666,
            0,
            Some([0, 1, 0, 0, rdev, 4096, 0, 0]),
            [Timespec::default(); 4],
            None,
        )
    }

    fn stat_path_record_std(path: &str, lstat: bool) -> std::io::Result<StatRecord> {
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
            return Ok(stat_record(&meta, StatSource::File(&file)));
        }
        let meta = if lstat {
            std::fs::symlink_metadata(path)?
        } else {
            std::fs::metadata(path)?
        };
        Ok(stat_record(&meta, StatSource::Path(path)))
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

    /// What an entry is, as far as the JS side's kind checks ask: a
    /// directory, a symbolic link, or anything else (a "file"; a stat's
    /// `mode` says which of the others). The discriminant is what a
    /// [`StatRecord`] carries at [`STAT_KIND`], and the order of the names
    /// the JS side maps it through.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[repr(u8)]
    pub enum EntryKind {
        File = 0,
        Dir = 1,
        Symlink = 2,
    }

    impl EntryKind {
        fn of(file_type: std::fs::FileType) -> Self {
            if file_type.is_symlink() {
                EntryKind::Symlink
            } else if file_type.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            }
        }

        /// The name the JS side gives this kind (a Dirent's `_kind`).
        pub fn name(self) -> &'static str {
            match self {
                EntryKind::File => "file",
                EntryKind::Dir => "dir",
                EntryKind::Symlink => "symlink",
            }
        }
    }

    /// How many doubles a [`StatRecord`] holds: node's fourteen Stats values
    /// in its constructor's order (dev, mode, nlink, uid, gid, rdev, blksize,
    /// ino, size, blocks, atimeMs, mtimeMs, ctimeMs, birthtimeMs), then
    /// [`STAT_KIND`], [`STAT_IDS_KNOWN`] and [`STAT_REPARSE_TAG`]. The JS
    /// side's `STAT_*` constants mirror these.
    pub const STAT_NUMBERS: usize = 17;
    /// Where a [`StatRecord`] keeps its [`EntryKind`].
    pub const STAT_KIND: usize = 14;
    /// 1 when the eight fields `std::fs::Metadata` does not carry (dev, ino,
    /// nlink, uid, gid, rdev, blksize, blocks) were read, 0 when they could
    /// not be -- the Stats then has none of them, rather than zeros.
    pub const STAT_IDS_KNOWN: usize = 15;
    /// A Windows reparse point's tag, read for an lstat of one (a symlink is
    /// IO_REPARSE_TAG_SYMLINK, a junction IO_REPARSE_TAG_MOUNT_POINT); -1
    /// everywhere else.
    pub const STAT_REPARSE_TAG: usize = 16;
    /// How many 64-bit integers a [`StatRecord`] holds: the ten integer
    /// fields again, exact, then each time's seconds and nanoseconds
    /// (atime, mtime, ctime, birthtime), from which a BigIntStats' `*Ns` is
    /// `sec * 1e9 + nsec`, as node's `nsFromTimeSpecBigInt` makes it.
    pub const STAT_EXACT: usize = 18;

    /// A stat as node's binding hands it to JS: what its `statValues`
    /// Float64Array and `bigintStatValues` BigInt64Array hold
    /// (src/node_file-inl.h `FillStatsArray`), from which the JS side
    /// constructs a Stats or a BigIntStats.
    ///
    /// The engine copies the two arrays into one ArrayBuffer -- the doubles,
    /// then the integers -- and the JS side reads them through typed-array
    /// views. It replaced a JSON payload, which the native serialized and V8
    /// parsed back on every stat at more than the cost of the stat itself on
    /// a warm file. The doubles are the numbers node reports, the `*Ms` ones
    /// computed here in the same double arithmetic as its `msFromTimeSpec`;
    /// the integers are exact where a double is not: an NTFS file ID is often
    /// past 2^53, and a nanosecond time always is (#392).
    ///
    /// node's BigInt64Array reads an unsigned field through int64
    /// (`static_cast<NativeT>`), and so do the integers here.
    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct StatRecord {
        pub numbers: [f64; STAT_NUMBERS],
        pub exact: [i64; STAT_EXACT],
    }

    impl StatRecord {
        /// Lays out one stat. `ids` are dev, nlink, uid, gid, rdev, blksize,
        /// ino, blocks -- None when this platform could not read them.
        fn new(
            kind: EntryKind,
            mode: u64,
            size: u64,
            ids: Option<[u64; 8]>,
            times: [Timespec; 4],
            reparse_tag: Option<u32>,
        ) -> Self {
            let [dev, nlink, uid, gid, rdev, blksize, ino, blocks] = ids.unwrap_or_default();
            let integers = [dev, mode, nlink, uid, gid, rdev, blksize, ino, size, blocks];
            let mut numbers = [0.0; STAT_NUMBERS];
            let mut exact = [0; STAT_EXACT];
            for (at, value) in integers.into_iter().enumerate() {
                // Rounded to the nearest double, as the u64 node stores into
                // its Float64Array is.
                numbers[at] = value as f64;
                exact[at] = value as i64;
            }
            for (at, time) in times.into_iter().enumerate() {
                numbers[10 + at] = time.node_ms();
                exact[10 + 2 * at] = time.node_sec();
                exact[11 + 2 * at] = time.nsec;
            }
            numbers[STAT_KIND] = f64::from(kind as u8);
            numbers[STAT_IDS_KNOWN] = if ids.is_some() { 1.0 } else { 0.0 };
            numbers[STAT_REPARSE_TAG] = reparse_tag.map_or(-1.0, f64::from);
            StatRecord { numbers, exact }
        }

        /// The entry's kind (the record's [`STAT_KIND`]).
        pub fn kind(&self) -> EntryKind {
            match self.numbers[STAT_KIND] as u8 {
                1 => EntryKind::Dir,
                2 => EntryKind::Symlink,
                _ => EntryKind::File,
            }
        }
    }

    /// The stat of `meta`, with the fields std does not carry read off
    /// `source`; shared by stat, lstat and fstat, sync and async.
    pub fn stat_record(meta: &std::fs::Metadata, source: StatSource<'_>) -> StatRecord {
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
        // libuv reports a directory's size as 0 on Windows whatever the file
        // system says; a drive root's is its index allocation (12288 on NTFS)
        // where node's `statSync("C:\\").size` is 0.
        #[cfg(windows)]
        let size = if meta.is_dir() { 0 } else { meta.len() };
        #[cfg(not(windows))]
        let size = meta.len();
        let atime = Timespec::of(meta.accessed());
        let mtime = Timespec::of(meta.modified());
        // Falls back to mtime only when the real change time could not be
        // read; the two genuinely differ (chmod moves ctime, not mtime).
        let ctime = extras
            .as_ref()
            .and_then(|extras| extras.ctime)
            .unwrap_or(mtime);
        let birthtime = Timespec::of(meta.created());
        StatRecord::new(
            EntryKind::of(meta.file_type()),
            u64::from(mode),
            size,
            extras.as_ref().map(|extras| {
                [
                    extras.dev,
                    extras.nlink,
                    extras.uid,
                    extras.gid,
                    extras.rdev,
                    extras.blksize,
                    extras.ino,
                    extras.blocks,
                ]
            }),
            [atime, mtime, ctime, birthtime],
            extras.and_then(|extras| extras.reparse_tag),
        )
    }

    /// The order libuv's scandir hands node's `readdir`: on unix sorted by
    /// name, bytewise (src/unix/fs.c `uv__fs_scandir_sort` is strcmp); on
    /// Windows the file system's own, which NTFS already keeps sorted by
    /// its own collation, so nothing is imposed there.
    #[cfg(not(windows))]
    fn sort_as_scandir(found: &mut [(std::ffi::OsString, EntryKind)]) {
        use std::os::unix::ffi::OsStrExt;
        found.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    }
    #[cfg(windows)]
    fn sort_as_scandir(_found: &mut [(std::ffi::OsString, EntryKind)]) {}

    /// What libuv reports where std's `read_dir` (FindFirstFileW on
    /// `path\*`) failed. std fails ERROR_DIRECTORY for anything that is no
    /// directory -- a file, a link of the wrong kind -- and libuv's table reads
    /// that as `ENOENT`; each of libuv's two listings looks at the path its
    /// own way first (src/win/fs.c, v1.51.0), and node reports that:
    ///
    /// - fs__scandir (`readdir`) OPENS the path, with
    ///   FILE_FLAG_BACKUP_SEMANTICS and following links, and reports that
    ///   open's failure: `EPERM` for a file symlink to a directory, `ENOENT`
    ///   for a directory symlink or junction to a file, and for a file named
    ///   with a trailing separator (unless one of its directories is a link).
    ///   What opens and is no directory fails the listing: `ENOTDIR`, which
    ///   is also the answer for a root that is no disk (`\\srv\IPC$`). The
    ///   open here is the bare one, not `open_path`: that refusal is
    ///   `fs.open`'s.
    /// - fs__opendir (`raw`) lists an empty path as the current directory;
    ///   that one is the caller's to arrange (node_compat's `opendirPath`).
    /// - fs__opendir (`raw`) reads the path's own attributes, a link's
    ///   rather than its target's, and reports `ENOTDIR` when they are not a
    ///   directory's -- a file symlink, wherever it leads. Attributes that
    ///   cannot be read pass, and so does a directory link to a file: both
    ///   fail the listing itself, the table's `ENOENT`.
    ///
    /// Asked only once the listing has failed, so a listing that works pays
    /// nothing -- except for a path that is a prefix with nothing under it
    /// (`listed_before_opened`). Measured on node v22.22.2: conformance case
    /// 393.
    #[cfg(windows)]
    fn listing_error(path: &str, raw: bool, error: std::io::Error) -> std::io::Error {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
        let not_a_directory = || std::io::Error::from(std::io::ErrorKind::NotADirectory);
        if raw {
            return match std::fs::symlink_metadata(path) {
                Ok(own) if own.file_attributes() & FILE_ATTRIBUTE_DIRECTORY == 0 => {
                    not_a_directory()
                }
                _ => error,
            };
        }
        match scandir_open(path) {
            Err(refused) => refused,
            Ok(()) => error,
        }
    }

    /// fs__scandir's open of `path` and its first question of what opened,
    /// the directory query itself (`win_fs::directory_query`): the open's
    /// own failure; `ENOTDIR` for what the query refuses
    /// STATUS_INVALID_PARAMETER (a file, the pipe namespace `\\.\pipe`);
    /// what any other handle says when it cannot answer (a device: `\\.\NUL`
    /// and KsecDD are STATUS_INVALID_DEVICE_REQUEST, ERROR_INVALID_FUNCTION,
    /// the table's `EISDIR`, which is what node reports); and nothing for a
    /// directory.
    #[cfg(windows)]
    fn scandir_open(path: &str) -> std::io::Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_LIST_DIRECTORY, SYNCHRONIZE};
        // libuv's access, not a read: a console (`\\.\CON`) refuses a
        // directory listing's open ERROR_INVALID_PARAMETER, the table's
        // `EINVAL`, where a read open would succeed.
        let mut options = std::fs::OpenOptions::new();
        options.access_mode(FILE_LIST_DIRECTORY | SYNCHRONIZE);
        let opened = super::opens_as_libuv(&mut options).open(path)?;
        crate::win_fs::directory_query(&opened)
    }

    /// A path std lists that libuv's scandir refuses (#332): a prefix with
    /// nothing under it, which names a volume or a device namespace rather
    /// than a directory -- `\\?\C:`, `\\.\C:`, `\\.\pipe`. std lists
    /// `path\*`, which is the root directory under it; libuv opens the path
    /// itself, and a volume refuses that open (`EPERM`) and the pipe
    /// namespace is no directory (`ENOTDIR`). With a separator after it, or
    /// a plain drive (`C:`, that drive's current directory), the two agree,
    /// and a UNC share is a directory either way.
    #[cfg(windows)]
    fn listed_before_opened(path: &str) -> bool {
        // Read off the string: std's Path gives a device prefix a root
        // directory it does not spell, so it cannot tell `\\.\C:` from
        // `\\.\C:\`.
        let rest = path
            .strip_prefix(r"\\?\")
            .or_else(|| path.strip_prefix(r"\\.\"));
        rest.is_some_and(|rest| !rest.is_empty() && !rest.contains(['\\', '/']))
    }

    /// A directory's entries as two parallel lists, the shape node's
    /// binding hands readdir's JS (`[names, types]` with `withFileTypes`):
    /// the engine turns it into an array of names and an array of kind
    /// names, so a plain readdir returns the first as it is and only a
    /// Dirent listing walks both. It replaced a JSON array of `{name, kind}`
    /// objects, which the native serialized and V8 parsed back -- at 2.8x
    /// node's cost for a 2000-entry directory.
    #[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct DirListing {
        pub names: Vec<String>,
        pub kinds: Vec<EntryKind>,
    }

    /// The entries of `path` as node hands them out: `readdir`'s come from
    /// libuv's scandir, sorted on unix (`sort_as_scandir`); `raw` asks for
    /// the file system's own order, which is what `opendir`'s Dir reads
    /// through uv_fs_readdir on every platform. Measured on node v22.23 on
    /// macOS (APFS returns `a, top.txt, lnk, c, b`; readdir answers
    /// `a, b, c, lnk, top.txt`) and Linux.
    pub fn read_dir_listing(path: &str, raw: bool) -> std::io::Result<DirListing> {
        let mut found = Vec::new();
        // readdir of a path std would list and libuv refuses: refused here
        // first, as libuv's open refuses it.
        #[cfg(windows)]
        if !raw && listed_before_opened(path) {
            scandir_open(path)?;
        }
        #[cfg(windows)]
        let listing = match std::fs::read_dir(path) {
            // A directory std cannot list and libuv's query finds nothing in:
            // the mailslot namespace, `\\.\mailslot\`, which has no entries
            // at all -- not even `.` and `..` -- and takes no `*` pattern
            // (FindFirstFileW: ERROR_INVALID_NAME; an empty directory of that
            // kind would be ERROR_FILE_NOT_FOUND). It opens as libuv opens a
            // directory, and readdir gives [].
            Err(e)
                if !raw
                    && e.raw_os_error().is_some_and(|code| {
                        use windows_sys::Win32::Foundation::{
                            ERROR_FILE_NOT_FOUND, ERROR_INVALID_NAME,
                        };
                        code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_INVALID_NAME as i32
                    })
                    && scandir_open(path).is_ok() =>
            {
                return Ok(DirListing::default());
            }
            listing => listing.map_err(|e| listing_error(path, raw, e))?,
        };
        #[cfg(not(windows))]
        let listing = std::fs::read_dir(path)?;
        for entry in listing {
            let entry = entry?;
            // An entry whose type cannot be read is a "file", as before.
            let kind = entry.file_type().map_or(EntryKind::File, EntryKind::of);
            found.push((entry.file_name(), kind));
        }
        if !raw {
            sort_as_scandir(&mut found);
        }
        let mut listed = DirListing {
            names: Vec::with_capacity(found.len()),
            kinds: Vec::with_capacity(found.len()),
        };
        for (name, kind) in found {
            listed.names.push(name.to_string_lossy().into_owned());
            listed.kinds.push(kind);
        }
        Ok(listed)
    }

    pub async fn fs_read_file(path: super::FsPath) -> OpOutcome {
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
                && let Ok(bytes) = uring.read_file(path.os().to_string()).await
            {
                return OpOutcome::Bytes(bytes);
            }
        }
        // One blocking-pool hop, as tokio::fs::read makes, but with the open
        // and the read reported apart (super::whole_file_error).
        let owned = path.clone();
        let result = tokio::task::spawn_blocking(move || super::read_whole_file(owned.os()))
            .await
            .unwrap_or_else(|e| Err(super::WholeFileError::Open(std::io::Error::other(e))));
        match result {
            Ok(bytes) => OpOutcome::Bytes(bytes),
            Err(e) => node_fail_whole_file(super::FsSite::ReadFile, e, &path),
        }
    }

    pub async fn fs_write_file(
        path: super::FsPath,
        data: Vec<u8>,
        append: bool,
        mode: u32,
    ) -> OpOutcome {
        // io_uring fast path (Linux, opt-in): non-append writes only (create +
        // truncate + write_all_at). `data` is moved in -- no clone on the happy
        // path. On a worker-channel failure write_file hands the un-consumed
        // `data` back (a non-empty Vec) so we fall through to the std path with
        // it; a genuine io error from the worker comes back with an empty Vec
        // and is surfaced directly (node_fail), matching the read path's
        // "io_uring is a pure optimization, never a correctness dependency"
        // contract. Append, the no-io_uring case and a `mode` other than the
        // default (the ring's open creates with 0o666) use the std path below.
        #[cfg(target_os = "linux")]
        {
            if !append
                && mode == 0o666
                && let Some(uring) = crate::io_uring_fs::global()
            {
                match uring.write_file(path.os().to_string(), data).await {
                    Ok(()) => return OpOutcome::Done,
                    // Channel failure with the buffer recovered: retry via std.
                    Err((_chan_err, recovered)) if !recovered.is_empty() => {
                        return fs_write_file_std(path, recovered, append, mode).await;
                    }
                    // Genuine io error (empty buffer), or an unrecoverable
                    // channel failure where the data is gone: surface directly.
                    Err((e, _)) => {
                        return node_fail_whole_file(super::FsSite::WriteFile, e, &path);
                    }
                }
            }
        }
        fs_write_file_std(path, data, append, mode).await
    }

    /// std write path: one blocking-pool hop, as `tokio::fs::write` makes,
    /// with the open and the write reported apart (`super::whole_file_error`).
    /// Factored out so the io_uring fast path can fall through to it with a
    /// recovered buffer on a worker-channel failure.
    async fn fs_write_file_std(
        path: super::FsPath,
        data: Vec<u8>,
        append: bool,
        mode: u32,
    ) -> OpOutcome {
        let owned = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            super::write_whole_file(owned.os(), &data, append, mode)
        })
        .await
        .unwrap_or_else(|e| Err(super::WholeFileError::Open(std::io::Error::other(e))));
        let site = if append {
            super::FsSite::AppendFile
        } else {
            super::FsSite::WriteFile
        };
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail_whole_file(site, e, &path),
        }
    }

    pub async fn fs_stat(path: super::FsPath, lstat: bool) -> OpOutcome {
        // One hop to a blocking thread for the whole operation. Awaiting
        // tokio's metadata and THEN opening a handle here would do the second
        // (blocking) open on a runtime thread.
        let owned = path.clone();
        let result = tokio::task::spawn_blocking(move || stat_path_record(owned.os(), lstat)).await;
        match result {
            Ok(Ok(record)) => OpOutcome::Stat(Box::new(record)),
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
    /// Takes the descriptor's shared handle rather than the registry, so the
    /// caller never holds the file-registry lock across the blocking read.
    pub async fn fs_fstat(file: super::OpenFile) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || fstat_record(&file)).await;
        match result {
            Ok(Ok(record)) => OpOutcome::Stat(Box::new(record)),
            Ok(Err(e)) => node_fail_fd_op(e, "fstat"),
            Err(e) => node_fail_fd_op(std::io::Error::other(e.to_string()), "fstat"),
        }
    }

    /// The stat of an open descriptor, as libuv's fstat builds it --
    /// shared by the sync and async fstat.
    ///
    /// On Windows a descriptor need not be a disk file: 0-2 are often a pipe,
    /// the console or NUL, which have no file information to read (asking
    /// failed, and NUL's answer was EISDIR). libuv's `fs__fstat_handle` /
    /// `fs__stat_assign_statbuf_null` (node v22.22.2) give them fixed shapes,
    /// reproduced here: a pipe is S_IFIFO with rdev FILE_DEVICE_NAMED_PIPE << 16,
    /// the console S_IFCHR with FILE_DEVICE_CONSOLE << 16 (both nlink 1 and the
    /// handle as ino), and any other character device NUL's S_IFCHR | 0o666,
    /// blksize 4096, rdev FILE_DEVICE_NULL << 16 -- libuv asks the device type
    /// and gives that shape to NUL alone; a serial port, the one other
    /// character device a process plausibly holds, gets NUL's here. Every other
    /// field and every time is 0.
    ///
    /// "The console" is a console handle libuv can see is one, which takes
    /// read access (GetConsoleMode). A console opened to write only is, to
    /// libuv, a file: it asks for the file information, which a console has
    /// none of, and fails ERROR_INVALID_FUNCTION -- the table's `EISDIR`, what
    /// node reports for `fstat` of `openSync("\\\\.\\CON", "w")` (#331). The
    /// same question tells NUL, which answers it, from such a console.
    pub fn fstat_record(file: &std::fs::File) -> std::io::Result<StatRecord> {
        #[cfg(windows)]
        {
            use std::io::IsTerminal;
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_PIPE};
            // FILE_DEVICE_* from winioctl.h.
            const FILE_DEVICE_CONSOLE: u64 = 0x50;
            const FILE_DEVICE_NAMED_PIPE: u64 = 0x11;
            let handle = file.as_raw_handle();
            let device = match crate::child_win::file_type(handle) {
                FILE_TYPE_CHAR if file.is_terminal() => {
                    Some((0o020000, FILE_DEVICE_CONSOLE, handle as u64))
                }
                // NUL: the shape a path stat gives it too.
                FILE_TYPE_CHAR => {
                    standard_info(file)?;
                    return Ok(null_device_record());
                }
                FILE_TYPE_PIPE => Some((0o010000, FILE_DEVICE_NAMED_PIPE, handle as u64)),
                _ => None,
            };
            if let Some((mode, device_type, ino)) = device {
                let rdev = device_type << 16;
                // dev, nlink, uid, gid, rdev, blksize, ino, blocks; every
                // time 0.
                return Ok(StatRecord::new(
                    EntryKind::File,
                    mode,
                    0,
                    Some([0, 1, 0, 0, rdev, 0, ino, 0]),
                    [Timespec::default(); 4],
                    None,
                ));
            }
        }
        file.metadata()
            .map(|meta| stat_record(&meta, StatSource::File(file)))
    }

    pub async fn fs_statfs(path: super::FsPath) -> OpOutcome {
        let owned = path.clone();
        let result = tokio::task::spawn_blocking(move || statfs_json(owned.os())).await;
        match result {
            Ok(Ok(json)) => OpOutcome::Json(json),
            Ok(Err(e)) => node_fail(e, "statfs", &path),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), "statfs", &path),
        }
    }

    pub async fn fs_readdir(path: super::FsPath, raw: bool) -> OpOutcome {
        match tokio::task::spawn_blocking({
            let path = path.clone();
            move || read_dir_listing(path.os(), raw)
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        {
            Ok(listing) => OpOutcome::Listing(listing),
            // node's Dir is opened by uv_fs_opendir, and says so.
            Err(e) => node_fail(e, if raw { "opendir" } else { "scandir" }, &path),
        }
    }

    /// `fs.mkdir` and `fs.promises.mkdir`: resolves with the first directory
    /// a recursive call made, if it made one (`super::make_dir`). A failure
    /// names the component that could not be made, as node's asynchronous
    /// mkdir does; the synchronous one names the path asked for.
    pub async fn fs_mkdir(path: super::FsPath, recursive: bool, mode: u32) -> OpOutcome {
        let owned = path.clone();
        let result =
            tokio::task::spawn_blocking(move || super::make_dir(owned.os(), mode, recursive, true))
                .await;
        let site = super::FsSite::Mkdir { recursive };
        match result {
            Ok(Ok(Some(first))) => OpOutcome::Text(first),
            Ok(Ok(None)) => OpOutcome::Done,
            Ok(Err(failure)) => node_fail_at(
                site,
                failure.error,
                "mkdir",
                &super::FsPath::from_os(failure.at),
            ),
            Err(e) => node_fail_at(site, std::io::Error::other(e.to_string()), "mkdir", &path),
        }
    }

    pub async fn fs_rm(path: super::FsPath, recursive: bool, force: bool) -> OpOutcome {
        let result = tokio::task::spawn_blocking({
            let path = path.clone();
            move || super::remove_path(path.os(), recursive)
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => OpOutcome::Done,
            Err(e) => node_fail(e, "rm", &path),
        }
    }

    pub async fn fs_unlink(path: super::FsPath) -> OpOutcome {
        let target = path.as_ref().to_path_buf();
        let removed = tokio::task::spawn_blocking(move || super::unlink_path(&target))
            .await
            .unwrap_or_else(|joined| Err(std::io::Error::other(joined)));
        match removed {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "unlink", &path),
        }
    }

    pub async fn fs_rename(from: super::FsPath, to: super::FsPath) -> OpOutcome {
        match tokio::fs::rename(&from, &to).await {
            Ok(()) => OpOutcome::Done,
            Err(e) => {
                let (path, dest) = (from.shown(), to.shown());
                node_fail_dest(e, "rename", &path, &dest)
            }
        }
    }

    /// node's async `copyFile`; `mode` as `super::copy_file` takes it.
    pub async fn fs_copy_file(from: super::FsPath, to: super::FsPath, mode: u32) -> OpOutcome {
        let result =
            tokio::task::spawn_blocking(move || match super::copy_file(&from, &to, mode) {
                Ok(()) => OpOutcome::Done,
                Err(e) => {
                    let (path, dest) = (from.shown(), to.shown());
                    node_fail_dest(e, "copyfile", &path, &dest)
                }
            })
            .await;
        result.unwrap_or_else(|e| OpOutcome::Failed(format!("copyfile: {e}")))
    }

    pub async fn fs_access(path: super::FsPath, mode: i32) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || match super::check_access(&path, mode) {
            Ok(()) => OpOutcome::Done,
            Err((code, message, errno, win_error)) => {
                let shown = path.shown();
                OpOutcome::node_failed_at(code, message, "access", Some(&*shown), errno)
                    .with_win_error(win_error)
            }
        })
        .await;
        result.unwrap_or_else(|e| OpOutcome::Failed(format!("access: {e}")))
    }

    pub async fn fs_realpath(path: super::FsPath) -> OpOutcome {
        match tokio::fs::canonicalize(&path).await {
            Ok(real) => OpOutcome::Text(super::strip_unc_prefix(&real)),
            Err(e) => node_fail(e, "realpath", &path),
        }
    }

    /// node's `mkdtemp` on the blocking pool: `template` comes from
    /// `mkdtemp_template`, built ONCE by the op layer so the template it
    /// permission-checked is the one created from (node checks the same
    /// template, X's and all). The work is `super::mkdtemp`, shared with the
    /// sync op.
    pub async fn fs_mkdtemp(template: String) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || super::mkdtemp(&template)).await;
        match result {
            Ok(Ok(dir)) => OpOutcome::Text(dir),
            Ok(Err((e, path))) => node_fail_as_passed(e, "mkdtemp", &path),
            Err(e) => OpOutcome::Failed(format!("mkdtemp: {e}")),
        }
    }

    /// node's async `symlink`: `target` is the string the link stores
    /// (`symlink_stored_target`), `kind` the type the call gave. On Windows,
    /// with no type, `probe` is the path statted to pick a directory or a
    /// file link (`symlink_probe_path`), `None` when the read grant refuses
    /// it; as in node's callback and promise forms, a probe that cannot be
    /// statted makes a file link. A given type is never probed. Elsewhere
    /// neither matters.
    pub async fn fs_symlink(
        target: String,
        kind: super::SymlinkType,
        probe: Option<super::FsPath>,
        path: super::FsPath,
    ) -> OpOutcome {
        #[cfg(windows)]
        let result = {
            let is_dir = match (kind, &probe) {
                (super::SymlinkType::Probe, Some(probe)) => tokio::fs::metadata(probe)
                    .await
                    .map(|m| m.is_dir())
                    .unwrap_or(false),
                (super::SymlinkType::Probe, None) | (super::SymlinkType::File, _) => false,
                (super::SymlinkType::Dir | super::SymlinkType::Junction, _) => true,
            };
            if is_dir {
                tokio::fs::symlink_dir(&target, &path).await
            } else {
                tokio::fs::symlink_file(&target, &path).await
            }
        };
        #[cfg(not(windows))]
        let result = {
            let _ = (kind, probe);
            tokio::fs::symlink(&target, &path).await
        };
        match result {
            Ok(()) => OpOutcome::Done,
            // The target as stored, strip-only, and the link's own path.
            Err(e) => node_fail_dest(e, "symlink", &super::fs_shown_path(&target), &path.shown()),
        }
    }

    pub async fn fs_readlink(path: super::FsPath) -> OpOutcome {
        match tokio::fs::read_link(&path).await {
            Ok(target) => OpOutcome::Text(super::strip_unc_prefix(&target)),
            Err(e) => node_fail_at(super::FsSite::Readlink, e, "readlink", &path),
        }
    }

    pub async fn fs_link(existing: super::FsPath, new_path: super::FsPath) -> OpOutcome {
        match tokio::fs::hard_link(&existing, &new_path).await {
            Ok(()) => OpOutcome::Done,
            Err(e) => {
                let path = existing.shown();
                node_fail_dest(e, "link", &path, &new_path.shown())
            }
        }
    }

    pub async fn fs_chmod(path: super::FsPath, mode: u32) -> OpOutcome {
        let owned = path.clone();
        match tokio::task::spawn_blocking(move || super::chmod_path(&owned, mode))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "chmod", &path),
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
    ///
    /// On Windows the attribute is not set on the descriptor's own handle. As
    /// libuv's fs__fchmod does (src/win/fs.c, v1.51.0), the handle is REOPENED
    /// for FILE_WRITE_ATTRIBUTES and the attribute set on that one, so what
    /// the descriptor was opened for does not matter: a handle opened `r` has
    /// no FILE_WRITE_ATTRIBUTES, and setting it there failed `EPERM` where
    /// node changes the mode (#322). What cannot be reopened fails there with
    /// node's code -- a directory ERROR_ACCESS_DENIED (`EPERM`), an anonymous
    /// pipe ERROR_PIPE_BUSY (`EBUSY`) -- and the NUL device reopens and has
    /// no basic information to read: ERROR_INVALID_PARAMETER (`EINVAL`).
    ///
    /// The rest is libuv's sequence too, on the file's basic information
    /// read whole and written back whole:
    ///
    /// - the times go back as they were read, so the change time does not
    ///   move (zeroes, which std's `set_permissions` writes, let the file
    ///   system stamp a new one);
    /// - an attribute word that comes to 0 is written as
    ///   FILE_ATTRIBUTE_NORMAL: 0 means "leave them as they are", so clearing
    ///   read-only where it was the only attribute did nothing, and said it
    ///   had;
    /// - a file without the archive bit is given it for the change and has
    ///   it taken away again, which libuv does because some file systems
    ///   ignore a read-only change without it.
    ///
    /// Measured on node v22.22.2: conformance case 395.
    fn fchmod_file(file: &std::fs::File, mode: u32) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
            };
            let reopened = reopen_for_attributes(file)?;
            let mut info = basic_info(&reopened)?;
            let archive_was_clear = info.FileAttributes & FILE_ATTRIBUTE_ARCHIVE == 0;
            if archive_was_clear {
                info.FileAttributes |= FILE_ATTRIBUTE_ARCHIVE;
                set_basic_info(&reopened, &info)?;
            }
            if mode & 0o200 != 0 {
                info.FileAttributes &= !FILE_ATTRIBUTE_READONLY;
            } else {
                info.FileAttributes |= FILE_ATTRIBUTE_READONLY;
            }
            set_basic_info(&reopened, &info)?;
            if archive_was_clear {
                info.FileAttributes &= !FILE_ATTRIBUTE_ARCHIVE;
                if info.FileAttributes == 0 {
                    info.FileAttributes = FILE_ATTRIBUTE_NORMAL;
                }
                set_basic_info(&reopened, &info)?;
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let mut perms = file.metadata()?.permissions();
            perms.set_readonly(mode & 0o200 == 0);
            file.set_permissions(perms)
        }
    }

    /// A second handle on the file `file` has open, with
    /// FILE_WRITE_ATTRIBUTES and nothing else: libuv's
    /// `ReOpenFile(handle, FILE_WRITE_ATTRIBUTES, 0, 0)`. Asking for no data
    /// access, it shares with every handle already open, whatever the share
    /// mode says.
    #[cfg(windows)]
    fn reopen_for_attributes(file: &std::fs::File) -> std::io::Result<std::fs::File> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{FILE_WRITE_ATTRIBUTES, ReOpenFile};
        // SAFETY: `file` is borrowed for the call, so its handle is open; the
        // other arguments are plain values. ReOpenFile makes a new handle and
        // leaves the one it is given as it was.
        let handle = unsafe { ReOpenFile(file.as_raw_handle(), FILE_WRITE_ATTRIBUTES, 0, 0) };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `handle` is the open handle ReOpenFile just returned (it is
        // not INVALID_HANDLE_VALUE), nothing else has seen it, and the File
        // takes the one ownership of it.
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }

    /// The file's times and attributes: FILE_BASIC_INFO, what libuv reads
    /// with NtQueryInformationFile(FileBasicInformation).
    #[cfg(windows)]
    pub(super) fn basic_info(
        file: &std::fs::File,
    ) -> std::io::Result<windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
        };
        let mut info = FILE_BASIC_INFO::default();
        // SAFETY: `file` is borrowed for the call, so its handle is open, and
        // `info` is a live FILE_BASIC_INFO -- the structure FileBasicInfo
        // fills -- of exactly the size passed.
        let read = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                std::ptr::from_mut(&mut info).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        if read == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(info)
    }

    /// Write `info` back: every time as given (0 leaves one alone) and the
    /// attributes as given (0 leaves them alone).
    #[cfg(windows)]
    fn set_basic_info(
        file: &std::fs::File,
        info: &windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO,
    ) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FileBasicInfo, SetFileInformationByHandle,
        };
        // SAFETY: `file` is borrowed for the call, so its handle is open, and
        // `info` is a live FILE_BASIC_INFO -- the structure FileBasicInfo
        // reads -- of exactly the size passed. The call does not keep the
        // pointer.
        let set = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileBasicInfo,
                std::ptr::from_ref(info).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        if set == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
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
    pub async fn fs_fsync(file: super::OpenFile, data_only: bool) -> OpOutcome {
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
            Ok(Err(e)) => node_fail_fd_op(e, syscall),
            Err(e) => node_fail_fd_op(std::io::Error::other(e.to_string()), syscall),
        }
    }

    pub async fn fs_ftruncate(file: super::OpenFile, len: u64) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || file.set_len(len)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail_fd_op(e, "ftruncate"),
            Err(e) => node_fail_fd_op(std::io::Error::other(e.to_string()), "ftruncate"),
        }
    }

    pub async fn fs_fchmod(file: super::OpenFile, mode: u32) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || fchmod_file(&file, mode)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail_fd_op(e, "fchmod"),
            Err(e) => node_fail_fd_op(std::io::Error::other(e.to_string()), "fchmod"),
        }
    }

    pub async fn fs_fchown(file: super::OpenFile, uid: u32, gid: u32) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || fchown_file(&file, uid, gid)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail_fd_op(e, "fchown"),
            Err(e) => node_fail_fd_op(std::io::Error::other(e.to_string()), "fchown"),
        }
    }

    pub async fn fs_futimes(file: super::OpenFile, atime_ms: f64, mtime_ms: f64) -> OpOutcome {
        let result =
            tokio::task::spawn_blocking(move || futimes_file(&file, atime_ms, mtime_ms)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail_fd_op(e, "futime"),
            Err(e) => node_fail_fd_op(std::io::Error::other(e.to_string()), "futime"),
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

    pub async fn fs_chown(path: super::FsPath, uid: u32, gid: u32, follow: bool) -> OpOutcome {
        let syscall = if follow { "chown" } else { "lchown" };
        let owned = path.clone();
        let result =
            tokio::task::spawn_blocking(move || chown_path(owned.os(), uid, gid, follow)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, syscall, &path),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), syscall, &path),
        }
    }

    pub async fn fs_utimes(
        path: super::FsPath,
        atime_ms: f64,
        mtime_ms: f64,
        follow: bool,
    ) -> OpOutcome {
        // node reports the SINGULAR syscall name here (measured).
        let syscall = if follow { "utime" } else { "lutime" };
        let owned = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            utimes_path(owned.os(), atime_ms, mtime_ms, follow)
        })
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

    pub async fn fs_lchmod(path: super::FsPath, mode: u32) -> OpOutcome {
        let owned = path.clone();
        let result = tokio::task::spawn_blocking(move || fs_lchmod_sync(owned.os(), mode)).await;
        match result {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail(e, "lchmod", &path),
            Err(e) => node_fail(std::io::Error::other(e.to_string()), "lchmod", &path),
        }
    }

    /// `oam.readTextFile`: reads the OS path (`FsPath::os`, namespaced on
    /// Windows as every fs op's is) and names the path shown strip-only.
    pub async fn read_text_file(path: super::FsPath) -> OpOutcome {
        match tokio::fs::read_to_string(path.os()).await {
            Ok(text) => OpOutcome::Text(text),
            Err(e) => OpOutcome::Failed(format!("could not read {}: {e}", path.shown())),
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
        FetchCancel, FetchCancels, FetchContinuations, FetchRequest, RedirectMode, fetch,
        fetch_abandon, fetch_cancel, fetch_continue, fetch_parked_port, fetch_supply,
    };

    /// zlibStreamCreate: allocate an incremental compressor or decompressor.
    /// Returns Json {handle} on success. compress=true for encoding,
    /// false for decoding. format must be "gzip", "deflate", "deflateRaw",
    /// "unzip" (decompress only), or "brotli". `dictionary` is node's option
    /// of that name (brotli has none).
    pub async fn zlib_stream_create(
        streams: super::ZlibRegistry,
        ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
        format: String,
        level: i32,
        compress: bool,
        dictionary: Option<Vec<u8>>,
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
            super::ZlibStream::Compress(super::zlib::StreamCompressor::new(
                fmt,
                level,
                dictionary.as_deref(),
            ))
        } else {
            let wrap = match format.as_str() {
                "gzip" => super::zlib::Wrap::Gzip,
                "deflate" => super::zlib::Wrap::Zlib,
                "deflateRaw" => super::zlib::Wrap::Raw,
                "unzip" => super::zlib::Wrap::Auto,
                _ => return OpOutcome::Failed(format!("zlib stream: unknown format '{format}'")),
            };
            super::ZlibStream::Decompress(super::zlib::StreamDecompressor::new(
                wrap,
                dictionary.as_deref(),
            ))
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
                return Err(Box::new(OpOutcome::Failed(format!(
                    "zlib stream: handle {handle} not found"
                ))));
            };
            let failed = |context: &str, e| Box::new(OpOutcome::Failed(format!("{context}: {e}")));
            match stream {
                super::ZlibStream::Compress(enc) => enc
                    .write_chunk(&chunk)
                    .map_err(|e| failed("zlib stream write", e)),
                super::ZlibStream::Decompress(dec) => dec
                    .write_chunk(&chunk)
                    .map_err(|e| Box::new(zlib_decode_failed("zlib stream write", e))),
                super::ZlibStream::BrotliCompress(enc) => enc
                    .write_chunk(&chunk)
                    .map_err(|e| failed("brotli stream write", e)),
                super::ZlibStream::BrotliDecompress(dec) => dec
                    .write_chunk(&chunk)
                    .map_err(|e| failed("brotli stream write", e)),
                super::ZlibStream::HandleCompress(_)
                | super::ZlibStream::HandleDecompress(_)
                | super::ZlibStream::Zstd(_) => Err(Box::new(OpOutcome::Failed(
                    "zlib handle: use its handle write, not zlibStreamWrite".into(),
                ))),
            }
        })
        .await;
        match result {
            Ok(Ok(bytes)) => OpOutcome::Bytes(bytes),
            Ok(Err(failure)) => *failure,
            Err(e) => OpOutcome::Failed(format!("zlib stream write task: {e}")),
        }
    }

    /// zlibStreamFlush: finalize and remove the stream. Returns the tail
    /// bytes. For compressors, this emits the format trailer (CRC etc.).
    /// For decompressors, this finalizes the inflate/brotli state machine
    /// and returns any remaining output bytes. `finish_flush` is node's
    /// `finishFlush` for an inflate stream (only Z_FINISH requires the stream
    /// to be complete); the deflaters always finish the stream.
    pub async fn zlib_stream_flush(
        streams: super::ZlibRegistry,
        handle: u64,
        finish_flush: i32,
    ) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            let stream = streams
                .lock()
                .expect("zlib stream registry lock")
                .remove(&handle);
            let Some(stream) = stream else {
                return Err(Box::new(OpOutcome::Failed(format!(
                    "zlib stream: handle {handle} not found"
                ))));
            };
            let failed = |context: &str, e| Box::new(OpOutcome::Failed(format!("{context}: {e}")));
            match stream {
                super::ZlibStream::Compress(enc) => {
                    enc.finish().map_err(|e| failed("zlib stream flush", e))
                }
                super::ZlibStream::Decompress(dec) => dec
                    .finish_with(finish_flush)
                    .map_err(|e| Box::new(zlib_decode_failed("zlib stream flush", e))),
                super::ZlibStream::BrotliCompress(enc) => {
                    enc.finish().map_err(|e| failed("brotli stream flush", e))
                }
                super::ZlibStream::BrotliDecompress(dec) => dec
                    .finish()
                    .map_err(|e| Box::new(zlib_decode_failed("brotli stream flush", e))),
                super::ZlibStream::HandleCompress(_)
                | super::ZlibStream::HandleDecompress(_)
                | super::ZlibStream::Zstd(_) => Err(Box::new(OpOutcome::Failed(
                    "zlib handle: use close(), not zlibStreamFlush".into(),
                ))),
            }
        })
        .await;
        match result {
            Ok(Ok(bytes)) => OpOutcome::Bytes(bytes),
            Ok(Err(failure)) => *failure,
            Err(e) => OpOutcome::Failed(format!("zlib stream flush task: {e}")),
        }
    }

    /// zlibStreamParams: node's `params()` on a stream. A deflater returns
    /// what it held, under a sync flush, and compresses what follows at
    /// `level` if there is one (node changes it on a deflate or deflateRaw
    /// stream, not gzip); an inflater or brotli stream has nothing to flush,
    /// and returns no bytes (every write already returned what it decoded).
    pub async fn zlib_stream_params(
        streams: super::ZlibRegistry,
        handle: u64,
        level: Option<i32>,
    ) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = streams.lock().unwrap_or_else(|e| e.into_inner());
            match guard.get_mut(&handle) {
                Some(super::ZlibStream::Compress(enc)) => Ok(enc.params(level)),
                Some(_) => Ok(Vec::new()),
                None => Err(format!("zlib stream: handle {handle} not found")),
            }
        })
        .await;
        match result {
            Ok(Ok(bytes)) => OpOutcome::Bytes(bytes),
            Ok(Err(message)) => OpOutcome::Failed(message),
            Err(e) => OpOutcome::Failed(format!("zlib stream params task: {e}")),
        }
    }

    /// A decompressor's failure: node's coded zlib error (`code`, `errno`,
    /// zlib's message) when it is one (#166), else an uncoded failure.
    fn zlib_decode_failed(context: &str, error: std::io::Error) -> OpOutcome {
        match super::zlib::zlib_error(&error) {
            Some(z) => OpOutcome::NodeFailed {
                code: z.code.into(),
                message: z.message.into(),
                syscall: None,
                path: None,
                errno: Some(z.errno),
                hostname: None,
                address: None,
                port: None,
                dest: None,
                win_error: None,
            },
            None => OpOutcome::Failed(format!("{context}: {error}")),
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

    /// zlibHandleCreate: allocate a low-level NodeDeflate or NodeInflate
    /// handle for Node's zlib binding interface (used by ssh2 etc.).
    /// mode: 1=DEFLATE, 2=INFLATE, 5=DEFLATERAW, 6=INFLATERAW. `dictionary`
    /// is the one node's `handle.init` takes.
    pub fn zlib_handle_create(
        streams: &super::ZlibRegistry,
        ids: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        mode: i32,
        level: i32,
        dictionary: Option<&[u8]>,
    ) -> Result<u64, String> {
        let dictionary = dictionary.filter(|d| !d.is_empty());
        let stream = match (mode, dictionary) {
            (1, _) => super::ZlibStream::HandleCompress(Box::new(super::zlib::NodeDeflate::new(
                super::zlib::Format::Deflate,
                level,
                dictionary,
            ))),
            (5, _) => super::ZlibStream::HandleCompress(Box::new(super::zlib::NodeDeflate::new(
                super::zlib::Format::DeflateRaw,
                level,
                dictionary,
            ))),
            (2, _) => super::ZlibStream::HandleDecompress(Box::new(
                super::zlib::NodeInflate::with_dictionary(super::zlib::Wrap::Zlib, dictionary),
            )),
            (6, _) => super::ZlibStream::HandleDecompress(Box::new(
                super::zlib::NodeInflate::with_dictionary(super::zlib::Wrap::Raw, dictionary),
            )),
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
    ) -> std::io::Result<(usize, usize)> {
        let mut guard = streams.lock().unwrap_or_else(|e| e.into_inner());
        let stream = guard
            .get_mut(&handle)
            .ok_or_else(|| std::io::Error::other(format!("zlib handle {handle} not found")))?;
        match stream {
            super::ZlibStream::HandleCompress(c) => {
                let (consumed, produced) = c.deflate(input, output, flush);
                Ok((output.len() - produced, input.len() - consumed))
            }
            super::ZlibStream::HandleDecompress(d) => {
                // Inflate until the output is full or nothing more is
                // decodable from the input. Failures are node's coded zlib
                // errors (an io::Error wrapping a ZlibError).
                let (mut consumed, mut produced) = (0, 0);
                while produced < output.len() {
                    let (used, made) = d
                        .step(&input[consumed..], &mut output[produced..])
                        .map_err(std::io::Error::other)?;
                    consumed += used;
                    produced += made;
                    if used == 0 && made == 0 {
                        break;
                    }
                }
                // node_zlib.cc CheckError: under Z_FINISH, a stream that is
                // not complete when the output still has room has run out of
                // input -- "unexpected end of file". Other flushes wait for
                // more.
                if flush == 4 && produced < output.len() && !d.is_complete() {
                    return Err(std::io::Error::other(
                        super::zlib::ZlibError::UNEXPECTED_EOF,
                    ));
                }
                Ok((output.len() - produced, input.len() - consumed))
            }
            _ => Err(std::io::Error::other(format!(
                "zlib handle {handle} is not a handle variant"
            ))),
        }
    }

    /// zstdHandleCreate: node's zstd binding `init()` (see
    /// [`super::zstd::ZstdHandle::new`]), registered under a fresh handle. The
    /// `Err` is node's message for the step that failed, which node throws as
    /// `ERR_ZLIB_INITIALIZATION_FAILED`.
    pub fn zstd_handle_create(
        streams: &super::ZlibRegistry,
        ids: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        compress: bool,
        pledged_src_size: Option<u64>,
        dictionary: Option<&[u8]>,
        params: &[(u32, i32)],
    ) -> Result<u64, &'static str> {
        let engine = super::zstd::ZstdHandle::new(compress, pledged_src_size, dictionary, params)?;
        let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        streams.lock().expect("zlib registry lock").insert(
            handle,
            super::ZlibStream::Zstd(std::sync::Arc::new(std::sync::Mutex::new(engine))),
        );
        Ok(handle)
    }

    /// The zstd engine registered under `handle`, if it is one and still open.
    pub fn zstd_handle(
        streams: &super::ZlibRegistry,
        handle: u64,
    ) -> Option<std::sync::Arc<std::sync::Mutex<super::zstd::ZstdHandle>>> {
        match streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&handle)
        {
            Some(super::ZlibStream::Zstd(engine)) => Some(engine.clone()),
            _ => None,
        }
    }

    /// The result of one zstd handle write as the shim reads it: the output
    /// bytes, then the count of input bytes consumed as 8 little-endian bytes.
    /// One buffer carries both numbers node's binding reports (through its
    /// write-state array) without a second op or a JSON round trip.
    pub fn zstd_write_result(consumed: usize, mut output: Vec<u8>) -> Vec<u8> {
        output.extend_from_slice(&(consumed as u64).to_le_bytes());
        output
    }

    /// node's coded zstd failure, for an async write to reject with.
    pub fn zstd_failed(error: super::zstd::ZstdError) -> OpOutcome {
        OpOutcome::NodeFailed {
            code: error.code.into(),
            message: error.message.into(),
            syscall: None,
            path: None,
            errno: Some(error.errno),
            hostname: None,
            address: None,
            port: None,
            dest: None,
            win_error: None,
        }
    }

    /// zstdHandleWrite: node's async `handle.write()` -- one libzstd call (see
    /// [`super::zstd::ZstdHandle::write`]) on the blocking pool, as node runs
    /// it on the threadpool. `input` is a new chunk to start on, or `None` to
    /// read on through the one the handle holds. Resolves with
    /// [`zstd_write_result`]'s buffer.
    pub async fn zstd_handle_write(
        engine: std::sync::Arc<std::sync::Mutex<super::zstd::ZstdHandle>>,
        flush: i32,
        input: Option<Vec<u8>>,
        out_len: usize,
    ) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            let mut engine = engine.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(input) = input {
                engine.set_input(input);
            }
            engine.write(flush, out_len)
        })
        .await;
        match result {
            Ok(Ok((consumed, output))) => OpOutcome::Bytes(zstd_write_result(consumed, output)),
            Ok(Err(error)) => zstd_failed(error),
            Err(e) => OpOutcome::Failed(format!("zstd write task: {e}")),
        }
    }

    /// Async zlib: CPU-bound, so spawn_blocking off the op channel
    /// (Node's threadpool model). compress=true encodes, false decodes;
    /// format "unzip" auto-detects on the decode side.
    /// `max_output` is node's `maxOutputLength` for a decode; `None` is no cap.
    /// `finish_flush` is node's `finishFlush` for a decode (see
    /// `zlib::decompress_capped`); an encode always finishes the stream.
    /// `dictionary` is node's option of that name.
    pub async fn zlib_transform(
        bytes: Vec<u8>,
        format: String,
        level: i32,
        compress: bool,
        max_output: Option<usize>,
        finish_flush: i32,
        dictionary: Option<Vec<u8>>,
    ) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            if !compress && format == "unzip" {
                return super::zlib::unzip_capped(
                    &bytes,
                    max_output,
                    finish_flush,
                    dictionary.as_deref(),
                );
            }
            let Some(parsed) = super::zlib::Format::parse(&format) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown zlib format '{format}'"),
                ));
            };
            if compress {
                super::zlib::compress_capped(
                    &bytes,
                    parsed,
                    level,
                    max_output,
                    dictionary.as_deref(),
                )
            } else {
                super::zlib::decompress_capped(
                    &bytes,
                    parsed,
                    max_output,
                    finish_flush,
                    dictionary.as_deref(),
                )
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
            Ok(Err(e)) => zlib_decode_failed("zlib", e),
            Err(e) => OpOutcome::Failed(format!("zlib task: {e}")),
        }
    }

    /// Open a file for a descriptor, with the platform's O_* bits: the same
    /// `OpenFlags::open` the sync open makes, so the two cannot disagree
    /// about what a flag means. Resolves with Json {handle}.
    pub async fn fs_open(
        files: super::FileRegistry,
        ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
        path: super::FsPath,
        flags: super::OpenFlags,
        mode: u32,
    ) -> OpOutcome {
        let options = flags;
        // The sync family's open on the blocking pool rather than tokio::fs,
        // so the descriptor lands in the ONE registry that family also reads.
        let owned = path.clone();
        let opened =
            tokio::task::spawn_blocking(move || options.open_with_mode(&owned, mode)).await;
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
                    .insert(handle, std::sync::Arc::new(file));
                OpOutcome::Json(serde_json::json!({ "handle": handle }).to_string())
            }
            Err(e) => node_fail_at(super::FsSite::Open(flags), e, "open", &path),
        }
    }

    /// Read up to `len` bytes. `position` = None reads from (and advances) the
    /// cursor; Some(p) is a `pread` -- reads at p and leaves the cursor alone.
    /// Bytes = data, Done = EOF (the handle stays open until fsClose).
    ///
    /// The position parameter used to not exist, so the JS `fs.read` callback
    /// form had nowhere to put the one it was given and silently dropped it:
    /// `fs.read(fd, buf, 0, 3, 10, cb)` read from the CURSOR instead of offset
    /// 10 and returned the wrong bytes with no error.
    ///
    /// The descriptor stays in the registry for the whole read (see
    /// `OpenFile`): any number of reads, writes and stats on it can be in
    /// flight at once, as in node, and a failed read leaves it open.
    pub async fn fs_read_chunk(
        files: super::FileRegistry,
        handle: u64,
        len: usize,
        position: Option<i64>,
    ) -> OpOutcome {
        let Some(file) = super::registered_file(&files, handle) else {
            return node_fail_ebadf("read");
        };
        // Exactly `len`, unclamped -- same as the sync twin's `vec![0u8; length]`.
        // An 8 MiB ceiling here silently short-read anything bigger (a 10 MiB
        // readv reported 8388608 where readvSync and node both report
        // 10485760), and the old min-of-1 turned a zero-length read into a
        // one-byte one. `len` is bounded by the caller's own destination buffer
        // on the JS side, which is where node bounds it too (ERR_OUT_OF_RANGE).
        let done = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            let r = super::read_at(&file, &mut buf, position);
            (buf, r)
        })
        .await;
        let (mut buf, result) = match done {
            Ok(t) => t,
            Err(e) => {
                return node_fail_fd(std::io::Error::other(e.to_string()), "read");
            }
        };
        match result {
            Ok(0) => OpOutcome::Done,
            Ok(n) => {
                buf.truncate(n);
                OpOutcome::Bytes(buf)
            }
            Err(e) => node_fail_fd(e, "read"),
        }
    }

    /// Write one chunk to an open handle.
    ///
    /// `position` = None appends at the cursor; Some(p) is a `pwrite` -- writes
    /// at p and leaves the cursor alone, which is what a positional
    /// `FileHandle.write` means. A descriptor opened in APPEND mode ignores the
    /// position and always writes at the end; that is the OS's behaviour and
    /// node's. Like `fs_read_chunk`, the descriptor stays registered for the
    /// whole write, so concurrent writes on it all reach it.
    pub async fn fs_write_chunk(
        files: super::FileRegistry,
        handle: u64,
        bytes: Vec<u8>,
        position: Option<i64>,
    ) -> OpOutcome {
        let Some(file) = super::registered_file(&files, handle) else {
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
        let done =
            tokio::task::spawn_blocking(move || super::write_all_at(&file, &bytes, position)).await;
        match done {
            Ok(Ok(())) => OpOutcome::Done,
            Ok(Err(e)) => node_fail_fd(e, "write"),
            Err(e) => node_fail_fd(std::io::Error::other(e.to_string()), "write"),
        }
    }

    /// Read one chunk from a streaming body. Bytes = a chunk, Done = EOF
    /// (handle dropped). Remove-read-reinsert keeps the lock short; the JS
    /// ReadableStream lock guarantees a single reader per handle. A cancel
    /// that landed while the read was in flight drops the body instead of
    /// reinserting it.
    pub use crate::http_client::body::read as fetch_body_read;
    pub use crate::http_client::body::take_trailers as fetch_body_trailers;

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

    /// `unknown_win32_error` (#442): the raw code rides along exactly when
    /// libuv's table says UNKNOWN and the code is a real one. ERROR_PIPE_LISTENING
    /// is a WriteFile on a named pipe's server end before any client opened it
    /// (the e2e's trigger); ERROR_EXE_MACHINE_TYPE_MISMATCH a spawn of a binary
    /// built for another architecture. libuv names neither.
    #[cfg(windows)]
    #[test]
    fn an_unknown_code_keeps_its_win32_number() {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_EXE_MACHINE_TYPE_MISMATCH, ERROR_PIPE_LISTENING,
        };
        for win32 in [ERROR_PIPE_LISTENING, ERROR_EXE_MACHINE_TYPE_MISMATCH] {
            let e = raw(win32);
            let code = node_error_code(&e);
            assert_eq!(code, "UNKNOWN");
            assert_eq!(unknown_win32_error(code, &e), Some(win32));
        }
        // A code libuv names: the name says it all, nothing rides along.
        let denied = raw(ERROR_ACCESS_DENIED);
        assert_eq!(node_error_code(&denied), "EPERM");
        assert_eq!(unknown_win32_error("EPERM", &denied), None);
        // The caller's code decides, not the table's: fd_error_code's EBADF for
        // a wrong-mode descriptor keeps nothing.
        assert_eq!(
            unknown_win32_error("EBADF", &raw(ERROR_PIPE_LISTENING)),
            None
        );
        // oam's own UV_UNKNOWN stand-in (the mailslot open) and NO_ERROR are
        // not Win32 codes; neither is an error with no OS code behind it.
        let sentinel = std::io::Error::from_raw_os_error(UV_UNKNOWN);
        assert_eq!(node_error_code(&sentinel), "UNKNOWN");
        assert_eq!(unknown_win32_error("UNKNOWN", &sentinel), None);
        assert_eq!(unknown_win32_error("UNKNOWN", &raw(0)), None);
        assert_eq!(
            unknown_win32_error("UNKNOWN", &std::io::Error::other("x")),
            None
        );
        // An HRESULT-shaped code, which std stores negative, comes back as the
        // DWORD winerror.h documents.
        let hresult = std::io::Error::from_raw_os_error(0x8007_0005_u32 as i32);
        assert_eq!(node_error_code(&hresult), "UNKNOWN");
        assert_eq!(unknown_win32_error("UNKNOWN", &hresult), Some(0x8007_0005));
        // An unlisted Winsock code goes the same way: the WSA arm of the table
        // has no row for WSAEPROTOTYPE, and the Win32 arm none either.
        use windows_sys::Win32::Networking::WinSock::WSAEPROTOTYPE;
        let wsa = std::io::Error::from_raw_os_error(WSAEPROTOTYPE);
        assert_eq!(node_error_code(&wsa), "UNKNOWN");
        assert_eq!(
            unknown_win32_error("UNKNOWN", &wsa),
            Some(WSAEPROTOTYPE as u32)
        );
    }

    /// Off Windows there is no Win32 code to keep, whatever the raw errno, and
    /// `node_error_code` never says UNKNOWN for one anyway.
    #[cfg(not(windows))]
    #[test]
    fn no_win32_code_off_windows() {
        let e = std::io::Error::from_raw_os_error(536);
        assert_eq!(unknown_win32_error("UNKNOWN", &e), None);
        assert_ne!(node_error_code(&e), "UNKNOWN");
        assert_eq!(unknown_win32_error(node_error_code(&e), &e), None);
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
            // A WriteFile on a named pipe's server end before any client has
            // opened it: the e2e trigger for #442, in neither of libuv's tables.
            (ERROR_PIPE_LISTENING, "UNKNOWN"),
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

    /// libuv's rule for a Windows open that may create its file: read-only
    /// when the mode, less the umask, has no owner-write bit.
    #[test]
    fn a_created_file_is_read_only_without_an_owner_write_bit_after_the_umask() {
        assert!(!creates_read_only(0o666, 0));
        assert!(!creates_read_only(0o200, 0));
        assert!(creates_read_only(0o444, 0));
        assert!(creates_read_only(0o577, 0));
        assert!(creates_read_only(0o666, 0o200));
        assert!(creates_read_only(0o777, 0o600));
        assert!(!creates_read_only(0o666, 0o400));
    }

    /// A recursive mkdir says which directory it made first, makes nothing
    /// of what is there, and tells a file in the way of a component
    /// (ENOTDIR) from one at the path itself (EEXIST), naming the path that
    /// failed.
    #[test]
    fn make_dir_reports_the_first_directory_it_made_and_where_it_failed() {
        let root = std::env::temp_dir().join(format!("oam-make-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let p = |rest: &[&str]| {
            let mut path = root.clone();
            for part in rest {
                path.push(part);
            }
            path.to_str().unwrap().to_string()
        };

        assert_eq!(make_dir(&p(&["one"]), 0o777, false, false).unwrap(), None);
        let failed = make_dir(&p(&["one"]), 0o777, false, false).unwrap_err();
        assert_eq!(node_error_code(&failed.error), "EEXIST");
        assert_eq!(failed.at, p(&["one"]));
        let failed = make_dir(&p(&["no", "leaf"]), 0o777, false, false).unwrap_err();
        assert_eq!(node_error_code(&failed.error), "ENOENT");

        assert_eq!(
            make_dir(&p(&["a", "b", "c"]), 0o777, true, false).unwrap(),
            Some(p(&["a"])),
            "none of it was there: the first made is the top"
        );
        assert_eq!(
            make_dir(&p(&["a", "b", "d", "e"]), 0o777, true, true).unwrap(),
            Some(p(&["a", "b", "d"])),
            "what was there is not counted"
        );
        assert_eq!(
            make_dir(&p(&["a", "b", "d", "e"]), 0o777, true, true).unwrap(),
            None
        );
        assert!(std::path::Path::new(&p(&["a", "b", "d", "e"])).is_dir());

        // Deeper than any fixed number of turns: the walk is bounded by the
        // path, not by a count.
        let deep: Vec<&str> = std::iter::once("deep")
            .chain(std::iter::repeat_n("d", 300))
            .collect();
        assert_eq!(
            make_dir(&p(&deep), 0o777, true, false).unwrap(),
            Some(p(&["deep"]))
        );
        assert!(std::path::Path::new(&p(&deep)).is_dir());

        std::fs::write(p(&["file"]), b"").unwrap();
        let failed = make_dir(&p(&["file"]), 0o777, true, false).unwrap_err();
        assert_eq!(node_error_code(&failed.error), "EEXIST");
        assert_eq!(failed.at, p(&["file"]));
        let failed = make_dir(&p(&["file", "x", "y"]), 0o777, true, true).unwrap_err();
        assert_eq!(node_error_code(&failed.error), "ENOTDIR");
        // Windows climbs to the file and names it; unix is told ENOTDIR at
        // the first mkdir and names that.
        let named = if cfg!(windows) {
            p(&["file"])
        } else {
            p(&["file", "x", "y"])
        };
        assert_eq!(failed.at, named);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &str| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            std::fs::create_dir(p(&["kept"])).unwrap();
            let kept = mode(&p(&["kept"]));
            make_dir(&p(&["kept", "m", "n"]), 0o700, true, false).unwrap();
            assert_eq!(
                mode(&p(&["kept", "m"])),
                0o700,
                "every directory made gets the mode"
            );
            assert_eq!(mode(&p(&["kept", "m", "n"])), 0o700);
            assert_eq!(mode(&p(&["kept"])), kept, "one already there keeps its own");
            make_dir(&p(&["single"]), 0o700, false, false).unwrap();
            assert_eq!(mode(&p(&["single"])), 0o700);
        }

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// #324: the mode of an open that creates its file is the file's. A mode
    /// with no owner-write bit makes it read-only on every platform; one
    /// that merely opens a file leaves it as it was.
    #[test]
    fn a_creating_open_applies_its_mode() {
        let dir = std::env::temp_dir().join(format!("oam-open-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let readonly = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().readonly();

        for (i, flags) in ["w", "a", "wx", "a+", "w+"].into_iter().enumerate() {
            let made = dir.join(format!("made-{i}"));
            drop(
                open_options_for(flags)
                    .open_with_mode(&made, 0o444)
                    .unwrap(),
            );
            assert!(
                readonly(&made),
                "{flags} with 0o444 creates a read-only file"
            );
            let plain = dir.join(format!("plain-{i}"));
            drop(open_options_for(flags).open(&plain).unwrap());
            assert!(
                !readonly(&plain),
                "{flags} with the default mode creates a writable one"
            );
        }

        // Already there: an open that does not replace it changes nothing.
        let kept = dir.join("kept");
        std::fs::write(&kept, b"x").unwrap();
        for flags in ["a", "a+", "r+", "r"] {
            drop(
                open_options_for(flags)
                    .open_with_mode(&kept, 0o444)
                    .unwrap(),
            );
            assert!(
                !readonly(&kept),
                "{flags} over an existing file leaves its mode"
            );
        }

        let through = dir.join("whole");
        write_whole_file(through.to_str().unwrap(), b"x", false, 0o444).unwrap();
        assert!(readonly(&through));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let private = dir.join("private");
            drop(
                open_options_for("w")
                    .open_with_mode(&private, 0o600)
                    .unwrap(),
            );
            let mode = std::fs::metadata(&private).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "0o600 leaves nothing for group or other");
        }

        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
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

    /// A scratch directory for the whole-file tests, unique per test.
    fn whole_file_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oam-whole-file-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stat record's layout, as the engine copies it and the JS side reads
    /// it (`STAT_*` in js/node_compat.js): the fourteen Stats values in
    /// node's constructor order, the kind, the ids flag and the reparse tag
    /// as doubles; the integers again exact, then each time's seconds and
    /// nanoseconds, from which the doubles' `*Ms` are node's
    /// `sec * 1e3 + nsec / 1e6`.
    #[test]
    fn a_stat_record_is_laid_out_as_the_js_side_reads_it() {
        let dir = whole_file_dir("stat-record");
        let file = dir.join("five.txt");
        std::fs::write(&file, b"hello").unwrap();
        let record = ops::stat_path_record(file.to_str().unwrap(), false).unwrap();
        assert_eq!(record.kind(), ops::EntryKind::File);
        assert_eq!(record.numbers[8], 5.0, "size");
        assert_eq!(record.exact[8], 5, "size, exact");
        assert_eq!(
            record.numbers[1] as i64 & 0o170000,
            0o100000,
            "mode: S_IFREG"
        );
        assert_eq!(
            record.numbers[1] as i64, record.exact[1],
            "mode, both forms"
        );
        assert_eq!(record.numbers[ops::STAT_IDS_KNOWN], 1.0);
        assert_eq!(record.numbers[ops::STAT_REPARSE_TAG], -1.0);
        for (at, value) in record.exact[..10].iter().enumerate() {
            assert_eq!(record.numbers[at], *value as u64 as f64, "integer {at}");
        }
        for time in 0..4 {
            let (sec, nsec) = (record.exact[10 + 2 * time], record.exact[11 + 2 * time]);
            assert!(
                (0..1_000_000_000).contains(&nsec),
                "time {time}: nsec {nsec}"
            );
            assert_eq!(
                record.numbers[10 + time],
                sec as f64 * 1000.0 + nsec as f64 / 1_000_000.0,
                "time {time}"
            );
        }
        let record = ops::stat_path_record(dir.to_str().unwrap(), true).unwrap();
        assert_eq!(record.kind(), ops::EntryKind::Dir);
        assert_eq!(record.numbers[ops::STAT_KIND], 1.0);
        let opened = std::fs::File::open(&file).unwrap();
        let by_fd = ops::fstat_record(&opened).unwrap();
        assert_eq!((by_fd.kind(), by_fd.exact[8]), (ops::EntryKind::File, 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A listing's two lists run in step, each name with its own kind; on
    /// unix sorted as libuv's scandir sorts, bytewise.
    #[test]
    fn a_listing_pairs_each_name_with_its_kind() {
        let dir = whole_file_dir("listing-kinds");
        std::fs::write(dir.join("b.txt"), b"b").unwrap();
        std::fs::create_dir(dir.join("a")).unwrap();
        std::fs::write(dir.join("C.txt"), b"c").unwrap();
        let listed = ops::read_dir_listing(dir.to_str().unwrap(), false).unwrap();
        assert_eq!(listed.names.len(), listed.kinds.len());
        let mut pairs: Vec<(String, ops::EntryKind)> =
            listed.names.into_iter().zip(listed.kinds).collect();
        #[cfg(not(windows))]
        assert_eq!(
            pairs
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["C.txt", "a", "b.txt"]
        );
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            pairs,
            [
                ("C.txt".to_string(), ops::EntryKind::File),
                ("a".to_string(), ops::EntryKind::Dir),
                ("b.txt".to_string(), ops::EntryKind::File),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The error a locked region gives a read or write: ERROR_LOCK_VIOLATION
    /// on Windows (node: EBUSY), and EIO standing in for any read failure
    /// elsewhere.
    fn transfer_failure() -> (std::io::Error, &'static str, &'static str) {
        #[cfg(windows)]
        {
            (
                std::io::Error::from_raw_os_error(
                    windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32,
                ),
                "EBUSY",
                "resource busy or locked",
            )
        }
        #[cfg(unix)]
        {
            (
                std::io::Error::from_raw_os_error(libc::EIO),
                "EIO",
                "i/o error",
            )
        }
    }

    /// node's readFile opens, then reads: a failed READ is syscall `read` with
    /// no path ("EBUSY: resource busy or locked, read"), never the open's
    /// `open '<path>'`. Injected through the read half, so it holds on every
    /// host.
    #[test]
    fn read_file_read_failure_is_read_without_path() {
        struct Failing(Option<std::io::Error>);
        impl std::io::Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(self.0.take().expect("read once"))
            }
        }
        let (error, code, reason) = transfer_failure();
        let failed = read_opened(Failing(Some(error)), Some(10)).unwrap_err();
        assert!(matches!(failed, WholeFileError::Transfer(_)), "{failed:?}");
        let failure = whole_file_error(FsSite::ReadFile, "f.env", &failed);
        assert_eq!(
            failure,
            FsError {
                code,
                syscall: "read",
                has_path: false,
            }
        );
        assert_eq!(
            fs_error_message(failure, "f.env", failed.io()),
            format!("{code}: {reason}, read")
        );
    }

    /// The same split for writeFile and appendFile: a failed WRITE is syscall
    /// `write` with no path.
    #[test]
    fn write_file_write_failure_is_write_without_path() {
        struct Failing(Option<std::io::Error>);
        impl std::io::Write for Failing {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(self.0.take().expect("write once"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for site in [FsSite::WriteFile, FsSite::AppendFile] {
            let (error, code, reason) = transfer_failure();
            let failed = write_opened(Failing(Some(error)), b"x").unwrap_err();
            let failure = whole_file_error(site, "f", &failed);
            assert_eq!(
                failure,
                FsError {
                    code,
                    syscall: "write",
                    has_path: false,
                }
            );
            assert_eq!(
                fs_error_message(failure, "f", failed.io()),
                format!("{code}: {reason}, write")
            );
        }
        // node writes nothing for empty data, so nothing can fail.
        assert!(write_opened(Failing(None), b"").is_ok());
    }

    /// A failed OPEN keeps syscall `open` and the path, for all three.
    #[test]
    fn whole_file_open_failure_names_the_path() {
        let dir = whole_file_dir("open");
        let missing = dir.join("missing").join("f.txt");
        let missing = missing.to_str().unwrap();
        let enoent = FsError {
            code: "ENOENT",
            syscall: "open",
            has_path: true,
        };
        let failed = read_whole_file(missing).unwrap_err();
        assert!(matches!(failed, WholeFileError::Open(_)), "{failed:?}");
        assert_eq!(whole_file_error(FsSite::ReadFile, missing, &failed), enoent);
        for (site, append) in [(FsSite::WriteFile, false), (FsSite::AppendFile, true)] {
            let failed = write_whole_file(missing, b"x", append, 0o666).unwrap_err();
            assert!(matches!(failed, WholeFileError::Open(_)), "{failed:?}");
            assert_eq!(whole_file_error(site, missing, &failed), enoent);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The split changes nothing that succeeds: bytes round-trip, writeFile
    /// truncates, appendFile appends, an empty file reads empty.
    #[test]
    fn whole_file_round_trips() {
        let dir = whole_file_dir("round-trip");
        let file = dir.join("f.bin");
        let file = file.to_str().unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        write_whole_file(file, &big, false, 0o666).unwrap();
        assert_eq!(read_whole_file(file).unwrap(), big);
        write_whole_file(file, b"ab", false, 0o666).unwrap();
        write_whole_file(file, b"cd", true, 0o666).unwrap();
        assert_eq!(read_whole_file(file).unwrap(), b"abcd");
        write_whole_file(file, b"", false, 0o666).unwrap();
        assert_eq!(read_whole_file(file).unwrap(), b"");
        // A stale size hint (the file grew or shrank since the fstat): the
        // read still returns what is there.
        let bytes = read_opened(std::io::Cursor::new(b"hello".to_vec()), Some(2)).unwrap();
        assert_eq!(bytes, b"hello");
        let bytes = read_opened(std::io::Cursor::new(b"hi".to_vec()), Some(64)).unwrap();
        assert_eq!(bytes, b"hi");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// readFile's success path makes std::fs::read's read calls, no more: with
    /// the fstat size hint, one read of the whole file and one EOF probe,
    /// whatever the size. The generic `read_to_end` (no hint) took nine for
    /// 1 MiB. Short reads, an interrupted read, a stale hint and no hint at
    /// all still return every byte.
    #[test]
    fn read_opened_reads_a_hinted_file_in_two_calls() {
        struct Counting<R> {
            inner: R,
            reads: usize,
            /// Cap on each read's length (a short-reading source), if any.
            chunk: Option<usize>,
            /// Fail this many reads with Interrupted first.
            interrupts: usize,
        }
        impl<R: std::io::Read> std::io::Read for Counting<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.interrupts > 0 {
                    self.interrupts -= 1;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let n = self.chunk.map_or(buf.len(), |c| c.min(buf.len()));
                self.inner.read(&mut buf[..n])
            }
        }
        let counting = |data: &[u8], chunk, interrupts| Counting {
            inner: std::io::Cursor::new(data.to_vec()),
            reads: 0,
            chunk,
            interrupts,
        };
        for size in [0usize, 1, 4096, 100_000, 1 << 20, 64 << 20] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut reader = counting(&data, None, 0);
            let bytes = read_opened(&mut reader, Some(size as u64)).unwrap();
            assert!(bytes == data, "{size} bytes read back wrong");
            assert_eq!(reader.reads, if size == 0 { 1 } else { 2 }, "{size} bytes");
        }
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        for (hint, chunk, interrupts) in [
            (Some(300_000), Some(7_000), 0),
            (Some(300_000), None, 3),
            (Some(10), None, 0),
            (Some(1 << 20), None, 0),
            (None, None, 0),
            (None, Some(4_096), 1),
        ] {
            let mut reader = counting(&data, chunk, interrupts);
            let bytes = read_opened(&mut reader, hint).unwrap();
            assert!(bytes == data, "{hint:?} {chunk:?} {interrupts}");
        }
    }

    /// readFile of a DIRECTORY fails on the read on every platform, as node's
    /// does: EISDIR, syscall `read`, no path. The open admits the directory
    /// (on Windows because of `opens_as_libuv`) and the read half fails.
    #[test]
    fn read_file_of_a_directory_fails_on_the_read_everywhere() {
        let dir = whole_file_dir("dir");
        let dir_s = dir.to_str().unwrap();
        let eisdir_read = FsError {
            code: "EISDIR",
            syscall: "read",
            has_path: false,
        };
        let failed = read_whole_file(dir_s).unwrap_err();
        assert!(matches!(failed, WholeFileError::Transfer(_)), "{failed:?}");
        let failure = whole_file_error(FsSite::ReadFile, dir_s, &failed);
        assert_eq!(failure, eisdir_read);
        assert_eq!(
            fs_error_message(failure, dir_s, failed.io()),
            "EISDIR: illegal operation on a directory, read"
        );
        // The read half's EISDIR, injected, is the same verdict everywhere.
        let kind = WholeFileError::Transfer(std::io::Error::from(std::io::ErrorKind::IsADirectory));
        assert_eq!(whole_file_error(FsSite::ReadFile, "d", &kind), eisdir_read);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The real thing on Windows: another handle holds an exclusive lock over
    /// the file (LockFileEx, mandatory there). The open succeeds and the read
    /// or write fails ERROR_LOCK_VIOLATION -- node's "EBUSY: resource busy or
    /// locked, read" / "..., write", no path.
    #[cfg(windows)]
    #[test]
    fn locked_file_fails_the_read_and_write_not_the_open() {
        let dir = whole_file_dir("locked");
        let file = dir.join("locked.env");
        std::fs::write(&file, "A=1\n").unwrap();
        let file_s = file.to_str().unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file)
            .unwrap();
        holder.lock().unwrap();
        let busy = |syscall| FsError {
            code: "EBUSY",
            syscall,
            has_path: false,
        };
        let failed = read_whole_file(file_s).unwrap_err();
        assert!(matches!(failed, WholeFileError::Transfer(_)), "{failed:?}");
        assert_eq!(
            whole_file_error(FsSite::ReadFile, file_s, &failed),
            busy("read")
        );
        assert_eq!(
            fs_error_message(busy("read"), file_s, failed.io()),
            "EBUSY: resource busy or locked, read"
        );
        for (site, append) in [(FsSite::AppendFile, true), (FsSite::WriteFile, false)] {
            let failed = write_whole_file(file_s, b"B=2\n", append, 0o666).unwrap_err();
            assert!(matches!(failed, WholeFileError::Transfer(_)), "{failed:?}");
            assert_eq!(whole_file_error(site, file_s, &failed), busy("write"));
        }
        holder.unlock().unwrap();
        drop(holder);
        assert!(read_whole_file(file_s).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The real thing on Linux: /proc/self/mem opens and fails the read at
    /// offset 0 with EIO; /dev/full opens and fails every write with ENOSPC.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_read_and_write_failures_are_the_transfer_half() {
        let failed = read_whole_file("/proc/self/mem").unwrap_err();
        assert!(matches!(failed, WholeFileError::Transfer(_)), "{failed:?}");
        assert_eq!(
            whole_file_error(FsSite::ReadFile, "/proc/self/mem", &failed),
            FsError {
                code: "EIO",
                syscall: "read",
                has_path: false,
            }
        );
        for (site, append) in [(FsSite::WriteFile, false), (FsSite::AppendFile, true)] {
            let failed = write_whole_file("/dev/full", b"x", append, 0o666).unwrap_err();
            assert!(matches!(failed, WholeFileError::Transfer(_)), "{failed:?}");
            assert_eq!(
                whole_file_error(site, "/dev/full", &failed),
                FsError {
                    code: "ENOSPC",
                    syscall: "write",
                    has_path: false,
                }
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_filesystem_rules_depend_on_the_operation() {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_DIRECTORY, ERROR_FILE_EXISTS, ERROR_INVALID_NAME,
            ERROR_NOT_A_REPARSE_POINT,
        };
        let dir = std::env::temp_dir().join(format!("oam-fs-error-at-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let dir_s = dir.to_str().unwrap();
        let file_s = file.to_str().unwrap();
        let expect = |code, syscall, has_path| FsError {
            code,
            syscall,
            has_path,
        };

        // A truncating open of a directory collides with the name: node's
        // EISDIR. Any open that is denied is the table's EPERM -- a hidden
        // file's, a root's, an ACL's.
        let denied = raw(ERROR_ACCESS_DENIED);
        for site in [
            FsSite::Open(open_options_for("w")),
            FsSite::Open(open_options_for("w+")),
            FsSite::WriteFile,
        ] {
            assert_eq!(
                fs_error_at(site, "open", dir_s, &raw(ERROR_FILE_EXISTS)),
                expect("EISDIR", "open", true),
                "{site:?}"
            );
            for path in [dir_s, file_s] {
                assert_eq!(
                    fs_error_at(site, "open", path, &denied),
                    expect("EPERM", "open", true),
                    "{site:?} {path}"
                );
            }
        }
        for site in [
            FsSite::ReadFile,
            FsSite::AppendFile,
            FsSite::Open(open_options_for("r")),
            FsSite::Open(open_options_for("r+")),
            FsSite::Open(open_options_for("a")),
            FsSite::Open(open_options_for("wx")),
        ] {
            for path in [dir_s, file_s] {
                assert_eq!(
                    fs_error_at(site, "open", path, &denied),
                    expect("EPERM", "open", true),
                    "{site:?} {path}"
                );
            }
        }
        // An exclusive open of a directory is the table's EEXIST.
        for flags in ["wx", "wx+", "ax", "ax+"] {
            assert_eq!(
                fs_error_at(
                    FsSite::Open(open_options_for(flags)),
                    "open",
                    dir_s,
                    &raw(ERROR_FILE_EXISTS)
                ),
                expect("EEXIST", "open", true),
                "{flags}"
            );
        }
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
        assert_eq!(
            fs_error_at(
                FsSite::Open(open_options_for("r")),
                "open",
                file_s,
                &raw(ERROR_DIRECTORY)
            ),
            expect("ENOENT", "open", true)
        );
        // ...but only mkdir: every other operation keeps the table's answer.
        assert_eq!(
            fs_error_at(
                FsSite::Open(open_options_for("r")),
                "open",
                "a*b",
                &raw(ERROR_INVALID_NAME)
            ),
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
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A directory opens on Windows as it does under open(2), and the
    /// descriptor's operations are what fail -- each row measured on node
    /// v22.22.2: `r`, `r+`, `a` and `a+` open; `w` is EISDIR and an exclusive
    /// open EEXIST, both naming the path; a read is EISDIR and a write EISDIR,
    /// or EBADF on a descriptor not opened for it.
    #[cfg(windows)]
    #[test]
    fn windows_a_directory_opens_and_its_descriptor_fails_the_transfer() {
        use std::io::Read;
        let dir = whole_file_dir("dirfd");
        let dir_s = dir.to_str().unwrap();
        let expect = |code, syscall, has_path| FsError {
            code,
            syscall,
            has_path,
        };
        for (flags, read, write) in [
            ("r", "EISDIR", "EBADF"),
            ("r+", "EISDIR", "EISDIR"),
            ("a", "EBADF", "EISDIR"),
            ("a+", "EISDIR", "EISDIR"),
        ] {
            let file = open_options_for(flags)
                .open(dir_s)
                .unwrap_or_else(|e| panic!("{flags}: {e:?}"));
            assert!(file.metadata().unwrap().is_dir(), "{flags}");
            let failed = (&file).read(&mut [0u8; 8]).unwrap_err();
            assert_eq!(fd_error_code(&failed), read, "{flags} read: {failed:?}");
            // An empty write reaches the descriptor, as node's does.
            for bytes in [&b"x"[..], &b""[..]] {
                let failed = write_all_checked(&file, bytes).unwrap_err();
                assert_eq!(fd_error_code(&failed), write, "{flags} write: {failed:?}");
            }
            // fchmod is refused on a directory whatever it was opened for.
            let failed = ops::fs_fchmod_sync(&file, 0o666).unwrap_err();
            assert_eq!(node_error_code(&failed), "EPERM", "{flags}: {failed:?}");
        }
        for (flags, code) in [
            ("w", "EISDIR"),
            ("w+", "EISDIR"),
            ("wx", "EEXIST"),
            ("wx+", "EEXIST"),
            ("ax", "EEXIST"),
            ("ax+", "EEXIST"),
        ] {
            let failed = open_options_for(flags).open(dir_s).unwrap_err();
            assert_eq!(
                fs_error_at(
                    FsSite::Open(open_options_for(flags)),
                    "open",
                    dir_s,
                    &failed
                ),
                expect(code, "open", true),
                "{flags}: {failed:?}"
            );
        }

        // The whole-file operations: writeFile fails on the open, appendFile
        // opens and fails on the write -- unless there is nothing to write.
        let failed = write_whole_file(dir_s, b"x", false, 0o666).unwrap_err();
        assert!(matches!(failed, WholeFileError::Open(_)), "{failed:?}");
        assert_eq!(
            whole_file_error(FsSite::WriteFile, dir_s, &failed),
            expect("EISDIR", "open", true)
        );
        let failed = write_whole_file(dir_s, b"x", true, 0o666).unwrap_err();
        assert!(matches!(failed, WholeFileError::Transfer(_)), "{failed:?}");
        let failure = whole_file_error(FsSite::AppendFile, dir_s, &failed);
        assert_eq!(failure, expect("EISDIR", "write", false));
        assert_eq!(
            fs_error_message(failure, dir_s, failed.io()),
            "EISDIR: illegal operation on a directory, write"
        );
        write_whole_file(dir_s, b"", true, 0o666).expect("an empty append to a directory succeeds");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A listing of what is no directory fails as libuv's does, not with
    /// the one ERROR_DIRECTORY std has for all of it: a file is `ENOTDIR` for
    /// `readdir` and `opendir` both, and `readdir` of a file named with a
    /// trailing separator -- which only `FsPath::for_scandir` keeps -- is
    /// the `ENOENT` of the open libuv makes first. Links of the wrong kind
    /// are conformance case 393's: making one needs a privilege a test run
    /// may not have.
    #[cfg(windows)]
    #[test]
    fn windows_a_listing_of_what_is_no_directory() {
        // The directory's real path: through a junction or symlink above it
        // the open of `f.txt\` succeeds and the answer is ENOTDIR (in node
        // too), and a temp directory may be reached through one.
        let dir = std::fs::canonicalize(whole_file_dir("listing")).unwrap();
        let dir_s = dir.to_str().unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, b"f").unwrap();
        let file_s = file.to_str().unwrap();
        let code = |path: &str, raw: bool| match ops::read_dir_listing(path, raw) {
            Ok(listing) => panic!("{path} listed: {listing:?}"),
            Err(e) => node_error_code(&e),
        };
        for raw in [false, true] {
            assert_eq!(code(file_s, raw), "ENOTDIR", "raw {raw}");
            assert_eq!(
                code(&format!(r"{dir_s}\missing"), raw),
                "ENOENT",
                "raw {raw}"
            );
            assert_eq!(code(&format!(r"{file_s}\x"), raw), "ENOENT", "raw {raw}");
        }
        for sep in ['\\', '/'] {
            let given = format!("{file_s}{sep}");
            let kept = FsPath::for_scandir(given.clone());
            assert!(kept.os().ends_with('\\'), "{}", kept.os());
            assert!(kept.shown().ends_with(r"f.txt\"), "{}", kept.shown());
            assert_eq!(code(kept.os(), false), "ENOENT");
            // Every other call's path drops it, opendir's among them.
            let dropped = FsPath::new(given);
            assert!(dropped.os().ends_with("f.txt"), "{}", dropped.os());
            assert_eq!(code(dropped.os(), true), "ENOTDIR");
        }
        // Not a separator: `f.txt\.` is `f.txt`.
        let dot = FsPath::for_scandir(format!(r"{file_s}\."));
        assert!(dot.os().ends_with("f.txt"), "{}", dot.os());
        // A directory lists either way, and a root keeps the one it has.
        let listed = FsPath::for_scandir(format!(r"{dir_s}\"));
        assert!(
            ops::read_dir_listing(listed.os(), false)
                .unwrap()
                .names
                .iter()
                .any(|name| name == "f.txt")
        );
        let root = FsPath::for_scandir(format!(r"{}\", &dir_s[..2]));
        assert!(
            root.os().ends_with(r":\") && !root.os().ends_with(r"\\"),
            "{}",
            root.os()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A truncating open REPLACES the file, as libuv's CREATE_ALWAYS does
    /// and std's open-then-truncate did not: an alternate data stream goes
    /// with the file it was on, a hidden file is refused (`EPERM`; `r+` and
    /// `a` still open it), and `w+` reads what it wrote. A named pipe is
    /// conformance case 394's.
    #[cfg(windows)]
    #[test]
    fn windows_a_truncating_open_replaces_the_file() {
        use std::io::{Read, Seek, Write};
        let dir = whole_file_dir("create-always");
        let file = dir.join("f.txt");
        let file_s = file.to_str().unwrap();
        let stream = format!("{file_s}:side");
        // Not every volume a temp directory sits on has streams (FAT, some
        // RAM disks); where there are none, only the rest is asserted.
        std::fs::write(&file, b"").unwrap();
        let streams = std::fs::write(&stream, b"stream").is_ok();
        let fill = || {
            std::fs::write(&file, b"0123456789").unwrap();
            if streams {
                std::fs::write(&stream, b"stream").unwrap();
            }
        };
        let stream_is_there = |there: bool| !streams || std::fs::read(&stream).is_ok() == there;
        for flags in ["r+", "a", "a+"] {
            fill();
            drop(open_options_for(flags).open(file_s).unwrap());
            assert!(stream_is_there(true), "{flags}");
            assert_eq!(std::fs::metadata(&file).unwrap().len(), 10, "{flags}");
        }
        for flags in ["w", "w+"] {
            fill();
            let mut opened = open_options_for(flags).open(file_s).unwrap();
            assert_eq!(opened.metadata().unwrap().len(), 0, "{flags}");
            assert!(stream_is_there(false), "{flags}");
            opened.write_all(b"new").unwrap();
            opened.rewind().unwrap();
            let mut back = String::new();
            let read = opened.read_to_string(&mut back);
            if flags == "w+" {
                read.unwrap();
                assert_eq!(back, "new");
            } else {
                assert_eq!(fd_error_code(&read.unwrap_err()), "EBADF");
            }
        }
        fill();
        write_whole_file(file_s, b"x", false, 0o666).unwrap();
        assert!(stream_is_there(false));
        assert_eq!(std::fs::read(&file).unwrap(), b"x");
        // A missing file is made; a missing directory is not.
        let fresh = dir.join("fresh.txt");
        drop(open_options_for("w").open(&fresh).unwrap());
        assert!(fresh.is_file());
        let failed = open_options_for("w")
            .open(dir.join("no").join("f"))
            .unwrap_err();
        assert_eq!(node_error_code(&failed), "ENOENT");
        // The name ends at a NUL for CreateFileW; it is refused before that.
        let failed = open_options_for("w")
            .open(format!("{file_s}\0x"))
            .unwrap_err();
        assert_eq!(failed.kind(), std::io::ErrorKind::InvalidInput);

        // A hidden file: CREATE_ALWAYS must be handed the attributes it has.
        fill();
        let hide = std::process::Command::new("attrib")
            .args(["+h", file_s])
            .status();
        if hide.is_ok_and(|status| status.success()) {
            for flags in ["w", "w+"] {
                let failed = open_options_for(flags).open(file_s).unwrap_err();
                assert_eq!(
                    fs_error_at(
                        FsSite::Open(open_options_for(flags)),
                        "open",
                        file_s,
                        &failed
                    )
                    .code,
                    "EPERM",
                    "{flags}: {failed:?}"
                );
            }
            let failed = write_whole_file(file_s, b"x", false, 0o666).unwrap_err();
            assert_eq!(
                whole_file_error(FsSite::WriteFile, file_s, &failed).code,
                "EPERM"
            );
            assert_eq!(std::fs::metadata(&file).unwrap().len(), 10);
            for flags in ["r+", "a"] {
                drop(open_options_for(flags).open(file_s).unwrap());
            }
            let _ = std::process::Command::new("attrib")
                .args(["-h", file_s])
                .status();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// fchmod reaches the file through any handle on it, one opened only to
    /// read included: the attribute is set on a reopened handle, as libuv's
    /// is. A directory does not reopen for it, and the NUL device has no
    /// attributes to read.
    #[cfg(windows)]
    #[test]
    fn windows_fchmod_does_not_need_a_handle_opened_to_write() {
        let dir = whole_file_dir("fchmod-reopen");
        let file = dir.join("f.txt");
        for flags in ["r", "r+", "a", "a+", "w", "w+"] {
            std::fs::write(&file, b"x").unwrap();
            let opened = open_options_for(flags).open(&file).unwrap();
            ops::fs_fchmod_sync(&opened, 0o444).unwrap_or_else(|e| panic!("{flags}: {e:?}"));
            assert!(
                opened.metadata().unwrap().permissions().readonly(),
                "{flags}"
            );
            ops::fs_fchmod_sync(&opened, 0o666).unwrap_or_else(|e| panic!("{flags}: {e:?}"));
            assert!(
                !opened.metadata().unwrap().permissions().readonly(),
                "{flags}"
            );
        }
        // A file that is already read-only opens only to read, and that is
        // enough to make it writable again.
        let opened = open_options_for("r").open(&file).unwrap();
        ops::fs_fchmod_sync(&opened, 0o444).unwrap();
        drop(opened);
        let opened = open_options_for("r").open(&file).unwrap();
        ops::fs_fchmod_sync(&opened, 0o644).unwrap();
        assert!(!std::fs::metadata(&file).unwrap().permissions().readonly());
        drop(opened);
        // Read-only as the file's ONLY attribute: clearing it comes to an
        // attribute word of 0, which must be written as "normal" to count.
        // And the archive bit a file does not have stays off.
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_READONLY,
            };
            let attrs = || std::fs::metadata(&file).unwrap().file_attributes();
            let clear = std::process::Command::new("attrib")
                .arg("-a")
                .arg(&file)
                .status();
            if clear.is_ok_and(|status| status.success()) {
                let opened = open_options_for("r").open(&file).unwrap();
                let changed = ops::basic_info(&opened).unwrap().ChangeTime;
                ops::fs_fchmod_sync(&opened, 0o444).unwrap();
                assert_eq!(
                    attrs() & (FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ARCHIVE),
                    FILE_ATTRIBUTE_READONLY
                );
                ops::fs_fchmod_sync(&opened, 0o666).unwrap();
                assert_eq!(
                    attrs() & (FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_ARCHIVE),
                    0
                );
                // The change time is written back as it was read.
                assert_eq!(ops::basic_info(&opened).unwrap().ChangeTime, changed);
            }
        }
        for flags in ["r", "r+", "a"] {
            let opened = open_options_for(flags).open(&dir).unwrap();
            let failed = ops::fs_fchmod_sync(&opened, 0o666).unwrap_err();
            assert_eq!(node_error_code(&failed), "EPERM", "{flags}: {failed:?}");
            let opened = open_options_for(flags).open(r"\\.\NUL").unwrap();
            let failed = ops::fs_fchmod_sync(&opened, 0o666).unwrap_err();
            assert_eq!(node_error_code(&failed), "EINVAL", "{flags}: {failed:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An open needs the write grant unless it can only read: read-only
    /// access and nothing else but flags that change nothing.
    #[test]
    fn an_open_that_is_not_plainly_a_read_needs_the_write_grant() {
        for flags in ["r", "rs", "sr"] {
            assert!(!open_options_for(flags).writes(), "{flags}");
        }
        for flags in [
            "r+", "rs+", "w", "wx", "w+", "wx+", "a", "ax", "as", "a+", "ax+", "as+",
        ] {
            assert!(open_options_for(flags).writes(), "{flags}");
        }
        // Read-only access with a flag that creates, empties or appends.
        for bit in [o_flag::CREAT, o_flag::TRUNC, o_flag::APPEND, o_flag::EXCL] {
            assert!(OpenFlags(o_flag::RDONLY | bit).writes(), "{bit:#x}");
        }
        // Every bit that is not on the short list, the unknown ones included.
        for shift in 0..31 {
            let bit = 1i32 << shift;
            let harmless = bit & o_flag::HARMLESS != 0;
            assert_eq!(OpenFlags(bit).writes(), !harmless, "{bit:#x}");
        }
        assert!(OpenFlags(-1).writes());
        assert!(OpenFlags(i32::MIN).writes());
        // The read grant: every access mode but write-only.
        for flags in ["r", "r+", "w+", "a+", "wx+", "ax+", "rs+"] {
            assert!(open_options_for(flags).reads(), "{flags}");
        }
        for flags in ["w", "a", "wx", "ax", "as"] {
            assert!(!open_options_for(flags).reads(), "{flags}");
        }
        // Delete-on-close is no read, whatever the access mode.
        #[cfg(windows)]
        {
            assert!(OpenFlags(o_flag::TEMPORARY).writes());
            assert!(OpenFlags(o_flag::FILEMAP).writes());
            assert!(!OpenFlags(o_flag::SEQUENTIAL | o_flag::RANDOM).writes());
        }
        #[cfg(unix)]
        assert!(!OpenFlags(o_flag::DIRECTORY | o_flag::NOFOLLOW).writes());
    }

    /// A device is no share root: `\\.\NUL` opens for every flag, the
    /// truncating ones included, and the refusal `open_path` keeps for a
    /// share that is no disk does not reach it.
    #[cfg(windows)]
    #[test]
    fn windows_a_device_opens_for_a_truncating_open() {
        for name in [r"\\.\NUL", r"\\?\NUL"] {
            assert!(!is_share_root(std::path::Path::new(name)), "{name}");
            for flags in ["r", "r+", "w", "w+", "a"] {
                let opened = open_options_for(flags).open(name);
                assert!(opened.is_ok(), "{name} {flags}: {opened:?}");
            }
            write_whole_file(name, b"gone", false, 0o666).unwrap();
        }
        for (name, root) in [
            (r"\\.\CON", false),
            (r"\\?\C:\", false),
            (r"\\?\UNC\srv\IPC$", true),
            (r"\\?\UNC\srv\IPC$\", true),
            (r"\\srv\share\", true),
            (r"\\srv\share\f", false),
        ] {
            assert_eq!(is_share_root(std::path::Path::new(name)), root, "{name}");
        }
    }

    /// A drive root or a UNC share root is a directory like any other for
    /// reading: it opens, and its readFile fails on the read (`EISDIR`,
    /// syscall `read`, no path). Any open of it for writing is refused
    /// (`EPERM` `open` with the path) -- measured on node v22.22.2, not
    /// elevated, for `C:\`, `\`, `/` and `\\localhost\C$`.
    #[cfg(windows)]
    #[test]
    fn windows_a_root_opens_for_reading_only() {
        let temp = std::env::temp_dir();
        let temp = temp.to_str().unwrap();
        let mut roots = vec![format!(r"\\?\{}\", &temp[..2])];
        // The loopback admin share exists only where file sharing is on.
        let share = format!(r"\\?\UNC\localhost\{}$\", &temp[..1]);
        if std::path::Path::new(&share).is_dir() {
            // The spelling the JS layer sends has no trailing separator.
            roots.push(share.trim_end_matches('\\').to_string());
            roots.push(share);
        }
        let expect = |code, syscall, has_path| FsError {
            code,
            syscall,
            has_path,
        };
        for root in &roots {
            let file = open_options_for("r")
                .open(root)
                .unwrap_or_else(|e| panic!("{root}: {e:?}"));
            assert!(file.metadata().unwrap().is_dir(), "{root}");
            let failed = read_whole_file(root).unwrap_err();
            assert!(matches!(failed, WholeFileError::Transfer(_)), "{failed:?}");
            assert_eq!(
                whole_file_error(FsSite::ReadFile, root, &failed),
                expect("EISDIR", "read", false),
                "{root}: {failed:?}"
            );
            // A truncating open never opens a root.
            let failed = write_whole_file(root, b"", false, 0o666).unwrap_err();
            assert_eq!(
                whole_file_error(FsSite::WriteFile, root, &failed),
                expect("EPERM", "open", true),
                "{root}: {failed:?}"
            );
            for flags in ["w", "w+"] {
                let failed = open_options_for(flags).open(root).unwrap_err();
                assert_eq!(
                    fs_error_at(FsSite::Open(open_options_for(flags)), "open", root, &failed),
                    expect("EPERM", "open", true),
                    "{root} {flags}: {failed:?}"
                );
            }
            // Any other open for writing is refused by the ACL, and an
            // elevated process is not refused: only a refusal has a shape.
            if let Err(failed) = write_whole_file(root, b"", true, 0o666) {
                assert_eq!(
                    whole_file_error(FsSite::AppendFile, root, &failed),
                    expect("EPERM", "open", true),
                    "{root}: {failed:?}"
                );
            }
            for flags in ["a", "r+"] {
                if let Err(failed) = open_options_for(flags).open(root) {
                    assert_eq!(
                        fs_error_at(FsSite::Open(open_options_for(flags)), "open", root, &failed),
                        expect("EPERM", "open", true),
                        "{root} {flags}: {failed:?}"
                    );
                }
            }
        }
        // A share that is no disk has a root that does not open, whatever
        // the flag: EINVAL, as a stat of it is. Only where the loopback
        // share answers at all.
        let ipc = r"\\?\UNC\localhost\IPC$";
        if std::fs::metadata(ipc).is_err_and(|e| node_error_code(&e) == "EINVAL") {
            for flags in ["r", "r+", "a", "ax", "w", "wx"] {
                let failed = open_options_for(flags).open(ipc).unwrap_err();
                assert_eq!(
                    fs_error_at(FsSite::Open(open_options_for(flags)), "open", ipc, &failed),
                    expect("EINVAL", "open", true),
                    "{flags}: {failed:?}"
                );
            }
            let failed = read_whole_file(ipc).unwrap_err();
            assert_eq!(
                whole_file_error(FsSite::ReadFile, ipc, &failed),
                expect("EINVAL", "open", true),
                "{failed:?}"
            );
            for (site, append) in [(FsSite::WriteFile, false), (FsSite::AppendFile, true)] {
                let failed = write_whole_file(ipc, b"", append, 0o666).unwrap_err();
                assert_eq!(
                    whole_file_error(site, ipc, &failed),
                    expect("EINVAL", "open", true),
                    "{site:?}: {failed:?}"
                );
            }
        }
        // A path that is not there is still ENOENT.
        let missing = format!(r"\\?\{temp}\oam-no-such-root-{}", std::process::id());
        let gone = read_whole_file(&missing).unwrap_err();
        assert_eq!(
            whole_file_error(FsSite::ReadFile, &missing, &gone),
            expect("ENOENT", "open", true)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn mkdtemp_template_matches_node_binding() {
        assert_eq!(mkdtemp_template("x-"), "x-XXXXXX");
        assert_eq!(mkdtemp_template("sub/"), "sub/XXXXXX");
        // node's snprintf bound leaves an empty prefix five X's.
        assert_eq!(mkdtemp_template(""), "XXXXX");
    }

    /// A fresh directory for one mkdtemp test, relative names joined to it.
    fn mkdtemp_test_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("oam-mkdtemp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        format!("{}/", dir.to_str().unwrap())
    }

    #[test]
    fn mkdtemp_names_six_characters_of_base62() {
        let base = mkdtemp_test_dir("shape");
        let template = mkdtemp_template(&format!("{base}p-"));
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let dir = mkdtemp(&template).unwrap();
            let suffix = dir.strip_prefix(&format!("{base}p-")).unwrap();
            assert_eq!(suffix.len(), 6, "{dir}");
            assert!(suffix.bytes().all(|b| b.is_ascii_alphanumeric()), "{dir}");
            assert!(std::path::Path::new(&dir).is_dir());
            assert!(seen.insert(dir));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn mkdtemp_tries_again_only_when_the_name_exists() {
        let base = mkdtemp_test_dir("retry");
        let template = format!("{base}r-XXXXXX");
        // Draw 0 spells "aaaaaa" (libuv's least-significant-first base 62),
        // draw 1 "baaaaa".
        std::fs::create_dir(format!("{base}r-aaaaaa")).unwrap();
        let mut draws = [0u64, 0, 1].into_iter();
        let dir = mkdtemp_drawing(&template, 10, false, || Ok(draws.next().unwrap())).unwrap();
        assert_eq!(dir, format!("{base}r-baaaaa"));
        assert_eq!(draws.next(), None);

        // Out of tries: EEXIST, after exactly `tries` draws.
        let mut count = 0;
        let (e, path) = mkdtemp_drawing(&template, 3, false, || {
            count += 1;
            Ok(0)
        })
        .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(count, 3);
        // libuv's Windows loop clobbers the path to "" when it runs out.
        let expected = if cfg!(windows) {
            String::new()
        } else {
            format!("{base}r-aaaaaa")
        };
        assert_eq!(path, expected);

        // Any other failure stops at once, leaving the template on Windows
        // (libuv fills it in only on success) and the name tried elsewhere.
        let missing = format!("{base}nope/q-XXXXXX");
        let mut count = 0;
        let (e, path) = mkdtemp_drawing(&missing, 10, false, || {
            count += 1;
            Ok(0)
        })
        .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(count, 1);
        if cfg!(windows) {
            assert_eq!(path, missing);
        } else {
            assert_eq!(path, format!("{base}nope/q-aaaaaa"));
        }

        // A template not ending in six X's is EINVAL, with no draw.
        let (e, path) = mkdtemp_drawing("XXXXX", 10, false, || unreachable!()).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(path, if cfg!(windows) { "" } else { "XXXXX" });

        // A failed draw is EIO-shaped and refuses like EINVAL.
        let (e, path) = mkdtemp_drawing(&template, 10, false, || {
            Err(std::io::Error::other("no entropy"))
        })
        .unwrap_err();
        assert_eq!(node_error_code(&e), "EIO");
        assert_eq!(path, if cfg!(windows) { "" } else { template.as_str() });
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn mkdtemp_x_rules_six_or_the_whole_run() {
        let base = mkdtemp_test_dir("xrun");
        let suffix_of = |dir: &str, stem: &str| {
            dir.strip_prefix(&format!("{base}{stem}"))
                .unwrap()
                .to_string()
        };

        // Windows / glibc: exactly the last six, prefix X's kept.
        let six = mkdtemp_drawing(
            &mkdtemp_template(&format!("{base}xXXXXXX")),
            10,
            false,
            || Ok(0),
        )
        .unwrap();
        assert_eq!(suffix_of(&six, "xXXXXXX"), "aaaaaa");

        // macOS: the whole trailing run, the prefix's own X's included, from
        // as many draws as it takes (ten characters per draw).
        let mut draws = 0;
        let run = mkdtemp_drawing(
            &mkdtemp_template(&format!("{base}yXXXXXX")),
            10,
            true,
            || {
                draws += 1;
                Ok(1)
            },
        )
        .unwrap();
        assert_eq!(suffix_of(&run, "y"), "baaaaaaaaaba", "12 X's from 2 draws");
        assert_eq!(draws, 2);
        let ax =
            mkdtemp_drawing(&mkdtemp_template(&format!("{base}a")), 10, true, || Ok(0)).unwrap();
        assert_eq!(
            suffix_of(&ax, "a"),
            "aaaaaa",
            "no X in the prefix: six, as elsewhere"
        );
        let ax =
            mkdtemp_drawing(&mkdtemp_template(&format!("{base}aX")), 10, true, || Ok(0)).unwrap();
        assert_eq!(suffix_of(&ax, "a"), "aaaaaaa", "aX: a plus seven");
        assert!(std::path::Path::new(&ax).is_dir());
        // The empty prefix's five X's are accepted and filled.
        let empty = format!("{base}XXXXX");
        let five = mkdtemp_drawing(&empty, 10, true, || Ok(2)).unwrap();
        assert_eq!(suffix_of(&five, ""), "caaaa");
        // ...and refused under the six-X rule.
        let (e, _) = mkdtemp_drawing(&empty, 10, false, || unreachable!()).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        // Only an empty template is refused outright; one with no X's is
        // tried once, as given.
        let (e, _) = mkdtemp_drawing("", 10, true, || unreachable!()).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        let (e, _) = mkdtemp_drawing(&five, 10, true, || unreachable!()).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_dir_all(&base);
    }

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
        core.spawn_op(ops::read_text_file(FsPath::new(
            "/definitely/not/here.txt".into(),
        )));
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

    // Guard 1: disable_stdio_inheritance skips a consumed fd. The existing test
    // proves inherited_eligible/adopt_inherited_fd respect consumption; this
    // pins the same rule at the selection the function passes to its platform
    // I/O. Tested through the pure helper, so no snapshot is needed and no live
    // descriptor is probed or touched.
    #[test]
    fn disabled_selection_skips_consumed_fds() {
        let snapshot = std::collections::HashSet::from([3u64, 9]);
        let consumed = std::collections::HashSet::from([9u64]);
        let mut got = inheritable_fds_to_disable(&snapshot, &consumed);
        got.sort_unstable();
        assert_eq!(got, vec![3]);
    }

    // Guard 2: the fd >= 3 restatement filter. The snapshot only ever holds
    // 3..=MAX_INHERITED_FD, so the filter is defensive -- but the unix rule is
    // NEVER touch 0-2, so the selection must drop them even if the snapshot some
    // day lists them.
    #[test]
    fn disabled_selection_excludes_stdio_fds() {
        let snapshot = std::collections::HashSet::from([0u64, 1, 2, 3, 9]);
        let consumed = std::collections::HashSet::new();
        let mut got = inheritable_fds_to_disable(&snapshot, &consumed);
        got.sort_unstable();
        assert_eq!(got, vec![3, 9]);
    }

    /// Descriptors 0-2 are the process's stdio for the fd calls, adopted on
    /// first use like an inherited descriptor; they used to be EBADF. Closing
    /// one on Windows is libuv's no-op, so it stays usable. (Not run on unix,
    /// where the close is real and would take the test runner's stdin.)
    #[cfg(windows)]
    #[test]
    fn stdio_descriptors_are_adopted_and_survive_close_on_windows() {
        let registry: FileRegistry =
            std::sync::Arc::new(std::sync::Mutex::new(FileState::default()));
        let stderr = registered_file(&registry, 2).expect("stderr is a descriptor");
        assert!(ops::fstat_record(&stderr).is_ok());
        assert!(close_descriptor(&registry, 2));
        assert!(registered_file(&registry, 2).is_some());
        assert!(registered_file(&registry, OWN_FD_BASE + 12345).is_none());
        assert!(!close_descriptor(&registry, OWN_FD_BASE + 12345));
    }

    /// A descriptor with several ops in flight at once must serve all of them,
    /// as node does. The chunk ops used to take the file out of the registry
    /// for their IO await, so every op that started while another was in
    /// flight found the slot empty and failed with EBADF.
    /// node's positions through libuv: -1 is the cursor everywhere; any other
    /// negative is the cursor on unix, and on Windows goes to the OS, which
    /// takes -2 as "at the cursor, which does not move" and refuses -5 with
    /// EINVAL, touching nothing.
    #[test]
    fn a_negative_position_is_what_libuv_makes_of_it() {
        assert_eq!(file_offset(None), None);
        assert_eq!(file_offset(Some(-1)), None);
        assert_eq!(file_offset(Some(0)), Some(0));
        assert_eq!(file_offset(Some(7)), Some(7));

        let dir = std::env::temp_dir().join(format!("oam-file-offset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        std::fs::write(&path, b"ABCDEF").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut one = [0u8; 1];
        assert_eq!(read_at(&file, &mut one, None).unwrap(), 1); // cursor at 1
        let refused = write_all_at(&file, b"x", Some(-5));
        let at_cursor = write_all_at(&file, b"y", Some(-2));
        let read = read_at(&file, &mut one, Some(-3));
        if cfg!(windows) {
            assert_eq!(node_error_code(&refused.unwrap_err()), "EINVAL");
            at_cursor.unwrap();
            assert_eq!(node_error_code(&read.unwrap_err()), "EINVAL");
            // "y" at the cursor (1), which stayed at 1.
            assert_eq!(std::fs::read(&path).unwrap(), b"AyCDEF");
            assert_eq!(read_at(&file, &mut one, None).unwrap(), 1);
            assert_eq!(&one, b"y");
        } else {
            assert_eq!(file_offset(Some(-5)), None);
            refused.unwrap();
            at_cursor.unwrap();
            assert_eq!(read.unwrap(), 1);
            assert_eq!(&one, b"D");
            assert_eq!(std::fs::read(&path).unwrap(), b"AxyDEF");
        }
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_chunk_ops_on_one_descriptor_all_reach_it() {
        let dir = std::env::temp_dir().join(format!("oam-fd-shared-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, b"ABCDEFGHIJKLMNOPQRSTUVWXYZ").unwrap();

        let mut core = CoreRuntime::new().unwrap();
        let files = core.files();
        let file = open_options_for("r+").open(&path).unwrap();
        files
            .lock()
            .unwrap()
            .files
            .insert(OWN_FD_BASE, std::sync::Arc::new(file));

        // A positional read leaves the cursor where it was: a cursor read
        // after it starts at 0. (Checked alone: on Windows, as in libuv, a
        // positional op saves and restores the cursor, so where CONCURRENT
        // ones leave it depends on their interleaving.)
        let file = registered_file(&files, OWN_FD_BASE).expect("registered");
        let mut two = [0u8; 2];
        assert_eq!(read_at(&file, &mut two, Some(10)).unwrap(), 2);
        assert_eq!(&two, b"KL");
        let mut head = [0u8; 3];
        assert_eq!(read_at(&file, &mut head, None).unwrap(), 3);
        assert_eq!(&head, b"ABC");
        drop(file);

        let mut reads = HashMap::new();
        for i in 0..8i64 {
            let id = core.spawn_op(ops::fs_read_chunk(
                files.clone(),
                OWN_FD_BASE,
                2,
                Some(i * 2),
            ));
            reads.insert(id, i);
        }
        let mut writes = Vec::new();
        for i in 0..4i64 {
            writes.push(core.spawn_op(ops::fs_write_chunk(
                files.clone(),
                OWN_FD_BASE,
                vec![b'0' + i as u8],
                Some(20 + i),
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        for _ in 0..12 {
            let done = core.recv_deadline(Some(deadline)).expect("op completes");
            match reads.get(&done.id) {
                Some(&i) => match done.outcome {
                    OpOutcome::Bytes(b) => {
                        let at = (i * 2) as usize;
                        assert_eq!(b, &b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"[at..at + 2], "read {i}");
                    }
                    _ => panic!("read {i} did not return bytes"),
                },
                None => assert!(matches!(done.outcome, OpOutcome::Done), "a write failed"),
            }
        }

        // Closing drops the registry's reference; the next op is EBADF.
        files.lock().unwrap().files.remove(&OWN_FD_BASE);
        core.spawn_op(ops::fs_read_chunk(files.clone(), OWN_FD_BASE, 2, None));
        let done = core.recv_deadline(Some(deadline)).expect("op completes");
        assert!(
            matches!(&done.outcome, OpOutcome::NodeFailed { code, .. } if code == "EBADF"),
            "a closed descriptor is EBADF"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"ABCDEFGHIJKLMNOPQRST0123YZ".to_vec()
        );
        let _ = std::fs::remove_dir_all(&dir);
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
            win_error: None,
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
                dest,
                win_error,
            } => {
                assert_eq!(win_error, None);
                assert_eq!(code, "ENOENT");
                assert_eq!(message, "ENOENT: no such file or directory, open 'x'");
                assert_eq!(syscall.as_deref(), Some("open"));
                assert_eq!(path.as_deref(), Some("x"));
                assert_eq!(errno, Some(-4058));
                assert_eq!((hostname, address, port), (None, None, None));
                assert_eq!(dest, None);
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
            win_error: None,
        };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"code":"EAI_AGAIN","message":"getaddrinfo EAI_AGAIN"}"#
        );
    }

    /// The Win32 code behind an UNKNOWN (#450) is written only when there is
    /// one -- a recording without it is what an older oam wrote -- and a
    /// recording that has it loads with it, as does one from before the field.
    #[test]
    fn win_error_is_written_only_when_present() {
        let with = OpOutcome::node_failed_at("UNKNOWN", "m", "write", None, Some(-4094))
            .with_win_error(Some(536));
        let json = serde_json::to_string(&with).unwrap();
        assert_eq!(
            json,
            r#"{"NodeFailed":{"code":"UNKNOWN","message":"m","syscall":"write","errno":-4094,"win_error":536}}"#
        );
        match serde_json::from_str::<OpOutcome>(&json).unwrap() {
            OpOutcome::NodeFailed { win_error, .. } => assert_eq!(win_error, Some(536)),
            other => panic!("{other:?}"),
        }
        let old =
            r#"{"NodeFailed":{"code":"UNKNOWN","message":"m","syscall":"write","errno":-4094}}"#;
        match serde_json::from_str::<OpOutcome>(old).unwrap() {
            OpOutcome::NodeFailed { win_error, .. } => assert_eq!(win_error, None),
            other => panic!("{other:?}"),
        }
        // `with_win_error` leaves any other outcome alone.
        assert!(matches!(
            OpOutcome::Done.with_win_error(Some(1)),
            OpOutcome::Done
        ));
        let mut sys_err = sys("UNKNOWN");
        sys_err.win_error = Some(10041);
        assert!(
            serde_json::to_string(&sys_err)
                .unwrap()
                .ends_with(r#""win_error":10041}"#)
        );
        match OpOutcome::sys(sys_err) {
            OpOutcome::NodeFailed { win_error, .. } => assert_eq!(win_error, Some(10041)),
            other => panic!("{other:?}"),
        }
    }

    /// `node_failed_io` reads errno and the Win32 code off one error (#450):
    /// on Windows an unlisted code carries both, a named one its errno alone.
    #[cfg(windows)]
    #[test]
    fn node_failed_io_keeps_the_win32_code_behind_unknown() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_LISTENING};
        let listening = std::io::Error::from_raw_os_error(ERROR_PIPE_LISTENING as i32);
        let code = node_error_code(&listening);
        match OpOutcome::node_failed_io(code, "write UNKNOWN", "write", None, &listening) {
            OpOutcome::NodeFailed {
                code,
                errno,
                win_error,
                ..
            } => {
                assert_eq!(code, "UNKNOWN");
                assert_eq!(errno, Some(-4094));
                assert_eq!(win_error, Some(ERROR_PIPE_LISTENING));
            }
            other => panic!("{other:?}"),
        }
        let denied = std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32);
        let code = node_error_code(&denied);
        match OpOutcome::node_failed_dest_io(code, "m", "rename", "a", "b", &denied) {
            OpOutcome::NodeFailed {
                code,
                errno,
                win_error,
                dest,
                ..
            } => {
                assert_eq!(code, "EPERM");
                assert_eq!(errno, Some(-4048));
                assert_eq!(win_error, None);
                assert_eq!(dest.as_deref(), Some("b"));
            }
            other => panic!("{other:?}"),
        }
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
                dest,
                win_error,
            } => {
                assert_eq!(code, "ECONNREFUSED");
                assert_eq!(message, "connect ECONNREFUSED 127.0.0.1:8080");
                assert_eq!(syscall.as_deref(), Some("connect"));
                assert_eq!((path, dest), (None, None));
                assert_eq!(win_error, None);
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

    /// The socket facts serialise under undici's own keys -- the engine hands
    /// that JSON to JS as the error's `socket` -- an absent fact is left out,
    /// and the outcome survives a replay file.
    #[test]
    fn a_peer_close_carries_undicis_socket_keys_and_round_trips() {
        let socket = SocketFacts {
            local_address: Some("127.0.0.1".into()),
            local_port: Some(50000),
            remote_address: Some("::1".into()),
            remote_port: Some(8080),
            remote_family: Some("IPv6".into()),
            bytes_written: Some(170),
            bytes_read: None,
        };
        assert_eq!(
            serde_json::to_string(&socket).unwrap(),
            r#"{"localAddress":"127.0.0.1","localPort":50000,"remoteAddress":"::1","remotePort":8080,"remoteFamily":"IPv6","bytesWritten":170}"#
        );
        let json = serde_json::to_string(&OpOutcome::socket_closed(socket.clone())).unwrap();
        match serde_json::from_str::<OpOutcome>(&json).unwrap() {
            OpOutcome::SocketClosed {
                message,
                socket: back,
            } => {
                assert_eq!(message, "other side closed");
                assert_eq!(back, socket);
            }
            other => panic!("expected SocketClosed, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod unlink_path_tests {
    /// libuv's unlink removes a directory link (on Windows a directory
    /// symlink, which std's remove_file refuses EPERM) and leaves its target,
    /// but refuses a real directory (#390).
    #[test]
    fn unlink_removes_a_directory_link_but_not_a_directory() {
        let dir = std::env::temp_dir().join(format!("oam-unlink-{}", std::process::id()));
        let target = dir.join("u");
        let link = dir.join("l");
        std::fs::create_dir_all(&target).unwrap();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&target, &link);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&target, &link);
        if made.is_ok() {
            super::unlink_path(&link).unwrap();
            assert!(
                std::fs::symlink_metadata(&link).is_err(),
                "the link is gone"
            );
            assert!(target.is_dir(), "its target is not");
        }
        assert!(
            super::unlink_path(&target).is_err(),
            "a directory is no file to unlink"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
