// server.close() drains. node's http and https servers stop taking
// connections at close(), close the idle ones, and let the busy ones finish:
// a request in flight is answered, its connection keeps counting in
// getConnections() until it ends, and 'close' -- with close(cb)'s callback --
// comes only after the last connection has gone. An upgraded socket counts
// and holds 'close' back the same way. A second close(cb) made during the
// drain is answered ERR_SERVER_NOT_RUNNING when the drain ends. A connection
// the server accepted before close() is served -- even one whose own
// 'connection' or 'secureConnection' listener called close(), and on https
// with its handshake still to finish.
//
// Regression guard: oam's server let go of every connection at close(). The
// count read 0 at once with a request still in flight, so a drain loop that
// polls it finished early; 'close' and the callback fired about 1 ms after
// close(), so the common `server.close(() => process.exit(0))` reset the
// request it meant to finish; a request already queued for JavaScript was
// lost, its client left waiting; and a connection whose 'connection'
// listener called close() was reset before its request was read.
//
// The output is the ORDER of what happened and counts sampled at fixed points,
// never a time, so a loaded machine cannot change it. Clients close-frame
// their requests (agent: false): node keeps a BUSY keep-alive connection open
// after close() until its keep-alive timeout, oam closes it once its exchange
// is done -- a documented difference this case stays clear of.
import http from "node:http";
import https from "node:https";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 55000).unref();

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

const line = (k, v) => console.log(k + ": " + v);
const tick = (ms) => new Promise((r) => setTimeout(r, ms));
const count = (server) =>
  new Promise((resolve, reject) =>
    server.getConnections((err, n) => (err ? reject(err) : resolve(n))));
async function drained(server) {
  let n = -1;
  for (let i = 0; i < 160; i++) {
    n = await count(server);
    if (n === 0) return 0;
    await tick(25);
  }
  return n;
}
const listen = (server) => new Promise((r) => server.listen(0, "127.0.0.1", r));

// ---- a request in flight when close() runs ---------------------------------
async function inFlight(kind) {
  const isTls = kind === "https";
  const order = [];
  let release;
  const held = new Promise((r) => (release = r));
  const handler = async (req, res) => {
    order.push("handler");
    await held;
    order.push("answering");
    res.end("after-close");
  };
  const server = isTls ? https.createServer({ cert: CERT, key: KEY }, handler) : http.createServer(handler);
  server.on("close", () => order.push("server close"));
  await listen(server);
  const port = server.address().port;
  const mod = isTls ? https : http;
  const answer = new Promise((resolve) => {
    const req = mod.request(
      { host: "127.0.0.1", port, path: "/", agent: false, rejectUnauthorized: false },
      (res) => {
        let body = "";
        res.setEncoding("utf8");
        res.on("data", (d) => (body += d));
        res.on("end", () => resolve(res.statusCode + " " + body));
      },
    );
    req.on("error", (e) => resolve("ERROR " + (e.code || e.message)));
    req.end();
  });
  while (!order.includes("handler")) await tick(10);
  let cbErr = "not called";
  server.close((err) => { cbErr = err ? err.code : "none"; order.push("close(cb)"); });
  line(kind + " in-flight, count right after close()", await count(server));
  await tick(100);
  line(kind + " in-flight, count 100 ms into the drain", await count(server));
  line(kind + " in-flight, 'close' fired before the answer", order.includes("server close"));
  release();
  line(kind + " in-flight, client got", await answer);
  line(kind + " in-flight, count once it is over", await drained(server));
  for (let i = 0; i < 160 && !order.includes("close(cb)"); i++) await tick(25);
  line(kind + " in-flight, close(cb) error", cbErr);
  line(kind + " in-flight, 'answering' before 'server close'", order.indexOf("answering") < order.indexOf("server close"));
  line(kind + " in-flight, 'server close' before close(cb)", order.indexOf("server close") < order.indexOf("close(cb)"));
}
await inFlight("http");
await inFlight("https");

// ---- an idle keep-alive connection when close() runs: closed at once --------
async function idle(kind) {
  const isTls = kind === "https";
  const mod = isTls ? https : http;
  const server = isTls
    ? https.createServer({ cert: CERT, key: KEY }, (_q, s) => s.end("ok"))
    : http.createServer((_q, s) => s.end("ok"));
  let closed = false;
  server.on("close", () => (closed = true));
  await listen(server);
  const port = server.address().port;
  const agent = new mod.Agent({ keepAlive: true, maxSockets: 1 });
  await new Promise((resolve, reject) => {
    const req = mod.request({ host: "127.0.0.1", port, path: "/", agent, rejectUnauthorized: false }, (res) => {
      res.resume();
      res.on("end", resolve);
    });
    req.on("error", reject);
    req.end();
  });
  line(kind + " idle keep-alive, count before close()", await count(server));
  server.close();
  line(kind + " idle keep-alive, count after close()", await drained(server));
  for (let i = 0; i < 160 && !closed; i++) await tick(25);
  line(kind + " idle keep-alive, 'close' fired", closed);
  agent.destroy();
}
await idle("http");
await idle("https");

// ---- a second close(cb) during the drain, and a connection after close() -----
{
  let release;
  const held = new Promise((r) => (release = r));
  let reached = false;
  const server = http.createServer(async (_q, res) => { reached = true; await held; res.end("ok"); });
  await listen(server);
  const port = server.address().port;
  const answer = new Promise((resolve) => {
    const req = http.request({ host: "127.0.0.1", port, path: "/", agent: false }, (res) => {
      res.resume();
      res.on("end", () => resolve(res.statusCode));
    });
    req.on("error", (e) => resolve("ERROR " + e.code));
    req.end();
  });
  while (!reached) await tick(10);
  const results = [];
  server.close((err) => results.push("first " + (err ? err.code : "none")));
  server.close((err) => results.push("second " + (err ? err.code : "none")));
  const refused = await new Promise((resolve) => {
    const c = net.connect(port, "127.0.0.1");
    c.on("connect", () => { c.destroy(); resolve("connected"); });
    c.on("error", (e) => resolve(e.code));
  });
  line("a new connection after close()", refused);
  line("second close(cb) answered before the drain ends", results.length > 0);
  release();
  line("the request in flight got", await answer);
  for (let i = 0; i < 160 && results.length < 2; i++) await tick(25);
  line("close callbacks", results.sort().join(", "));
}

// ---- an upgraded socket open when close() runs holds 'close' back -------------
{
  const held = [];
  const server = http.createServer((_q, s) => s.end("ok"));
  server.on("upgrade", (_req, socket) => {
    socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: probe\r\nConnection: Upgrade\r\n\r\n");
    socket.on("error", () => {});
    held.push(socket);
  });
  let closed = false;
  server.on("close", () => (closed = true));
  await listen(server);
  const port = server.address().port;
  const client = await new Promise((resolve) => {
    const c = net.connect(port, "127.0.0.1", () =>
      c.write("GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: probe\r\nConnection: Upgrade\r\n\r\n"));
    c.on("error", () => {});
    c.once("data", () => resolve(c));
  });
  server.close();
  await tick(150);
  line("upgraded, count while it is open after close()", await count(server));
  line("upgraded, 'close' fired while it is open", closed);
  client.end();
  held.forEach((s) => s.end());
  line("upgraded, count once it has closed", await drained(server));
  for (let i = 0; i < 160 && !closed; i++) await tick(25);
  line("upgraded, 'close' fired once it has closed", closed);
}
// ---- a listener that closes the server: its own connection is served --------
async function closedByListener(kind, event) {
  const isTls = kind === "https";
  const mod = isTls ? https : http;
  const handler = (_q, res) => res.end("served");
  const server = isTls ? https.createServer({ cert: CERT, key: KEY }, handler) : http.createServer(handler);
  let cbErr = "not called";
  server.on(event, () => server.close((err) => (cbErr = err ? err.code : "none")));
  await listen(server);
  const port = server.address().port;
  const got = await new Promise((resolve) => {
    const req = mod.request(
      { host: "127.0.0.1", port, path: "/", agent: false, rejectUnauthorized: false },
      (res) => {
        let body = "";
        res.setEncoding("utf8");
        res.on("data", (d) => (body += d));
        res.on("end", () => resolve(res.statusCode + " " + body));
      },
    );
    req.on("error", (e) => resolve("ERROR " + (e.code || e.message)));
    req.end();
  });
  line(kind + " '" + event + "' listener calls close(), the client got", got);
  for (let i = 0; i < 160 && cbErr === "not called"; i++) await tick(25);
  line(kind + " '" + event + "' listener calls close(), close(cb) error", cbErr);
}
await closedByListener("http", "connection");
await closedByListener("https", "connection");
await closedByListener("https", "secureConnection");

process.exit(0);
