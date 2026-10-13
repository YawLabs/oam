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
  await new Promise((res) => rs.once("data", () => { const p = h.close(); p.then(() => log.push("close() resolved")); res(); }));
  await new Promise((r) => rs.on("close", r)); await tick(); await tick();
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
  a.on("close", () => log.push("a close")); b.on("close", () => log.push("b close errored=" + (b.errored && b.errored.code))); b.on("error", (e) => log.push("b error " + e.code));
  let got = ""; a.on("data", (c) => (got += c)); b.on("data", (c) => (got += c));
  await Promise.all([new Promise((r) => a.on("close", r)), new Promise((r) => b.on("close", r))]); await tick();
  log.push("read " + got.length + " fd " + h.fd);
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
  const read = h.read(Buffer.alloc(4), 0, 4, 0).then((r) => log.push("read " + r.bytesRead + " fd during " + (typeof h.fd)), (e) => log.push("read " + e.code));
  const closing = h.close().then(() => log.push("close() resolved"));
  log.push("fd right after close() " + typeof h.fd);
  const late = h.stat().then(() => log.push("stat while closing ok"), (e) => log.push("stat while closing " + e.code));
  await Promise.all([read, closing, late]);
  log.push("fd " + h.fd);
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
