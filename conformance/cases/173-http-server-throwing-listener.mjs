// A listener that throws does not stop an http server. node raises the
// throw as 'uncaughtException' and goes on serving; an application that
// installs process.on('uncaughtException') to log and carry on keeps
// answering. That has to hold for every listener the server calls in the
// course of a connection: a 'request' handler, an 'upgrade' listener, a
// 'connect' listener, and a 'close' listener on the socket 'connection'
// handed out.
//
// Regression guard: oam dispatched those from one loop with no guard around
// the 'request', 'upgrade' and 'connect' emits, nor around the 'close' the
// connection's stand-in socket emits at an upgrade handover. A throw ended
// the loop, after which every later connection was accepted and then never
// dispatched -- the server was up, and answered nothing. 0.16.4's Security
// entry closed the same hole for 'connection' / 'secureConnection' only.
//
// Each scenario: install the throwing listener, trigger it, wait for the
// uncaught exception, remove the listener, then make an ordinary request and
// print whether it was answered. What differs between runtimes -- WHEN the
// stand-in's 'close' fires relative to the upgrade -- is kept out of the
// output: only the set of messages that surfaced and the answer after.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 55000).unref();

const uncaught = [];
process.on("uncaughtException", (e) => uncaught.push(e.message));
const tick = (ms) => new Promise((r) => setTimeout(r, ms));

const server = http.createServer((req, res) => {
  if (req.url === "/boom") throw new Error("request boom");
  res.end("ok");
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

// An ordinary request, bounded: resolves with the status, or "no answer".
const plain = () =>
  new Promise((resolve) => {
    const req = http.request({ host: "127.0.0.1", port, path: "/", agent: false }, (res) => {
      res.resume();
      res.on("end", () => resolve(res.statusCode));
    });
    req.on("error", () => resolve("no answer"));
    req.setTimeout(3000, () => { req.destroy(); resolve("no answer"); });
    req.end();
  });
// A raw request whose answer nobody expects: sent, held 200 ms, dropped.
const raw = (head) =>
  new Promise((resolve) => {
    const c = net.connect(port, "127.0.0.1", () => c.write(head));
    c.on("error", () => {});
    setTimeout(() => { c.destroy(); resolve(); }, 200);
  });
async function report(label) {
  await tick(50);
  console.log(label + ": uncaught " + JSON.stringify(uncaught.sort()) + ", request after: " + (await plain()));
  uncaught.length = 0;
}

// 1. A 'request' handler that throws.
await raw("GET /boom HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
await report("request handler throws");

// 2. An 'upgrade' listener that throws.
const badUpgrade = () => { throw new Error("upgrade boom"); };
server.on("upgrade", badUpgrade);
await raw("GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: probe\r\nConnection: Upgrade\r\n\r\n");
server.off("upgrade", badUpgrade);
await report("upgrade listener throws");

// 3. A 'connect' listener that throws.
const badConnect = () => { throw new Error("connect boom"); };
server.on("connect", badConnect);
await raw("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n");
server.off("connect", badConnect);
await report("connect listener throws");

// 4. A 'close' listener on the 'connection' socket that throws, on a
// connection that is then upgraded. On node the socket handed to
// 'connection' is the upgraded socket, so its 'close' fires when that
// closes; on oam the stand-in closes at the handover. Either way the throw
// surfaces and the server goes on.
const onConn = (socket) => { socket.on("close", () => { throw new Error("close boom"); }); };
server.on("connection", onConn);
const takeIt = (_req, socket) => { socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: probe\r\nConnection: Upgrade\r\n\r\n"); socket.end(); };
server.on("upgrade", takeIt);
await raw("GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: probe\r\nConnection: Upgrade\r\n\r\n");
server.off("connection", onConn);
server.off("upgrade", takeIt);
await report("connection socket 'close' listener throws");

server.close();
process.exit(0);
