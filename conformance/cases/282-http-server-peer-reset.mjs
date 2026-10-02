// A client that resets its connection to an http or https server
// (resetAndDestroy() on its socket): node's server socket reads
// `read ECONNRESET`. socketOnError hands it to the server's 'clientError'
// first, then the socket's own 'error' listeners hear it, and the socket
// closes saying true -- on a kept-alive connection between requests, with
// the response under way, and before the handler has answered. An
// exchange the reset cut short is aborted from that 'close' ('aborted',
// the response's 'close', then ECONNRESET "aborted"), and a write to its
// response after that fails with ERR_STREAM_DESTROYED. On an https server
// the TLS socket reports it and the plain socket under it closes saying
// false. oam's socket closed saying false with no error and no
// 'clientError', and a late write called back with no error. Measured on
// node v22.22.2.
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


const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// The server's side of one connection the client resets, in the order the
// server sees it; printed once the connection's socket has closed.
async function scenario(label, { secure = false, clientError = true, handler, keptAlive = false }) {
  const log = [];
  // The late write's callback, kept apart: when it runs against the
  // request's 'error' is the scheduler's.
  const late = [];
  let socketClosed;
  const closed = new Promise((resolve) => { socketClosed = resolve; });
  const server = secure
    ? https.createServer({ cert: CERT, key: KEY })
    : http.createServer();
  if (clientError) {
    server.on("clientError", (err, socket) => {
      log.push(`server clientError ${err.code} | ${err.message} | ${err.syscall} | ` +
        `errno ${typeof err.errno} | destroyed ${socket.destroyed}`);
    });
  }
  const watch = (socket, name, last) => {
    socket.on("error", (e) => log.push(`${name} error ${e.code} | ${e.message}`));
    socket.on("close", (hadError) => {
      log.push(`${name} close ${hadError}`);
      if (last) socketClosed();
    });
  };
  server.on("connection", (socket) => watch(socket, secure ? "plain socket" : "socket", !secure));
  if (secure) server.on("secureConnection", (socket) => watch(socket, "tls socket", true));
  server.on("request", (req, res) => {
    req.on("aborted", () => log.push("req aborted"));
    req.on("error", (e) => log.push(`req error ${e.code} | ${e.message}`));
    // A request read to the end closes on its own in node, before the
    // reset; oam's does not (docs/node-divergences.md entry 39).
    if (!keptAlive) req.on("close", () => log.push("req close"));
    res.on("finish", () => log.push("res finish"));
    res.on("close", () => {
      log.push(`res close finished ${res.writableFinished}`);
      if (!res.writableFinished) {
        const ok = res.write("late", (e) => late.push(`late write callback ${e && e.code}`));
        log.push(`late write returned ${ok}`);
      }
    });
    handler(req, res);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const raw = net.connect(server.address().port, "127.0.0.1");
  await new Promise((r) => raw.on("connect", r));
  let client = raw;
  if (secure) {
    client = tls.connect({ socket: raw, servername: "localhost", ca: CA });
    client.on("error", () => {});
    await new Promise((r) => client.on("secureConnect", r));
  }
  raw.on("error", () => {});
  client.write("GET / HTTP/1.1\r\nHost: x\r\n\r\n");
  if (handler.answers) await new Promise((r) => client.once("data", r));
  await sleep(50);
  raw.resetAndDestroy();
  await closed;
  await sleep(100);
  server.close();
  console.log(label);
  for (const line of log) console.log("  " + line);
  for (const line of late) console.log("  " + line);
}

const answer = (req, res) => res.end("ok");
answer.answers = true;
const part = (req, res) => res.write("part");
part.answers = true;
const silent = () => {};

await scenario("http, kept-alive between requests", { handler: answer, keptAlive: true });
await scenario("http, response under way", { handler: part });
await scenario("http, before the answer", { handler: silent });
await scenario("http, response under way, no clientError listener", { handler: part, clientError: false });
await scenario("http, kept-alive, no clientError listener", { handler: answer, keptAlive: true, clientError: false });
await scenario("https, response under way", { handler: part, secure: true });
await scenario("https, kept-alive between requests", { handler: answer, keptAlive: true, secure: true });
