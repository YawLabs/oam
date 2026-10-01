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
    log.push(`[ended ${socket._writableState.ended}]`);
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
    console.log(`${kind} ${name}: ${log.join(" | ")} || close(${hadError}) [ended ${socket._writableState.ended}]`);
  }
}

server.close();
