// What readdir and opendir report for a path that is no directory (#285).
//
// On Windows libuv's two listings look at the path differently before they
// list it. fs__scandir (readdir) OPENS it, following links, and reports that
// open's failure: a file symlink to a directory is EPERM, a directory symlink
// or junction to a file ENOENT, and a file named with a trailing separator
// ENOENT too -- readdir is the one call whose path keeps that separator
// (`scandir 'C:\cwd\f\'`). What opens and is no directory is ENOTDIR.
// fs__opendir reads the path's own attributes, a link's rather than its
// target's: a file symlink is ENOTDIR wherever it leads, a directory link to
// a file ENOENT. Everywhere, a failed opendir names syscall `opendir`, and
// opendirSync's error carries no path at all.
//
// Regression guard: oam listed through std's read_dir, whose one answer for
// anything that is no directory -- ERROR_DIRECTORY -- it reported as ENOTDIR
// for every one of these, dropped the trailing separator, and named `scandir`
// and the path in opendir's errors on every platform.
//
// The plain rows run everywhere. The link rows are Windows's: only there
// does a link have a kind to get wrong. Creating a symlink needs Developer
// Mode or elevation; a refused one prints its refusal, the same under both
// runtimes, and its rows are skipped. A junction needs neither.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const win = process.platform === "win32";
const start = process.cwd();
const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-393-"));
process.chdir(made);
const D = process.cwd();
// Whichever row throws, the directory goes.
process.on("exit", () => {
  try {
    process.chdir(start);
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});

// node shows `\\?\\\srv\sh\x` with the prefix off and no `\\` back on, so a temp
// directory on a share is scrubbed without its leading separators too.
const bare = D.startsWith("\\\\") ? D.slice(2) : D;
const scrub = (s) =>
  typeof s === "string" ? s.split(D).join("<D>").split(made).join("<D>").split(bare).join("<D>") : s;
const shape = (e) =>
  e && e.code
    ? `${e.code} ${e.errno} ${e.syscall} path=${JSON.stringify(scrub(e.path))} ${scrub(e.message)}`
    : "threw " + String(e);
const called = (f) => new Promise((res, rej) => f((e, v) => (e ? rej(e) : res(v))));
const first = (dir) => {
  const entry = dir.readSync();
  dir.closeSync();
  return entry && entry.name;
};

fs.mkdirSync("dir");
fs.writeFileSync("dir/a.txt", "a");
fs.writeFileSync("file.txt", "f");

// The empty path, which readdir refuses everywhere.
const names = ["dir", "file.txt", "missing", "", "file.txt/x", "dir/a.txt", "dir/a.txt/x", "dir/missing"];
// A trailing separator, and what is not one.
for (const base of ["dir", "file.txt", "missing", "dir/a.txt"]) {
  names.push(base + "/", base + "//", base + "/.", base + "/./");
  if (win) names.push(base + "\\", base + "\\\\", base + "\\.", base + "\\.\\");
}
if (win) {
  names.push(path.join(D, "file.txt") + "\\", "\\\\?\\" + path.join(D, "file.txt") + "\\", "\\\\?\\" + path.join(D, "file.txt"));
  names.push(path.join(D, "dir") + "\\", "\\\\?\\" + path.join(D, "dir") + "\\");
}

// The root of a share that is no disk: it opens, and is no directory.
if (win && fs.existsSync("\\\\localhost\\" + D[0] + "$")) {
  names.push("\\\\localhost\\IPC$", "\\\\localhost\\IPC$\\");
}

if (win) {
  const links = [
    ["file-link-to-dir", "dir", "file"],
    ["dir-link-to-file", "file.txt", "dir"],
    ["junction-to-file", path.join(D, "file.txt"), "junction"],
    ["file-link-to-file", "file.txt", "file"],
    ["dir-link-to-dir", "dir", "dir"],
    ["junction-to-dir", path.join(D, "dir"), "junction"],
    ["file-link-to-gone", "gone", "file"],
    ["dir-link-to-gone", "gone", "dir"],
    ["junction-to-gone", path.join(D, "gone"), "junction"],
    // A directory link whose target is a link of the wrong kind.
    ["dir-link-to-file-link-to-dir", "file-link-to-dir", "dir"],
    ["junction-to-file-link-to-dir", path.join(D, "file-link-to-dir"), "junction"],
  ];
  for (const [name, target, type] of links) {
    try {
      fs.symlinkSync(target, name, type);
      names.push(name, name + "\\", name + "/", name + "/a.txt", name + "/missing");
    } catch (e) {
      console.log("symlinkSync " + name + ": " + e.code);
    }
  }
}

for (const n of names) {
  const forms = [
    ["readdirSync", () => fs.readdirSync(n)],
    ["readdirSync withFileTypes", () => fs.readdirSync(n, { withFileTypes: true }).map((d) => d.name)],
    ["readdirSync recursive", () => fs.readdirSync(n, { recursive: true })],
    ["readdir", () => called((cb) => fs.readdir(n, cb))],
    ["readdir recursive", () => called((cb) => fs.readdir(n, { recursive: true }, cb))],
    ["promises.readdir", () => fs.promises.readdir(n)],
    ["promises.readdir withFileTypes", async () => (await fs.promises.readdir(n, { withFileTypes: true })).map((d) => d.name)],
    ["opendirSync", () => first(fs.opendirSync(n))],
    ["opendir", async () => first(await called((cb) => fs.opendir(n, cb)))],
    ["promises.opendir", async () => first(await fs.promises.opendir(n))],
  ];
  for (const [label, f] of forms) {
    // node's opendir of an empty path on Windows lists the current directory
    // -- nearly always. Under a loaded machine its promise form has answered
    // ENOENT instead (seen once in a parallel run of this suite, never in a
    // run alone), so the row has no fixed answer to hold oam to.
    if (win && n === "" && label.includes("opendir")) continue;
    let line;
    try {
      line = JSON.stringify(await f());
    } catch (e) {
      line = shape(e);
    }
    console.log(scrub(n) + " | " + label + ": " + line);
  }
}

// A prefix with nothing under it names a volume or a device namespace, not
// a directory (#332): libuv's open of the volume is refused (EPERM), the
// pipe namespace and NUL open and are no directory (ENOTDIR, and EISDIR for
// NUL as libuv reads its answer). With a separator after them they list in
// both runtimes, which is not shown: what they list is the machine's.
// readdir only -- opendir lists all of them, the machine's again.
if (win) {
  const drive = D.slice(0, 2);
  for (const n of [`\\\\?\\${drive}`, `\\\\.\\${drive}`, "\\\\.\\pipe", "\\\\.\\NUL"]) {
    const forms = [
      ["readdirSync", () => fs.readdirSync(n)],
      ["readdirSync withFileTypes", () => fs.readdirSync(n, { withFileTypes: true })],
      ["readdirSync recursive", () => fs.readdirSync(n, { recursive: true })],
      ["readdir", () => called((cb) => fs.readdir(n, cb))],
      ["readdir recursive", () => called((cb) => fs.readdir(n, { recursive: true }, cb))],
      ["promises.readdir", () => fs.promises.readdir(n)],
    ];
    for (const [label, f] of forms) {
      let line;
      try {
        const listed = await f();
        line = `lists ${listed.length > 0}`;
      } catch (e) {
        line = shape(e);
      }
      console.log(n.replace(drive, "<drive>") + " | " + label + ": " + line);
    }
  }
}
