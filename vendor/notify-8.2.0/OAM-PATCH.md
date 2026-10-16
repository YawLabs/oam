# notify 8.2.0, patched for oam

This directory is notify **8.2.0** as published on crates.io, plus three
changes to its Windows backend (`src/windows.rs`) that oam's `fs.watch`
needs to behave as node's does (`crates/oam_core/src/fs_watch.rs`). The
root `Cargo.toml` swaps it in with `[patch.crates-io]`.

- **Upstream:** `notify-8.2.0.crate`, sha256
  `4d3d07927151ff8575b7087f245456e549fea62edf0ec4e565a5ee50c8402bc3`
  (`OAM-PATCH.sha256`), the checksum Cargo.lock recorded before the swap.
- **The patch:** `OAM-PATCH.diff`, against the unpacked crate.
- **The gate:** `scripts/check-vendor.sh` (`scripts/ci-local.sh` step 11),
  as for every crate under `vendor/`: the published crate plus
  `OAM-PATCH.diff` must equal this directory byte for byte, and with
  `--build` it must compile warning-free with each feature set in
  `OAM-PATCH.features`. This directory is outside the workspace and the
  unsafe-budget scan. The patch adds no `unsafe` block: the new code runs
  inside the existing `handle_event` completion routine and calls only safe
  functions. After an edit here, `scripts/check-vendor.sh --regen` rewrites
  the diff.

## The diff

1. **A watch the OS ends is reported.** When ReadDirectoryChangesW completes
   with `ERROR_ACCESS_DENIED` for a directory that no longer exists (the
   watched directory, or a watched file's directory, was deleted), and on
   any error the backend does not know, 8.2.0 drops the watch without
   telling the event handler. oam's watcher then never learned the watch had
   ended: no 'error' for a watched file whose directory went (node emits
   `EPERM`), and no event for a deleted watched directory. The patch sends
   the handler `Err(io::Error::from_raw_os_error(code))`, with the
   directory's path, before it drops the watch.
2. **An overflow is a rescan.** ReadDirectoryChangesW reports that it
   discarded notifications by completing successfully with zero bytes, or
   with `ERROR_NOTIFY_ENUM_DIR`. 8.2.0 ignored the byte count, parsed the
   zeroed buffer and emitted nothing, so an overflow was lost silently. The
   patch emits one `EventKind::Other` event flagged `Flag::Rescan`, after
   re-arming the read, which oam reports as node's `rename` with a `null`
   filename. This is `completion_rescan_event` from notify 9.0.0-rc.5,
   backported unchanged.
3. **Last-access changes are watched.** The notify filter gains
   `FILE_NOTIFY_CHANGE_LAST_ACCESS`, which libuv's filter
   (`src/win/fs-event.c`) has and node reports as `change`.

## Remove it when

A stable notify release reports a watch the OS ends (item 1) and an overflow
(item 2) on Windows, and watches last-access changes or lets the caller add
them (item 3). notify 9.0.0-rc.5 has item 2 and, for a deleted watched
directory, an `EventKind::Remove` event in place of item 1; oam's
`forward()` would need to read that event instead. To remove it:

1. Delete this directory.
2. Delete the `notify` entry under `[patch.crates-io]` in the root
   `Cargo.toml` (and the `exclude = ["vendor"]` line if nothing else is
   vendored).
3. Run `cargo update -p notify`.
