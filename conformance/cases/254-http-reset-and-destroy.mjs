// socket.resetAndDestroy() on the sockets node's http and https servers
// hand out. A server's req.socket and its 'connection' socket reset the
// connection: the client's read fails with `read ECONNRESET`, the request is
// aborted, and the socket closes saying no error. An https connection's
// socket is a TLSSocket and refuses with ERR_INVALID_HANDLE_TYPE.
//
// Each object's events are kept apart and printed once everything has
// closed: the order across objects is the scheduler's.
import http from "node:http";
import https from "node:https";
import net from "node:net";

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

const out = [];
const log = (line) => out.push(line);
const describe = (e) =>
  `${e.code} | ${e.message} | syscall ${e.syscall} | errno ${typeof e.errno}`;

// One object's events, in its own order; `closed` settles on its 'close'.
function record(emitter, names) {
  const events = [];
  const handlers = {
    aborted: () => events.push("aborted"),
    end: () => events.push("end"),
    error: (e) => events.push(`error ${describe(e)}`),
    data: () => events.push("data"),
  };
  for (const name of names) emitter.on(name, handlers[name]);
  const closed = new Promise((resolve) =>
    emitter.on("close", (hadError) => {
      events.push(hadError === undefined ? "close" : `close ${hadError}`);
      resolve();
    }));
  return { events, closed };
}

const resetFn = net.Socket.prototype.resetAndDestroy;
function reset(socket) {
  const rv = socket.resetAndDestroy();
  log(`  returns the socket ${rv === socket}, destroyed ${socket.destroyed}, ` +
    `readable ${socket.readable}, writable ${socket.writable}`);
  log(`  again returns the socket ${socket.resetAndDestroy() === socket}`);
}

const listen = (server) =>
  new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server.address().port)));

// A raw client that sends `request` (if any) and records what comes back.
function rawClient(port, request) {
  const client = net.connect(port, "127.0.0.1", () => {
    if (request) client.write(request);
  });
  return record(client, ["data", "end", "error"]);
}

// 1. A server's req.socket, while the request body is still arriving.
{
  log("--- http server: req.socket.resetAndDestroy()");
  let inHandler;
  const handled = new Promise((resolve) => { inHandler = resolve; });
  const server = http.createServer((req, res) => {
    const socket = req.socket;
    log(`  instanceof net.Socket ${socket instanceof net.Socket}, ` +
      `node's function ${socket.resetAndDestroy === resetFn}`);
    const recs = {
      socket: record(socket, ["error"]),
      req: record(req, ["aborted", "error"]),
      res: record(res, ["error"]),
    };
    reset(socket);
    inHandler(recs);
  });
  const port = await listen(server);
  const client = rawClient(port, "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabc");
  const recs = await handled;
  await Promise.all([client.closed, recs.socket.closed, recs.req.closed, recs.res.closed]);
  for (const [name, rec] of Object.entries(recs)) log(`  ${name}: ${rec.events.join(", ")}`);
  log(`  client: ${client.events.join(", ")}`);
  server.close();
}

// 2. The socket a server's 'connection' listener gets: reset before a byte
// is read, so no request reaches the handler.
{
  log("--- http server: the 'connection' socket");
  let requests = 0;
  const server = http.createServer(() => { requests++; });
  let socketRec;
  server.on("connection", (socket) => {
    socketRec = record(socket, ["error"]);
    reset(socket);
  });
  const port = await listen(server);
  const client = rawClient(port, null);
  await client.closed;
  await socketRec.closed;
  log(`  socket: ${socketRec.events.join(", ")}`);
  log(`  client: ${client.events.join(", ")}`);
  log(`  requests handled ${requests}`);
  server.close();
}

// 3. An https connection's socket is a TLSSocket: the call throws, and the
// exchange carries on.
{
  log("--- https server: req.socket refuses");
  const server = https.createServer({ cert: CERT, key: KEY }, (req, res) => {
    try {
      req.socket.resetAndDestroy();
      log("  did not throw");
    } catch (e) {
      log(`  threw ${e.name} ${e.code} | ${e.message}`);
    }
    log(`  destroyed ${req.socket.destroyed}`);
    res.end("ok");
  });
  const port = await listen(server);
  const body = await new Promise((resolve, reject) => {
    https.get({ host: "127.0.0.1", port, servername: "localhost", ca: CA, agent: false }, (res) => {
      let text = "";
      res.setEncoding("utf8");
      res.on("data", (chunk) => { text += chunk; });
      res.on("end", () => resolve(`${res.statusCode} ${text}`));
    }).on("error", reject);
  });
  log(`  client got ${body}`);
  server.close();
}

process.stdout.write(out.join("\n") + "\n");
