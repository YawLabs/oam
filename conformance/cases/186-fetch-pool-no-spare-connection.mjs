// fetch's client pool opens a connection only to send on it (undici's model),
// so a node:http server in the same process never sees a connection that
// carried zero requests, and its `close()` is not held by such a spare. Before
// #216 oam's hyper-util pool raced a fresh connect against a pooled checkout and
// parked the loser idle, so a mix of sequential and concurrent fetches left the
// server holding an extra, never-written-to connection until the pool's 90 s
// idle timeout -- and `server.close()` with it. Printed: the number of server
// connections that carried no request (the spare), and whether `close()`'s
// callback fires promptly. Measured on node v22.22.2 (#216).
import http from "node:http";

const watchdog = setTimeout(() => { console.log("WATCHDOG"); process.exit(9); }, 20000);

// Count the requests each accepted connection carries.
const perConnection = new WeakMap();
let connections = [];
const server = http.createServer((req, res) => {
  const n = (perConnection.get(req.socket) || 0) + 1;
  perConnection.set(req.socket, n);
  res.end("ok");
});
server.on("connection", (socket) => {
  connections.push(socket);
  perConnection.set(socket, perConnection.get(socket) || 0);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = "http://127.0.0.1:" + server.address().port + "/";

// Four sequential fetches, then two concurrent -- the shape that produced the
// spare. Every one drains its body so its connection is free to be reused.
for (let i = 0; i < 4; i++) {
  const res = await fetch(base);
  await res.text();
}
await Promise.all([
  fetch(base).then((r) => r.text()),
  fetch(base).then((r) => r.text()),
]);

const spares = connections.filter((s) => (perConnection.get(s) || 0) === 0).length;
console.log("spare connections (carried no request): " + spares);

// `close()` must not be held by a spare. Give it a fixed window, then report
// whether it fired -- deterministic whether it is prompt or held.
let closed = false;
server.close(() => { closed = true; });
await new Promise((r) => setTimeout(r, 3000));
console.log("close callback fired within 3s: " + closed);

clearTimeout(watchdog);
process.exit(0);
