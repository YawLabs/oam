// A FileHandle is an EventEmitter that emits 'close', and what runs over it
// is tied to it (#343). node's handle counts who is using its descriptor:
// each method while its operation is in flight, and a stream made over the
// handle for its life. close() emits 'close' at once -- which closes a
// stream over it, cleanly -- and the descriptor is closed, and close()'s
// promise settled, when the last user is done.
//
// Regression guard: oam's handle was a plain object. `handle.on` threw, and
// close() closed the descriptor on the spot: a stream over the handle
// failed its next read `EBADF` where node closes it without an error, a
// second stream on the same handle failed when the first ended, and an
// `autoClose: false` stream outlived the handle, `fd` still set.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { EventEmitter } from "node:events";
const d = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-401-"));
// Whichever row throws, the directory goes.
process.on("exit", () => { try { fs.rmSync(d, { recursive: true, force: true }); } catch {} });
const file = path.join(d, "f"); fs.writeFileSync(file, "0123456789".repeat(10));
const tick = () => new Promise((r) => setImmediate(r));
const log = [];
const out = (label) => { console.log(label + ": " + log.join(" | ")); log.length = 0; };

{ // shape
  const h = await fs.promises.open(file, "r");
  console.log("shape:", h instanceof EventEmitter, typeof h.on, typeof h.once, typeof h.emit, h.constructor.name, Object.keys(h).join(","), Object.getOwnPropertyNames(Object.getPrototypeOf(h)).filter((k) => !k.startsWith("_")).sort().join(","));
  h.on("close", (...a) => log.push("close event args=" + a.length));
  const p = h.close(); log.push("close() returned " + (p instanceof Promise));
  const p2 = h.close(); log.push("second close same promise " + (p === p2));
  await p; log.push("fd " + h.fd); await tick(); await h.close(); log.push("third close ok");
  out("plain close");
}
{ // close under a read stream
  const h = await fs.promises.open(file, "r");
  h.on("close", () => log.push("handle close"));
  const rs = h.createReadStream({ highWaterMark: 16 });
  rs.on("error", (e) => log.push("stream error " + e.code)); rs.on("end", () => log.push("stream end")); rs.on("close", () => log.push("stream close errored=" + (rs.errored && rs.errored.code)));
  let closing;
  await new Promise((res) => rs.once("data", () => { closing = h.close(); closing.then(() => log.push("close() resolved")); res(); }));
  await new Promise((r) => rs.on("close", r)); await closing;
  log.push("fd " + h.fd + " rs.fd " + rs.fd + " destroyed " + rs.destroyed);
  out("close under a read stream");
}
{ // stream ends: who closes the handle
  const h = await fs.promises.open(file, "r");
  h.on("close", () => log.push("handle close"));
  const rs = h.createReadStream();
  rs.on("close", () => log.push("stream close")); rs.resume();
  await new Promise((r) => rs.on("close", r)); await tick(); await tick();
  log.push("fd " + h.fd);
  out("stream runs to its end");
}
{ // autoClose false
  const h = await fs.promises.open(file, "r");
  h.on("close", () => log.push("handle close"));
  const rs = h.createReadStream({ autoClose: false });
  rs.on("close", () => log.push("stream close")); rs.on("end", () => log.push("stream end")); rs.resume();
  await new Promise((r) => rs.on("end", r)); await tick(); await tick();
  log.push("after end: fd " + typeof h.fd + " rs.destroyed " + rs.destroyed);
  await h.close(); await tick(); await tick();
  log.push("after handle.close: fd " + h.fd + " rs.fd " + rs.fd + " rs.destroyed " + rs.destroyed);
  out("autoClose false");
}
{ // two streams
  const h = await fs.promises.open(file, "r");
  h.on("close", () => log.push("handle close"));
  const a = h.createReadStream({ start: 0, end: 9 }); const b = h.createReadStream({ start: 10, end: 19 });
  // Which of the two reaches its end first is a race; that both close, and
  // neither with an error, is not.
  const ended = {};
  a.on("close", () => (ended.a = String(a.errored && a.errored.code))); b.on("close", () => (ended.b = String(b.errored && b.errored.code)));
  a.on("error", (e) => log.push("a error " + e.code)); b.on("error", (e) => log.push("b error " + e.code));
  a.resume(); b.resume();
  await Promise.all([new Promise((r) => a.on("close", r)), new Promise((r) => b.on("close", r))]); await tick();
  log.push("a closed errored=" + ended.a + " b closed errored=" + ended.b + " fd " + h.fd);
  out("two streams");
}
{ // write stream
  const p = path.join(d, "w"); const h = await fs.promises.open(p, "w");
  h.on("close", () => log.push("handle close"));
  const ws = h.createWriteStream();
  ws.on("error", (e) => log.push("ws error " + e.code)); ws.on("close", () => log.push("ws close"));
  ws.write("abc", (e) => log.push("cb1 " + (e ? e.code : "ok")));
  const cp = h.close(); cp.then(() => log.push("close() resolved"));
  ws.write("def", (e) => log.push("cb2 " + (e ? e.code : "ok")));
  await new Promise((r) => ws.on("close", r)); await cp; await tick();
  log.push("holds " + JSON.stringify(fs.readFileSync(p, "utf8")) + " bytesWritten " + ws.bytesWritten);
  out("write, close the handle, write");
}
{ // fd option
  const h = await fs.promises.open(file, "r");
  h.on("close", () => log.push("handle close"));
  const rs = fs.createReadStream(null, { fd: h, start: 0, end: 4, encoding: "utf8" });
  let got = ""; rs.on("data", (c) => (got += c)); rs.on("close", () => log.push("stream close"));
  await new Promise((r) => rs.on("close", r)); await tick();
  log.push("read " + JSON.stringify(got) + " fd " + h.fd);
  out("createReadStream({ fd: handle })");
}
{ // readLines + close; and methods after close
  const h = await fs.promises.open(file, "r");
  await h.close();
  for (const [name, f] of [["read", () => h.read(Buffer.alloc(1), 0, 1, 0)], ["stat", () => h.stat()], ["createReadStream", async () => h.createReadStream()]]) {
    log.push(name + " " + (await f().then(() => "ok", (e) => e.code + "/" + e.message)));
  }
  out("after close");
}
{ // listeners and Symbol.asyncDispose
  const h = await fs.promises.open(file, "r");
  const rs = h.createReadStream();
  log.push("close listeners with a stream " + h.listenerCount("close"));
  rs.destroy(); await new Promise((r) => rs.on("close", r)); await tick();
  log.push("after stream destroy: fd " + h.fd + " listeners " + h.listenerCount("close"));
  log.push("asyncDispose " + typeof h[Symbol.asyncDispose]);
  out("bookkeeping");
}

{ // an operation in flight when close() is called finishes first
  const h = await fs.promises.open(file, "r");
  // The read and the stat are two operations in flight: either may land
  // first, and close() settles after both.
  const done = {};
  const read = h.read(Buffer.alloc(4), 0, 4, 0).then((r) => (done.read = "read " + r.bytesRead + " fd during " + typeof h.fd), (e) => (done.read = "read " + e.code));
  const closing = h.close().then(() => (done.close = "close() resolved after both " + (done.read !== undefined && done.stat !== undefined)));
  log.push("fd right after close() " + typeof h.fd);
  const late = h.stat().then(() => (done.stat = "stat while closing ok"), (e) => (done.stat = "stat while closing " + e.code));
  await Promise.all([read, closing, late]);
  log.push(done.read, done.stat, done.close, "fd " + h.fd);
  out("close with a read in flight");
}
{ // the web stream of a handle that is closing, and one that is closed
  const h = await fs.promises.open(file, "r");
  const read = h.read(Buffer.alloc(4), 0, 4, 0);
  const closing = h.close();
  try { h.readableWebStream(); log.push("closing: made"); } catch (e) { log.push("closing: " + e.code + " " + e.message); }
  await read; await closing;
  try { h.readableWebStream(); log.push("closed: made"); } catch (e) { log.push("closed: " + e.code + " " + e.message); }
  out("readableWebStream");
}
{ // readLines over a handle that is then closed
  const h = await fs.promises.open(file, "r");
  const rl = h.readLines();
  rl.on("close", () => log.push("readline close"));
  rl.on("error", (e) => log.push("readline error " + e.code));
  await h.close(); await tick(); await tick();
  log.push("fd " + h.fd);
  out("readLines, then close the handle");
}
{ // listeners after several streams that are still alive
  const h = await fs.promises.open(file, "r");
  for (let i = 0; i < 5; i++) {
    const rs = h.createReadStream({ autoClose: false, start: 0, end: 0 });
    rs.resume();
    await new Promise((r) => rs.on("end", r));
  }
  log.push("listeners after 5 streams " + h.listenerCount("close") + " max " + h.getMaxListeners());
  await h.close(); await tick(); await tick();
  log.push("fd " + h.fd);
  out("many streams on one handle");
}
{ // asyncDispose and `await using`'s method
  const h = await fs.promises.open(file, "r");
  h.on("close", () => log.push("handle close"));
  await h[Symbol.asyncDispose]();
  log.push("fd " + h.fd);
  out("asyncDispose");
}

{ // a stream that cannot be made leaves the handle able to close
  for (const [label, make] of [
    ["read start -1", (h) => h.createReadStream({ start: -1 })],
    ["read start past end", (h) => h.createReadStream({ start: 5, end: 2 })],
    ["write flush 1", (h) => h.createWriteStream({ flush: 1 })],
    ["fd with options.fs", (h) => fs.createReadStream(null, { fd: h, fs: { open() {}, read() {}, close() {} } })],
  ]) {
    const h = await fs.promises.open(file, "r");
    try { make(h); log.push(label + ": made"); } catch (e) { log.push(label + ": " + e.constructor.name + " " + e.code); }
    await h.stat().then(() => log.push("still usable"), (e) => log.push("stat " + e.code));
    // node leaves the handle unable to close after the first three (its
    // close() throws); oam gives the reference back. Not compared: the
    // handle is abandoned here in both.
    if (label === "fd with options.fs") { await h.close(); log.push("fd " + h.fd); }
    out("unmakeable stream");
  }
}
{ // closed behind the handle's back: close() says so
  const h = await fs.promises.open(file, "r");
  fs.closeSync(h.fd);
  log.push(await h.close().then(() => "close() resolved", (e) => "close() rejected " + e.code + " " + e.syscall));
  log.push("fd " + h.fd);
  out("descriptor closed underneath");
}
{ // close() settles after the immediates of its turn
  const h = await fs.promises.open(file, "r");
  h.on("close", () => setImmediate(() => log.push("immediate from the 'close' listener")));
  const closing = h.close().then(() => log.push("close() resolved"));
  setImmediate(() => log.push("immediate after close()"));
  await closing; await tick();
  out("close and immediates");
}
{ // an autoClose: false stream whose read fails keeps the handle
  const p = path.join(d, "wo"); fs.writeFileSync(p, "x");
  const h = await fs.promises.open(p, "a");
  const rs = h.createReadStream({ autoClose: false });
  rs.on("error", (e) => log.push("stream error " + e.code + " destroyed " + rs.destroyed));
  rs.on("close", () => log.push("stream close"));
  rs.resume();
  await new Promise((r) => rs.once("error", r)); await tick(); await tick();
  log.push("handle fd " + typeof h.fd + " stat " + (await h.stat().then(() => "ok", (e) => e.code)));
  await h.close(); await tick();
  log.push("then fd " + h.fd);
  out("read error, autoClose false");
}
{ // arities, and readFile of a handle through fs.promises
  const h = await fs.promises.open(file, "r");
  log.push(["createReadStream", "createWriteStream", "readLines", "readableWebStream", "stat", "close", "readFile", "read", "write", "truncate"].map((n) => n + "=" + h[n].length).join(" "));
  log.push("readFile(handle) " + (await fs.promises.readFile(h, "utf8")).length);
  await h.close();
  log.push("readFile(closed handle) " + (await fs.promises.readFile(h).then(() => "read", (e) => e.code)));
  class Limited extends (await import("node:events")).EventEmitter {}
  Limited.prototype._maxListeners = 3;
  const e = new Limited();
  log.push("inherited limit: own key " + Object.hasOwn(e, "_maxListeners") + " value " + e.getMaxListeners());
  out("surface");
}
