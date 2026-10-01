// A port that is not a port. Node validates it synchronously
// (internal/validators validatePort) wherever a socket is given one --
// net.connect, tls.connect, every server's listen(), dgram's send() -- and
// throws RangeError ERR_SOCKET_BAD_PORT; a value that is not even a number
// or a string is a TypeError, and a connect with neither a port nor a path
// is ERR_MISSING_ARGS.
//
// Regression guard (#163): oam checked none of it in JS, and the ops cast
// the value with `as u16`, so a bad port was dialled or bound as a
// DIFFERENT, valid one: 65536 and 70000 as 65535, 1.5 as 1, 'abc' as 0. A
// config typo talked to (or bound) whatever port that was instead of
// failing.
//
// Nothing here dials or binds a port the case does not own: a connect that
// node accepts is destroyed on the same tick, before anything is opened,
// and the one listen on a named port uses a port this process has just
// been given.
import dgram from "node:dgram";
import http from "node:http";
import http2 from "node:http2";
import net from "node:net";
import tls from "node:tls";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

function attempt(label, fn) {
  try {
    const made = fn();
    console.log(`${label}: accepted`);
    return made;
  } catch (e) {
    console.log(`${label}: ${e.name} ${e.code} | ${e.message}`);
    return undefined;
  }
}

function discard(socket) {
  if (!socket) return;
  socket.on("error", () => {});
  socket.destroy();
}

const show = (v) => (typeof v === "string" ? JSON.stringify(v) : String(v));

console.log("--- net.connect / tls.connect");
const CONNECT_PORTS = [
  -1, 65536, 70000, 1.5, NaN, Infinity, "abc", "80x", "", " ", "-1", "65536",
  null, true, {}, [],
  0, 65535, "80", "0x50", " 80 ",
];
for (const port of CONNECT_PORTS) {
  discard(attempt(`net.connect({ port: ${show(port)} })`, () =>
    net.connect({ host: "127.0.0.1", port })));
  discard(attempt(`tls.connect({ port: ${show(port)} })`, () =>
    tls.connect({ host: "127.0.0.1", port })));
}
discard(attempt("net.connect(65536, host)", () => net.connect(65536, "127.0.0.1")));
discard(attempt("net.createConnection(1.5)", () => net.createConnection(1.5)));
discard(attempt("new net.Socket().connect(-1, host)", () =>
  new net.Socket().connect(-1, "127.0.0.1")));
discard(attempt("tls.connect(65536, host)", () => tls.connect(65536, "127.0.0.1")));
discard(attempt("net.connect({})", () => net.connect({})));
discard(attempt("net.connect({ host })", () => net.connect({ host: "127.0.0.1" })));
discard(attempt("tls.connect({ host })", () => tls.connect({ host: "127.0.0.1" })));
{
  // A socket whose connect() threw is not left half-connected by oam's own
  // bookkeeping: node reports `connecting` true (it throws after the flag
  // is set), and both destroy cleanly.
  const socket = new net.Socket();
  attempt("socket.connect(70000)", () => socket.connect(70000, "127.0.0.1"));
  console.log("after the throw: connecting", socket.connecting, "destroyed", socket.destroyed);
  discard(socket);
}

console.log("--- server.listen");
const SERVERS = [
  ["net", () => net.createServer()],
  ["tls", () => tls.createServer({})],
  ["http", () => http.createServer()],
  ["http2", () => http2.createServer()],
];
const LISTEN_PORTS = [-1, 65536, 1.5, NaN, "", "65536", true, {}];
for (const [kind, make] of SERVERS) {
  for (const port of LISTEN_PORTS) {
    const server = make();
    server.on("error", () => {});
    const listening = attempt(`${kind} listen(${show(port)}, host)`, () =>
      server.listen(port, "127.0.0.1"));
    if (listening) server.close();
  }
  for (const port of [-1, 70000, 1.5, "80x", true]) {
    const server = make();
    server.on("error", () => {});
    const listening = attempt(`${kind} listen({ port: ${show(port)} })`, () =>
      server.listen({ port, host: "127.0.0.1" }));
    if (listening) server.close();
  }
  attempt(`${kind} listen({ host })`, () => make().listen({ host: "127.0.0.1" }));
}

// What node accepts: no port, null, an explicit undefined and a callback
// first are all "any free port"; a numeric string is that port.
function listening(label, kind, make, args) {
  return new Promise((resolve) => {
    const server = make();
    server.on("error", (e) => {
      console.log(`${label}: error ${e.code}`);
      resolve(null);
    });
    try {
      server.listen(...args(), () => {
        const { port } = server.address();
        server.close(() => resolve(port));
      });
    } catch (e) {
      console.log(`${label}: ${e.name} ${e.code}`);
      resolve(null);
    }
  });
}

for (const [kind, make] of SERVERS) {
  for (const [what, args] of [
    ["listen(cb)", () => []],
    ["listen(null, host, cb)", () => [null, "127.0.0.1"]],
    ["listen(undefined, host, cb)", () => [undefined, "127.0.0.1"]],
    ["listen({ port: undefined, host }, cb)", () => [{ port: undefined, host: "127.0.0.1" }]],
    ["listen({ port: null, host }, cb)", () => [{ port: null, host: "127.0.0.1" }]],
    ["listen(0, host, cb)", () => [0, "127.0.0.1"]],
  ]) {
    const port = await listening(`${kind} ${what}`, kind, make, args);
    console.log(`${kind} ${what}: listening on a port of its own: ${port > 0}`);
  }
  // A port named as a string: take one the OS just gave out, then ask for
  // it by its spelling.
  const free = await listening(`${kind} free port`, kind, make, () => [0, "127.0.0.1"]);
  const named = await listening(`${kind} listen('<port>')`, kind, make, () => [String(free), "127.0.0.1"]);
  console.log(`${kind} listen('<port>', host, cb): the port it named: ${named === free}`);
}

console.log("--- dgram send");
const udp = dgram.createSocket("udp4");
udp.on("error", () => {});
for (const port of [0, -1, 65536, 1.5, NaN, "abc", "", null, undefined, true]) {
  attempt(`send(msg, ${show(port)}, host)`, () => udp.send("x", port, "127.0.0.1"));
}
attempt("send(msg, 0, 1, 65536, host)", () => udp.send("x", 0, 1, 65536, "127.0.0.1"));
// A valid port, spelled as a number and as a string, to a socket this
// process owns.
const sink = dgram.createSocket("udp4");
await new Promise((resolve) => sink.bind(0, "127.0.0.1", resolve));
const sinkPort = sink.address().port;
let received = 0;
const both = new Promise((resolve) => {
  sink.on("message", () => {
    received += 1;
    if (received === 2) resolve();
  });
});
await new Promise((resolve) => udp.send("x", sinkPort, "127.0.0.1", resolve));
await new Promise((resolve) => udp.send("x", String(sinkPort), "127.0.0.1", resolve));
await both;
console.log("send to a port as a number and as a string: delivered", received);
udp.close();
sink.close();
