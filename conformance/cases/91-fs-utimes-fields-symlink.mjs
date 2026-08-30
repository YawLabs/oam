// Timestamp FIELDS and the l* symlink variants -- the two things case 74 and
// case 73 leave undistinguished.
//
// Case 74 and case 73 pass `when, when` to every utimes/futimes/lutimes call
// and assert only mtime, so a backend that stamps the pair in the WRONG ORDER
// is indistinguishable from a correct one. Here atime and mtime are given
// different instants and BOTH are read back from their own field.
//
// Case 74 also only ever calls lutimesSync on a REGULAR FILE and on a missing
// path, so the entire point of the l* variant -- do not follow the link -- is
// never exercised. Below, lutimes must stamp the LINK and leave the target
// alone, while utimes on the same link must stamp the TARGET.
//
// Symlink creation is gated: on Windows it needs SeCreateSymbolicLinkPrivilege
// (Developer Mode or an elevated shell), so the whole symlink section prints
// its expected lines there instead -- node == oam either way, mirroring case
// 23's convention. lchmod is gated tighter still: it exists only on macOS
// (node binds the name to undefined elsewhere -- see case 74's header).
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-utimes-fields-${process.pid}`);
fs.mkdirSync(dir, { recursive: true });

// Four distinct fixed UTC instants. Distinct so a swapped pair is visible, and
// fixed so nothing drifts with the clock or the machine's timezone.
const A1 = new Date(Date.UTC(2001, 0, 2, 3, 4, 5));
const M1 = new Date(Date.UTC(2011, 10, 12, 13, 14, 15));
const A2 = new Date(Date.UTC(2003, 3, 4, 5, 6, 7));
const M2 = new Date(Date.UTC(2013, 8, 10, 11, 12, 13));

const iso = (d) => d.toISOString();
const stamps = (label, st) => console.log(label, "atime", iso(st.atime), "mtime", iso(st.mtime));
// mtime alone, for the one readback taken AFTER something traversed the link.
// Linux's default `relatime` bumps a symlink's own atime when the link is
// followed and its atime is older than its mtime -- which is exactly the state
// lutimes leaves below -- so an atime assertion there would read the wall
// clock and differ between the node run and the oam run. A symlink's mtime is
// never touched by traversal, and mtime is what "did utimes follow?" is about.
const mtimeOnly = (label, st) => console.log(label, "mtime", iso(st.mtime));

// --- path form, distinct fields -----------------------------------------
const file = path.join(dir, "plain.txt");
fs.writeFileSync(file, "x");
fs.utimesSync(file, A1, M1);
stamps("utimesSync      ", fs.statSync(file));

// --- fd form, distinct fields (case 73 passes the same value twice) ------
const fd = fs.openSync(file, "r+");
fs.futimesSync(fd, A2, M2);
stamps("futimesSync     ", fs.fstatSync(fd));
fs.closeSync(fd);

// --- lutimes on a NON-link is just utimes, but still per-field -----------
fs.lutimesSync(file, A1, M1);
stamps("lutimesSync(reg)", fs.statSync(file));

// --- seconds and numeric-string coercion must not swap the fields either -
fs.utimesSync(file, 1000000000, 1200000000);
stamps("utimesSync(sec) ", fs.statSync(file));
fs.utimesSync(file, "1000000000", "1200000000");
stamps("utimesSync(str) ", fs.statSync(file));

// --- promise and callback forms carry the same pair ---------------------
await fsp.utimes(file, A2, M2);
stamps("fsp.utimes      ", fs.statSync(file));
await new Promise((resolve, reject) =>
  fs.utimes(file, A1, M1, (e) => (e ? reject(e) : resolve())),
);
stamps("utimes cb       ", fs.statSync(file));

// NOT covered here: FileHandle.utimes. oam's FileHandle carries only
// read/write/readFile/writeFile/stat/close, so `(await fsp.open(f)).utimes` is
// a TypeError rather than a divergence in the timestamp path this case is
// about -- a whole-surface gap (chmod/chown/truncate/sync/datasync/readv/writev
// are absent too), tracked separately.

// --- the symlink half ----------------------------------------------------
const SYMLINK_EXPECTED = [
  "link  after lutimes atime 2003-04-04T05:06:07.000Z mtime 2013-09-10T11:12:13.000Z",
  "target after lutimes atime 2001-01-02T03:04:05.000Z mtime 2011-11-12T13:14:15.000Z",
  "target after utimes(link) atime 2003-04-04T05:06:07.000Z mtime 2013-09-10T11:12:13.000Z",
  "link  after utimes(link) mtime 2013-09-10T11:12:13.000Z",
];

// The link's own mode is what lchmod must move; the target's is pinned so the
// "unchanged" assertion cannot be read as a umask artifact.
const LCHMOD_EXPECTED = ["lchmodSync link mode 600 changed true", "lchmodSync target mode 644"];

const canSymlink = process.platform !== "win32";
const target = path.join(dir, "target.txt");
const link = path.join(dir, "link");

if (!canSymlink) {
  for (const line of SYMLINK_EXPECTED) console.log(line);
} else {
  fs.writeFileSync(target, "y");
  fs.symlinkSync(target, link);
  fs.utimesSync(target, A1, M1);
  // lutimes stamps the LINK; the target keeps A1/M1.
  fs.lutimesSync(link, A2, M2);
  stamps("link  after lutimes", fs.lstatSync(link));
  stamps("target after lutimes", fs.statSync(target));
  // utimes FOLLOWS. The target must move to A2/M2 -- so re-stamp it to A1/M1
  // first, otherwise "followed" and "did not follow" print the same thing.
  fs.utimesSync(target, A1, M1);
  fs.utimesSync(link, A2, M2);
  stamps("target after utimes(link)", fs.statSync(target));
  mtimeOnly("link  after utimes(link)", fs.lstatSync(link));
}

if (process.platform !== "darwin") {
  for (const line of LCHMOD_EXPECTED) console.log(line);
} else {
  fs.chmodSync(target, 0o644);
  const before = fs.lstatSync(link).mode & 0o777;
  fs.lchmodSync(link, 0o600);
  const after = fs.lstatSync(link).mode & 0o777;
  console.log("lchmodSync link mode", after.toString(8), "changed", after !== before);
  console.log("lchmodSync target mode", (fs.statSync(target).mode & 0o777).toString(8));
}

fs.rmSync(dir, { recursive: true, force: true });
