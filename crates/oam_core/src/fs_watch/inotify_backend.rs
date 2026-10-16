//! The Linux backend: libuv's design, one inotify instance per run shared by
//! every watcher (src/unix/linux.c keeps one per loop), through the
//! `inotify` crate's safe API.
//!
//! The kernel hands out one watch descriptor per inode per instance, so two
//! watchers on the same directory, or a watcher on a directory and one on a
//! file inside it, share or split descriptors exactly as libuv's handles do.
//! Every descriptor keeps a list of the watchers it serves (with the path
//! prefix each one names its entries by), and an event goes to the watchers
//! of the descriptor that produced it: a directory watcher and a watcher on a
//! file in it each see one event per change, as in node. (A path-keyed
//! demultiplexer over one `notify` watcher could not do that: notify reports
//! the same path for both descriptors' events.)
//!
//! One thread reads the instance. It lives while any descriptor does: the
//! last descriptor's removal queues an `IN_IGNORED`, which wakes the
//! blocking read, and the thread then sees nothing left, takes the instance
//! out of the shared slot and closes it. The next watch makes a new one.

use super::{Note, WatchEvent, WatchEventType, WatchStartError};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc::UnboundedSender;

/// The run's inotify instance, while it has descriptors.
#[derive(Clone, Default)]
pub(super) struct Shared {
    slot: Arc<Mutex<Option<Instance>>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Option<Instance>> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One watch; dropping it detaches it from its descriptors and removes the
/// ones nothing else uses.
pub(super) struct Handle {
    shared: Shared,
    id: u64,
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(instance) = self.shared.lock().as_mut() {
            instance.detach(self.id);
        }
    }
}

struct Instance {
    watches: Watches,
    /// By descriptor number: the descriptor and the watchers it serves.
    wds: HashMap<i32, Descriptor>,
    watchers: HashMap<u64, Watcher>,
    next: u64,
}

struct Descriptor {
    wd: WatchDescriptor,
    users: Vec<User>,
}

struct User {
    watcher: u64,
    /// Where this descriptor's directory sits under the watcher's root,
    /// which is what its entries are named by: empty for the root itself,
    /// `sub/dir` for a directory a recursive watcher reached.
    prefix: PathBuf,
}

struct Watcher {
    tx: UnboundedSender<Note>,
    /// The watched path, whose basename names an event on the path itself.
    root: PathBuf,
    recursive: bool,
    /// The descriptors it is attached to.
    wds: Vec<i32>,
}

/// libuv's mask (uv_fs_event_start in src/unix/linux.c).
fn mask() -> WatchMask {
    WatchMask::ATTRIB
        | WatchMask::CREATE
        | WatchMask::MODIFY
        | WatchMask::DELETE
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
}

/// Start the OS watch on `root`, queueing its events on `tx`.
pub(super) fn watch(
    shared: &Shared,
    root: &Path,
    is_dir: bool,
    recursive: bool,
    tx: UnboundedSender<Note>,
) -> Result<Handle, WatchStartError> {
    let mut slot = shared.lock();
    // A new instance's reader starts only once the instance has a
    // descriptor: a reader blocked on an instance with none would never
    // wake.
    let fresh = match slot.as_ref() {
        Some(_) => None,
        None => {
            let inotify = Inotify::init().map_err(WatchStartError::Io)?;
            *slot = Some(Instance {
                watches: inotify.watches(),
                wds: HashMap::new(),
                watchers: HashMap::new(),
                next: 0,
            });
            Some(inotify)
        }
    };
    let Some(instance) = slot.as_mut() else {
        unreachable!("the slot was filled above");
    };
    instance.next += 1;
    let id = instance.next;
    instance.watchers.insert(
        id,
        Watcher {
            tx,
            root: root.to_path_buf(),
            recursive: recursive && is_dir,
            wds: Vec::new(),
        },
    );
    if let Err(error) = instance.attach(id, root, PathBuf::new()) {
        instance.detach(id);
        if fresh.is_some() {
            // Nothing was added, so no reader: drop the instance, and with
            // it (and `fresh`) the descriptor.
            *slot = None;
        }
        return Err(add_error(error));
    }
    if recursive && is_dir {
        instance.attach_tree(id, root, Path::new(""));
    }
    if let Some(inotify) = fresh {
        let reader = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("oam-fs-watch".into())
            .spawn(move || read(reader, inotify));
        if let Err(error) = spawned {
            instance.detach(id);
            *slot = None;
            return Err(WatchStartError::Io(error));
        }
    }
    Ok(Handle {
        shared: shared.clone(),
        id,
    })
}

/// ENOSPC from inotify_add_watch(2) is the per-user watch limit
/// (`fs.inotify.max_user_watches`), which node reports with its own message.
fn add_error(error: std::io::Error) -> WatchStartError {
    if error.raw_os_error() == Some(libc::ENOSPC) {
        WatchStartError::LimitReached
    } else {
        WatchStartError::Io(error)
    }
}

impl Instance {
    /// Watch `path` for `watcher`, naming its entries under `prefix`.
    fn attach(&mut self, watcher: u64, path: &Path, prefix: PathBuf) -> std::io::Result<()> {
        // The same mask every time, so a second add of an inode only hands
        // back its descriptor (inotify replaces a mask with an equal one).
        let wd = self.watches.add(path, mask())?;
        let number = wd.get_watch_descriptor_id();
        let descriptor = self.wds.entry(number).or_insert_with(|| Descriptor {
            wd,
            users: Vec::new(),
        });
        if descriptor.users.iter().any(|user| user.watcher == watcher) {
            return Ok(());
        }
        descriptor.users.push(User { watcher, prefix });
        if let Some(w) = self.watchers.get_mut(&watcher) {
            w.wds.push(number);
        }
        Ok(())
    }

    /// A recursive watcher: every directory under `dir` (named `prefix`
    /// under the root), not following symlinks, as libuv's recursive walk
    /// does not. A directory that cannot be read or watched is skipped; it
    /// may have gone, or be someone else's.
    fn attach_tree(&mut self, watcher: u64, dir: &Path, prefix: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let path = entry.path();
            let name = prefix.join(entry.file_name());
            if self.attach(watcher, &path, name.clone()).is_ok() {
                self.attach_tree(watcher, &path, &name);
            }
        }
    }

    /// Stop serving `watcher`; remove each descriptor nobody else uses.
    fn detach(&mut self, watcher: u64) {
        let Some(w) = self.watchers.remove(&watcher) else {
            return;
        };
        for number in w.wds {
            let Some(descriptor) = self.wds.get_mut(&number) else {
                continue;
            };
            descriptor.users.retain(|user| user.watcher != watcher);
            if descriptor.users.is_empty()
                && let Some(descriptor) = self.wds.remove(&number)
            {
                // Its IN_IGNORED wakes the reader. An error means the
                // descriptor is already gone (its inode was deleted), and
                // that IN_IGNORED is already queued.
                let _ = self.watches.remove(descriptor.wd);
            }
        }
    }

    /// Deliver one inotify event to the watchers of its descriptor.
    fn dispatch(&mut self, number: i32, mask: EventMask, cookie: u32, name: Option<&OsStr>) {
        if mask.contains(EventMask::Q_OVERFLOW) {
            // The kernel dropped events; it says nothing of which. Every
            // watcher hears one unnamed `rename`, as libuv's Windows backend
            // reports an overflow (its Linux one reports nothing).
            for w in self.watchers.values() {
                let _ = w.tx.send(Note::Event(
                    WatchEvent {
                        kind: WatchEventType::Rename,
                        filename: None,
                    },
                    None,
                ));
            }
            return;
        }
        let Some(descriptor) = self.wds.get(&number) else {
            // A descriptor detached since the event was queued.
            return;
        };
        // libuv's rule: an event with any bit but IN_ATTRIB and IN_MODIFY
        // is a rename (IN_ISDIR counts, so an attribute change on a
        // subdirectory is `rename` in node, and so is the IN_IGNORED that
        // ends a deleted watch).
        let kind = if mask.bits() & !(EventMask::ATTRIB | EventMask::MODIFY).bits() != 0 {
            WatchEventType::Rename
        } else {
            WatchEventType::Change
        };
        let new_dir = name.is_some()
            && mask.contains(EventMask::ISDIR)
            && mask.intersects(EventMask::CREATE | EventMask::MOVED_TO);
        let mut grow = Vec::new();
        for user in &descriptor.users {
            let Some(w) = self.watchers.get(&user.watcher) else {
                continue;
            };
            let filename = match name {
                Some(name) => user.prefix.join(name),
                // An event on the watched path itself is named by its
                // basename (libuv: `uv__basename_r(w->path)`). One on a
                // directory a recursive watcher reached is reported by its
                // parent, by name, already.
                None if user.prefix.as_os_str().is_empty() => match w.root.file_name() {
                    Some(base) => PathBuf::from(base),
                    None => continue,
                },
                None => continue,
            };
            if new_dir && w.recursive {
                grow.push((user.watcher, filename.clone()));
            }
            let _ = w.tx.send(Note::Event(
                WatchEvent {
                    kind,
                    filename: Some(filename.as_os_str().as_bytes().to_vec()),
                },
                Some((number, mask.bits(), cookie)),
            ));
        }
        if mask.contains(EventMask::IGNORED) {
            // The kernel has dropped the descriptor (its inode was deleted,
            // or it was removed above).
            if let Some(descriptor) = self.wds.remove(&number) {
                for user in descriptor.users {
                    if let Some(w) = self.watchers.get_mut(&user.watcher) {
                        w.wds.retain(|n| *n != number);
                    }
                }
            }
        }
        // A directory made (or moved) inside a recursive watcher's tree is
        // watched too, with what is already in it.
        for (watcher, prefix) in grow {
            let Some(root) = self.watchers.get(&watcher).map(|w| w.root.clone()) else {
                continue;
            };
            let path = root.join(&prefix);
            if self.attach(watcher, &path, prefix.clone()).is_ok() {
                self.attach_tree(watcher, &path, &prefix);
            }
        }
    }

    /// The read failed: every watcher hears it, and the instance ends.
    fn fail(&mut self, error: &std::io::Error) {
        for w in self.watchers.values() {
            let copy = match error.raw_os_error() {
                Some(code) => std::io::Error::from_raw_os_error(code),
                None => std::io::Error::new(error.kind(), error.to_string()),
            };
            let _ = w.tx.send(Note::Failed(copy));
        }
    }
}

/// The reader thread: deliver every event until no descriptor is left.
fn read(shared: Shared, mut inotify: Inotify) {
    // Room for many events per read; an event is 16 bytes plus its name.
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let events = match inotify.read_events_blocking(&mut buffer) {
            Ok(events) => events,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let mut slot = shared.lock();
                if let Some(instance) = slot.as_mut() {
                    instance.fail(&error);
                }
                *slot = None;
                return;
            }
        };
        let mut slot = shared.lock();
        let Some(instance) = slot.as_mut() else {
            return;
        };
        for event in events {
            instance.dispatch(
                event.wd.get_watch_descriptor_id(),
                event.mask,
                event.cookie,
                event.name,
            );
        }
        if instance.wds.is_empty() {
            // Nothing left to read: close the instance. A watch started
            // after this makes a new one; until then the watchers left (a
            // deleted root's) are idle, as libuv's are.
            *slot = None;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oam-fsw-ino-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Every event that arrives within `wait`, as `type:name`.
    fn drain(rx: &mut UnboundedReceiver<Note>, wait: Duration) -> Vec<String> {
        let deadline = std::time::Instant::now() + wait;
        let mut out = Vec::new();
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(Note::Event(e, _)) => out.push(format!(
                    "{}:{}",
                    if e.kind == WatchEventType::Rename {
                        "rename"
                    } else {
                        "change"
                    },
                    e.filename
                        .map(|f| String::from_utf8_lossy(&f).into_owned())
                        .unwrap_or_else(|| "null".into())
                )),
                Ok(Note::Failed(e)) => out.push(format!("error:{e}")),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        out
    }

    fn start(shared: &Shared, path: &Path, recursive: bool) -> (Handle, UnboundedReceiver<Note>) {
        let (tx, rx) = unbounded_channel();
        let is_dir = path.is_dir();
        let handle = watch(shared, path, is_dir, recursive, tx).unwrap();
        (handle, rx)
    }

    fn instances(shared: &Shared) -> bool {
        shared.lock().is_some()
    }

    #[test]
    fn many_watchers_share_one_instance() {
        // Past the default per-user instance limit (128): one instance each
        // failed EMFILE here.
        let root = temp("many");
        let shared = Shared::default();
        let mut handles = Vec::new();
        for i in 0..300 {
            let dir = root.join(format!("d{i}"));
            std::fs::create_dir(&dir).unwrap();
            handles.push(start(&shared, &dir, false));
        }
        std::fs::write(root.join("d299").join("f"), b"x").unwrap();
        let (_, rx) = handles.last_mut().unwrap();
        let got = drain(rx, Duration::from_millis(300));
        assert!(got.contains(&"rename:f".to_string()), "{got:?}");
        drop(handles);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_and_a_file_in_it_each_see_one_event() {
        let root = temp("overlap");
        let file = root.join("f");
        std::fs::write(&file, b"x").unwrap();
        let shared = Shared::default();
        let (_dir_watch, mut dir_rx) = start(&shared, &root, false);
        let (_file_watch, mut file_rx) = start(&shared, &file, false);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"y"))
            .unwrap();
        assert_eq!(drain(&mut dir_rx, Duration::from_millis(300)), ["change:f"]);
        assert_eq!(
            drain(&mut file_rx, Duration::from_millis(100)),
            ["change:f"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_watchers_on_one_directory_each_see_it_and_close_alone() {
        let root = temp("twice");
        let shared = Shared::default();
        let (first, mut first_rx) = start(&shared, &root, false);
        let (_second, mut second_rx) = start(&shared, &root, false);
        std::fs::write(root.join("a"), b"").unwrap();
        assert_eq!(
            drain(&mut first_rx, Duration::from_millis(300)),
            ["rename:a"]
        );
        assert_eq!(
            drain(&mut second_rx, Duration::from_millis(100)),
            ["rename:a"]
        );
        // Closing one keeps the descriptor for the other.
        drop(first);
        std::fs::write(root.join("b"), b"").unwrap();
        assert_eq!(
            drain(&mut second_rx, Duration::from_millis(300)),
            ["rename:b"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_root_is_renamed_twice_as_libuv_reports_it() {
        // node v22.22.2 on Linux, measured: `rename:w rename:w` -- the
        // IN_DELETE_SELF and the IN_IGNORED after it, each named by the
        // watched path's basename.
        let root = temp("gone");
        let dir = root.join("w");
        std::fs::create_dir(&dir).unwrap();
        let shared = Shared::default();
        let (_watch, mut rx) = start(&shared, &dir, false);
        std::fs::remove_dir(&dir).unwrap();
        assert_eq!(
            drain(&mut rx, Duration::from_millis(300)),
            ["rename:w", "rename:w"]
        );
        // No descriptor is left, so the reader has closed the instance.
        assert!(!instances(&shared));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_attribute_change_on_a_subdirectory_is_a_rename() {
        // libuv counts IN_ISDIR as a rename bit (node v22.22.2 on Linux,
        // measured: chmod of a subdirectory reports `rename:sub`).
        use std::os::unix::fs::PermissionsExt;
        let root = temp("attr");
        std::fs::create_dir(root.join("sub")).unwrap();
        let shared = Shared::default();
        let (_watch, mut rx) = start(&shared, &root, false);
        std::fs::set_permissions(root.join("sub"), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(drain(&mut rx, Duration::from_millis(300)), ["rename:sub"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_recursive_watcher_names_nested_entries_and_follows_new_directories() {
        let root = temp("rec");
        std::fs::create_dir_all(root.join("s1").join("s2")).unwrap();
        let shared = Shared::default();
        let (_watch, mut rx) = start(&shared, &root, true);
        std::fs::write(root.join("s1").join("s2").join("deep"), b"").unwrap();
        std::fs::create_dir(root.join("n")).unwrap();
        // Give the reader a moment to watch `n` before writing into it.
        let first = drain(&mut rx, Duration::from_millis(300));
        assert!(
            first.contains(&"rename:s1/s2/deep".to_string()),
            "{first:?}"
        );
        assert!(first.contains(&"rename:n".to_string()), "{first:?}");
        std::fs::write(root.join("n").join("inside"), b"").unwrap();
        let second = drain(&mut rx, Duration::from_millis(300));
        assert!(
            second.contains(&"rename:n/inside".to_string()),
            "{second:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn closing_the_last_watcher_closes_the_instance() {
        let root = temp("last");
        let shared = Shared::default();
        let (watch, _rx) = start(&shared, &root, false);
        assert!(instances(&shared));
        drop(watch);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while instances(&shared) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!instances(&shared), "the reader closed the instance");
        // And a later watch makes a new one that works.
        let (_again, mut rx) = start(&shared, &root, false);
        std::fs::write(root.join("x"), b"").unwrap();
        assert_eq!(drain(&mut rx, Duration::from_millis(300)), ["rename:x"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
