//! `fs.watch` / `fs.promises.watch`: OS file-change notifications.
//!
//! node's `FSWatcher` sits on libuv's `uv_fs_event_t`, which is
//! ReadDirectoryChangesW on Windows, inotify on Linux and FSEvents (a
//! directory) or kqueue (a file) on macOS. oam reaches the same OS facilities
//! through the `notify` crate rather than its own FFI: every one of those
//! backends is a raw-pointer API (an OVERLAPPED buffer of variable-length
//! FILE_NOTIFY_INFORMATION records, an FSEventStream callback), and writing
//! them here would grow the `unsafe` ceiling (AI-POLICY.md gate 5) by exactly
//! the obligations a maintained crate already discharges.
//!
//! One `notify` watcher per `fs.watch` call. That keeps each watcher's events
//! its own: `notify` reports an event's path but not which of its watches
//! produced it, so a shared watcher could not tell a directory watch from a
//! watch on a file inside it, both of which see that file's events. The cost
//! is a native thread per watcher on Windows and Linux (the backends' event
//! loops); libuv shares one loop. That is fine for the handful of watchers a
//! CLI holds and is recorded in docs/node-divergences.md.
//!
//! Delivery follows the pattern of a socket's parked read: JS keeps exactly
//! one `fsWatchNext` op in flight per watcher, filed under
//! [`crate::HandleKey::FsWatch`] so `watcher.unref()` stops it pinning the
//! loop. The op resolves with every event queued by then, as one framed byte
//! batch (see [`encode`]), resolves empty (`Done`) once the watcher is
//! closed, or rejects with the error the watch failed with.

use crate::OpOutcome;
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

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

/// What the backend thread queues for the watcher.
enum Note {
    Event(WatchEvent),
    Failed(std::io::Error),
}

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
    /// A failure drained after events already in the batch: reported by the
    /// NEXT `fsWatchNext`, so no event queued before it is lost.
    held: Option<std::io::Error>,
}

struct Watch {
    /// Dropping it stops the OS watch (and the backend's thread).
    _watcher: notify::RecommendedWatcher,
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
    let namer = Namer::new(&root, meta.is_dir());
    let (tx, rx) = unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        forward(&namer, &tx, res)
    })
    .map_err(start_error)?;
    let mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    watcher.watch(&root, mode).map_err(start_error)?;
    let mut state = registry.lock();
    state.next += 1;
    let id = state.next;
    state.watches.insert(
        id,
        Watch {
            _watcher: watcher,
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

/// The parked read: the events queued since the last one (at least one),
/// framed by [`encode`]; `Done` once the watcher is closed; or the error the
/// watch failed with.
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
        let mut queue = queue.lock().await;
        if let Some(error) = queue.held.take() {
            return failed(error);
        }
        // Close first: node emits nothing after close(), so events still
        // queued when it ran are dropped, not delivered.
        let first = tokio::select! {
            biased;
            () = closed.notified() => None,
            note = queue.rx.recv() => note,
        };
        let mut batch = Vec::new();
        match first {
            None => return OpOutcome::Done,
            Some(Note::Failed(error)) => return failed(error),
            Some(Note::Event(event)) => batch.push(event),
        }
        // Everything else already queued goes in the same batch: one
        // completion per OS burst, not one per event (a recursive `rm -rf`
        // can queue thousands).
        while let Ok(note) = queue.rx.try_recv() {
            match note {
                Note::Event(event) => batch.push(event),
                Note::Failed(error) => {
                    queue.held = Some(error);
                    break;
                }
            }
        }
        OpOutcome::Bytes(encode(&batch))
    }
}

/// A failure of a running watch, shaped by the engine into node's
/// `UVException` (syscall `watch`); the path is filled in by the JS, which
/// knows the filename the watcher was started with.
fn failed(error: std::io::Error) -> OpOutcome {
    let code = crate::node_error_code(&error);
    let errno = crate::node_errno(code, &error);
    let reason = crate::node_error_reason(code, &error);
    OpOutcome::node_failed_at(code, format!("{code}: {reason}, watch"), "watch", None, errno)
}

/// The batch as one byte string: per event, one byte for the type (0
/// rename, 1 change), a little-endian u32 filename length (`u32::MAX` for no
/// filename) and the filename's bytes. Bytes rather than JSON so a filename
/// that is not UTF-8 survives to `encoding: 'buffer'`.
pub fn encode(batch: &[WatchEvent]) -> Vec<u8> {
    let mut out = Vec::new();
    for event in batch {
        out.push(match event.kind {
            WatchEventType::Rename => 0,
            WatchEventType::Change => 1,
        });
        match &event.filename {
            Some(name) => {
                let len = u32::try_from(name.len()).unwrap_or(u32::MAX - 1);
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&name[..len as usize]);
            }
            None => out.extend_from_slice(&u32::MAX.to_le_bytes()),
        }
    }
    out
}

fn start_error(error: notify::Error) -> WatchStartError {
    match error.kind {
        notify::ErrorKind::MaxFilesWatch => WatchStartError::LimitReached,
        other => WatchStartError::Io(notify_io(other)),
    }
}

fn notify_io(kind: notify::ErrorKind) -> std::io::Error {
    match kind {
        notify::ErrorKind::Io(error) => error,
        notify::ErrorKind::PathNotFound => std::io::Error::from(std::io::ErrorKind::NotFound),
        notify::ErrorKind::MaxFilesWatch => limit_error(),
        notify::ErrorKind::Generic(message) => std::io::Error::other(message),
        notify::ErrorKind::WatchNotFound | notify::ErrorKind::InvalidConfig(_) => {
            std::io::Error::from(std::io::ErrorKind::InvalidInput)
        }
    }
}

/// ENOSPC, which is what inotify_add_watch(2) fails with at the limit.
fn limit_error() -> std::io::Error {
    #[cfg(unix)]
    {
        std::io::Error::from_raw_os_error(libc::ENOSPC)
    }
    #[cfg(not(unix))]
    {
        std::io::Error::from(std::io::ErrorKind::StorageFull)
    }
}

/// Runs on the backend's thread: turn one `notify` event into node's events
/// and queue them. A send that fails means the watcher was closed; the
/// event is then dropped, as libuv drops events for a closed handle.
fn forward(namer: &Namer, tx: &UnboundedSender<Note>, res: notify::Result<notify::Event>) {
    match res {
        Ok(event) => {
            for event in translate(namer, &event) {
                let _ = tx.send(Note::Event(event));
            }
        }
        Err(error) => {
            let _ = tx.send(Note::Failed(notify_io(error.kind)));
        }
    }
}

/// node's event type for a `notify` kind, or `None` for one libuv never
/// reports.
///
/// libuv's rule, the same on every backend: a content or attribute change is
/// `change` (IN_MODIFY / IN_ATTRIB, FILE_ACTION_MODIFIED, an FSEvents
/// modify), and anything that makes or unmakes a name -- create, delete,
/// either half of a rename -- is `rename`.
fn event_type(kind: &EventKind) -> Option<WatchEventType> {
    match kind {
        // libuv does not watch opens or closes (inotify IN_OPEN /
        // IN_CLOSE_WRITE, which notify subscribes to); a close-after-write
        // already produced its IN_MODIFY.
        EventKind::Access(_) => None,
        // notify's synthetic pairing of a rename's two halves, each of which
        // it has already reported on its own (as libuv reports them).
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => None,
        EventKind::Modify(ModifyKind::Name(_)) => Some(WatchEventType::Rename),
        EventKind::Modify(_) => Some(WatchEventType::Change),
        EventKind::Create(_) | EventKind::Remove(_) => Some(WatchEventType::Rename),
        EventKind::Any => Some(WatchEventType::Change),
        // A rescan (an overflowed queue) is handled by the caller; any other
        // `Other` is backend metadata, not a change.
        EventKind::Other => None,
    }
}

fn translate(namer: &Namer, event: &notify::Event) -> Vec<WatchEvent> {
    if event.need_rescan() {
        // The OS dropped events. libuv's Windows backend reports an overflow
        // as one event with no filename; node's listener sees `null`.
        return vec![WatchEvent {
            kind: WatchEventType::Rename,
            filename: None,
        }];
    }
    let Some(kind) = event_type(&event.kind) else {
        return Vec::new();
    };
    event
        .paths
        .iter()
        .filter_map(|path| namer.name(path))
        .map(|filename| WatchEvent {
            kind,
            filename: Some(filename),
        })
        .collect()
}

/// Names a changed path the way libuv does: relative to the watched
/// directory (a nested path, with the platform's separators, for a recursive
/// watch), or the watched entry's own name for an event on the watched path
/// itself -- the file of a file watch, or a watched directory that was
/// deleted or moved (libuv: `uv__basename_r(w->path)`).
struct Namer {
    root: PathBuf,
    /// Other spellings `notify` may report the root under: FSEvents reports
    /// real paths (`/private/var/...` for `/var/...`).
    real_root: Option<PathBuf>,
    /// The directory names are relative to: the root for a directory watch,
    /// its parent for a file watch.
    base: PathBuf,
    real_base: Option<PathBuf>,
}

impl Namer {
    fn new(root: &Path, is_dir: bool) -> Self {
        let base = if is_dir {
            root.to_path_buf()
        } else {
            root.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        let real_root = std::fs::canonicalize(root).ok().filter(|r| r != root);
        let real_base = real_root.as_ref().map(|real| {
            if is_dir {
                real.clone()
            } else {
                real.parent().map(Path::to_path_buf).unwrap_or_default()
            }
        });
        Self {
            root: root.to_path_buf(),
            real_root,
            base,
            real_base,
        }
    }

    fn name(&self, path: &Path) -> Option<Vec<u8>> {
        let is_root = path == self.root || self.real_root.as_deref() == Some(path);
        if is_root {
            return path.file_name().map(os_bytes);
        }
        let relative = path.strip_prefix(&self.base).ok().or_else(|| {
            self.real_base
                .as_deref()
                .and_then(|base| path.strip_prefix(base).ok())
        })?;
        if relative.as_os_str().is_empty() {
            return None;
        }
        Some(os_bytes(relative.as_os_str()))
    }
}

#[cfg(unix)]
fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    // libuv converts the UTF-16 name to UTF-8; an unpaired surrogate becomes
    // U+FFFD either way.
    s.to_string_lossy().into_owned().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind, RemoveKind};

    fn names(namer: &Namer, kind: EventKind, paths: &[&Path]) -> Vec<(WatchEventType, String)> {
        let mut event = notify::Event::new(kind);
        for p in paths {
            event = event.add_path(p.to_path_buf());
        }
        translate(namer, &event)
            .into_iter()
            .map(|e| (e.kind, String::from_utf8(e.filename.unwrap()).unwrap()))
            .collect()
    }

    #[test]
    fn kinds_map_to_libuv_event_types() {
        use WatchEventType::*;
        let cases = [
            (EventKind::Create(CreateKind::File), Some(Rename)),
            (EventKind::Remove(RemoveKind::Any), Some(Rename)),
            (EventKind::Modify(ModifyKind::Name(RenameMode::From)), Some(Rename)),
            (EventKind::Modify(ModifyKind::Name(RenameMode::To)), Some(Rename)),
            (EventKind::Modify(ModifyKind::Name(RenameMode::Both)), None),
            (EventKind::Modify(ModifyKind::Data(DataChange::Any)), Some(Change)),
            (EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)), Some(Change)),
            (EventKind::Modify(ModifyKind::Any), Some(Change)),
            (EventKind::Access(AccessKind::Any), None),
            (EventKind::Other, None),
        ];
        for (kind, want) in cases {
            assert_eq!(event_type(&kind), want, "{kind:?}");
        }
    }

    #[test]
    fn names_are_relative_to_the_watched_directory() {
        let root = std::env::temp_dir().join("oam-fsw-namer");
        let namer = Namer::new(&root, true);
        let nested = root.join("sub").join("f.txt");
        let got = names(
            &namer,
            EventKind::Create(CreateKind::File),
            &[&root.join("a.txt"), &nested],
        );
        let want_nested = Path::new("sub").join("f.txt").to_string_lossy().into_owned();
        assert_eq!(
            got,
            vec![
                (WatchEventType::Rename, "a.txt".to_string()),
                (WatchEventType::Rename, want_nested),
            ]
        );
        // The watched directory itself going away is named by its basename.
        assert_eq!(
            names(&namer, EventKind::Remove(RemoveKind::Folder), &[&root]),
            vec![(WatchEventType::Rename, "oam-fsw-namer".to_string())]
        );
        // A path outside the watch (no common prefix) is not reported.
        assert!(names(&namer, EventKind::Any, &[Path::new("/elsewhere/x")]).is_empty());
    }

    #[test]
    fn a_file_watch_names_the_file() {
        let file = std::env::temp_dir().join("oam-fsw-file.txt");
        let namer = Namer::new(&file, false);
        assert_eq!(
            names(&namer, EventKind::Modify(ModifyKind::Any), &[&file]),
            vec![(WatchEventType::Change, "oam-fsw-file.txt".to_string())]
        );
    }

    #[test]
    fn a_rescan_is_one_unnamed_rename() {
        let namer = Namer::new(Path::new("/w"), true);
        let event = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        assert_eq!(
            translate(&namer, &event),
            vec![WatchEvent {
                kind: WatchEventType::Rename,
                filename: None
            }]
        );
    }

    #[test]
    fn encode_frames_type_length_and_name() {
        let batch = [
            WatchEvent {
                kind: WatchEventType::Change,
                filename: Some(b"ab".to_vec()),
            },
            WatchEvent {
                kind: WatchEventType::Rename,
                filename: None,
            },
        ];
        assert_eq!(
            encode(&batch),
            [&[1u8, 2, 0, 0, 0, b'a', b'b'][..], &[0, 255, 255, 255, 255]].concat()
        );
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
                    "batch names the file: {bytes:?}"
                );
            }
            Ok(_) => panic!("expected an event batch"),
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
