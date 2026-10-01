// When an http or https server's connection closes under an exchange, the
// events come in node's order: the connection's socket closes, and its
// 'close' listeners run in the order they were added -- the server's own
// first (the request's 'aborted'), then a 'connection' listener's, then the
// response's (its 'close' without 'finish'), then the handler's. The
// request's 'error' and 'close' follow on the next tick. On an https server
// the socket under the TLS socket closes first. This holds whether the
// handler destroys the socket (destroy(), destroy(err), resetAndDestroy()),
// destroys the request while the response is under way, or the client goes
// away after the response has started.
import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const CA = `-----BEGIN CERTIFICATE-----
MIIBmjCCAUGgAwIBAgIUHjF3aO/Nr2SNMEQNV9GNuumIljswCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAaMRgwFgYDVQQDDA9vYW0gaDJzIHRlc3QgQ0EwWTATBgcq
hkjOPQIBBggqhkjOPQMBBwNCAAR6EfahtynuI8VLuixWn6GiZ3BYWFdJEqP1FfLE
lCBVF/69Rm6fDrzSVP/GWO7qsNhAZmyIVWyRQJcQiBv55omto2MwYTAdBgNVHQ4E
FgQUOlIo6O4tIFNjD7vXJV51FU2DLQcwHwYDVR0jBBgwFoAUOlIo6O4tIFNjD7vX
JV51FU2DLQcwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYwCgYIKoZI
zj0EAwIDRwAwRAIgFFCfCAiuzT1cHBF7zAQEVxSrWsoco8cOD49S6whO4vsCIC/T
xtSxdoSsByDfaJz7qxOrhJzSD5lDwUdNMe3EoP9l
-----END CERTIFICATE-----
`;
const CERT = `-----BEGIN CERTIFICATE-----
MIIBvjCCAWWgAwIBAgIUOy7BLDqzc+0IZz2NWG95hnXgrd4wCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIB
BggqhkjOPQMBBwNCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0bj0ngz5dpnyIRlajs
UptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1Vo4GMMIGJMBoGA1UdEQQTMBGC
CWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDATBgNVHSUE
DDAKBggrBgEFBQcDATAdBgNVHQ4EFgQUmxnUU2rP4FwgoXrkCkeRxNgCVycwHwYD
VR0jBBgwFoAUOlIo6O4tIFNjD7vXJV51FU2DLQcwCgYIKoZIzj0EAwIDRwAwRAIg
ItB5f9aIsf9D8cXBvJvvr5ahB57RK7DgAsIVf5uJ0zcCIBPOR2Z+ycbeeByMKH2v
shKfeR1QdaoQHwJKJln0q1fo
-----END CERTIFICATE-----
`;
const KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgQLidYpqFITu5wno8
Fw5b5Ahrg5eTwH0UqA7RU57egNKhRANCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0b
j0ngz5dpnyIRlajsUptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1V
-----END PRIVATE KEY-----
`;

const POST = "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabc";
const GET = "GET / HTTP/1.1\r\nHost: x\r\n\r\n";

const listen = (server) =>
  new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server.address().port)));

// One exchange, every event in the order it came. `aborts` says whether the
// request is aborted (it has not been read to the end), so its 'error' and
// 'close' are part of the story; a request that was read to the end is left
// out of it.
async function exchange(label, { secure = false, request, aborts, handle, client: act }) {
  const events = [];
  const waits = [];
  const settles = (emitter, name) =>
    waits.push(new Promise((resolve) => emitter.once(name, resolve)));
  let handled;
  const inHandler = new Promise((resolve) => { handled = resolve; });
  const handler = (req, res) => {
    const socket = req.socket;
    socket.on("close", (hadError) => events.push(`socket close ${hadError}`));
    socket.on("error", (e) => events.push(`socket error ${e.message}`));
    req.on("aborted", () => events.push("req aborted"));
    res.on("close", () => events.push("res close"));
    res.on("finish", () => events.push("res finish"));
    settles(socket, "close");
    settles(res, "close");
    if (aborts) {
      req.on("error", (e) => events.push(`req error ${e.code} ${e.message}`));
      req.on("close", () => events.push("req close"));
      settles(req, "close");
    }
    handle(req, res);
    handled();
  };
  const server = secure
    ? https.createServer({ cert: CERT, key: KEY }, handler)
    : http.createServer(handler);
  server.on("connection", (socket) =>
    socket.on("close", (hadError) => events.push(`'connection' socket close ${hadError}`)));
  server.on("secureConnection", (socket) =>
    socket.on("close", (hadError) => events.push(`'secureConnection' socket close ${hadError}`)));
  const port = await listen(server);
  const client = secure
    ? tls.connect({ port, host: "127.0.0.1", servername: "localhost", ca: CA })
    : net.connect(port, "127.0.0.1");
  client.on("error", () => {});
  client.write(request);
  if (act) act(client);
  await inHandler;
  await Promise.all(waits);
  // Anything still to come on these objects comes on the next ticks.
  await new Promise((resolve) => setImmediate(resolve));
  console.log(`--- ${label}`);
  for (const line of events) console.log(`  ${line}`);
  client.destroy();
  server.close();
}

await exchange("http: socket.destroy() while the body arrives", {
  request: POST,
  aborts: true,
  handle: (req) => req.socket.destroy(),
});
await exchange("http: socket.destroy(err) while the body arrives", {
  request: POST,
  aborts: true,
  handle: (req) => req.socket.destroy(new Error("boom")),
});
await exchange("http: socket.resetAndDestroy() while the body arrives", {
  request: POST,
  aborts: true,
  handle: (req) => req.socket.resetAndDestroy(),
});
await exchange("http: socket.destroy() with the response under way", {
  request: POST,
  aborts: true,
  handle: (req, res) => {
    res.write("x");
    req.socket.destroy();
  },
});
await exchange("http: req.destroy() with the response under way", {
  request: POST,
  aborts: true,
  handle: (req, res) => {
    res.write("x");
    req.destroy();
  },
});
await exchange("http: req.destroy(err) while the body arrives", {
  request: POST,
  aborts: true,
  handle: (req) => req.destroy(new Error("boom")),
});
await exchange("http: the client goes away after the response started", {
  request: GET,
  aborts: false,
  handle: (req, res) => {
    req.resume();
    res.writeHead(200, { "content-length": 100 });
    res.write("x");
  },
  client: (client) => client.once("data", () => client.destroy()),
});
await exchange("https: socket.destroy() while the body arrives", {
  secure: true,
  request: POST,
  aborts: true,
  handle: (req) => req.socket.destroy(),
});
await exchange("https: the client goes away after the response started", {
  secure: true,
  request: GET,
  aborts: false,
  handle: (req, res) => {
    req.resume();
    res.writeHead(200, { "content-length": 100 });
    res.write("x");
  },
  client: (client) => client.once("data", () => client.destroy()),
});
