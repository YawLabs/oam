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
        ERROR_PIPE_BUSY,
    };

    /// How long a dial waits for a busy pipe (every instance taken) to free
    /// one: libuv's `WaitNamedPipeW(name, 30000)`.
    const BUSY_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

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
        /// As much of `data` as the pipe takes now, without waiting.
        pub(crate) fn try_write(&self, data: &[u8]) -> io::Result<usize> {
            if data.is_empty() {
                // No zero-length WriteFile: on a message-mode pipe it would
                // be an empty message.
                return Ok(0);
            }
            self.0.end.try_write(data)
        }

        pub(crate) async fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
            while !data.is_empty() {
                self.0.end.writable().await?;
                match self.0.end.try_write(data) {
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

    /// Dial the named pipe `path` as libuv's `uv_pipe_connect` does:
    /// CreateFileW read-write; while every instance of the pipe is taken
    /// (ERROR_PIPE_BUSY) wait for one, up to libuv's 30 s; and refuse a
    /// handle that is not a pipe (a regular file opens fine) with node's
    /// `ENOTSOCK`. The error is node's `connect` shape.
    pub(crate) async fn connect(
        path: &str,
    ) -> Result<(PipeRead, PipeWrite), Box<crate::NodeSysError>> {
        let deadline = tokio::time::Instant::now() + BUSY_WAIT;
        let mut pause = std::time::Duration::from_millis(1);
        let client = loop {
            match ClientOptions::new().open(path) {
                Ok(client) => break client,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    if tokio::time::Instant::now() >= deadline {
                        // WaitNamedPipeW's timeout: ERROR_SEM_TIMEOUT.
                        let timed_out = io::Error::from_raw_os_error(
                            windows_sys::Win32::Foundation::ERROR_SEM_TIMEOUT as i32,
                        );
                        return Err(super::connect_error(&timed_out, path));
                    }
                    tokio::time::sleep(pause).await;
                    pause = (pause * 2).min(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(super::connect_error(&e, path)),
            }
        };
        if let Err(e) = client.info() {
            // libuv: SetNamedPipeHandleState on something that is not a
            // pipe fails, and is reported as WSAENOTSOCK.
            return Err(super::connect_error_coded("ENOTSOCK", &e, path));
        }
        Ok(split(End::Client(client)))
    }

    /// A listening named pipe: the instance the next client will connect
    /// to. A connected instance is handed out by [`PipeListener::accept`]
    /// and a fresh one takes its place at once, so the name is never
    /// without a listening instance while the server is open.
    pub(crate) struct PipeListener {
        path: String,
        next: Option<NamedPipeServer>,
    }

    fn instance(path: &str, first: bool) -> io::Result<NamedPipeServer> {
        ServerOptions::new().first_pipe_instance(first).create(path)
    }

    /// Listen on the named pipe `path`, as libuv's `uv_pipe_bind`: the first
    /// instance is created with FILE_FLAG_FIRST_PIPE_INSTANCE, so a name
    /// another server holds is refused -- node's EADDRINUSE -- and a name
    /// that is not a pipe's (`C:\x.sock`, a bare `x`) is EACCES.
    pub(crate) fn bind(path: &str) -> Result<PipeListener, Box<crate::NodeSysError>> {
        match instance(path, true) {
            Ok(first) => Ok(PipeListener {
                path: path.to_string(),
                next: Some(first),
            }),
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
        /// The next client to connect.
        pub(crate) async fn accept(&mut self) -> io::Result<(PipeRead, PipeWrite)> {
            loop {
                let server = match self.next.take() {
                    Some(server) => server,
                    None => instance(&self.path, false)?,
                };
                match server.connect().await {
                    Ok(()) => {
                        // The next instance before this one is handed out.
                        // If it cannot be had now, the next accept tries
                        // again and reports the failure.
                        self.next = instance(&self.path, false).ok();
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

    /// Dial the Unix domain socket `path`. The error is node's `connect`
    /// shape (ENOENT: no such file; ECONNREFUSED: nothing listening on it;
    /// EACCES: not permitted).
    pub(crate) async fn connect(
        path: &str,
    ) -> Result<(OwnedReadHalf, OwnedWriteHalf), Box<crate::NodeSysError>> {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => Ok(stream.into_split()),
            Err(e) => Err(super::connect_error(&e, path)),
        }
    }

    /// A listening Unix domain socket, and the path it is bound at.
    pub(crate) struct PipeListener {
        listener: tokio::net::UnixListener,
        path: String,
    }

    /// Listen on the Unix domain socket `path`. As node's, an existing file
    /// at `path` is not removed first: the bind fails with EADDRINUSE.
    pub(crate) fn bind(path: &str) -> Result<PipeListener, Box<crate::NodeSysError>> {
        match tokio::net::UnixListener::bind(path) {
            Ok(listener) => Ok(PipeListener {
                listener,
                path: path.to_string(),
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
