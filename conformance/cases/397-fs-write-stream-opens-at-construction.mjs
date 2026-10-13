// fs.createWriteStream opens its file when the stream is constructed (#323).
// node's WriteStream opens in `_construct`: the file exists, `'open'` (with the
// descriptor) and then `'ready'` have fired, `fd` is a number and `pending` is
// false before anything is written -- and whether anything ever is. A path
// that cannot be opened destroys the stream with the open's error, `'error'`
// then `'close'`, with no write needed to find out. A write issued in the
// same tick as the constructor waits behind the open.
//
// Regression guard: oam opened at the first write, or in `final` when there
// was none. A stream made and not yet written had no file, an undefined `fd`
// and `pending`, and had emitted nothing; one ended without data emitted
// `'finish'` and `'close'` but never `'open'` or `'ready'`; and one over a
// path that does not open said nothing at all until it was written to. The
// open also knew two flags, `"a"` and everything else as `"w"`; `start` was
// ignored; and neither stream had `fd`, `pending`, `flags`, `mode`, `start` or
// `pos`.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { finished } from "node:stream/promises";

const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-397-"));
// Whichever row throws, the directory goes.
process.on("exit", () => {
  try {
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});
const scrub = (s) => (typeof s === "string" ? s.split(made).join("<D>").split(path.sep).join("/") : s);
const shape = (e) => (e && e.code ? `${e.code} ${e.syscall} ${scrub(e.message)}` : "threw " + String(e));
let made_n = 0;
const fresh = (content) => {
  const p = path.join(made, "f" + made_n++);
  if (content !== undefined) fs.writeFileSync(p, content);
  return p;
};
const read = (p) => {
  try {
    return fs.readFileSync(p, "utf8");
  } catch (e) {
    return e.code;
  }
};
// Every event a stream emits, in order, with what `'open'` and `'error'` carry.
const watch = (stream, log = []) => {
  for (const name of ["open", "ready", "drain", "finish", "close"]) {
    stream.on(name, (arg) => log.push(name === "open" ? "open:" + typeof arg : name));
  }
  stream.on("error", (e) => log.push("error:" + shape(e)));
  return log;
};
const closed = (stream) => new Promise((res) => stream.on("close", res));
const once = (stream, name) => new Promise((res) => stream.once(name, res));
const state = (ws) => `fd=${ws.fd === null ? "null" : typeof ws.fd} pending=${ws.pending} destroyed=${ws.destroyed} bytesWritten=${ws.bytesWritten}`;

// ---- Made and not written ----
{
  const p = fresh();
  const ws = fs.createWriteStream(p);
  const log = watch(ws);
  console.log("constructed | " + state(ws) + " exists=" + fs.existsSync(p));
  console.log("constructed | path " + scrub(ws.path) + " flags=" + ws.flags + " mode=" + ws.mode + " start=" + ws.start + " pos=" + ws.pos);
  await once(ws, "ready");
  console.log("ready | " + JSON.stringify(log) + " " + state(ws) + " exists=" + fs.existsSync(p));
  ws.end();
  await closed(ws);
  console.log("ended with no data | " + JSON.stringify(log) + " " + state(ws) + " content=" + JSON.stringify(read(p)));
}

// ---- The open decides the file, written or not ----
for (const flags of [undefined, "w", "w+", "a", "a+", "r+", "wx", "ax", "as", "rs+", 0, "zz"]) {
  for (const existing of [true, false]) {
    const p = fresh(existing ? "0123456789" : undefined);
    let line;
    try {
      const ws = fs.createWriteStream(p, flags === undefined ? undefined : { flags });
      const log = watch(ws);
      const done = closed(ws);
      // Nothing is written until the open has landed or failed.
      await Promise.race([once(ws, "ready"), done]);
      const before = read(p);
      if (!ws.destroyed) ws.end("AB");
      await done;
      line = `${JSON.stringify(log)} flags=${ws.flags} before=${JSON.stringify(before)} after=${JSON.stringify(read(p))}`;
    } catch (e) {
      line = "threw " + (e.code || e.name) + " " + scrub(e.message);
    }
    console.log(`flags ${JSON.stringify(flags)} ${existing ? "existing" : "missing"} | ${line}`);
  }
}

// ---- A path that does not open ----
fs.mkdirSync(path.join(made, "dir"));
for (const [label, p] of [["missing directory", path.join(made, "no", "x.txt")], ["a directory", path.join(made, "dir")], ["under a file", path.join(fresh("x"), "y")]]) {
  const ws = fs.createWriteStream(p);
  const log = watch(ws);
  console.log(`${label} | constructed ${state(ws)}`);
  await closed(ws);
  console.log(`${label} | ${JSON.stringify(log)} ${state(ws)}`);
  // finished() of a stream that never opened rejects with the open's error.
  const again = fs.createWriteStream(p);
  again.on("error", () => {});
  console.log(`${label} | finished: ` + (await finished(again).then(() => "resolved", shape)));
  // A write to it fails its callback with the same error.
  const third = fs.createWriteStream(p);
  third.on("error", () => {});
  const answer = await new Promise((res) => third.write("x", (e) => res(e ? shape(e) : "written")));
  console.log(`${label} | write callback: ${answer}`);
}

// ---- A write in the same tick waits behind the open ----
{
  const p = fresh();
  const ws = fs.createWriteStream(p);
  const log = watch(ws);
  const first = ws.write("one,", () => log.push("wrote one"));
  ws.write("two,", () => log.push("wrote two"));
  ws.end("three", () => log.push("end callback"));
  console.log("same tick | write returned " + first + " " + state(ws) + " writableLength=" + ws.writableLength);
  await closed(ws);
  console.log("same tick | " + JSON.stringify(log) + " " + state(ws) + " content=" + JSON.stringify(read(p)));
}
{
  const p = fresh();
  const ws = fs.createWriteStream(p);
  const log = watch(ws);
  ws.cork();
  ws.write("a");
  ws.write("b");
  ws.uncork();
  ws.close(() => log.push("close callback"));
  await closed(ws);
  await new Promise((res) => setImmediate(res));
  console.log("cork then close | " + JSON.stringify(log) + " bytesWritten=" + ws.bytesWritten + " content=" + JSON.stringify(read(p)));
}

// ---- Destroyed before the open lands ----
{
  const p = fresh("0123456789");
  const ws = fs.createWriteStream(p);
  const log = watch(ws);
  ws.destroy();
  console.log("destroyed at once | " + state(ws));
  await closed(ws);
  console.log("destroyed at once | " + JSON.stringify(log) + " " + state(ws) + " content=" + JSON.stringify(read(p)));
}
{
  const p = fresh();
  const ws = fs.createWriteStream(p);
  const log = watch(ws);
  ws.destroy(new Error("mine"));
  await closed(ws);
  console.log("destroyed with an error | " + JSON.stringify(log).replace(/error:threw Error: mine/, "error:mine") + " exists=" + fs.existsSync(p));
}
{
  const p = fresh();
  const ws = fs.createWriteStream(p);
  const log = watch(ws);
  ws.write("kept?", () => log.push("write callback"));
  ws.destroy();
  await closed(ws);
  console.log("written then destroyed | " + JSON.stringify(log) + " content=" + JSON.stringify(read(p)));
}

// ---- Options that reach the open and the stream ----
{
  const p = fresh();
  const ws = fs.createWriteStream(p, { autoClose: false });
  const log = watch(ws);
  await once(ws, "ready");
  const fd = ws.fd;
  ws.end("kept open");
  await once(ws, "finish");
  await new Promise((res) => setImmediate(res));
  let stillOpen;
  try {
    stillOpen = fs.fstatSync(fd).size;
  } catch (e) {
    stillOpen = e.code;
  }
  console.log("autoClose false | " + JSON.stringify(log) + " " + state(ws) + " fstat=" + stillOpen);
  fs.closeSync(fd);
}
{
  const p = fresh();
  const ws = fs.createWriteStream(p, { emitClose: false });
  const log = watch(ws);
  ws.end("x");
  await once(ws, "finish");
  await new Promise((res) => setTimeout(res, 20));
  console.log("emitClose false | " + JSON.stringify(log) + " " + state(ws));
}
{
  // A descriptor that is handed over was not opened by the stream.
  const p = fresh();
  const fd = fs.openSync(p, "w");
  const ws = fs.createWriteStream(null, { fd });
  const log = watch(ws);
  console.log("fd supplied | constructed " + state(ws) + " path=" + ws.path);
  ws.end("by fd");
  await closed(ws);
  console.log("fd supplied | " + JSON.stringify(log) + " content=" + JSON.stringify(read(p)));
}
{
  const p = fresh();
  const handle = await fs.promises.open(p, "w");
  const ws = handle.createWriteStream();
  const log = watch(ws);
  console.log("FileHandle | constructed " + state(ws));
  ws.end("by handle");
  await closed(ws);
  console.log("FileHandle | " + JSON.stringify(log) + " content=" + JSON.stringify(read(p)));
}
{
  // An encoding and a string path as a Buffer and a URL.
  const p = fresh();
  const ws = fs.createWriteStream(Buffer.from(p), "latin1");
  const log = watch(ws);
  ws.end("é");
  await closed(ws);
  console.log("Buffer path, encoding | " + JSON.stringify(log) + " bytes=" + fs.readFileSync(p).length + " pathIsBuffer=" + Buffer.isBuffer(ws.path));
}
// Two streams on one file, `'open'` each.
{
  const p = fresh();
  const a = fs.createWriteStream(p, { flags: "a" });
  const b = fs.createWriteStream(p, { flags: "a" });
  const la = watch(a);
  const lb = watch(b);
  a.end("A");
  await closed(a);
  b.end("B");
  await closed(b);
  console.log("two streams | " + JSON.stringify(la) + " " + JSON.stringify(lb) + " content=" + JSON.stringify(read(p)));
}
// pipeline and finished see the open's failure without a write.
{
  const { Readable } = await import("node:stream");
  const { pipeline } = await import("node:stream/promises");
  const answer = await pipeline(Readable.from([]), fs.createWriteStream(path.join(made, "no", "z"))).then(() => "resolved", shape);
  console.log("pipeline of nothing into a path that does not open | " + answer);
  const p = fresh();
  const ok = await pipeline(Readable.from(["x", "y"]), fs.createWriteStream(p)).then(() => "resolved", shape);
  console.log("pipeline | " + ok + " content=" + JSON.stringify(read(p)));
}

// ---- `start`: every write names its position ----
for (const [flags, start] of [["r+", 3], ["r+", 0], ["r+", 12], ["w", 3], ["a", 3]]) {
  const p = fresh("0123456789");
  const ws = fs.createWriteStream(p, { flags, start });
  const log = watch(ws);
  const before = `start=${ws.start} pos=${ws.pos}`;
  ws.write("AB");
  ws.end("C");
  await closed(ws);
  console.log(`start ${start} flags ${flags} | ${JSON.stringify(log)} ${before} then pos=${ws.pos} bytesWritten=${ws.bytesWritten} content=${JSON.stringify(read(p))}`);
}
for (const options of [{ start: -1 }, { start: 1.5 }, { start: "1" }, { start: 2 ** 53 }, { flush: 1 }, { fd: -1 }, { fd: 1.5 }, { start: 5, end: 2 }, { end: -1 }, { end: "3" }, { end: Infinity }, { start: 2, end: 2 }]) {
  for (const [kind, make] of [["write", fs.createWriteStream], ["read", fs.createReadStream]]) {
    const p = fresh("0123456789");
    let line;
    try {
      const stream = make(p, options);
      stream.on("error", () => {});
      const done = closed(stream);
      line = `made start=${stream.start} end=${kind === "read" ? stream.end : "-"} pos=${stream.pos}`;
      stream.destroy();
      await done;
    } catch (e) {
      line = e.code + " " + e.message;
    }
    console.log(`${kind} stream ${JSON.stringify(options)} | ${line}`);
  }
}

// ---- What the two streams say about themselves ----
{
  const p = fresh("0123456789");
  const ws = fs.createWriteStream(p, { flags: "r+", mode: 0o600, start: 1, flush: true });
  console.log("write stream | keys " + Object.keys(ws).filter((k) => !k.startsWith("_")).join(",") + " highWaterMark " + ws.writableHighWaterMark);
  console.log("write stream | " + [ws.flags, ws.mode, ws.flush, ws.start, ws.pos, ws.fd, ws.pending].join(" "));
  ws.end("Z");
  await closed(ws);
  console.log("write stream | flushed " + JSON.stringify(read(p)));

  const rs = fs.createReadStream(p, { start: 1, end: 5, encoding: "utf8" });
  console.log("read stream | keys " + Object.keys(rs).filter((k) => !k.startsWith("_")).join(",") + " highWaterMark " + rs.readableHighWaterMark);
  console.log("read stream | constructed " + [rs.flags, rs.mode, rs.start, rs.end, rs.pos, rs.fd, rs.pending, rs.bytesRead].join(" "));
  await once(rs, "ready");
  console.log("read stream | ready " + [typeof rs.fd, rs.pending].join(" "));
  let got = "";
  rs.on("data", (c) => (got += c));
  await closed(rs);
  console.log("read stream | closed " + [JSON.stringify(got), rs.pos, rs.fd, rs.pending, rs.bytesRead].join(" "));
}
// A read stream's flags reach its open too.
for (const flags of ["r", "r+", "w+", "a+", "w", "wx", "zz"]) {
  const p = fresh("0123456789");
  const rs = fs.createReadStream(p, { flags, encoding: "utf8" });
  const log = [];
  rs.on("open", () => log.push("open"));
  rs.on("error", (e) => log.push("error:" + shape(e)));
  let got = "";
  rs.on("data", (c) => (got += c));
  await closed(rs);
  console.log(`read stream flags ${flags} | ${JSON.stringify(log)} read ${JSON.stringify(got)} holds ${JSON.stringify(read(p))}`);
}

// ---- Destroyed under a write: the write lands first ----
for (let round = 0; round < 5; round++) {
  const p = fresh();
  const ws = fs.createWriteStream(p);
  const log = watch(ws);
  await once(ws, "ready");
  ws.write("abc", (e) => log.push("write callback " + (e ? e.code : "ok")));
  ws.destroy();
  await closed(ws);
  await new Promise((res) => setImmediate(res));
  console.log(`destroyed under a write ${round} | ${JSON.stringify(log.map((l) => l.replace(/^error:.*$/, "error")))} bytesWritten=${ws.bytesWritten} content=${JSON.stringify(read(p))}`);
}

// ---- autoClose is the stream's autoDestroy; destroy() always closes ----
const fdState = (fd) => {
  try {
    fs.fstatSync(fd);
    return "open";
  } catch (e) {
    return e.code;
  }
};
{
  const p = fresh();
  const ws = fs.createWriteStream(p, { autoClose: false });
  const log = watch(ws);
  await once(ws, "ready");
  const fd = ws.fd;
  // The two default streams are closed before the row goes on: a descriptor
  // node closes is a number its next open may be handed, so a stream left
  // open here would decide what `fdState` reads below.
  const defaults = [fs.createWriteStream(fresh()), fs.createReadStream(p)];
  console.log("autoClose | " + [ws.autoClose, "autoClose" in ws, ...defaults.map((stream) => stream.autoClose)].join(" "));
  for (const stream of defaults) {
    const done = closed(stream);
    stream.destroy();
    await done;
  }
  ws.end("x");
  await once(ws, "finish");
  ws.destroy();
  await closed(ws);
  console.log("autoClose false, destroy | " + JSON.stringify(log) + " " + state(ws) + " descriptor " + fdState(fd));
}
{
  const p = fresh();
  const ws = fs.createWriteStream(p, { autoClose: false });
  const log = watch(ws);
  await once(ws, "ready");
  const fd = ws.fd;
  ws.write("z");
  await new Promise((res) => ws.close(res));
  console.log("autoClose false, close(cb) | " + JSON.stringify(log) + " " + state(ws) + " descriptor " + fdState(fd));
}
{
  const p = fresh("0123456789");
  const rs = fs.createReadStream(p, { autoClose: false, encoding: "utf8" });
  const seen = [];
  rs.on("data", (c) => seen.push(c));
  await once(rs, "end");
  await new Promise((res) => setImmediate(res));
  const fd = rs.fd;
  seen.push(`after end fd=${typeof fd} destroyed=${rs.destroyed} descriptor ${fdState(fd)}`);
  rs.destroy();
  await closed(rs);
  console.log("read autoClose false | " + JSON.stringify(seen) + " then fd=" + rs.fd + " descriptor " + fdState(fd));
  const set = fs.createWriteStream(fresh());
  set.autoClose = false;
  set.end("x");
  await once(set, "finish");
  await new Promise((res) => setImmediate(res));
  console.log("autoClose assigned false | destroyed=" + set.destroyed + " fd=" + typeof set.fd);
  set.destroy();
  await closed(set);
}
// The descriptor is the stream's until it is destroyed: at 'end' it is still there.
{
  const p = fresh("0123456789");
  const rs = fs.createReadStream(p);
  const at = [];
  rs.on("data", () => at.push(`data fd=${typeof rs.fd} pending=${rs.pending}`));
  rs.on("end", () => at.push(`end fd=${typeof rs.fd} pending=${rs.pending}`));
  await closed(rs);
  at.push(`close fd=${rs.fd} pending=${rs.pending}`);
  console.log("read stream descriptor | " + JSON.stringify(at));
}
// `pos` is the cursor: assigning it moves the next read or write.
{
  const p = fresh("0123456789");
  const rs = fs.createReadStream(p, { encoding: "utf8", highWaterMark: 2, start: 0 });
  const got = [];
  for await (const chunk of rs) {
    got.push(chunk);
    if (got.length === 1) rs.pos = 6;
  }
  console.log("read pos assigned | " + JSON.stringify(got));
  const ws = fs.createWriteStream(p, { flags: "r+", start: 0 });
  await once(ws, "ready");
  ws.write("AB");
  const during = ws.pos;
  ws.pos = 8;
  ws.end("YZ");
  await closed(ws);
  console.log("write pos | during " + during + " content " + JSON.stringify(read(p)));
}
// What queued behind the open goes out as one write.
{
  const p = fresh();
  const ws = fs.createWriteStream(p);
  const counts = [];
  ws.write("aa", () => counts.push(ws.bytesWritten));
  ws.write("bbb", () => counts.push(ws.bytesWritten));
  ws.write("c", () => counts.push(ws.bytesWritten));
  ws.end();
  await closed(ws);
  console.log("queued writes | bytesWritten in each callback " + JSON.stringify(counts) + " content " + JSON.stringify(read(p)));
  const other = fs.createWriteStream(fresh());
  console.log("destroySoon | " + typeof other.destroySoon + " " + (other.destroySoon === other.end));
  other.destroySoon();
  await closed(other);
}
// Handed a descriptor, a stream has no path, flags or mode.
{
  const p = fresh("0123456789");
  const keys = (stream) => Object.keys(stream).filter((k) => !k.startsWith("_")).join(",");
  const wfd = fs.openSync(p, "r+");
  const ws = fs.createWriteStream("ignored", { fd: wfd, flags: "a", mode: 0o600 });
  console.log("write stream with fd | keys " + keys(ws) + " flags " + ws.flags + " mode " + ws.mode);
  ws.destroy();
  await closed(ws);
  const rfd = fs.openSync(p, "r");
  const rs = fs.createReadStream("ignored", { fd: rfd, flags: "a" });
  console.log("read stream with fd | keys " + keys(rs) + " flags " + rs.flags + " mode " + rs.mode);
  rs.destroy();
  await closed(rs);
  const url = new URL("file:///" + p.split(path.sep).join("/").replace(/^\//, ""));
  const byUrl = fs.createReadStream(url);
  console.log("path from a URL | " + typeof byUrl.path + " " + (byUrl.path === p));
  byUrl.destroy();
  await closed(byUrl);
}
// The checks a bad call meets, in node's order.
for (const [label, make] of [
  ["options 5", (k) => k(fresh(""), 5)],
  ["encoding", (k) => k(fresh(""), { encoding: "nope" })],
  ["encoding string", (k) => k(fresh(""), "nope")],
  ["encoding before path", (k) => k(5, { encoding: "nope" })],
  ["path before start", (k) => k(5, { start: -1 })],
  ["path before flush", (k) => k(5, { flush: 1 })],
  ["flush before start", (k) => k(fresh(""), { flush: 1, start: -1 })],
  ["fd before start", (k) => k(fresh(""), { fd: -1, start: -1 })],
  ["start before end", (k) => k(fresh(""), { start: -1, end: -1 })],
]) {
  for (const [kind, k] of [["write", fs.createWriteStream], ["read", fs.createReadStream]]) {
    let line;
    try {
      const stream = make(k);
      stream.on("error", () => {});
      const done = closed(stream);
      stream.destroy();
      await done;
      line = "made";
    } catch (e) {
      line = e.code + " " + scrub(e.message);
    }
    console.log(`${kind} stream, ${label} | ${line}`);
  }
}
{
  const p = fresh("0123456789");
  const rs = fs.createReadStream(p, { emitClose: false });
  const log = [];
  for (const name of ["open", "end", "close"]) rs.on(name, () => log.push(name));
  rs.resume();
  await once(rs, "end");
  await new Promise((res) => setTimeout(res, 20));
  console.log("read emitClose false | " + JSON.stringify(log) + " destroyed=" + rs.destroyed);
}

// A setImmediate made at 'finish' or 'end' runs before 'close': the close
// takes a turn of the event loop.
{
  const order = [];
  const ws = fs.createWriteStream(fresh());
  ws.on("finish", () => {
    order.push("finish closed=" + ws.closed);
    setImmediate(() => order.push("immediate closed=" + ws.closed));
  });
  ws.on("close", () => order.push("close"));
  ws.end("x");
  await closed(ws);
  await new Promise((res) => setImmediate(res));
  const rs = fs.createReadStream(fresh("0123"));
  rs.on("end", () => {
    order.push("end closed=" + rs.closed);
    setImmediate(() => order.push("immediate closed=" + rs.closed));
  });
  rs.on("close", () => order.push("close"));
  rs.resume();
  await closed(rs);
  await new Promise((res) => setImmediate(res));
  console.log("close takes a turn | " + JSON.stringify(order));
}
// `flush` whose sync fails still closes the descriptor.
{
  const p = fresh("0123456789");
  for (const how of ["destroy", "end"]) {
    const fd = fs.openSync(p, "r");
    const ws = fs.createWriteStream(null, { fd, flush: true });
    const log = [];
    ws.on("error", (e) => log.push("error " + e.syscall));
    ws.on("close", () => log.push("close"));
    if (how === "destroy") ws.destroy();
    else ws.end();
    await closed(ws);
    // What the sync says of a descriptor opened to read is the platform's;
    // that the descriptor is closed afterwards is not.
    console.log(`flush on a read-only descriptor, ${how} | ${log.includes("close")} fd=${ws.fd} descriptor ${fdState(fd)}`);
  }
}
