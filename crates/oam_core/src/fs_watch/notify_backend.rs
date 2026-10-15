//! The `notify` backend (Windows and macOS): one `notify` watcher per
//! `fs.watch` call. That keeps each watcher's events its own: `notify`
//! reports an event's path but not which of its watches produced it, so a
//! shared watcher could not tell a directory watch from a watch on a file
//! inside it, both of which see that file's events. The cost is a native
//! thread per watcher on Windows (the backend's event loop); libuv shares one
//! loop. That is recorded in docs/node-divergences.md.

use super::{Note, WatchEvent, WatchEventType, WatchStartError};
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::UnboundedSender;

/// Nothing is shared between watchers here.
#[derive(Clone, Default)]
pub(super) struct Shared;

/// One watch; dropping it stops the OS watch (and the backend's thread).
pub(super) struct Handle {
    _watcher: notify::RecommendedWatcher,
}

/// Start the OS watch on `root`, queueing its events on `tx`.
pub(super) fn watch(
    _shared: &Shared,
    root: &Path,
    is_dir: bool,
    recursive: bool,
    tx: UnboundedSender<Note>,
) -> Result<Handle, WatchStartError> {
    #[cfg(windows)]
    open_watched_directory(root, is_dir).map_err(WatchStartError::Io)?;
    let namer = Namer::new(root, is_dir);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        forward(&namer, &tx, res)
    })
    .map_err(start_error)?;
    let mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    watcher.watch(root, mode).map_err(start_error)?;
    Ok(Handle { _watcher: watcher })
}

/// Open the directory ReadDirectoryChangesW will watch -- the path itself, or
/// a file's parent, as libuv's `uv_fs_event_start` does -- and close it
/// again, so a refusal is the OS's own error (`EPERM` for a directory the
/// caller may not list, as node reports it). `notify` opens the same
/// directory, but folds a failure into `PathNotFound` or a generic error with
/// no OS code, which reached JS as `EIO` and a Rust debug dump.
#[cfg(windows)]
fn open_watched_directory(root: &Path, is_dir: bool) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    // winnt.h / winbase.h: the access right and flag notify's CreateFileW
    // passes (it adds FILE_FLAG_OVERLAPPED, which does not change whether
    // the open is allowed).
    const FILE_LIST_DIRECTORY: u32 = 0x0001;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const SHARE_READ_WRITE_DELETE: u32 = 0x0000_0007;
    let dir = if is_dir {
        root
    } else {
        root.parent().unwrap_or(root)
    };
    std::fs::OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY)
        .share_mode(SHARE_READ_WRITE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .map(drop)
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
        // A generic error's text is notify's own (on Windows, a Debug dump of
        // an inner error); node never shows anything like it, so report the
        // plain EIO.
        notify::ErrorKind::Generic(_) => {
            std::io::Error::other(crate::uv_strerror("EIO").unwrap_or("i/o error"))
        }
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
                let _ = tx.send(Note::Event(event, None));
            }
        }
        Err(error) => {
            // Windows: the watched directory itself was deleted, and the
            // backend ended its watch (ERROR_ACCESS_DENIED from
            // ReadDirectoryChangesW). node reports `rename` naming the
            // directory by its full `\\?\` path -- over and over, in a tight
            // loop, until the watcher is closed (v22.22.2, measured: 66,878
            // events in a second). oam reports that event once; the watcher
            // then stays open, as node's does, and so a referenced one keeps
            // the process alive until it is closed. A watched FILE whose
            // directory goes is node's `EPERM` 'error', which closes it.
            #[cfg(windows)]
            if namer.is_dir && matches!(namer.root.try_exists(), Ok(false)) {
                let _ = tx.send(Note::Event(
                    WatchEvent {
                        kind: WatchEventType::Rename,
                        filename: Some(namespaced(&namer.root).into_bytes()),
                    },
                    None,
                ));
                return;
            }
            let _ = tx.send(Note::Failed(notify_io(error.kind)));
        }
    }
}

/// `path` as libuv opened it: Win32 namespaced (`\\?\C:\...`,
/// `\\?\UNC\srv\share\...`), which is how node names a deleted watched
/// directory.
#[cfg(windows)]
fn namespaced(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\?\") || text.starts_with(r"\\.\") {
        text.into_owned()
    } else if let Some(unc) = text.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{unc}")
    } else {
        format!(r"\\?\{text}")
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
    /// A directory watch (not a file's).
    #[cfg_attr(not(windows), allow(dead_code))]
    is_dir: bool,
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
            is_dir,
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
            (
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                Some(Rename),
            ),
            (
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                Some(Rename),
            ),
            (EventKind::Modify(ModifyKind::Name(RenameMode::Both)), None),
            (
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                Some(Change),
            ),
            (
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
                Some(Change),
            ),
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
        let want_nested = Path::new("sub")
            .join("f.txt")
            .to_string_lossy()
            .into_owned();
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

    #[cfg(windows)]
    #[test]
    fn a_deleted_directory_is_named_by_its_namespaced_path() {
        assert_eq!(namespaced(Path::new(r"C:\w\d")), r"\\?\C:\w\d");
        assert_eq!(namespaced(Path::new(r"\\?\C:\w")), r"\\?\C:\w");
        assert_eq!(namespaced(Path::new(r"\\srv\sh\d")), r"\\?\UNC\srv\sh\d");
    }

    #[test]
    fn a_generic_backend_error_is_a_plain_eio() {
        let e = notify_io(notify::ErrorKind::Generic(
            "Error in watcher: Error {..}".into(),
        ));
        assert_eq!(e.to_string(), "i/o error");
    }
}
