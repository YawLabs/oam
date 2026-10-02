// Descriptors 0, 1 and 2 through the fd-taking fs calls.
//
// node's fd calls operate on the process's own stdin, stdout and stderr:
// fstatSync(0) stats whatever stdin is, readSync(0, ...) reads it, and
// readFileSync(0) is the usual way to slurp piped input. oam's descriptor
// table held only what fs had opened (and fds a parent passed in), so every
// one of these was EBADF; only writeSync(1|2) worked, routed to the stdout /
// stderr streams.
//
// The harness runs cases with stdin on the null device and stdout / stderr on
// pipes. Only fields that are the same for two processes are printed (an ino
// or a pipe's dev would differ). On Windows libuv gives a pipe and NUL fixed
// stat shapes (S_IFIFO, and S_IFCHR | 0o666 with rdev FILE_DEVICE_NULL << 16)
// and node's Stats never reports a FIFO there; closing 0-2 is a no-op that
// leaves them working. On unix they are really closed.
import fs from "node:fs";

function shape(e) {
  return `${e.constructor.name} ${e.code} ${e.syscall} ${JSON.stringify(e.message)}`;
}
function t(label, fn) {
  try {
    console.log(label, "->", JSON.stringify(fn()));
  } catch (e) {
    console.log(label, "!!", shape(e));
  }
}
const kinds = (s) =>
  ["isFile", "isDirectory", "isCharacterDevice", "isFIFO", "isSocket", "isBlockDevice", "isSymbolicLink"]
    .filter((k) => s[k]())
    .join(",") || "none";
const stable = (s) => ({ mode: s.mode, nlink: s.nlink, uid: s.uid, gid: s.gid, size: s.size, kinds: kinds(s) });

t("fstatSync(0)", () => stable(fs.fstatSync(0)));
t("fstatSync(-0)", () => stable(fs.fstatSync(-0)));
t("fstatSync(1)", () => {
  const s = fs.fstatSync(1);
  return { mode: s.mode, nlink: s.nlink, rdev: s.rdev, kinds: kinds(s) };
});
t("fstatSync(2).kinds", () => kinds(fs.fstatSync(2)));
t("fstatSync(0) is a Stats", () => fs.fstatSync(0) instanceof fs.Stats);
t("readSync(0, b)", () => fs.readSync(0, Buffer.alloc(4), 0, 4, null));
t("readSync(0, b, 0, 4, 0)", () => fs.readSync(0, Buffer.alloc(4), 0, 4, 0));
t("readFileSync(0)", () => fs.readFileSync(0).length);
t("readFileSync(0, 'utf8')", () => fs.readFileSync(0, "utf8"));
t("writeSync(0, 'x')", () => fs.writeSync(0, "x"));
t("writeSync(1, '')", () => fs.writeSync(1, ""));
await new Promise((resolve) =>
  fs.fstat(0, (err, s) => {
    console.log("fstat(0, cb)", err ? shape(err) : JSON.stringify(stable(s)));
    resolve();
  }),
);
await new Promise((resolve) =>
  fs.read(0, Buffer.alloc(4), 0, 4, null, (err, n) => {
    console.log("read(0, cb)", err ? shape(err) : n);
    resolve();
  }),
);
await new Promise((resolve) =>
  fs.readFile(0, (err, data) => {
    console.log("readFile(0, cb)", err ? shape(err) : data.length);
    resolve();
  }),
);
await new Promise((resolve) =>
  fs.readFile(0, "utf8", (err, data) => {
    console.log("readFile(0, 'utf8', cb)", err ? shape(err) : JSON.stringify(data));
    resolve();
  }),
);

// Closing stdin: a no-op on Windows (libuv's fs__close leaves 0-2 open), a
// real close on unix, after which it is EBADF.
const win = process.platform === "win32";
t("closeSync(0)", () => fs.closeSync(0));
t("fstatSync(0) after closeSync(0)", () => (win ? kinds(fs.fstatSync(0)) : fs.fstatSync(0).mode));
t("closeSync(0) again", () => fs.closeSync(0));
console.log("done");
