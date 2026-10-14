//! The files and directories oam writes for itself -- its caches, its crash
//! reports, the scratch files a run needs -- with modes of oam's choosing.
//!
//! Why this exists. A program can set the process's file mode creation mask
//! (`process.umask`), and the kernel then takes it off the mode of every
//! file the process creates, oam's own included. Those are not the program's
//! files: a cache outlives the run and is read by the next one, and a
//! scratch file has to be readable by the process that just wrote it. So
//! what oam writes for itself is created here, with the mode it would have
//! had under the mask the process STARTED with (noted by
//! [`note_inherited_umask`] before any program code runs), set explicitly
//! so that a mask set since does not come into it. The owner's own bits are
//! always kept: a mask that takes them away -- one inherited from a parent
//! that set it -- must not leave oam unable to use what it wrote.
//!
//! It lives in this crate because it is the one every other oam crate links;
//! it has nothing to do with diagnostics.
//!
//! On Windows, and on unix when no starting mask was noted (a test, an
//! embedder), these are the plain `std::fs` calls.

use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

/// No starting mask has been noted.
const UNKNOWN: u32 = u32::MAX;

static INHERITED_UMASK: AtomicU32 = AtomicU32::new(UNKNOWN);

/// Note the mask the process started with. The first call is the one kept:
/// later ones come from code that has since changed the mask.
pub fn note_inherited_umask(mask: u32) {
    let _ = INHERITED_UMASK.compare_exchange(
        UNKNOWN,
        mask & 0o777,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

fn inherited_umask() -> Option<u32> {
    match INHERITED_UMASK.load(Ordering::Acquire) {
        UNKNOWN => None,
        mask => Some(mask),
    }
}

/// Who a file is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// Something kept: a cache entry, a crash report. The mode the starting
    /// mask gives a new file, so whoever could read oam's files before
    /// still can (a cache built by one user of an image and read by
    /// another).
    Kept,
    /// Scratch for this process alone: the owner's, nobody else's.
    Scratch,
}

/// The mode of a directory oam creates for `audience` under a starting mask.
fn dir_mode(audience: Audience, inherited: u32) -> u32 {
    match audience {
        Audience::Kept => (0o777 & !inherited) | 0o700,
        Audience::Scratch => 0o700,
    }
}

/// The mode of a file oam creates for `audience` under a starting mask.
fn file_mode(audience: Audience, inherited: u32) -> u32 {
    match audience {
        Audience::Kept => (0o666 & !inherited) | 0o600,
        Audience::Scratch => 0o600,
    }
}

/// `std::fs::create_dir_all`, each directory it creates getting oam's mode
/// for `audience`. One that is already there is left as it is.
pub fn create_dir_all(dir: &Path, audience: Audience) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(inherited) = inherited_umask() {
        return create_dir_all_mode(dir, dir_mode(audience, inherited));
    }
    let _ = audience;
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn create_dir_all_mode(dir: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let mut missing = Vec::new();
    let mut at = Some(dir);
    while let Some(path) = at {
        // `symlink_metadata`, not `exists`: a dangling link is there, and
        // creating over it is the error `create_dir_all` reports too.
        if path.as_os_str().is_empty() || path.symlink_metadata().is_ok() {
            break;
        }
        missing.push(path);
        at = path.parent();
    }
    for path in missing.into_iter().rev() {
        match std::fs::DirBuilder::new().mode(mode).create(path) {
            // mkdir's mode is under the current mask; chmod is not. Left as
            // created where the filesystem has no modes to set.
            Ok(()) => {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
            }
            // Another process made it first: its directory, its mode.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(e) => return Err(e),
        }
    }
    // Nothing was missing: say what `create_dir_all` says of what is there.
    if dir.is_dir() {
        Ok(())
    } else {
        std::fs::create_dir_all(dir)
    }
}

/// `File::create` -- create, or truncate what is there -- with oam's mode
/// for `audience`. A file already there that its owner cannot write (one a
/// mask without the owner's bits left behind) is replaced.
pub fn create(path: &Path, audience: Audience) -> std::io::Result<File> {
    #[cfg(unix)]
    if let Some(inherited) = inherited_umask() {
        return create_mode(path, file_mode(audience, inherited));
    }
    let _ = audience;
    File::create(path)
}

#[cfg(unix)]
fn create_mode(path: &Path, mode: u32) -> std::io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let open = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(path)
    };
    let file = match open() {
        // Removing it needs the directory's permission, not the file's; on
        // a directory that cannot be written both this and the retry fail.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && path.is_file() => {
            let _ = std::fs::remove_file(path);
            open()?
        }
        other => other?,
    };
    // On the descriptor: not under the mask, and it holds for a file the
    // mask created with no permissions at all. Left as created where the
    // filesystem has no modes to set; the write below goes through the
    // descriptor either way.
    let _ = file.set_permissions(std::fs::Permissions::from_mode(mode));
    Ok(file)
}

/// `std::fs::write` through [`create`].
pub fn write(path: &Path, bytes: impl AsRef<[u8]>, audience: Audience) -> std::io::Result<()> {
    create(path, audience)?.write_all(bytes.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_follow_the_starting_mask_and_keep_the_owner() {
        assert_eq!(dir_mode(Audience::Kept, 0o022), 0o755);
        assert_eq!(file_mode(Audience::Kept, 0o022), 0o644);
        assert_eq!(dir_mode(Audience::Kept, 0o077), 0o700);
        assert_eq!(file_mode(Audience::Kept, 0o077), 0o600);
        assert_eq!(dir_mode(Audience::Kept, 0), 0o777);
        assert_eq!(file_mode(Audience::Kept, 0), 0o666);
        // A mask that takes the owner's bits leaves oam its own.
        assert_eq!(dir_mode(Audience::Kept, 0o777), 0o700);
        assert_eq!(file_mode(Audience::Kept, 0o777), 0o600);
        assert_eq!(dir_mode(Audience::Kept, 0o627), 0o750);
        assert_eq!(file_mode(Audience::Kept, 0o627), 0o640);
        for mask in [0, 0o022, 0o777] {
            assert_eq!(dir_mode(Audience::Scratch, mask), 0o700);
            assert_eq!(file_mode(Audience::Scratch, mask), 0o600);
        }
    }

    #[test]
    fn the_first_mask_noted_is_the_one_kept() {
        // The one test that touches the process-wide note; the functions
        // that read it are tested through their `_mode` halves below.
        note_inherited_umask(0o027);
        note_inherited_umask(0);
        assert_eq!(inherited_umask(), Some(0o027));
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("oam-own-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The modes are set, not left to the mask: under the usual 0o022 a
    /// plain create could not make a 0o775 directory or a 0o664 file.
    #[cfg(unix)]
    #[test]
    fn what_is_created_has_the_mode_asked_for_and_what_was_there_keeps_its_own() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let root = scratch("modes");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o750)).unwrap();

        let deep = root.join("a").join("b");
        create_dir_all_mode(&deep, 0o775).unwrap();
        assert_eq!(mode(&root), 0o750, "an existing directory is left alone");
        assert_eq!(mode(&root.join("a")), 0o775);
        assert_eq!(mode(&deep), 0o775);
        create_dir_all_mode(&deep, 0o700).unwrap();
        assert_eq!(mode(&deep), 0o775, "and so is one made by an earlier call");

        let file = deep.join("f");
        create_mode(&file, 0o664)
            .unwrap()
            .write_all(b"one")
            .unwrap();
        assert_eq!(mode(&file), 0o664);
        create_mode(&file, 0o600).unwrap().write_all(b"2").unwrap();
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"2",
            "a second create truncates"
        );
        assert_eq!(mode(&file), 0o600);

        // A file over a path component is the error create_dir_all gives.
        assert!(create_dir_all_mode(&file.join("x"), 0o700).is_err());
        assert!(create_dir_all_mode(&file, 0o700).is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A file its owner cannot write -- what a mask without the owner's
    /// bits left behind -- is replaced rather than refused.
    #[cfg(unix)]
    #[test]
    fn a_file_the_owner_cannot_write_is_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("replace");
        let file = root.join("stamp");
        std::fs::write(&file, b"old").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
        create_mode(&file, 0o600)
            .unwrap()
            .write_all(b"new")
            .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
