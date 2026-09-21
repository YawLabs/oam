// The Connection header an http client sends. node's _storeHeader puts one on
// every request: `close` when the socket is not to be kept alive, `keep-alive`
// when it is, and nothing when the caller removed it. What the peer does with
// the connection follows from it -- a server holds a keep-alive connection, and
// its socket, its getConnections() count and its maxConnections slot, until its
// own keep-alive timeout fires.
//
// Regression guard: oam computed the header correctly but only wrote it on the
// raw-socket path. Over its own transport -- where `http.request` goes -- the
// value was dropped, so every request went out as an ordinary HTTP/1.1
// keep-alive one. An `agent: false` request that node has the server close as
// soon as the response is read left the connection held for keepAliveTimeout
// plus node's keepAliveTimeoutBuffer instead: measured 2007 ms against node's
// own server, where node's client got 7 ms.
//
// What the caller sees of its own headers must not change with it: node reports
// no `connection` from getHeader()/getHeaders() unless the caller set one.
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 55000).unref();

const seen = [];
const server = http.createServer((req, res) => {
  seen.push(req.headers.connection);
  res.end("ok");
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

// One request, described by what the caller passes; returns what the server saw
// and what the request reported about its own headers.
function ask(label, opts, mutate) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: "127.0.0.1", port, path: "/", ...opts }, (res) => {
      res.resume();
      res.on("end", () => {
        console.log(
          label +
            ": server saw " + JSON.stringify(seen[seen.length - 1]) +
            ", getHeader " + JSON.stringify(req.getHeader("connection")) +
            ", shouldKeepAlive " + req.shouldKeepAlive,
        );
        resolve();
      });
    });
    req.on("error", reject);
    if (mutate) mutate(req);
    req.end();
  });
}

// No agent at all: node's own default, which keeps the socket alive.
await ask("default agent", {});
// agent: false is a one-off agent that does not keep the socket.
await ask("agent false", { agent: false });
await ask("agent keepAlive true", { agent: new http.Agent({ keepAlive: true }) });
await ask("agent keepAlive false", { agent: new http.Agent({ keepAlive: false, maxSockets: 1 }) });
// A header the caller set is sent as set, and one that is not `close` keeps the
// socket alive whatever the agent said.
await ask("caller set close", { agent: false, headers: { connection: "close" } });
await ask("caller set keep-alive", { agent: false, headers: { connection: "keep-alive" } });
// A removed one is not sent at all.
await ask("caller removed it", { agent: false }, (req) => req.removeHeader("connection"));
// POST, which is chunked by default, over a one-off agent.
await ask("post agent false", { method: "POST", agent: false });

// Pooling still works: two sequential requests over one keep-alive agent reach
// the server on a single connection.
{
  const agent = new http.Agent({ keepAlive: true, maxSockets: 1 });
  const ports = new Set();
  const pooled = () =>
    new Promise((resolve, reject) => {
      const req = http.request({ host: "127.0.0.1", port, path: "/pooled", agent }, (res) => {
        res.resume();
        res.on("end", resolve);
      });
      req.on("error", reject);
      req.end();
    });
  server.on("request", (req) => ports.add(req.socket.remotePort));
  await pooled();
  await pooled();
  console.log("keep-alive agent, two requests, one connection: " + (ports.size === 1));
  agent.destroy();
}

server.close();
process.exit(0);
