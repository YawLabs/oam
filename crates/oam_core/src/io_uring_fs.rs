//! io_uring FS fast path (Linux-only). See `docs/design/io_uring.md`.
//!
//! A dedicated `oam-io-uring` thread runs a `tokio_uring` runtime (a tokio
//! current-thread runtime + an io_uring driver). The rest of oam runs on the
//! multi-thread tokio runtime and dispatches file reads to this thread over a
//! channel, awaiting a oneshot reply -- so the existing op-completion contract
//! is unchanged. io_uring does true async file I/O, replacing the blocking-pool
//! thread hop of `tokio::fs::read`.
//!
//! Opt-in (`OAM_IO_URING=1`) + a runtime probe + a full fallback: if io_uring
//! is unavailable (old kernel, seccomp) the probe fails and callers use the
//! std path.
//!
//! Requests run concurrently: the worker spawns one task per request
//! (`tokio_uring::spawn`) behind a 128-permit semaphore, so up to 128 reads
//! and writes are in flight on the ring at once and the rest wait for a
//! permit before they open a file or allocate. Each read holds its own
//! growing output buffer plus one 64 KiB chunk buffer; each write holds the
//! caller's data until the ring hands it back.

use std::sync::OnceLock;

use tokio::sync::{mpsc, oneshot};

/// A request to the io_uring worker thread.
enum UringRequest {
    Read {
        path: String,
        reply: oneshot::Sender<std::io::Result<Vec<u8>>>,
    },
    Write {
        path: String,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<(), crate::WholeFileError>>,
    },
}

/// Handle to the dedicated io_uring worker thread. `UnboundedSender` is `Send +
/// Sync`, so this is safe to hold in the process-global below and call from any
/// isolate thread.
pub struct UringFs {
    tx: mpsc::UnboundedSender<UringRequest>,
}

impl UringFs {
    /// Start the io_uring worker thread, or return `None` if io_uring is
    /// unavailable. The probe (`io_uring::IoUring::new`) creates and drops a
    /// real ring; it fails cleanly (no panic) on kernels < 5.1 or when
    /// `io_uring_setup` is blocked by seccomp -- unlike `tokio_uring::start`,
    /// which would panic. A successful probe means the worker's
    /// `tokio_uring::start` will also succeed.
    pub fn try_start() -> Option<Self> {
        if io_uring::IoUring::new(8).is_err() {
            return None;
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<UringRequest>();
        std::thread::Builder::new()
            .name("oam-io-uring".to_string())
            .spawn(move || {
                tokio_uring::start(async move {
                    // Backpressure: cap concurrent in-flight ops so a worker
                    // stall can't let unbounded ops/buffers pile up (the request
                    // channel is unbounded). Each spawned task acquires a permit
                    // and holds it across read_whole/write_all, so at most
                    // CONCURRENCY ops -- and their per-op buffers, which are only
                    // allocated after the permit is held -- are live at once.
                    // Tasks past the cap park on acquire() before touching the
                    // ring or allocating, which is where the bound bites.
                    const CONCURRENCY: usize = 128;
                    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
                    while let Some(req) = rx.recv().await {
                        // Spawn each op so multiple are in flight on the ring
                        // concurrently (tokio_uring::spawn = spawn_local on the
                        // io_uring current-thread runtime). The recv loop keeps
                        // accepting while prior ops complete -- the point of
                        // io_uring over the blocking pool.
                        let sem = sem.clone();
                        tokio_uring::spawn(async move {
                            // Hold the permit for the lifetime of the op so it
                            // bounds in-flight ops, not just dispatch.
                            let _permit = sem.acquire().await;
                            match req {
                                UringRequest::Read { path, reply } => {
                                    let _ = reply.send(read_whole(&path).await);
                                }
                                UringRequest::Write { path, data, reply } => {
                                    let _ = reply.send(write_all(&path, data).await);
                                }
                            }
                        });
                    }
                });
            })
            .ok()?;
        Some(Self { tx })
    }

    /// Read an entire file via io_uring. Any error (including "worker gone") is
    /// returned to the caller, which falls back to the std path -- io_uring is a
    /// pure optimization, never a correctness dependency.
    pub async fn read_file(&self, path: String) -> std::io::Result<Vec<u8>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(UringRequest::Read { path, reply })
            .map_err(|_| std::io::Error::other("io_uring worker stopped"))?;
        rx.await
            .map_err(|_| std::io::Error::other("io_uring reply dropped"))?
    }

    /// Write a file via io_uring (create + truncate + write). `data` is moved
    /// in -- no clone on the happy path.
    ///
    /// On a worker-channel failure ("worker stopped" / "reply dropped") the
    /// un-consumed `data` is handed back as `Err((err, Some(data)))` so the
    /// caller can retry via the std path with no clone. A genuine io error from
    /// the worker (the buffer is already consumed by then) is returned as
    /// `Err((err, None))` -- the caller's "this is a real error, surface it"
    /// signal. It is an `Option` and not the buffer's emptiness because the
    /// caller's own data may be empty (`writeFile(p, '')`), and that write
    /// has to reach the std path as any other does.
    /// The error says which half failed, the create or the write, as node
    /// reports them apart (`crate::whole_file_error`); a channel failure is
    /// reported as the open's.
    pub async fn write_file(
        &self,
        path: String,
        data: Vec<u8>,
    ) -> Result<(), (crate::WholeFileError, Option<Vec<u8>>)> {
        let (reply, rx) = oneshot::channel();
        // Send failure: the worker is gone and `data` rides back inside the
        // returned UringRequest::Write, so we can hand it to the caller for a
        // std retry without cloning.
        if let Err(send_err) = self.tx.send(UringRequest::Write { path, data, reply }) {
            let data = match send_err.0 {
                UringRequest::Write { data, .. } => Some(data),
                // Unreachable: we only ever send a Write here. Report nothing
                // recovered rather than panic.
                _ => None,
            };
            return Err((
                crate::WholeFileError::Open(std::io::Error::other("io_uring worker stopped")),
                data,
            ));
        }
        match rx.await {
            // Worker replied: the buffer was consumed by write_all, so a genuine
            // io error carries no recoverable data -- signal "real error" with
            // `None`.
            Ok(res) => res.map_err(|e| (e, None)),
            // Reply dropped: the worker died after we sent but before replying.
            // The buffer went with the dropped task, so we can't recover it;
            // signal "real error" with `None`. The caller surfaces this
            // directly (it has no buffer of its own to retry std with).
            Err(_) => Err((
                crate::WholeFileError::Open(std::io::Error::other("io_uring reply dropped")),
                None,
            )),
        }
    }
}

/// Process-global io_uring handle. `None` when opted out (`OAM_IO_URING` unset)
/// or when the probe failed -- callers then use the std fallback. Lazily started
/// once on first use; shared across isolates (the worker is a stateless file
/// reader).
pub fn global() -> Option<&'static UringFs> {
    static G: OnceLock<Option<UringFs>> = OnceLock::new();
    G.get_or_init(|| {
        if !enabled() {
            return None;
        }
        UringFs::try_start()
    })
    .as_ref()
}

/// io_uring is opt-in: engaged only when `OAM_IO_URING` is `1`/`true`/`on`/`yes`.
fn enabled() -> bool {
    matches!(
        std::env::var("OAM_IO_URING").as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

/// Read a whole file via io_uring: open, `read_at` in 64 KiB chunks until EOF,
/// close. A fresh buffer per chunk keeps the owned-buffer dance unambiguous.
///
/// (A doubling-to-4 MiB chunk variant was tried to speed large reads, but the
/// re-sweep showed it didn't move the large-file loss -- the bottleneck is the
/// dispatch hop, not chunk count -- while adding a 4 MiB x concurrency buffer
/// spike. Reverted. See docs/design/io_uring.md.)
async fn read_whole(path: &str) -> std::io::Result<Vec<u8>> {
    let file = tokio_uring::fs::File::open(path).await?;
    let mut out = Vec::new();
    let mut pos = 0u64;
    loop {
        let buf = vec![0u8; 64 * 1024];
        // BufResult<usize, Vec<u8>> = (io::Result<usize>, Vec<u8>): the buffer
        // is moved in and handed back.
        let (res, buf) = file.read_at(buf, pos).await;
        let n = res?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        pos += n as u64;
    }
    // Best-effort close; the bytes are already read.
    let _ = file.close().await;
    Ok(out)
}

/// Write a whole file via io_uring: create (truncate) + `write_all_at` + close.
/// Matches the non-append `fs.writeFile` semantics.
/// The create and the write fail apart, as node reports them.
async fn write_all(path: &str, data: Vec<u8>) -> Result<(), crate::WholeFileError> {
    let file = tokio_uring::fs::File::create(path)
        .await
        .map_err(crate::WholeFileError::Open)?;
    // BufResult<(), Vec<u8>>: data is moved in and handed back; we only need
    // the io::Result.
    let (res, _data) = file.write_all_at(data, 0).await;
    res.map_err(crate::WholeFileError::Transfer)?;
    let _ = file.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Start the lane for a test, or say LOUDLY that the test covered nothing.
    ///
    /// A host that blocks `io_uring_setup` (an old kernel, a container's
    /// seccomp profile) fails the probe, and these tests then have nothing to
    /// exercise. Returning quietly made that indistinguishable from a pass:
    /// the lane could go green with zero coverage. So:
    ///
    /// - `OAM_REQUIRE_IO_URING=1` turns the failed probe into a test failure,
    ///   for a host that is known to allow io_uring and must keep doing so;
    /// - otherwise the skip is written straight to the process's stderr. Not
    ///   `eprintln!`: libtest captures that for a passing test, which is how
    ///   the old message was never seen.
    fn start_or_skip(test: &str) -> Option<UringFs> {
        if let Some(uring) = UringFs::try_start() {
            return Some(uring);
        }
        let required = matches!(std::env::var("OAM_REQUIRE_IO_URING").as_deref(), Ok("1"));
        assert!(
            !required,
            "{test}: OAM_REQUIRE_IO_URING=1 but the io_uring probe failed on this host \
             (kernel < 5.1, or io_uring_setup is blocked) -- the io_uring lane has no coverage here"
        );
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "SKIP {test}: io_uring probe failed on this host (kernel < 5.1, or io_uring_setup \
             is blocked) -- the io_uring lane has NO coverage in this run; set \
             OAM_REQUIRE_IO_URING=1 to make this a failure"
        );
        None
    }

    // Real io_uring read on a Linux kernel: write a >64 KiB file (to
    // exercise the multi-chunk loop), read it back through the worker thread,
    // and assert the bytes. Without io_uring it skips, visibly (start_or_skip).
    #[test]
    fn uring_reads_file_contents() {
        let Some(uring) = start_or_skip("uring_reads_file_contents") else {
            return;
        };

        let dir = std::env::temp_dir().join(format!(
            "oam-uring-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &content).unwrap();

        // read_file resolves a oneshot the worker thread fills; drive the await
        // on a throwaway current-thread runtime.
        let got = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(uring.read_file(path.to_string_lossy().into_owned()))
            .expect("io_uring read should succeed");

        assert_eq!(got, content, "io_uring read must match the written bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Slice 2: write a file via io_uring, then read it back via io_uring, and
    // assert the round-trip matches. Exercises the write path + the concurrent
    // worker on a real kernel. Without io_uring it skips, visibly (start_or_skip).
    #[test]
    fn uring_write_then_read_roundtrips() {
        let Some(uring) = start_or_skip("uring_write_then_read_roundtrips") else {
            return;
        };

        let dir = std::env::temp_dir().join(format!(
            "oam-uring-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.bin");
        let pstr = path.to_string_lossy().into_owned();
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(uring.write_file(pstr.clone(), content.clone()))
            .expect("io_uring write should succeed");
        // Verify via std (independent of io_uring) AND via the io_uring reader.
        assert_eq!(std::fs::read(&path).unwrap(), content, "on-disk bytes");
        let got = rt
            .block_on(uring.read_file(pstr))
            .expect("io_uring read should succeed");
        assert_eq!(got, content, "io_uring write->read round-trip");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
