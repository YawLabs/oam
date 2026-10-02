// end(cb) and write(cb) made BEFORE destroy() on a socket that has not
// connected: what each callback is handed, and when relative to 'error' and
// 'close'.
//
// Regression guard, three ways oam differed from node v22.22.2:
// - A socket with no connection and no connect in flight (new net.Socket()):
//   node's _final has nothing to shut down, so end() finishes on the next
//   tick -- the callback gets null and 'finish' is emitted, a destroy(err)
//   right after notwithstanding. oam handed the callback the destroy error.
// - A socket still connecting (or looking its name up): end()'s callbacks
//   get the stream's error, or ERR_STREAM_DESTROYED "Cannot call end after
//   a stream was destroyed", on the next tick (errorBuffer) -- oam's got
//   ERR_SOCKET_CLOSED_BEFORE_CONNECTION. A write held behind the connect
//   fails from a 'close' listener the write added, so after 'close' (and
//   after the 'close' listeners added before the write, before those added
//   after it), with ERR_SOCKET_CLOSED_BEFORE_CONNECTION; the writes behind
//   it and the end() callbacks then get the stream's error -- the destroy
//   error when there is one. oam ran them all before 'error' and 'close',
//   end()'s first.
// - A tls.TLSSocket over a net.Socket with no connection is `connecting`
//   until that socket connects (node's _init: `socket.connecting ||
//   !socket._handle`), so it behaves as one built with no socket at all.
//   oam's was not connecting, so end() finished and a write failed with
//   ERR_SOCKET_CLOSED.
import net from "node:net";
import tls from "node:tls";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const describe = (v) => {
  if (v === undefined || v === null) return String(v);
  if (typeof v !== "object") return String(v);
  return `${v.constructor.name}${v.code ? ` ${v.code}` : ""}: ${v.message}`;
};

const server = net.createServer((peer) => peer.on("error", () => {}));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

const kinds = [
  ["fresh", () => new net.Socket()],
  ["connecting", () => net.connect(port, "127.0.0.1")],
  ["lookup", () => net.connect(port, "localhost")],
  ["tls over fresh", () => new tls.TLSSocket(new net.Socket())],
  ["tls bare", () => new tls.TLSSocket()],
];

const rows = [
  ["end(cb); destroy(err)", (s, cb) => { s.end(cb("end")); s.destroy(new Error("boom")); }],
  ["end(cb); destroy()", (s, cb) => { s.end(cb("end")); s.destroy(); }],
  ["end(cb); end(cb); destroy(err)", (s, cb) => { s.end(cb("end1")); s.end(cb("end2")); s.destroy(new Error("boom")); }],
  ["end(data, cb); destroy(err)", (s, cb) => { s.end("x", cb("end")); s.destroy(new Error("boom")); }],
  ["write(cb); end(cb); destroy(err)", (s, cb) => { s.write("x", cb("write")); s.end(cb("end")); s.destroy(new Error("boom")); }],
  ["write(cb); end(cb); destroy()", (s, cb) => { s.write("x", cb("write")); s.end(cb("end")); s.destroy(); }],
  [
    "write(cb); write(cb); end(cb); destroy(err)",
    (s, cb) => { s.write("x", cb("write1")); s.write("y", cb("write2")); s.end(cb("end")); s.destroy(new Error("boom")); },
  ],
  [
    "close listener; write(cb); close listener; destroy()",
    (s, cb) => {
      s.on("close", cb("close listener before"));
      s.write("x", cb("write"));
      s.on("close", cb("close listener after"));
      s.destroy();
    },
  ],
  [
    "write(cb); destroy(); end(cb)",
    (s, cb) => { s.write("x", cb("write")); s.destroy(); s.end(cb("end")); },
  ],
];

for (const [kind, make] of kinds) {
  for (const [name, act] of rows) {
    const socket = make();
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
    log.push(`[ended ${socket.writableEnded} destroyed ${socket.destroyed}]`);
    await closed;
    await new Promise((resolve) => setTimeout(resolve, 50));
    console.log(`${kind} ${name}: ${log.join(" | ")}`);
  }
}

// The held write's 'close' listener is node's, one per connect, taken off
// when the socket connects.
{
  const socket = net.connect(port, "127.0.0.1");
  const before = socket.listenerCount("close");
  socket.write("a");
  socket.write("b");
  const held = socket.listenerCount("close");
  await new Promise((resolve) => socket.once("connect", resolve));
  console.log(`close listeners: before ${before}, while held ${held - before}, connected ${socket.listenerCount("close") - before}`);
  socket.destroy();
}

server.close();
