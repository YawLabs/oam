// fs.ReadStream and fs.WriteStream open, read, write, writev, fsync and
// close through `options.fs`, as node's do (#356): each method is called
// with node's arguments (the stream's path, flags and mode as given; a
// buffer of the stream's own, its offset, length and position), a short
// write is followed by one for the rest, and the `options.fs` methods a
// stream needs are checked to be functions, in node's order.
//
// Regression guard: oam's streams opened, read, wrote and closed natively,
// so `options.fs` was never called and never checked.
//
// Each row records the calls the stream made (the descriptor shown as FD
// when it is the one `open` gave back) and what reached the file.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import url from "node:url";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-530-")));
process.chdir(root);
const show = (v) =>
  typeof v === "function" ? "fn"
  : Buffer.isBuffer(v) ? `buf(${v.length})`
  : Array.isArray(v) ? `[${v.map(show).join(",")}]`
  : JSON.stringify(v) ?? "undefined";
function recorder(log) {
  const fds = new Set();
  const tag = (fd) => (fds.has(fd) ? "FD" : typeof fd);
  return {
    open(p, flags, mode, cb) {
      log.push(`open ${show(p)} ${show(flags)} ${show(mode)} ${show(cb)} argc=${arguments.length}`);
      fs.open(p, flags, mode, (e, fd) => { if (!e) fds.add(fd); cb(e, fd); });
    },
    read(fd, buf, off, len, pos, cb) {
      log.push(`read ${tag(fd)} ${show(buf)} ${off} ${len} ${show(pos)} ${show(cb)} argc=${arguments.length}`);
      fs.read(fd, buf, off, len, pos, cb);
    },
    write(fd, buf, off, len, pos, cb) {
      log.push(`write ${tag(fd)} ${show(buf)} ${off} ${len} ${show(pos)} ${show(cb)} argc=${arguments.length}`);
      fs.write(fd, buf, off, len, pos, cb);
    },
    writev(fd, bufs, pos, cb) {
      log.push(`writev ${tag(fd)} ${show(bufs)} ${show(pos)} ${show(cb)} argc=${arguments.length}`);
      fs.writev(fd, bufs, pos, cb);
    },
    fsync(fd, cb) {
      log.push(`fsync ${tag(fd)} argc=${arguments.length}`);
      fs.fsync(fd, cb);
    },
    close(fd, cb) {
      log.push(`close ${tag(fd)} ${show(cb)} argc=${arguments.length}`);
      fs.close(fd, cb);
    },
  };
}
const closed = (s) => new Promise((resolve) => s.on("close", resolve));
const row = (label, ...rest) => console.log(label, JSON.stringify(rest));

// A write stream: one write, then a corked batch (writev).
{
  const log = [];
  const ws = fs.createWriteStream("a.txt", { mode: 0o644, flags: "w+", fs: recorder(log) });
  // The batch waits for the first write to land: when a write's callback
  // comes (a later turn in node) is not what this case is about.
  await new Promise((resolve) => ws.write("one", resolve));
  ws.cork(); ws.write("two"); ws.write("three"); ws.uncork();
  ws.end("!");
  await closed(ws);
  row("write stream", log, fs.readFileSync("a.txt", "utf8"), ws.bytesWritten);
}
// `start` gives every write its position; `flush` syncs before the close.
{
  const log = [];
  const ws = fs.createWriteStream("a.txt", { flags: "r+", start: 2, flush: true, fs: recorder(log) });
  ws.end("XY");
  await closed(ws);
  row("start + flush", log, fs.readFileSync("a.txt", "utf8"));
}
// No writev in options.fs: a corked batch goes out one write at a time.
{
  const log = [];
  const ops = recorder(log);
  delete ops.writev;
  const ws = fs.createWriteStream("b.txt", { fs: ops });
  await new Promise((resolve) => ws.once("ready", resolve));
  ws.cork(); ws.write("a"); ws.write("b"); ws.uncork();
  ws.end();
  await closed(ws);
  row("write only", log, fs.readFileSync("b.txt", "utf8"), typeof ws._writev);
}
// A read stream with start, end and a small highWaterMark, and one without.
fs.writeFileSync("r.txt", "0123456789");
for (const opts of [{ start: 1, end: 6, highWaterMark: 4 }, { highWaterMark: 8 }]) {
  const log = [];
  const rs = fs.createReadStream("r.txt", { ...opts, fs: recorder(log) });
  const got = [];
  rs.on("data", (c) => got.push(c.toString()));
  await closed(rs);
  row("read stream", opts, log, got, rs.bytesRead);
}
// A descriptor handed over: no open, and the close through options.fs.
{
  const log = [];
  const fd = fs.openSync("r.txt", "r");
  const rs = fs.createReadStream(null, { fd, fs: recorder(log) });
  rs.resume();
  await closed(rs);
  row("given fd", log.map((l) => l.replace(/ number /, " given ")));
}
// options.fs that answers synchronously, and short writes.
{
  const log = [];
  const ws = fs.createWriteStream("unused", {
    fs: {
      open(p, f, m, cb) { log.push("open"); cb(null, 777); },
      write(fd, b, o, l, p, cb) { log.push(`write ${fd} ${b} ${o} ${l} ${p}`); setImmediate(cb, null, Math.min(2, l), b); },
      close(fd, cb) { log.push(`close ${fd} fd=${ws.fd}`); cb(); },
    },
  });
  ws.on("open", (fd) => log.push(`'open' ${fd}`));
  ws.on("ready", () => log.push("'ready'"));
  ws.on("finish", () => log.push("'finish'"));
  ws.end("abcde");
  await closed(ws);
  row("short writes", log, ws.bytesWritten, ws.fd, fs.existsSync("unused"));
}
// The failures options.fs reports: open, read (with the close's error
// winning), a sync that fails, writes of nothing, EAGAIN retried.
{
  const fail = (code) => Object.assign(new Error(code), { code });
  const base = { open(p, f, m, cb) { cb(null, 9); }, read() {}, write() {}, close(fd, cb) { cb(); } };
  const outcome = async (s, start) => {
    const log = [];
    s.on("error", (e) => log.push(`error ${e.name} ${e.code} ${e.message}`));
    start(s, log);
    await closed(s);
    return log;
  };
  row("open fails", await outcome(fs.createWriteStream("x", { fs: { ...base, open(p, f, m, cb) { cb(fail("EOPEN")); } } }), () => {}));
  row("read fails", await outcome(fs.createReadStream("x", {
    fs: { ...base, read(fd, b, o, l, p, cb) { cb(fail("EREAD")); }, close(fd, cb) { cb(fail("ECLOSE")); } },
  }), (s) => s.resume()));
  row("fsync fails", await outcome(fs.createWriteStream("x", {
    flush: true,
    fs: { ...base, write(fd, b, o, l, p, cb) { cb(null, l, b); }, fsync(fd, cb) { cb(fail("EFSYNC")); } },
  }), (s) => s.end("z")));
  row("writes of nothing", await outcome(fs.createWriteStream("x", {
    fs: { ...base, write(fd, b, o, l, p, cb) { setImmediate(cb, null, 0, b); } },
  }), (s) => s.end("abc")));
  let n = 0;
  const again = fs.createWriteStream("x", {
    fs: { ...base, write(fd, b, o, l, p, cb) { n++; if (n === 1) cb(fail("EAGAIN"), 0, b); else cb(null, l, b); } },
  });
  row("EAGAIN", await outcome(again, (s) => s.end("abc")), n, again.bytesWritten);
  row("open throws", await outcome(fs.createWriteStream("x", { fs: { ...base, open() { throw fail("ETHROWN"); } } }), () => {}));
}
// Destroyed under a write: the write's callback and 'error' are
// ERR_STREAM_DESTROYED, for write and for writev.
for (const method of ["write", "writev"]) {
  const log = [];
  let land;
  const ops = {
    open(p, f, m, cb) { cb(null, 9); },
    write(fd, b, o, l, p, cb) { land = () => cb(null, l, b); },
    writev(fd, bs, p, cb) { land = () => cb(null, 2, bs); },
    close(fd, cb) { log.push("close"); cb(); },
  };
  const ws = fs.createWriteStream("x", { fs: ops });
  ws.on("error", (e) => log.push(`error ${e.code} ${e.message}`));
  await new Promise((resolve) => ws.once("ready", resolve));
  if (method === "writev") { ws.cork(); ws.write("a"); }
  ws.write("b", (e) => log.push(`cb ${e && e.code}`));
  if (method === "writev") ws.uncork();
  await new Promise((resolve) => setImmediate(resolve));
  ws.destroy();
  land();
  await closed(ws);
  row(`destroyed under ${method}`, log);
}
// Buffer and URL paths reach options.fs.open as node hands them over.
{
  const seen = [];
  for (const p of [Buffer.from("b.txt"), url.pathToFileURL(path.join(root, "u.txt"))]) {
    const s = fs.createReadStream(p, {
      fs: { open(q, f, m, cb) { seen.push(Buffer.isBuffer(q) ? `buffer ${q}` : `${typeof q} ${path.basename(q)}`); cb(new Error("no")); }, read() {}, close() {} },
    });
    s.on("error", () => {});
    await closed(s);
  }
  row("path forms", seen);
}

// The checks, in node's order.
const check = (label, make) => {
  try {
    const s = make();
    s.on("error", () => {});
    s.destroy();
    console.log(label, "ok");
  } catch (e) {
    console.log(label, e.code, e.message);
  }
};
const o = () => {};
check("read: no open", () => fs.createReadStream("r.txt", { fs: {} }));
check("read: open not a function", () => fs.createReadStream("r.txt", { fs: { open: 1 } }));
check("read: no read", () => fs.createReadStream("r.txt", { fs: { open: o } }));
check("read: no close", () => fs.createReadStream("r.txt", { fs: { open: o, read: o } }));
check("read: no close, autoClose false", () => fs.createReadStream("r.txt", { autoClose: false, fs: { open: o, read: o } }));
check("read: bad path, no open", () => fs.createReadStream(5, { fs: {} }));
check("read: bad path", () => fs.createReadStream(5, { fs: { open: o } }));
check("read: fd, no read", () => fs.createReadStream(null, { fd: 99, fs: {} }));
check("read: bad fd, no read", () => fs.createReadStream(null, { fd: -1, fs: {} }));
check("read: bad start, no read", () => fs.createReadStream("r.txt", { start: -1, fs: { open: o } }));
check("read: start > end", () => fs.createReadStream("r.txt", { start: 5, end: 2, fs: { open: o, read: o, close: o } }));
check("read: bad encoding first", () => fs.createReadStream("r.txt", { encoding: "zz", fs: {} }));
check("read: fs null", () => fs.createReadStream("r.txt", { fs: null }));
check("read: fs a number", () => fs.createReadStream("r.txt", { fs: 5 }));
check("write: no open", () => fs.createWriteStream("w.txt", { fs: {} }));
check("write: no write", () => fs.createWriteStream("w.txt", { fs: { open: o } }));
check("write: write not a function", () => fs.createWriteStream("w.txt", { fs: { open: o, write: 1 } }));
check("write: writev not a function", () => fs.createWriteStream("w.txt", { fs: { open: o, writev: 1 } }));
check("write: write ok, writev not", () => fs.createWriteStream("w.txt", { fs: { open: o, write: o, writev: 1 } }));
check("write: no close", () => fs.createWriteStream("w.txt", { fs: { open: o, write: o } }));
check("write: no close, autoClose false", () => fs.createWriteStream("w.txt", { autoClose: false, fs: { open: o, write: o } }));
check("write: bad flush, no close", () => fs.createWriteStream("w.txt", { flush: 1, fs: { open: o, write: o } }));
check("write: bad flush", () => fs.createWriteStream("w.txt", { flush: 1, fs: { open: o, write: o, close: o } }));
check("write: flush, no fsync", () => fs.createWriteStream("w.txt", { flush: true, fs: { open: o, write: o, close: o } }));
check("write: flush, bad fsync", () => fs.createWriteStream("w.txt", { flush: true, fs: { open: o, write: o, close: o, fsync: 1 } }));
check("write: bad start, no fsync", () => fs.createWriteStream("w.txt", { start: -1, flush: true, fs: { open: o, write: o, close: o } }));
check("write: bad start", () => fs.createWriteStream("w.txt", { start: -1, fs: { open: o, write: o, close: o } }));
check("write: fd, no write", () => fs.createWriteStream(null, { fd: 99, fs: {} }));

process.chdir(os.tmpdir());
fs.rmSync(root, { recursive: true, force: true });
