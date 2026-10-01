// The descriptor argument of the callback fs calls and of the fs streams.
//
// Like the *Sync forms (conformance/cases/246), these refuse a descriptor
// that is not an int32 in [0, 2**31-1] with ERR_OUT_OF_RANGE /
// ERR_INVALID_ARG_TYPE, THROWN at the call -- never EBADF, and never through
// the callback. oam delivered EBADF to the callback for all of them.
//
// Where the check happens decides both the wording and what wins over what:
// - fs.read, write, readv, writev and the createReadStream / createWriteStream
//   `fd` option use node's JS getValidatedFd, FIRST, before every other
//   argument and the callback. It words a failure the JS way: -0.5 is "an
//   integer", 2**53 is grouped (9_007_199_254_740_992), a bigint keeps its n.
// - close, fstat, fsync, fdatasync, ftruncate, fchmod, fchown and futimes
//   check in C++, after their other arguments and after the callback, which
//   must be a function (close's may be left out). Their wording is the C++
//   one.
// - readFile(fd) looks at the descriptor a tick later, so readFile(-1, cb)
//   neither throws at the call nor calls back: it is an uncaught exception.
// A missing callback is node's ERR_INVALID_ARG_TYPE for "cb" (oam's was a
// bare TypeError with no code), and writeFile / readFile validate their
// options argument as the callback when none follows it (`callback ||=
// options`). The path forms take the callback from its position, not by
// popping the last argument: a missing one is "Received undefined", not the
// path or uid before it, and an argument after it is ignored (oam threw on
// it). opendir calls it "callback"; symlink alone takes its last argument.
// fs.read takes node's overloads by argument count and checks its buffer,
// offset and length at the call, the offset even for a read of length 0;
// FileHandle.read shares those checks with its own overloads.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const B = Buffer.alloc(4);
function shape(err) {
  const out = { name: err.constructor.name, code: err.code, message: err.message };
  out.keys = Object.keys(err);
  return JSON.stringify(out);
}
const events = [];
function call(label, fn) {
  let returned = false;
  try {
    fn((err) => events.push(`${label} -> callback ${returned ? "" : "(synchronous!) "}${err ? shape(err) : "null"}`));
    returned = true;
    console.log(label, "returned");
  } catch (err) {
    returned = true;
    console.log(label, "threw", shape(err));
  }
}

const values = [
  ["-1", -1],
  ["2**31", 2 ** 31],
  ["2**53", 2 ** 53],
  ["1e21", 1e21],
  ["1.5", 1.5],
  ["-0.5", -0.5],
  ["NaN", NaN],
  ["-Infinity", -Infinity],
  ["'3'", "3"],
  ["long", "x".repeat(40)],
  ["long quoted", "it's".repeat(10)],
  // The quote is judged on the CUT string: past the cut it is single-quoted.
  ["quote after the cut", "a".repeat(30) + "'"],
  ["quote just after the cut", "a".repeat(25) + "'bcdefghijk"],
  ["quote just before the cut", "a".repeat(24) + "'bcdefghijk"],
  ["29 with a quote", "a".repeat(28) + "'"],
  ["28 with a quote", "a".repeat(27) + "'"],
  // The C++ wording counts and cuts UTF-8 bytes, and shows a lone surrogate
  // and a character the cut splits as U+FFFD.
  ["15 x 2-byte", "é".repeat(15)],
  ["cut inside a 3-byte", "a".repeat(24) + "€€€"],
  ["cut inside a 4-byte", "a".repeat(22) + "\u{1F600}\u{1F600}"],
  ["cut after a 4-byte", "a".repeat(21) + "\u{1F600}\u{1F600}"],
  ["lone surrogate", "\ud800x"],
  ["lone surrogate cut", "a".repeat(24) + "\udc00bbbbb"],
  ["2-byte, quote after the cut", "a".repeat(23) + "é'bbbbb"],
  ["undefined", undefined],
  ["null proto", Object.create(null)],
  ["1n", 1n],
  ["Symbol", Symbol("s")],
  ["function", function named() {}],
];
// The JS check and the C++ check side by side, for every value.
for (const [label, value] of values) {
  call(`read(${label})`, (cb) => fs.read(value, B, 0, 1, null, cb));
  call(`close(${label})`, (cb) => fs.close(value, cb));
}

const forms = {
  close: (fd, cb) => fs.close(fd, cb),
  fstat: (fd, cb) => fs.fstat(fd, cb),
  read: (fd, cb) => fs.read(fd, B, 0, 1, null, cb),
  write: (fd, cb) => fs.write(fd, B, 0, 1, null, cb),
  "write(string)": (fd, cb) => fs.write(fd, "x", cb),
  readv: (fd, cb) => fs.readv(fd, [B], cb),
  writev: (fd, cb) => fs.writev(fd, [B], cb),
  fsync: (fd, cb) => fs.fsync(fd, cb),
  fdatasync: (fd, cb) => fs.fdatasync(fd, cb),
  ftruncate: (fd, cb) => fs.ftruncate(fd, 0, cb),
  "ftruncate(no len)": (fd, cb) => fs.ftruncate(fd, cb),
  fchmod: (fd, cb) => fs.fchmod(fd, 0o644, cb),
  fchown: (fd, cb) => fs.fchown(fd, 0, 0, cb),
  futimes: (fd, cb) => fs.futimes(fd, 0, 0, cb),
  writeFile: (fd, cb) => fs.writeFile(fd, "x", cb),
  appendFile: (fd, cb) => fs.appendFile(fd, "x", cb),
};
for (const [name, fn] of Object.entries(forms)) {
  call(`${name}(-1)`, (cb) => fn(-1, cb));
  call(`${name}(1.5)`, (cb) => fn(1.5, cb));
  // The descriptor against the callback: which is refused first.
  call(`${name}(-1, no callback)`, () => fn(-1, undefined));
  call(`${name}(-1, callback 5)`, () => fn(-1, 5));
}

// Other arguments against the descriptor.
call("read(-1, 'x', ...)", (cb) => fs.read(-1, "x", 0, 1, null, cb));
call("read(-1, B, 0, 99, ...)", (cb) => fs.read(-1, B, 0, 99, null, cb));
call("read(-1, B, 0, 0, ...)", (cb) => fs.read(-1, B, 0, 0, null, cb));
call("readv(-1, [])", (cb) => fs.readv(-1, [], cb));
call("writev(-1, [])", (cb) => fs.writev(-1, [], cb));
call("readv(0, 'x', no callback)", () => fs.readv(0, "x"));
call("writev(0, 'x', no callback)", () => fs.writev(0, "x"));
call("futimes(-1, {}, 0)", (cb) => fs.futimes(-1, {}, 0, cb));
call("fstat(-1, { bigint: true })", (cb) => fs.fstat(-1, { bigint: true }, cb));
call("writeFile(-1, '')", (cb) => fs.writeFile(-1, "", cb));
const warn = process.emitWarning;
process.emitWarning = () => {}; // DEP0081; stderr is not compared anyway
call("truncate(-1)", (cb) => fs.truncate(-1, cb));
call("truncate(-1, 0)", (cb) => fs.truncate(-1, 0, cb));
process.emitWarning = warn;

// The streams' fd option.
for (const [label, fd] of [["-1", -1], ["1.5", 1.5], ["2**53", 2 ** 53], ["'3'", "3"], ["{}", {}]]) {
  call(`createReadStream({ fd: ${label} })`, () => fs.createReadStream(null, { fd }));
  call(`createWriteStream({ fd: ${label} })`, () => fs.createWriteStream(null, { fd }));
}

// The fd callback forms carry node's names and arities.
for (const name of ["fsync", "fdatasync", "ftruncate", "fchmod", "fchown", "futimes"]) {
  console.log(`fs.${name}`, fs[name].name, fs[name].length);
}

// Where the path forms find their callback. A missing one is "Received
// undefined" -- never the path, uid or time before it -- an optional slot may
// hold it, and arguments after it are ignored. opendir names it "callback";
// symlink takes its last argument; readFile and realpath.native use
// `callback || options`.
const P = "no-such-dir-247/x";
const noCallback = {
  "stat(p)": () => fs.stat(P),
  "stat(p, {})": () => fs.stat(P, {}),
  "stat(p, {}, 5)": () => fs.stat(P, {}, 5),
  "lstat(p, undefined)": () => fs.lstat(P, undefined),
  "mkdir(p)": () => fs.mkdir(P),
  "unlink(p)": () => fs.unlink(P),
  "rename(p, q)": () => fs.rename(P, P),
  "copyFile(p, q, 0)": () => fs.copyFile(P, P, 0),
  "access(p, 0)": () => fs.access(P, 0),
  "chmod(p, 0o644)": () => fs.chmod(P, 0o644),
  "chown(p, 0, 0)": () => fs.chown(P, 0, 0),
  "lchown(p, 0, 0)": () => fs.lchown(P, 0, 0),
  "utimes(p, 1, 1)": () => fs.utimes(P, 1, 1),
  "lutimes(p, 1, 1)": () => fs.lutimes(P, 1, 1),
  "opendir('.')": () => fs.opendir("."),
  "opendir('.', {}, 5)": () => fs.opendir(".", {}, 5),
  "symlink(p, q)": () => fs.symlink(P, "q"),
  "symlink(p, q, 'file')": () => fs.symlink(P, "q", "file"),
  "realpath(p)": () => fs.realpath(P),
  "realpath.native(p, {})": () => fs.realpath.native(P, {}),
  "readFile(p, {})": () => fs.readFile(P, {}),
  "truncate(p, 0)": () => fs.truncate(P, 0),
  "cp(p, q)": () => fs.cp(P, P),
};
for (const [label, fn] of Object.entries(noCallback)) call(label, () => fn());
const extra = {
  stat: (cb) => fs.stat(".", cb, "extra"),
  "stat({})": (cb) => fs.stat(".", {}, cb, "extra"),
  lstat: (cb) => fs.lstat(".", cb, "extra"),
  access: (cb) => fs.access(".", cb, "extra"),
  readdir: (cb) => fs.readdir(".", () => cb(null), "extra"),
  realpath: (cb) => fs.realpath(".", () => cb(null), "extra"),
  opendir: (cb) => fs.opendir(".", (err, dir) => { dir.closeSync(); cb(err); }, "extra"),
  symlink: (cb) => fs.symlink(P, "q", cb, "extra"),
};
// One at a time, after the rest, so the order of their callbacks is fixed.
async function extraArguments() {
  for (const [name, fn] of Object.entries(extra)) {
    await new Promise((resolve) => {
      try {
        fn((err) => {
          console.log(`${name}(callback, 'extra') -> callback ${err ? shape(err) : "null"}`);
          resolve();
        });
      } catch (err) {
        console.log(`${name}(callback, 'extra') threw`, shape(err));
        resolve();
      }
    });
  }
}

// fs.read's overloads go by argument count, and its offset and length are
// checked at the call -- the offset even for a read of length 0, which calls
// back 0 without reading. Each on a fresh descriptor, one at a time.
const readForms = {
  "(fd, B, cb)": (fd, b, cb) => fs.read(fd, b, cb),
  "(fd, cb)": (fd, b, cb) => fs.read(fd, (err, n, buf) => { buf.copy(b, 0, 0, 4); cb(err, n); }),
  "(fd, { buffer, position: 2 }, cb)": (fd, b, cb) => fs.read(fd, { buffer: b, position: 2 }, cb),
  "(fd, { buffer, offset: 1 }, cb)": (fd, b, cb) => fs.read(fd, { buffer: b, offset: 1 }, cb),
  "(fd, { length: 3 }, cb)": (fd, b, cb) => fs.read(fd, { length: 3 }, (err, n, buf) => { cb(err, n + "/" + buf.length); }),
  "(fd, null, cb)": (fd, b, cb) => fs.read(fd, null, (err, n, buf) => { cb(err, n + "/" + buf.length); }),
  "(fd, B, { position: 3 }, cb)": (fd, b, cb) => fs.read(fd, b, { position: 3 }, cb),
  "(fd, B, { offset: 1, length: 2 }, cb)": (fd, b, cb) => fs.read(fd, b, { offset: 1, length: 2 }, cb),
  "(fd, B, null, cb)": (fd, b, cb) => fs.read(fd, b, null, cb),
  "(fd, B, 0, cb)": (fd, b, cb) => fs.read(fd, b, 0, cb),
  "(fd, B, 0, 2, cb)": (fd, b, cb) => fs.read(fd, b, 0, 2, cb),
  "(fd, B, null, 2, 0, cb)": (fd, b, cb) => fs.read(fd, b, null, 2, 0, cb),
  "(fd, B, undefined, undefined, 0, cb)": (fd, b, cb) => fs.read(fd, b, undefined, undefined, 0, cb),
  "(fd, B, -5, 0, 0, cb)": (fd, b, cb) => fs.read(fd, b, -5, 0, 0, cb),
  "(fd, B, 'x', 1, 0, cb)": (fd, b, cb) => fs.read(fd, b, "x", 1, 0, cb),
  "(fd, B, 1.5, 1, 0, cb)": (fd, b, cb) => fs.read(fd, b, 1.5, 1, 0, cb),
  "(fd, B, 2**53, 0, 0, cb)": (fd, b, cb) => fs.read(fd, b, 2 ** 53, 0, 0, cb),
  "(fd, B, 5, 0, 0, cb)": (fd, b, cb) => fs.read(fd, b, 5, 0, 0, cb),
  "(fd, B, 5, 1, 0, cb)": (fd, b, cb) => fs.read(fd, b, 5, 1, 0, cb),
  "(fd, B, 0, -1, 0, cb)": (fd, b, cb) => fs.read(fd, b, 0, -1, 0, cb),
  "(fd, B, { offset: -1 }, cb)": (fd, b, cb) => fs.read(fd, b, { offset: -1 }, cb),
  "(fd, { buffer, offset: 'x' }, cb)": (fd, b, cb) => fs.read(fd, { buffer: b, offset: "x" }, cb),
  "(fd, { buffer: 'x' }, cb)": (fd, b, cb) => fs.read(fd, { buffer: "x" }, cb),
  "(fd, empty Buffer, 0, 1, 0, cb)": (fd, b, cb) => fs.read(fd, Buffer.alloc(0), 0, 1, 0, cb),
  "(fd, B, 0, 1, 0) no callback": (fd, b) => fs.read(fd, b, 0, 1, 0),
};
async function readOverloads() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-read-"));
  const file = path.join(dir, "f.txt");
  fs.writeFileSync(file, "abcdefgh");
  for (const [label, fn] of Object.entries(readForms)) {
    const fd = fs.openSync(file, "r");
    const b = Buffer.alloc(4);
    await new Promise((resolve) => {
      let returned = false;
      try {
        fn(fd, b, (err, n) => {
          const how = returned ? "" : "(synchronous!) ";
          console.log(`read${label} -> callback ${how}${err ? shape(err) : `null ${n} ${JSON.stringify(b.toString())}`}`);
          resolve();
        });
        returned = true;
      } catch (err) {
        console.log(`read${label} threw`, shape(err));
        resolve();
      }
    });
    fs.closeSync(fd);
  }
  // FileHandle.read shares the offset and range checks, with its own
  // overloads and no `| 0` on the length.
  const fhForms = {
    "()": () => [],
    "(B, 1)": (b) => [b, 1],
    "(B, undefined, undefined, 2)": (b) => [b, undefined, undefined, 2],
    "(B, { offset: 1, length: 2, position: 4 })": (b) => [b, { offset: 1, length: 2, position: 4 }],
    "({ buffer: B, position: 2 })": (b) => [{ buffer: b, position: 2 }],
    "({ length: 3 })": () => [{ length: 3 }],
    "(5)": () => [5],
    "({ buffer: 'x' })": () => [{ buffer: "x" }],
    "(B, -5, 0)": (b) => [b, -5, 0],
    "(B, 'x', 1)": (b) => [b, "x", 1],
    "(B, 0, -1)": (b) => [b, 0, -1],
    "(B, 5)": (b) => [b, 5],
    "(empty Buffer, 0, 1)": () => [Buffer.alloc(0), 0, 1],
  };
  for (const [label, args] of Object.entries(fhForms)) {
    const fh = await fs.promises.open(file, "r");
    const b = Buffer.alloc(4);
    try {
      const { bytesRead, buffer } = await fh.read(...args(b));
      const head = Buffer.from(buffer.buffer, buffer.byteOffset, Math.min(buffer.byteLength, 8));
      console.log(`fh.read${label} ->`, bytesRead, JSON.stringify(head.toString()), buffer.byteLength, buffer === b);
    } catch (err) {
      console.log(`fh.read${label} rejected`, shape(err));
    }
    await fh.close();
  }
  fs.rmSync(dir, { recursive: true, force: true });
}

// readFile(-1, cb): thrown a tick later, as an uncaught exception.
process.on("uncaughtException", (err) => events.push(`uncaughtException ${shape(err)}`));
call("readFile(-1)", (cb) => fs.readFile(-1, cb));
setTimeout(() => {
  for (const line of events) console.log(line);
  extraArguments().then(readOverloads);
}, 50);
