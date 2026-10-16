// node:tty's ReadStream and WriteStream are node's classes (#391): callable
// without `new`; ERR_INVALID_FD (a RangeError) for a descriptor that is no
// int32 >= 0; ERR_TTY_INIT_FAILED -- a SystemError raised as its code's
// NodeError class, context from uv_tty_init -- for one that is no terminal;
// stream.Duplex underneath and `instanceof net.Socket`; and the prototype
// surface node's lib/tty.js defines, with getColorDepth / hasColors ported
// from internal/tty.js and cursorTo / clearLine / ... delegating to
// readline. tty.isatty takes only an integer descriptor.
//
// The harness runs a case with stdin on the null device (NUL / /dev/null)
// and stdout / stderr on pipes, so nothing here has a terminal; the terminal
// side (process.stdout an instance, columns, raw mode, a console opened by
// name) is the ConPTY test's.
//
// Platform rows: uv_tty_init answers EBADF on Windows for anything that is
// not a console, EINVAL elsewhere for a file, the null device or an unopened
// descriptor, and on POSIX takes a pipe -- so the stdio descriptors are only
// tried on Windows, and the errno/message of a refusal print on Windows only.
// Windows also gets a named pipe whose name is an msys/cygwin pty's -- what
// Git Bash's mintty hands a program -- which libuv calls a pipe though Rust's
// IsTerminal calls it a terminal: not a TTY, and refused (#391).
//
// Regression guard: oam's ReadStream / WriteStream were empty stub classes
// that constructed over any descriptor.
import tty, { ReadStream as NamedRead, WriteStream as NamedWrite, isatty as namedIsatty } from "node:tty";
import net from "node:net";
import stream from "node:stream";
import { EventEmitter } from "node:events";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const isWin = process.platform === "win32";
const { ReadStream, WriteStream } = tty;

const shape = (e) => {
  const row = [e.constructor.name, e.name, e.code, JSON.stringify(Object.keys(e)), e instanceof Error];
  if (e.code === "ERR_TTY_INIT_FAILED") {
    row.push(e.syscall, e.info.syscall);
    if (isWin) row.push(e.errno, JSON.stringify(e.info), e.message, String(e));
    else row.push("(errno/message: windows-only row)");
  } else {
    row.push(e.message, String(e));
  }
  return row.join(" | ");
};
const attempt = (label, f) => {
  try {
    const r = f();
    console.log(label, "constructed", r.constructor.name);
  } catch (e) {
    console.log(label, shape(e));
  }
};

console.log("--- constructors");
for (const [name, C] of [["ReadStream", ReadStream], ["WriteStream", WriteStream]]) {
  for (const fd of [9999, -1, 1.5, "1", undefined, null, 2 ** 31, {}, NaN]) {
    attempt(`new ${name}(${typeof fd === "object" && fd ? "{}" : String(fd)})`, () => new C(fd));
  }
  attempt(`${name}(9999) without new`, () => C(9999));
  if (isWin) {
    for (const [fd, what] of [[0, "NUL"], [1, "a pipe"], [2, "a pipe"]]) {
      attempt(`new ${name}(${fd}) on the harness's stdio (${what})`, () => new C(fd));
    }
  } else {
    console.log(`new ${name}(0..2): windows-only row`);
  }
}
const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-418-")));
try {
  const fd = fs.openSync(path.join(dir, "file"), "w");
  try {
    attempt("new WriteStream(<file>)", () => new WriteStream(fd));
    attempt("new ReadStream(<file>)", () => new ReadStream(fd));
    console.log("isatty(<file>)", tty.isatty(fd));
  } finally {
    fs.closeSync(fd);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
{
  const fd = fs.openSync(isWin ? "\\\\.\\NUL" : "/dev/null", "r+");
  try {
    attempt("new WriteStream(<null device>)", () => new WriteStream(fd));
    attempt("new ReadStream(<null device>)", () => new ReadStream(fd));
    console.log("isatty(<null device>)", tty.isatty(fd));
  } finally {
    fs.closeSync(fd);
  }
}
if (isWin) {
  // The pid keeps two runs apart; the name itself is never printed.
  const name = `\\\\.\\pipe\\msys-${process.pid.toString(16)}0418-pty0-to-master`;
  const server = net.createServer((socket) => socket.on("error", () => {}));
  await new Promise((resolve) => server.listen(name, resolve));
  const fd = fs.openSync(name, "r+");
  try {
    attempt("new WriteStream(<msys pty pipe>)", () => new WriteStream(fd));
    attempt("new ReadStream(<msys pty pipe>)", () => new ReadStream(fd));
    console.log("isatty(<msys pty pipe>)", tty.isatty(fd));
  } finally {
    fs.closeSync(fd);
    await new Promise((resolve) => server.close(resolve));
  }
} else {
  console.log("<msys pty pipe>: windows-only row");
}

console.log("--- isatty");
for (const fd of [9999, -1, 1.5, "1", "0", 2 ** 31, null, undefined, 0, 1, 2]) {
  console.log(`isatty(${String(fd)})`, tty.isatty(fd));
}

console.log("--- class shape");
console.log(Object.keys(tty), NamedRead === ReadStream, NamedWrite === WriteStream, namedIsatty === tty.isatty);
console.log(typeof ReadStream, ReadStream.name, ReadStream.length, WriteStream.name, WriteStream.length);
for (const C of [ReadStream, WriteStream]) {
  console.log(C.name, Object.getOwnPropertyNames(C.prototype));
  for (const key of Object.getOwnPropertyNames(C.prototype)) {
    const d = Object.getOwnPropertyDescriptor(C.prototype, key);
    if (typeof d.value === "function") {
      console.log(" ", key, JSON.stringify(d.value.name), d.value.length, d.writable, d.enumerable, d.configurable);
    } else {
      console.log(" ", key, JSON.stringify(d));
    }
  }
}

console.log("--- instances without a terminal");
for (const C of [ReadStream, WriteStream]) {
  const o = Object.create(C.prototype);
  console.log(
    C.name,
    o instanceof C,
    o instanceof net.Socket,
    o instanceof stream.Duplex,
    o instanceof stream.Readable,
    o instanceof stream.Writable,
    o instanceof stream.Stream,
    o instanceof EventEmitter,
  );
}
const w = Object.create(WriteStream.prototype);
console.log("getWindowSize", w.getWindowSize(), w.isTTY);
w.columns = 80;
w.rows = 24;
console.log("getWindowSize after set", w.getWindowSize());
console.log("stdio", process.stdout instanceof WriteStream, process.stderr instanceof WriteStream, process.stdin instanceof ReadStream);

console.log("--- getColorDepth / hasColors");
const envs = [
  {},
  { FORCE_COLOR: "" }, { FORCE_COLOR: "1" }, { FORCE_COLOR: "true" }, { FORCE_COLOR: "2" }, { FORCE_COLOR: "3" }, { FORCE_COLOR: "0" }, { FORCE_COLOR: "x" },
  { NO_COLOR: "" }, { NODE_DISABLE_COLORS: "1" }, { TERM: "dumb" }, { TERM: "dumb", FORCE_COLOR: "2" },
  { TMUX: "1" }, { TF_BUILD: "1", AGENT_NAME: "a" }, { CI: "1" }, { CI: "1", GITHUB_ACTIONS: "1" }, { CI: "1", TRAVIS: "1" }, { CI: "1", CI_NAME: "codeship" },
  { TEAMCITY_VERSION: "9.1.0" }, { TEAMCITY_VERSION: "8.0" },
  { TERM_PROGRAM: "iTerm.app" }, { TERM_PROGRAM: "iTerm.app", TERM_PROGRAM_VERSION: "3.4" }, { TERM_PROGRAM: "Apple_Terminal" }, { TERM_PROGRAM: "HyperTerm" },
  { COLORTERM: "truecolor" }, { COLORTERM: "24bit" }, { COLORTERM: "yes" },
  { TERM: "xterm-truecolor" }, { TERM: "xterm-256color" }, { TERM: "xterm" }, { TERM: "Konsole" }, { TERM: "mosh" }, { TERM: "screen.xterm" }, { TERM: "con132x25" }, { TERM: "unknown" },
];
for (const env of envs) {
  console.log(JSON.stringify(env), w.getColorDepth(env), w.hasColors(env), w.hasColors(256, env), w.hasColors(2 ** 24, env));
}
for (const args of [[1], [1.5], ["16"], [2, null], [-3]]) {
  try {
    console.log("hasColors", JSON.stringify(args), w.hasColors(...args, { TERM: "dumb" }));
  } catch (e) {
    console.log("hasColors", JSON.stringify(args), e.name, e.code, e.message);
  }
}

console.log("--- readline delegation");
const out = [];
const fake = { write(data, cb) { out.push(JSON.stringify(data)); if (cb) cb(); return true; } };
console.log(
  WriteStream.prototype.cursorTo.call(fake, 3),
  WriteStream.prototype.cursorTo.call(fake, 3, 4),
  WriteStream.prototype.moveCursor.call(fake, 1, -1),
  WriteStream.prototype.moveCursor.call(fake, 0, 0),
  WriteStream.prototype.clearLine.call(fake, -1),
  WriteStream.prototype.clearLine.call(fake, 0),
  WriteStream.prototype.clearLine.call(fake, 1),
  WriteStream.prototype.clearScreenDown.call(fake),
);
console.log(out.join(" "));
for (const [label, f] of [
  ["cursorTo NaN", () => WriteStream.prototype.cursorTo.call(fake, NaN)],
  ["cursorTo row only", () => WriteStream.prototype.cursorTo.call(fake, undefined, 3)],
  ["cursorTo neither", () => WriteStream.prototype.cursorTo.call(fake)],
  ["clearLine bad callback", () => WriteStream.prototype.clearLine.call(fake, 0, 1)],
  ["moveCursor bad callback", () => WriteStream.prototype.moveCursor.call(fake, 1, 1, "x")],
]) {
  try {
    console.log(label, f());
  } catch (e) {
    console.log(label, e.name, e.code, e.message);
  }
}

// FORCE_COLOR next to NO_COLOR / NODE_DISABLE_COLORS warns, once per process.
process.on("warning", (warning) => console.log("warning", warning.name, warning.message));
console.log("depth", w.getColorDepth({ FORCE_COLOR: "1", NO_COLOR: "1", NODE_DISABLE_COLORS: "1" }));
console.log("depth", w.getColorDepth({ FORCE_COLOR: "3", NO_COLOR: "1" }));
await new Promise((resolve) => setTimeout(resolve, 10));
console.log("done");
