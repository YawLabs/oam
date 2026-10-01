// net.Socket end() / write() on a socket already destroyed: what each
// callback is handed, when, and whether the stream ends.
//
// Regression guard: after destroy(err) on a socket never ended, oam's
// end(cb) went on to end the stream and handed cb destroy()'s own error
// (already delivered to 'error'). Node v22.22.2's Writable.end does not end
// an errored stream: cb gets ERR_STREAM_DESTROYED "Cannot call end after a
// stream was destroyed" on the next tick. end(data, cb) is refused at the
// write -- "Cannot call write after a stream was destroyed" -- and does not
// end the stream either. After destroy() with no error, end(cb) ends the
// stream and parks cb on the 'finish' list, which a destroyed stream drains
// only when a write outstanding at destroy() settles: with none, node does
// not call it (oam called it with ERR_SOCKET_CLOSED); with one, cb gets the
// stream's error or ERR_STREAM_DESTROYED (oam dropped it).
//
// Rows run on a socket that never had a connection ('close' on the next
// tick, ahead of the callbacks) and on a connected one ('close' from the
// handle's close, after them).
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const describe = (v) => {
  if (v === undefined || v === null) return String(v);
  if (typeof v !== "object") return String(v);
  return `${v.constructor.name}${v.code ? ` ${v.code}` : ""}: ${v.message}`;
};

const rows = [
  ["destroy(err); end(cb)", (s, cb) => { s.destroy(new Error("boom")); s.end(cb("end")); }],
  ["destroy(err); end(data, cb)", (s, cb) => { s.destroy(new Error("boom")); s.end("x", cb("end")); }],
  ["destroy(err); end(cb); end(cb)", (s, cb) => { s.destroy(new Error("boom")); s.end(cb("end1")); s.end(cb("end2")); }],
  ["destroy(err); write(cb)", (s, cb) => { s.destroy(new Error("boom")); cb("write returned")(s.write("x", cb("write"))); }],
  ["destroy(err); write(cb); end(cb)", (s, cb) => { s.destroy(new Error("boom")); s.write("x", cb("write")); s.end(cb("end")); }],
  ["destroy(); end(cb)", (s, cb) => { s.destroy(); s.end(cb("end")); }],
  ["destroy(); end(data, cb)", (s, cb) => { s.destroy(); s.end("x", cb("end")); }],
  ["destroy(); end(data, cb); end(cb)", (s, cb) => { s.destroy(); s.end("x", cb("end1")); s.end(cb("end2")); }],
  ["destroy(); end(cb); end(cb)", (s, cb) => { s.destroy(); s.end(cb("end1")); s.end(cb("end2")); }],
  ["destroy(); write(cb)", (s, cb) => { s.destroy(); s.write("x", cb("write")); }],
];

async function run(kind, make) {
  for (const [name, act] of rows) {
    const socket = await make();
    const log = [];
    let sync = true;
    const cb = (label) => (v) => log.push(`${label}${sync ? " (sync)" : ""} ${describe(v)}`);
    socket.on("error", (e) => log.push(`error ${describe(e)}`));
    const closed = new Promise((resolve) => socket.once("close", (hadError) => {
      log.push(`close(${hadError})`);
      resolve();
    }));
    act(socket, cb);
    sync = false;
    log.push(`[ended ${socket.writableEnded} finished ${socket.writableFinished}]`);
    await closed;
    // Long enough for anything still queued -- a callback node never calls
    // must stay uncalled.
    await new Promise((resolve) => setTimeout(resolve, 50));
    console.log(`${kind} ${name}: ${log.join(" | ")}`);
  }
}

await run("fresh", async () => new net.Socket());

const server = net.createServer((peer) => peer.on("error", () => {}));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
await run("connected", async () => {
  const socket = net.connect(server.address().port, "127.0.0.1");
  await new Promise((resolve) => socket.once("connect", resolve));
  return socket;
});

// An end() made before the destroy keeps its own path (destroy()'s error,
// before 'close'); the one after it is told the stream was destroyed.
{
  const socket = net.connect(server.address().port, "127.0.0.1");
  await new Promise((resolve) => socket.once("connect", resolve));
  const log = [];
  socket.on("error", (e) => log.push(`error ${describe(e)}`));
  const closed = new Promise((resolve) => socket.once("close", (h) => { log.push(`close(${h})`); resolve(); }));
  socket.end((e) => log.push(`end1 ${describe(e)}`));
  socket.destroy(new Error("boom"));
  socket.end((e) => log.push(`end2 ${describe(e)}`));
  await closed;
  await new Promise((resolve) => setTimeout(resolve, 50));
  console.log(`connected end(cb); destroy(err); end(cb): ${log.join(" | ")}`);
}

// An end() made after destroy() while a write is still outstanding: node
// parks its callback, and the write settling hands it the stream's error --
// the held write's ERR_SOCKET_CLOSED_BEFORE_CONNECTION on a socket destroyed
// while connecting, ERR_STREAM_DESTROYED on a connected one. With nothing
// outstanding it is never called (the rows above). oam called none of these
// callbacks. 'close' is reported apart: on a socket destroyed while
// connecting, node's held write fails from its 'close' listener, after the
// one here, and oam's before it (docs/node-divergences.md).
const outstanding = [
  ["write(cb); destroy(); end(cb)", (s, cb) => { s.write("x", cb("write")); s.destroy(); s.end(cb("end")); }],
  // With no callback, a write the connected socket takes whole inside the
  // call does not count (node's onwrite skips afterWrite for it); one held
  // behind the connect does. (Whether a big write is taken whole depends on
  // the OS's send buffer, so no row leans on one.)
  ["write(); destroy(); end(cb)", (s, cb) => { s.write("x"); s.destroy(); s.end(cb("end")); }],
  ["write(cb); destroy(); end(cb); end(cb)", (s, cb) => { s.write("x", cb("write")); s.destroy(); s.end(cb("end1")); s.end(cb("end2")); }],
  ["write(cb); destroy(); end(data, cb); end(cb)", (s, cb) => { s.write("x", cb("write")); s.destroy(); s.end("y", cb("end1")); s.end(cb("end2")); }],
  // The write has failed by now: its error is the stream's, so end() is
  // told the stream was destroyed and does not end it.
  ["write(cb); destroy(); after 'close' end(cb)", async (s, cb, closed) => { s.write("x", cb("write")); s.destroy(); await closed; s.end(cb("end")); }],
];
for (const connected of [false, true]) {
  for (const [name, act] of outstanding) {
    const socket = net.connect(server.address().port, "127.0.0.1");
    if (connected) await new Promise((resolve) => socket.once("connect", resolve));
    const log = [];
    let sync = true;
    const cb = (label) => (v) => log.push(`${label}${sync ? " (sync)" : ""} ${describe(v)}`);
    socket.on("error", (e) => log.push(`error ${describe(e)}`));
    let hadError;
    const closed = new Promise((resolve) => socket.once("close", (h) => { hadError = h; resolve(); }));
    const acted = act(socket, cb, closed);
    sync = false;
    await acted;
    await closed;
    await new Promise((resolve) => setTimeout(resolve, 50));
    const kind = connected ? "connected" : "connecting";
    console.log(`${kind} ${name}: ${log.join(" | ")} || close(${hadError}) [ended ${socket.writableEnded} finished ${socket.writableFinished}]`);
  }
}

// writableEnded / writableFinished, node's Writable getters (oam's
// net.Socket had neither): end() called on a stream it could end, and
// 'finish' emitted. A socket made with writable: false has both from the
// start, so end() is told it already finished.
{
  const state = (s) => `${s.writableEnded}/${s.writableFinished}`;
  const fresh = new net.Socket();
  const shut = new net.Socket({ writable: false });
  const log = [`fresh ${state(fresh)}`, `writable:false ${state(shut)}`];
  shut.end((e) => log.push(`writable:false end ${describe(e)}`));
  const socket = net.connect(server.address().port, "127.0.0.1");
  await new Promise((resolve) => socket.once("connect", resolve));
  log.push(`connected ${state(socket)}`);
  const finished = new Promise((resolve) => socket.end(resolve));
  log.push(`after end() ${state(socket)}`);
  await finished;
  log.push(`after 'finish' ${state(socket)}`);
  socket.destroy();
  fresh.destroy();
  console.log(`getters: ${log.join(" | ")}`);
}

// The error objects themselves, not just code and message: node's coded
// errors share one prototype per code, between the instance and the base's
// prototype, whose `constructor` is the base and whose toString renders
// "Name [CODE]: message". oam built them two ways -- net.Socket's on
// Error.prototype with an own toString, the vendored streams' (Writable,
// and tls.TLSSocket over them) as classes named after the code -- so
// constructor.name was "ERR_STREAM_DESTROYED" on one path and the
// prototype was Error.prototype on the other.
{
  const { Writable } = await import("node:stream");
  const errs = [];
  const sock = new net.Socket();
  sock.on("error", () => {});
  sock.destroy(new Error("boom"));
  sock.end((e) => errs.push(["net end", e]));
  sock.write("x", (e) => errs.push(["net write", e]));
  const w = new Writable({ write(chunk, enc, cb) { cb(); } });
  w.on("error", () => {});
  w.destroy(new Error("boom"));
  w.end((e) => errs.push(["Writable end", e]));
  w.write("x", (e) => errs.push(["Writable write", e]));
  try { net.connect({}); } catch (e) { errs.push(["net.connect({})", e]); }
  try { new net.Socket().setTimeout(-1); } catch (e) { errs.push(["setTimeout(-1)", e]); }
  await new Promise((resolve) => setTimeout(resolve, 20));
  const protoOf = new Map();
  for (const [label, e] of errs) {
    const proto = Object.getPrototypeOf(e);
    const base = Object.getPrototypeOf(proto);
    const first = protoOf.get(e.code);
    if (first === undefined) protoOf.set(e.code, proto);
    const desc = (o, k) => {
      const d = Object.getOwnPropertyDescriptor(o, k);
      return d ? `${d.writable ? "w" : ""}${d.enumerable ? "e" : ""}${d.configurable ? "c" : ""}` : "-";
    };
    console.log(
      `shape ${label}: ${e.code} ctor=${e.constructor.name} name=${e.name}` +
      ` proto-is-base=${proto === e.constructor.prototype} base=${base === e.constructor.prototype}` +
      ` shared=${first === undefined || first === proto}` +
      ` proto.toString=${desc(proto, "toString")} proto.constructor=${desc(proto, "constructor")}` +
      ` own=${Object.getOwnPropertyNames(e).sort().join(",")} keys=${Object.keys(e).join(",")}` +
      ` instanceof=${e instanceof Error} string=${String(e)} stack=${e.stack.split("\n")[0]}`,
    );
  }
}

server.close();
