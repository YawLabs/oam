// net.Socket write() / end(data) on a socket that has no connection and none
// on the way (never connected, connect() never called).
//
// Regression guard: oam handed the write to the natives with no handle, and
// the callback and 'error' got "tcp: write handle 0 is gone" -- no code, and
// nothing a caller could act on -- and end(data) marked the stream ended.
// Node v22.22.2's _writeGeneric has no handle to write to and fails the
// write with ERR_SOCKET_CLOSED "Socket is closed": the error is the stream's
// at once (writable false, a later end() does not end the stream), the
// callback gets it on the next tick, then the socket is destroyed with it
// ('error', then 'close' with no argument). A later write is buffered behind
// the failed one and gets the same error right after it; end(cb) calls back
// with it on its own tick. end() with no data on such a socket finishes.
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
  ["write(cb)", (s, cb) => cb("write returned")(s.write("x", cb("write")))],
  ["write()", (s, cb) => cb("write returned")(s.write("x"))],
  ["end(data, cb)", (s, cb) => s.end("x", cb("end"))],
  ["end(data)", (s) => s.end("x")],
  ["write(cb); write(cb)", (s, cb) => { s.write("x", cb("write1")); s.write("y", cb("write2")); }],
  ["write(cb); end(cb)", (s, cb) => { s.write("x", cb("write")); s.end(cb("end")); }],
  ["write(cb); end(data, cb)", (s, cb) => { s.write("x", cb("write")); s.end("y", cb("end")); }],
  ["write(cb); destroy()", (s, cb) => { s.write("x", cb("write")); s.destroy(); }],
  ["write(cb); destroy(err)", (s, cb) => { s.write("x", cb("write")); s.destroy(new Error("boom")); }],
  [
    "write(cb); end(cb) with a tick between",
    (s, cb) => {
      s.write("x", cb("write"));
      process.nextTick(cb("tick"));
      s.end(cb("end"));
    },
  ],
];

for (const [name, act] of rows) {
  const socket = new net.Socket();
  const log = [];
  let sync = true;
  const cb = (label) => (v) => log.push(`${label}${sync ? " (sync)" : ""} ${describe(v)}`);
  socket.on("error", (e) => log.push(`error ${describe(e)}`));
  socket.on("finish", () => log.push("finish"));
  const closed = new Promise((resolve) => socket.once("close", (hadError) => {
    log.push(`close(${hadError})`);
    resolve();
  }));
  act(socket, cb);
  sync = false;
  log.push(
    `[writable ${socket.writable} ended ${socket.writableEnded} destroyed ${socket.destroyed}]`,
  );
  await closed;
  await new Promise((resolve) => setTimeout(resolve, 50));
  log.push(`[errored ${describe(socket._writableState.errored)}]`);
  console.log(`${name}: ${log.join(" | ")}`);
}

// end() with nothing to write has no connection to shut down: it finishes,
// and the socket is neither errored nor destroyed.
for (const [name, act] of [
  ["end(cb)", (s, cb) => s.end(cb("end"))],
  ["end(cb); end(cb)", (s, cb) => { s.end(cb("end1")); s.end(cb("end2")); }],
]) {
  const socket = new net.Socket();
  const log = [];
  const cb = (label) => (v) => log.push(`${label} ${describe(v)}`);
  socket.on("error", (e) => log.push(`error ${describe(e)}`));
  socket.on("finish", () => log.push("finish"));
  act(socket, cb);
  await new Promise((resolve) => setTimeout(resolve, 50));
  log.push(`[ended ${socket.writableEnded} finished ${socket.writableFinished} destroyed ${socket.destroyed}]`);
  console.log(`${name}: ${log.join(" | ")}`);
}
