//! Pipe streams for node:net: a Windows named pipe or a Unix domain socket,
//! what `net.connect({ path })` dials and `server.listen(path)` listens on
//! (#219).
//!
//! The streams live in the TCP registry beside the TCP ones ([`crate::tcp`]),
//! so a pipe socket reads, writes, ends, closes and refs through the very ops
//! a TCP socket does. This module holds what differs: how a pipe is dialled
//! and listened on, the error each of those reports in node's shape, and on
//! Windows the stream halves themselves.
//!
//! - Unix: a `UnixStream` split into owned halves, which behave as a TCP
//!   stream's do (a write half dropped is a `shutdown(SHUT_WR)`).
//! - Windows: one named-pipe handle shared by both halves. A named pipe has
//!   no half-close, so libuv's shutdown of one -- what `end()` asks for --
//!   waits until the peer has read everything written (FlushFileBuffers) and
//!   then ends the stream both ways: the side that ended reads EOF too
//!   (measured on node v22.22.2: `end()` gives 'finish', then the socket's
//!   own 'end', whatever the peer does).

use crate::{NodeSysError, node_errno, node_error_code};

/// node's `connect` error for a pipe (lib/net.js `afterConnect` ->
/// `ExceptionWithHostPort(status, 'connect', address)`): `connect ENOENT
/// <path>` with errno, code, syscall and the path as `address`, no port.
pub(crate) fn connect_error(error: &std::io::Error, path: &str) -> Box<NodeSysError> {
    let code = node_error_code(error);
    connect_error_coded(code, error, path)
}

fn connect_error_coded(
    code: &'static str,
    error: &std::io::Error,
    path: &str,
) -> Box<NodeSysError> {
    Box::new(NodeSysError {
        code: code.to_string(),
        message: format!("connect {code} {path}"),
        errno: node_errno(code, error),
        syscall: Some("connect".to_string()),
        hostname: None,
        address: Some(path.to_string()),
        port: None,
    })
}

/// node's `listen` error for a pipe (`uvExceptionWithHostPort(err, 'listen',
/// path, -1)`): `listen EADDRINUSE: address already in use <path>`. The
/// port node puts on it (-1) is added by the JS, which owns the key order.
pub(crate) fn listen_error(
    code: &'static str,
    error: &std::io::Error,
    path: &str,
) -> Box<NodeSysError> {
    let text = crate::uv_strerror(code)
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    Box::new(NodeSysError {
        code: code.to_string(),
        message: format!("listen {code}: {text} {path}"),
        errno: node_errno(code, error),
        syscall: Some("listen".to_string()),
        hostname: None,
        address: Some(path.to_string()),
        port: None,
    })
}

#[cfg(windows)]
pub(crate) use windows::{PipeListener, PipeRead, PipeWrite, bind, connect};

#[cfg(unix)]
pub(crate) use unix::{PipeListener, bind, connect};

#[cfg(windows)]
mod windows {
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INVALID_NAME, ERROR_NO_DATA, ERROR_PATH_NOT_FOUND,
        ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT,
    };

    /// How long a dial waits for a busy pipe (every instance taken) to free
    /// one: libuv's `WaitNamedPipeW(name, 30000)`.
    const BUSY_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

    /// How many instances of a listening pipe wait for clients at once:
    /// libuv's `pending_instances` default (4). With one, a burst of clients
    /// was admitted one per accept, the rest finding the pipe busy.
    const PENDING_INSTANCES: usize = 4;

    /// The most one write hands the pipe at a time: the pipe's buffer
    /// (tokio's default in and out buffer, as libuv's 64 KiB). tokio's named
    /// pipe takes a write whole into a buffer of its own and reports every
    /// byte written while its overlapped WriteFile is still waiting for room,
    /// so a write handed over whole was "done" before the peer could have
    /// any of it. In chunks, a chunk is handed over only once the one before
    /// it has gone into the pipe, so a write is reported done with at most
    /// its last chunk still waiting for room (see docs/node-divergences.md,
    /// entry 49).
    const WRITE_CHUNK: usize = 64 * 1024;

    /// One end of a connected named pipe.
    enum End {
        Client(NamedPipeClient),
        Server(NamedPipeServer),
    }

    impl End {
        async fn readable(&self) -> io::Result<()> {
            match self {
                End::Client(pipe) => pipe.readable().await,
                End::Server(pipe) => pipe.readable().await,
            }
        }

        fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
            match self {
                End::Client(pipe) => pipe.try_read(buf),
                End::Server(pipe) => pipe.try_read(buf),
            }
        }

        async fn writable(&self) -> io::Result<()> {
            match self {
                End::Client(pipe) => pipe.writable().await,
                End::Server(pipe) => pipe.writable().await,
            }
        }

        fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
            match self {
                End::Client(pipe) => pipe.try_write(buf),
                End::Server(pipe) => pipe.try_write(buf),
            }
        }

        fn handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
            use std::os::windows::io::AsHandle;
            match self {
                End::Client(pipe) => pipe.as_handle(),
                End::Server(pipe) => pipe.as_handle(),
            }
        }
    }

    /// The pipe both halves share, and whether it has been shut down.
    struct Shared {
        end: End,
        shut: AtomicBool,
        shut_wake: tokio::sync::Notify,
    }

    impl Shared {
        fn is_shut(&self) -> bool {
            self.shut.load(Ordering::Acquire)
        }
    }

    /// The read half of a pipe stream.
    pub(crate) struct PipeRead(Arc<Shared>);

    /// The write half of a pipe stream. The pipe closes when both halves
    /// have gone.
    pub(crate) struct PipeWrite(Arc<Shared>);

    fn split(end: End) -> (PipeRead, PipeWrite) {
        let shared = Arc::new(Shared {
            end,
            shut: AtomicBool::new(false),
            shut_wake: tokio::sync::Notify::new(),
        });
        (PipeRead(shared.clone()), PipeWrite(shared))
    }

    impl PipeRead {
        /// Up to `buf.len()` bytes; 0 at the end of the stream -- the peer
        /// closed its end (ERROR_BROKEN_PIPE, libuv's UV_EOF), or this
        /// stream was shut down ([`PipeWrite::shutdown`]).
        pub(crate) async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let shared = &*self.0;
            loop {
                if shared.is_shut() {
                    return Ok(0);
                }
                match shared.end.try_read(buf) {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    other => return other,
                }
                // Registered before the flag is read again, so a shutdown
                // between the two cannot be missed.
                let shut = shared.shut_wake.notified();
                tokio::pin!(shut);
                shut.as_mut().enable();
                if shared.is_shut() {
                    return Ok(0);
                }
                tokio::select! {
                    biased;
                    () = shut => return Ok(0),
                    ready = shared.end.readable() => ready?,
                }
            }
        }
    }

    impl PipeWrite {
        /// As much of `data` as the pipe takes now, without waiting: at most
        /// [`WRITE_CHUNK`], and nothing while the chunk before is still
        /// waiting for room in the pipe.
        pub(crate) fn try_write(&self, data: &[u8]) -> io::Result<usize> {
            if data.is_empty() {
                // No zero-length WriteFile: on a message-mode pipe it would
                // be an empty message.
                return Ok(0);
            }
            self.0.end.try_write(&data[..data.len().min(WRITE_CHUNK)])
        }

        pub(crate) async fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
            while !data.is_empty() {
                self.0.end.writable().await?;
                match self.try_write(data) {
                    Ok(n) => data = &data[n..],
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        /// libuv's shutdown of a pipe (`uv__pipe_shutdown`): wait until the
        /// peer has read everything written (FlushFileBuffers, which libuv
        /// runs on a thread-pool thread as it blocks), then end the stream
        /// both ways -- the read half reads EOF from here on. The flush's
        /// own failure (the peer gone) is not reported, as node's
        /// afterShutdown reports none.
        pub(crate) async fn shutdown(&mut self) {
            if let Ok(handle) = self.0.end.handle().try_clone_to_owned() {
                let (done, flushed) = tokio::sync::oneshot::channel();
                // A thread of its own, not the runtime's blocking pool: a
                // flush whose peer never reads blocks until that peer goes,
                // and must not hold the runtime's shutdown hostage.
                let spawned = std::thread::Builder::new()
                    .name("oam-pipe-flush".to_string())
                    .spawn(move || {
                        // sync_all is FlushFileBuffers on Windows.
                        let _ = std::fs::File::from(handle).sync_all();
                        let _ = done.send(());
                    });
                if spawned.is_ok() {
                    let _ = flushed.await;
                }
            }
            self.0.shut.store(true, Ordering::Release);
            self.0.shut_wake.notify_waiters();
        }
    }

    /// Dial the named pipe `target` as libuv's `uv_pipe_connect` does:
    /// CreateFileW read-write; while every instance of the pipe is taken
    /// (ERROR_PIPE_BUSY) wait for one, up to libuv's 30 s; and refuse a
    /// handle that is not a pipe (a regular file opens fine) with node's
    /// `ENOTSOCK`. The error is node's `connect` shape, naming `path`, the
    /// path as the script gave it.
    pub(crate) async fn connect(
        target: &str,
        path: &str,
    ) -> Result<(PipeRead, PipeWrite), Box<crate::NodeSysError>> {
        let deadline = tokio::time::Instant::now() + BUSY_WAIT;
        // WaitNamedPipeW's timeout: ERROR_SEM_TIMEOUT.
        let timed_out = || {
            let e = io::Error::from_raw_os_error(ERROR_SEM_TIMEOUT as i32);
            super::connect_error(&e, path)
        };
        let mut turn = None;
        let client = loop {
            match ClientOptions::new().open(target) {
                Ok(client) => break client,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if left.is_zero() {
                        return Err(timed_out());
                    }
                    if turn.is_none() {
                        // A turn to wait first, then the open again: the
                        // pipe may have freed an instance meanwhile.
                        match tokio::time::timeout(left, BusyTurn::take(target)).await {
                            Ok(taken) => turn = Some(taken),
                            Err(_) => return Err(timed_out()),
                        }
                        continue;
                    }
                    if !wait_for_instance(target, left).await {
                        return Err(timed_out());
                    }
                }
                Err(e) => return Err(super::connect_error(&e, path)),
            }
        };
        drop(turn);
        if let Err(e) = client.info() {
            // libuv: SetNamedPipeHandleState on something that is not a
            // pipe fails, and is reported as WSAENOTSOCK.
            return Err(super::connect_error_coded("ENOTSOCK", &e, path));
        }
        Ok(split(End::Client(client)))
    }

    /// How many of this process's dials to one pipe name wait for it in
    /// WaitNamedPipeW at once -- as many as libuv's thread pool runs (4).
    /// Every instance the server frees wakes every waiter, and only one of
    /// them gets it: with every dial of a burst waiting, each instance cost
    /// a wake-up and a failed open per waiter, and 200 clients took seconds.
    const BUSY_WAITERS: usize = 4;

    /// A dial's turn to wait for a busy pipe ([`BUSY_WAITERS`] per name);
    /// the dials past those wait for a turn, asleep. Dropped, the turn goes
    /// to the next, and the name's entry goes with its last user.
    struct BusyTurn {
        name: String,
        _permit: tokio::sync::OwnedSemaphorePermit,
    }

    type BusyGates =
        std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>;

    fn busy_gates() -> &'static BusyGates {
        static GATES: std::sync::OnceLock<BusyGates> = std::sync::OnceLock::new();
        GATES.get_or_init(Default::default)
    }

    impl BusyTurn {
        async fn take(name: &str) -> BusyTurn {
            let gate = busy_gates()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(BUSY_WAITERS)))
                .clone();
            // Dropped before the permit is had (the dial abandoned), the
            // clone is just released: the entry goes with a later turn.
            let permit = gate
                .acquire_owned()
                .await
                .expect("the gate is never closed");
            BusyTurn {
                name: name.to_string(),
                _permit: permit,
            }
        }
    }

    impl Drop for BusyTurn {
        fn drop(&mut self) {
            let mut gates = busy_gates().lock().unwrap_or_else(|e| e.into_inner());
            // Ours and the map's: nobody else holds or waits for a turn.
            if gates
                .get(&self.name)
                .is_some_and(|gate| Arc::strong_count(gate) == 2)
            {
                gates.remove(&self.name);
            }
        }
    }

    /// libuv's wait for a busy pipe: WaitNamedPipeW, which returns the
    /// moment an instance of the pipe is free to connect to (or the pipe is
    /// gone), on a blocking thread, for at most `left`. False when it timed
    /// out. Any other failure -- the pipe went away (the next open reports
    /// it), a name WaitNamedPipeW does not take -- is followed by a short
    /// pause, so the dial tries again without spinning.
    async fn wait_for_instance(name: &str, left: std::time::Duration) -> bool {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // Never 0: that is NMPWAIT_USE_DEFAULT_WAIT, the server's default.
        let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX).max(1);
        let waited = tokio::task::spawn_blocking(move || {
            // SAFETY: `wide` is a NUL-terminated UTF-16 string, alive for the
            // whole call; WaitNamedPipeW only reads it.
            let ok =
                unsafe { windows_sys::Win32::System::Pipes::WaitNamedPipeW(wide.as_ptr(), ms) };
            if ok != 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        })
        .await;
        match waited {
            Ok(Ok(())) => true,
            Ok(Err(e)) if e.raw_os_error() == Some(ERROR_SEM_TIMEOUT as i32) => false,
            _ => {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                true
            }
        }
    }

    /// A listening named pipe: the instances waiting for the next clients,
    /// [`PENDING_INSTANCES`] of them as libuv keeps. Each connected instance
    /// is handed out by [`PipeListener::accept`] and a fresh one takes its
    /// place at once, so the name is never without a listening instance
    /// while the server is open.
    pub(crate) struct PipeListener {
        path: String,
        pending: Vec<NamedPipeServer>,
    }

    fn instance(path: &str, first: bool) -> io::Result<NamedPipeServer> {
        ServerOptions::new().first_pipe_instance(first).create(path)
    }

    /// Listen on the named pipe `target`, as libuv's `uv_pipe_bind`: the
    /// first instance is created with FILE_FLAG_FIRST_PIPE_INSTANCE, so a
    /// name another server holds is refused -- node's EADDRINUSE -- and a
    /// name that is not a pipe's (`C:\x.sock`, a bare `x`) is EACCES. The
    /// error names `path`, the path as the script gave it.
    pub(crate) fn bind(target: &str, path: &str) -> Result<PipeListener, Box<crate::NodeSysError>> {
        match instance(target, true) {
            Ok(first) => {
                let mut listener = PipeListener {
                    path: target.to_string(),
                    pending: Vec::with_capacity(PENDING_INSTANCES),
                };
                listener.pending.push(first);
                // The rest now; any not to be had now, by the next accept.
                let _ = listener.top_up();
                Ok(listener)
            }
            Err(e) => {
                let code = match e.raw_os_error().and_then(|raw| u32::try_from(raw).ok()) {
                    Some(ERROR_ACCESS_DENIED) => "EADDRINUSE",
                    Some(ERROR_PATH_NOT_FOUND | ERROR_INVALID_NAME) => "EACCES",
                    _ => crate::node_error_code(&e),
                };
                Err(super::listen_error(code, &e, path))
            }
        }
    }

    impl PipeListener {
        /// Create instances until [`PENDING_INSTANCES`] wait.
        fn top_up(&mut self) -> io::Result<()> {
            while self.pending.len() < PENDING_INSTANCES {
                self.pending.push(instance(&self.path, false)?);
            }
            Ok(())
        }

        /// The next client to connect, on whichever waiting instance it took.
        pub(crate) async fn accept(&mut self) -> io::Result<(PipeRead, PipeWrite)> {
            loop {
                // A failure to make one more instance is reported only when
                // none is left waiting: the ones there still take clients.
                if let Err(e) = self.top_up()
                    && self.pending.is_empty()
                {
                    return Err(e);
                }
                // tokio's connect is cancel safe: the waits not taken here
                // stay armed for the next accept.
                let (index, connected) = {
                    let mut waits: Vec<_> = self
                        .pending
                        .iter()
                        .map(|server| Box::pin(server.connect()))
                        .collect();
                    std::future::poll_fn(|cx| {
                        for (index, wait) in waits.iter_mut().enumerate() {
                            if let std::task::Poll::Ready(connected) = wait.as_mut().poll(cx) {
                                return std::task::Poll::Ready((index, connected));
                            }
                        }
                        std::task::Poll::Pending
                    })
                    .await
                };
                let server = self.pending.swap_remove(index);
                match connected {
                    Ok(()) => {
                        // Its replacement before this one is handed out. If
                        // it cannot be had now, the next accept tries again.
                        let _ = self.top_up();
                        return Ok(split(End::Server(server)));
                    }
                    // A client that connected and went before the connect
                    // was seen: nothing to hand out; the instance is spent.
                    Err(e) if e.raw_os_error() == Some(ERROR_NO_DATA as i32) => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

#[cfg(unix)]
mod unix {
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    /// Dial the Unix domain socket `target`. The error is node's `connect`
    /// shape (ENOENT: no such file; ECONNREFUSED: nothing listening on it;
    /// EACCES: not permitted), naming `path`, the path as the script gave
    /// it.
    pub(crate) async fn connect(
        target: &str,
        path: &str,
    ) -> Result<(OwnedReadHalf, OwnedWriteHalf), Box<crate::NodeSysError>> {
        match tokio::net::UnixStream::connect(target).await {
            Ok(stream) => Ok(stream.into_split()),
            Err(e) => Err(super::connect_error(&e, path)),
        }
    }

    /// A listening Unix domain socket, and the path it is bound at.
    pub(crate) struct PipeListener {
        listener: tokio::net::UnixListener,
        path: String,
    }

    /// Listen on the Unix domain socket `target`. As node's, an existing
    /// file there is not removed first: the bind fails with EADDRINUSE. The
    /// error names `path`, the path as the script gave it; the file the
    /// close unlinks is `target`, the one bound.
    pub(crate) fn bind(target: &str, path: &str) -> Result<PipeListener, Box<crate::NodeSysError>> {
        match tokio::net::UnixListener::bind(target) {
            Ok(listener) => Ok(PipeListener {
                listener,
                path: target.to_string(),
            }),
            Err(e) => Err(super::listen_error(crate::node_error_code(&e), &e, path)),
        }
    }

    impl PipeListener {
        /// The next client to connect.
        pub(crate) async fn accept(&mut self) -> std::io::Result<(OwnedReadHalf, OwnedWriteHalf)> {
            let (stream, _) = self.listener.accept().await?;
            Ok(stream.into_split())
        }
    }

    impl Drop for PipeListener {
        /// libuv's `uv__pipe_close` for a bound pipe: the socket file is
        /// unlinked BEFORE the descriptor closes (the other order races a
        /// process that has just bound a socket of the same name), so a
        /// closed server leaves no file behind and a dial gets ENOENT.
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
