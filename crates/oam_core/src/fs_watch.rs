//! `fs.watch` / `fs.promises.watch`: OS file-change notifications.
//!
//! node's `FSWatcher` sits on libuv's `uv_fs_event_t`, which is
//! ReadDirectoryChangesW on Windows, inotify on Linux and FSEvents (a
//! directory) or kqueue (a file) on macOS. oam has two backends:
//!
//! - **Linux** ([`inotify_backend`]): libuv's own design, one inotify
//!   instance per run shared by every watcher, through the `inotify` crate's
//!   safe API. An instance per watcher, which is what a `notify` watcher is,
//!   ran into the kernel's per-user instance limit
//!   (`fs.inotify.max_user_instances`, 128 by default, shared with every other
//!   process of the user) after about a hundred `fs.watch` calls -- and a
//!   chokidar-style tool watches every directory of a tree on its own.
//! - **Windows and macOS** ([`notify_backend`]): the `notify` crate, one
//!   watcher per `fs.watch` call. Those backends are raw-pointer APIs (an
//!   OVERLAPPED buffer of variable-length FILE_NOTIFY_INFORMATION records, an
//!   FSEventStream callback), and writing them here would grow the `unsafe`
//!   ceiling (AI-POLICY.md gate 5) by exactly the obligations a maintained
//!   crate already discharges. notify is vendored with three Windows fixes
//!   (vendor/notify-8.2.0/OAM-PATCH.md).
//!
//! Delivery follows the pattern of a socket's parked read: JS keeps exactly
//! one `fsWatchNext` op in flight per watcher, filed under
//! [`crate::HandleKey::FsWatch`] so `watcher.unref()` stops it pinning the
//! loop. The op completes with ONE event (framed by [`encode`]), empty
//! (`Done`) once the watcher is closed, or with the error the watch failed
//! with. One event per completion because node delivers each libuv event as
//! its own MakeCallback, draining `process.nextTick` and the microtasks after
//! every one: a listener that closes the watcher from a tick, or a throw from
//! one listener call, must not reach the events queued behind it. The engine
//! settles the op by calling the JS callback from the loop (a callback op,
//! oam_engine `spawn_handle_callback_op`), with a tick point after each.

use crate::OpOutcome;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

#[cfg(target_os = "linux")]
mod inotify_backend;
#[cfg(not(target_os = "linux"))]
mod notify_backend;

#[cfg(target_os = "linux")]
use inotify_backend as backend;
#[cfg(not(target_os = "linux"))]
use notify_backend as backend;

/// node's two event types (libuv's `UV_RENAME` / `UV_CHANGE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEventType {
    Rename,
    Change,
}

/// One change, as node's `FSEvent.onchange` receives it.
#[derive(Debug, PartialEq, Eq)]
pub struct WatchEvent {
    pub kind: WatchEventType,
    /// The changed entry relative to the watched directory (the watched
    /// file's own name for a file watch), as raw bytes so a non-UTF-8 Linux
    /// name reaches `encoding: 'buffer'` intact. `None` is node's `null`
    /// filename: the OS reported that something changed but not what (an
    /// overflowed event queue).
    pub filename: Option<Vec<u8>>,
}

/// What a backend queues for the watcher.
enum Note {
    /// An event, with the identity of the OS record it came from when the
    /// OS merges repeats (see [`MergeKey`]).
    Event(WatchEvent, Option<MergeKey>),
    Failed(std::io::Error),
}

/// inotify's identity of an event: descriptor, mask and cookie (the name is
/// the event's filename). The kernel merges an event into the one queued
/// before it when the two are identical and that one is still unread
/// (fs/notify/inotify/inotify_fsnotify.c). libuv reads the instance from the
/// event loop, so a burst that lands while JS runs -- `writeFileSync` on an
/// existing file truncates and then writes, two IN_MODIFYs -- reaches node as
/// one event. oam's reader thread drains the instance at once, so the merge
/// is done where the events wait instead: in the watcher's queue, until JS
/// takes them (see [`next`]).
type MergeKey = (i32, u32, u32);

/// Why a watch could not start.
#[derive(Debug)]
pub enum WatchStartError {
    /// An OS error on the path (ENOENT, EACCES, ...).
    Io(std::io::Error),
    /// inotify's per-user watch limit (`fs.inotify.max_user_watches`): node
    /// throws `ENOSPC` with its own message for it.
    LimitReached,
}

struct WatchQueue {
    rx: UnboundedReceiver<Note>,
    /// The note read past the end of a run of merged repeats.
    held: Option<Note>,
}

struct Watch {
    /// Dropping it stops the OS watch.
    _backend: backend::Handle,
    queue: Arc<tokio::sync::Mutex<WatchQueue>>,
    closed: Arc<tokio::sync::Notify>,
}

/// Every live watcher of a run, by the handle JS holds.
#[derive(Clone, Default)]
pub struct FsWatchRegistry {
    inner: Arc<Mutex<WatchState>>,
}

#[derive(Default)]
struct WatchState {
    next: u64,
    watches: HashMap<u64, Watch>,
    /// What the run's watchers share: the one inotify instance on Linux,
    /// nothing elsewhere.
    shared: backend::Shared,
}

impl FsWatchRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Start watching `os_path` and return the watcher's handle.
///
/// The path is checked first, as libuv's `uv_fs_event_start` opens it: a
/// missing one fails here, synchronously, which is when node throws.
pub fn start(
    registry: &FsWatchRegistry,
    os_path: &str,
    recursive: bool,
) -> Result<u64, WatchStartError> {
    let meta = std::fs::metadata(os_path).map_err(WatchStartError::Io)?;
    let root = std::path::absolute(os_path).map_err(WatchStartError::Io)?;
    let shared = registry.lock().shared.clone();
    let (tx, rx) = unbounded_channel();
    let handle = backend::watch(&shared, &root, meta.is_dir(), recursive, tx)?;
    let mut state = registry.lock();
    state.next += 1;
    let id = state.next;
    state.watches.insert(
        id,
        Watch {
            _backend: handle,
            queue: Arc::new(tokio::sync::Mutex::new(WatchQueue { rx, held: None })),
            closed: Arc::new(tokio::sync::Notify::new()),
        },
    );
    Ok(id)
}

/// `watcher.close()`: stop the OS watch, and settle the parked
/// `fsWatchNext` (empty) so it stops holding the loop open.
pub fn close(registry: &FsWatchRegistry, id: u64) {
    let watch = registry.lock().watches.remove(&id);
    if let Some(watch) = watch {
        // notify_one, not notify_waiters: it stores a permit, so a next op
        // that has not reached its await yet still sees the close.
        watch.closed.notify_one();
        // The watcher drops here, outside the registry lock: a backend may
        // wait for its thread to acknowledge the stop.
        drop(watch);
    }
}

/// The parked read: the next event, framed by [`encode`]; `Done` once the
/// watcher is closed; or the error the watch failed with.
pub fn next(registry: &FsWatchRegistry, id: u64) -> impl Future<Output = OpOutcome> + use<> {
    let parts = registry
        .lock()
        .watches
        .get(&id)
        .map(|w| (w.queue.clone(), w.closed.clone()));
    async move {
        let Some((queue, closed)) = parts else {
            return OpOutcome::Done;
        };
        let mut guard = queue.lock().await;
        let queue = &mut *guard;
        // Close first: node emits nothing after close(), so events still
        // queued when it ran are dropped, not delivered.
        let note = tokio::select! {
            biased;
            () = closed.notified() => None,
            note = async {
                match queue.held.take() {
                    Some(note) => Some(note),
                    None => queue.rx.recv().await,
                }
            } => note,
        };
        match note {
            None => OpOutcome::Done,
            Some(Note::Failed(error)) => failed(error),
            Some(Note::Event(event, key)) => {
                if key.is_some() {
                    // The repeats of this record that queued behind it are
                    // the kernel's to merge (see MergeKey).
                    while let Ok(next) = queue.rx.try_recv() {
                        match &next {
                            Note::Event(repeat, k) if *k == key && *repeat == event => {}
                            _ => {
                                queue.held = Some(next);
                                break;
                            }
                        }
                    }
                }
                OpOutcome::Bytes(encode(&event))
            }
        }
    }
}

/// A failure of a running watch, shaped by the engine into node's
/// `UVException` (syscall `watch`); the path is filled in by the JS, which
/// knows the filename the watcher was started with.
fn failed(error: std::io::Error) -> OpOutcome {
    let code = crate::node_error_code(&error);
    let reason = crate::node_error_reason(code, &error);
    OpOutcome::node_failed_io(
        code,
        format!("{code}: {reason}, watch"),
        "watch",
        None,
        &error,
    )
}

/// One event as a byte string: one byte for the type (0 rename, 1 change),
/// then the filename's bytes; the type byte alone is node's `null` filename.
/// Bytes rather than JSON so a filename that is not UTF-8 survives to
/// `encoding: 'buffer'`. (No backend reports an empty name, so a lone type
/// byte is unambiguous.)
pub fn encode(event: &WatchEvent) -> Vec<u8> {
    let kind = match event.kind {
        WatchEventType::Rename => 0,
        WatchEventType::Change => 1,
    };
    let name = event.filename.as_deref().unwrap_or_default();
    let mut out = Vec::with_capacity(1 + name.len());
    out.push(kind);
    out.extend_from_slice(name);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_frames_type_and_name() {
        let change = WatchEvent {
            kind: WatchEventType::Change,
            filename: Some(b"ab".to_vec()),
        };
        assert_eq!(encode(&change), vec![1u8, b'a', b'b']);
        let unnamed = WatchEvent {
            kind: WatchEventType::Rename,
            filename: None,
        };
        assert_eq!(encode(&unnamed), vec![0u8]);
    }

    #[test]
    fn a_missing_path_fails_to_start_with_enoent() {
        let registry = FsWatchRegistry::default();
        let missing = std::env::temp_dir().join("oam-fsw-definitely-missing");
        match start(&registry, missing.to_str().unwrap(), false) {
            Err(WatchStartError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected ENOENT, got {:?}", other.map(|_| ())),
        }
    }

    /// node v22.22.2 on Linux, measured: `writeFileSync` on an existing file
    /// (a truncating open, then a write: two IN_MODIFYs) is one `change`.
    #[cfg(target_os = "linux")]
    #[test]
    fn repeats_queued_before_js_reads_them_are_one_event() {
        let dir = std::env::temp_dir().join(format!("oam-fsw-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), b"x").unwrap();
        let registry = FsWatchRegistry::default();
        let id = start(&registry, dir.to_str().unwrap(), false).unwrap();
        std::fs::write(dir.join("f"), b"yy").unwrap();
        std::fs::write(dir.join("g"), b"").unwrap();
        // Let both writes' events reach the queue before JS "reads".
        std::thread::sleep(std::time::Duration::from_millis(200));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut got = Vec::new();
        for _ in 0..2 {
            let outcome = rt.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), next(&registry, id)).await
            });
            match outcome {
                Ok(OpOutcome::Bytes(bytes)) => got.push(bytes),
                _ => panic!("expected an event"),
            }
        }
        // f's two IN_MODIFYs were merged; the next event is g's creation.
        assert_eq!(got, [b"\x01f".to_vec(), b"\x00g".to_vec()]);
        close(&registry, id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_created_file_is_reported_and_close_settles_the_read() {
        let dir = std::env::temp_dir().join(format!("oam-fsw-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let registry = FsWatchRegistry::default();
        let id = start(&registry, dir.to_str().unwrap(), false).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        std::fs::write(dir.join("new.txt"), b"x").unwrap();
        let outcome = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), next(&registry, id)).await
        });
        match outcome {
            Ok(OpOutcome::Bytes(bytes)) => {
                let needle = b"new.txt";
                assert!(
                    bytes.windows(needle.len()).any(|w| w == needle),
                    "the event names the file: {bytes:?}"
                );
            }
            Ok(_) => panic!("expected an event"),
            Err(_) => panic!("no event within 10 s"),
        }
        // A read parked when close() runs settles empty.
        let parked = next(&registry, id);
        close(&registry, id);
        let outcome = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), parked).await
        });
        assert!(matches!(outcome, Ok(OpOutcome::Done)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
