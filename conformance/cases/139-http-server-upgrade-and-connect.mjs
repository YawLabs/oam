// Which requests an http server hands to its 'upgrade' and 'connect'
// listeners, as node decides it:
//
// - a request with an `Upgrade` header and `upgrade` in `Connection` goes to
//   'upgrade' while the server has an 'upgrade' listener -- checked for each
//   request, so a listener added after listen() counts and one removed stops
//   counting -- and is an ordinary request when it has none;
// - every CONNECT goes to 'connect', with the authority as req.url, and its
//   socket is destroyed when there is no 'connect' listener;
// - any request on a keep-alive connection can be either; the head decides,
//   however the client splits it across writes and however long it is (a
//   request that arrives in the same read as an earlier one is left out:
//   node hands the socket over before that earlier request is answered);
// - the bytes the client sent after the head are the listener's `head`.
//
// oam decided from the first bytes of a connection, peeked once: a head split
// across two writes or over 8 KiB was an ordinary request, only a
// connection's first request could be an upgrade, an upgrade with no
// listener was closed without an answer, and CONNECT went to the 'request'
// handler (req.url '') where a catch-all handler answered it.
import http from "node:http";
import net from "node:net";

const log = (...parts) => console.log(parts.join(" | "));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Write `writes` ([delay ms, text] in turn), then report the status lines
// the server sent and whether it closed the connection.
function exchange(port, writes, hold = 700) {
  return new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1");
    let raw = "";
    let settled = false;
    const done = (closed) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      const statuses = [...raw.matchAll(/HTTP\/1\.1 \d{3}[^\r\n]*/g)].map((m) => m[0]);
      resolve(`${statuses.join(" + ") || "(nothing)"}, ${closed ? "closed" : "open"}`);
    };
    const timer = setTimeout(() => done(false), hold);
    socket.on("data", (d) => (raw += d.toString("latin1")));
    socket.on("close", () => done(true));
    socket.on("error", () => {});
    socket.on("connect", async () => {
      for (const [delay, text] of writes) {
        if (delay) await sleep(delay);
        if (!socket.destroyed) socket.write(text);
      }
    });
  });
}

const seen = [];
const server = http.createServer((req, res) => {
  seen.push(`request ${req.method} ${req.url} upgrade=${req.upgrade}`);
  req.resume();
  req.on("end", () => res.end("ok"));
});
const onUpgrade = (req, socket, head) => {
  seen.push(
    `upgrade ${req.method} ${req.url} upgrade=${req.upgrade} head=${JSON.stringify(head.toString("latin1"))} ` +
      `headers=${Object.keys(req.headers)} raw=${req.rawHeaders.filter((_, i) => i % 2 === 0)} ` +
      `connection=${req.headers.connection} same=${req.socket === socket}`,
  );
  socket.end("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n");
};
const onConnect = (req, socket, head) => {
  seen.push(`connect ${req.method} ${req.url} upgrade=${req.upgrade} head=${JSON.stringify(head.toString("latin1"))}`);
  socket.end("HTTP/1.1 403 Forbidden\r\n\r\n");
};
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

const UP = "GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
const H2C = "GET /h2c HTTP/1.1\r\nHost: x\r\nConnection: Upgrade, HTTP2-Settings\r\nUpgrade: h2c\r\nHTTP2-Settings: AAMAAABkAAQCAAAAAAIAAAAA\r\n\r\n";
const GET = "GET /plain HTTP/1.1\r\nHost: x\r\n\r\n";
const CONNECT = "CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n";

async function run(name, writes) {
  seen.length = 0;
  const outcome = await exchange(port, writes);
  await sleep(20);
  log(name, outcome, seen.join(" ; ") || "no handler");
}

await run("upgrade, no listener", [[0, UP]]);
await run("h2c offer, no listener", [[0, H2C]]);
await run("CONNECT, no listener", [[0, CONNECT + "tunnel"]]);
await run("CONNECT after a request, no listener", [[0, GET], [100, CONNECT]]);

server.on("upgrade", onUpgrade);
server.on("connect", onConnect);
await run("upgrade", [[0, UP]]);
await run("upgrade with bytes after the head", [[0, UP + "EXTRA"]]);
await run("upgrade head in two writes", [[0, UP.slice(0, 25)], [60, UP.slice(25)]]);
await run("upgrade head in bytes", [...UP].map((c, i) => [i % 16 === 0 ? 5 : 0, c]));
await run("upgrade after a request", [[0, GET], [100, UP]]);
await run("upgrade with a 9 KiB head", [[0, UP.replace("\r\n\r\n", `\r\nX-A: ${"a".repeat(9000)}\r\n\r\n`)]]);
await run("upgrade, leading empty line", [[0, "\r\n" + UP]]);
await run("Connection without upgrade", [[0, GET.replace("\r\n\r\n", "\r\nConnection: keep-alive\r\nUpgrade: x\r\n\r\n")]]);
await run("Upgrade without Connection", [[0, GET.replace("\r\n\r\n", "\r\nUpgrade: x\r\n\r\n")]]);
await run("CONNECT", [[0, CONNECT + "tunnel"]]);
await run("CONNECT in two writes", [[0, CONNECT.slice(0, 12)], [60, CONNECT.slice(12) + "tunnel"]]);
await run("CONNECT after a request", [[0, GET], [100, CONNECT]]);
await run("CONNECT with a length", [[0, CONNECT.replace("\r\n\r\n", "\r\nContent-Length: 3\r\n\r\n") + "abcdef"]]);

server.removeListener("upgrade", onUpgrade);
await run("upgrade, listener removed", [[0, UP]]);
server.once("upgrade", onUpgrade);
await run("upgrade, once listener", [[0, UP]]);
await run("upgrade, once listener used", [[0, UP]]);
server.prependListener("upgrade", onUpgrade);
await run("upgrade, prepended listener", [[0, UP]]);
server.removeAllListeners("upgrade");
await run("upgrade, all removed", [[0, UP]]);
server.removeAllListeners("connect");
await run("CONNECT, all removed", [[0, CONNECT]]);
server.close();

// A listener added in the 'listening' callback counts from the first request.
{
  const late = http.createServer((req, res) => res.end("ok"));
  const events = [];
  await new Promise((resolve) =>
    late.listen(0, "127.0.0.1", () => {
      late.on("upgrade", (req, socket) => {
        events.push(`upgrade ${req.url}`);
        socket.end("HTTP/1.1 101 Switching Protocols\r\n\r\n");
      });
      resolve();
    }),
  );
  const outcome = await exchange(late.address().port, [[0, UP]]);
  log("listener added on 'listening'", outcome, events.join(" ; "));
  late.close();
}
