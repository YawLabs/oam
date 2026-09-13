// The code, errno, syscall and message node reports for an OS error on Windows,
// run against live node on the same host.
//
// Regression guard. oam derived the code from std's `io::ErrorKind` instead of
// the raw Win32 code libuv translates. That folded ERROR_ACCESS_DENIED into
// EACCES where node says EPERM (writing a read-only file, unlinking a
// directory, setRawMode on a read-only console), turned a sharing violation
// into EIO where node says EBUSY, and leaked the OS's own sentence into the
// message. On top of the table, libuv applies a few rules per operation that
// the table alone cannot express -- a directory opened for writing is EISDIR,
// a directory as a program is ENOENT, spawn() THROWS the codes it does not
// emit -- and this pins those too.
//
// Windows-only by design: every scenario is a Windows access-control or
// file-sharing rule, and on POSIX the same calls answer from different rules.
// Elsewhere the case prints one line so node == oam still holds.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn, spawnSync } from "node:child_process";

if (process.platform !== "win32") {
  console.log("windows-only");
  process.exit(0);
}

const root = fs.mkdtempSync(path.join(os.tmpdir(), "oam-oserr-"));
// The fixture root differs between the node run and the oam run; everything
// printed goes through this so the two transcripts compare byte for byte.
const clean = (value) =>
  typeof value === "string" ? value.split(root).join("<root>") : value;

function shape(err) {
  if (!err) return "no error";
  const out = { code: err.code, errno: err.errno, syscall: clean(err.syscall) };
  if ("path" in err) out.path = clean(err.path);
  out.message = clean(err.message);
  return JSON.stringify(out);
}

function sync(label, fn) {
  try {
    fn();
    console.log(label, "ok");
  } catch (err) {
    console.log(label, shape(err));
  }
}

async function later(label, fn) {
  try {
    await fn();
    console.log(label, "ok");
  } catch (err) {
    console.log(label, shape(err));
  }
}

// --- ERROR_ACCESS_DENIED: EPERM ----------------------------------------------
const readOnly = path.join(root, "read-only.txt");
fs.writeFileSync(readOnly, "x");
fs.chmodSync(readOnly, 0o444);
sync("writeFileSync(read-only)", () => fs.writeFileSync(readOnly, "y"));
sync("appendFileSync(read-only)", () => fs.appendFileSync(readOnly, "y"));
sync("openSync(read-only, r+)", () => fs.closeSync(fs.openSync(readOnly, "r+")));
await later("promises.writeFile(read-only)", () => fs.promises.writeFile(readOnly, "y"));
await new Promise((resolve) =>
  fs.writeFile(readOnly, "y", (err) => {
    console.log("writeFile(read-only) cb", shape(err));
    resolve();
  }),
);

const dir = path.join(root, "a-dir");
fs.mkdirSync(dir);
sync("unlinkSync(dir)", () => fs.unlinkSync(dir));

// --- a directory where a file was expected -------------------------------------
sync("openSync(dir, w)", () => fs.closeSync(fs.openSync(dir, "w")));
sync("openSync(dir, wx)", () => fs.closeSync(fs.openSync(dir, "wx")));
sync("writeFileSync(dir)", () => fs.writeFileSync(dir, "y"));
sync("appendFileSync(dir)", () => fs.appendFileSync(dir, "y"));
sync("readFileSync(dir)", () => fs.readFileSync(dir));
sync("readFileSync(dir, utf8)", () => fs.readFileSync(dir, "utf8"));
await later("promises.readFile(dir)", () => fs.promises.readFile(dir));
await later("promises.writeFile(dir)", () => fs.promises.writeFile(dir, "y"));
await later("promises.open(dir, w)", async () => (await fs.promises.open(dir, "w")).close());

// --- per-operation EINVAL ------------------------------------------------------
sync("mkdirSync(invalid name)", () => fs.mkdirSync(path.join(root, 'a"b')));
await later("promises.mkdir(invalid name)", () => fs.promises.mkdir(path.join(root, "a*b")));
sync("readlinkSync(regular file)", () => fs.readlinkSync(readOnly));
await later("promises.readlink(regular file)", () => fs.promises.readlink(readOnly));

// ERROR_SHARING_VIOLATION (EBUSY) is not exercised here: holding a file without
// FILE_SHARE_DELETE takes a second process, and node itself deletes a
// directory another process merely has as its working directory. The raw-code
// table's unit tests in oam_core cover it.

// --- child_process -------------------------------------------------------------
const text = path.join(root, "not-a-program.txt");
fs.writeFileSync(text, "hello");

console.log("spawnSync(dir)", shape(spawnSync(dir).error));
console.log("spawnSync(text file)", shape(spawnSync(text).error));

await new Promise((resolve) => {
  const child = spawn(dir);
  child.once("error", (err) => {
    console.log("spawn(dir) emitted", shape(err));
    resolve();
  });
});

// node THROWS the codes it does not emit, synchronously, from spawn() itself.
try {
  const child = spawn(text);
  child.once("error", (err) => console.log("spawn(text file) emitted", shape(err)));
  console.log("spawn(text file) returned");
} catch (err) {
  console.log("spawn(text file) threw", shape(err));
}

fs.chmodSync(readOnly, 0o666);
fs.rmSync(root, { recursive: true, force: true });
