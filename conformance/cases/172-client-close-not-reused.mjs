// A connection the client asked to close is not used again. node's client
// sends `Connection: close` for a socket it will not keep -- agent: false, a
// `new http.Agent()` with no options, a caller-set `Close` -- and destroys the
// socket once the response is read, whether or not the server said close
// back (RFC 9112 s9.6). So against a server that never closes and never
// echoes the header, every such request reaches it on a connection of its
// own.
//
// Regression guard: oam's transport put such a connection back in its pool
// when the response did not say close, and the next request went out on a
// connection the server was entitled to be closing -- a POST failed with
// ECONNRESET, a GET was sent twice. The server here never closes, so a
// runtime that reuses the connection shows as FEWER connections than
// requests, deterministically.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 55000).unref();

// A raw server: answers every request with a body and NO Connection header,
// keeps every connection open, counts the connections it accepted and the
// requests it saw.
let connections = 0;
let requests = 0;
const server = net.createServer((sock) => {
  connections++;
  let buf = "";
  sock.on("error", () => {});
  sock.on("data", (d) => {
    buf += d;
    for (;;) {
      const end = buf.indexOf("\r\n\r\n");
      if (end < 0) return;
      const head = buf.slice(0, end);
      const len = Number((/content-length:\s*(\d+)/i.exec(head) || [0, 0])[1]);
      if (buf.length < end + 4 + len) return;
      buf = buf.slice(end + 4 + len);
      requests++;
      sock.write("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    }
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

const ask = (opts, body) =>
  new Promise((resolve) => {
    const req = http.request({ host: "127.0.0.1", port, path: "/", ...opts }, (res) => {
      res.resume();
      res.on("end", () => resolve(res.statusCode));
    });
    req.on("error", (e) => resolve("ERROR " + (e.code || e.message)));
    req.end(body);
  });

async function group(label, count, opts, body) {
  const before = connections;
  const statuses = [];
  for (let i = 0; i < count; i++) statuses.push(await ask(typeof opts === "function" ? opts() : opts, body));
  console.log(
    label + ": statuses " + statuses.join(",") +
      ", new connections " + (connections - before) + " for " + count + " requests",
  );
}

await group("agent false, POST", 4, { agent: false, method: "POST", headers: { "content-length": 5 } }, "hello");
await group("agent false, GET", 3, { agent: false });
await group("new http.Agent() (no options)", 3, () => ({ agent: new http.Agent() }));
await group("agent false, caller-set Connection: Close", 2, { agent: false, headers: { connection: "Close" } });
console.log("requests the server saw: " + requests);

// And the contrast: a keep-alive agent DOES share one connection.
{
  const agent = new http.Agent({ keepAlive: true, maxSockets: 1 });
  const before = connections;
  await ask({ agent });
  await ask({ agent });
  console.log("keep-alive agent, two requests: new connections " + (connections - before));
  agent.destroy();
}

server.close();
process.exit(0);
