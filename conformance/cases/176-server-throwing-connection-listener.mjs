// A listener that throws does not stop a tls, net or http2 server, and the
// connection whose listener threw is still served. node raises the throw as
// 'uncaughtException'; an application that installs
// process.on('uncaughtException') to log and carry on keeps serving -- the
// connection in hand included, since the server's own listener for it has
// already run or still runs. Case 173 holds the same for an http server's
// 'request', 'upgrade', 'connect' and socket 'close' listeners; this case
// holds 'connection' on net.createServer, tls.createServer and
// http2.createSecureServer, 'secureConnection' on the last two, and a socket
// 'close' listener that throws when server.close() ends an idle connection.
//
// Regression guard: on oam a throwing 'secureConnection' listener on a tls or
// http2 secure server, or a throwing 'connection' listener on a net server,
// ended the process with an unhandled promise rejection (OAM-RT0004) despite
// the handler; a throwing 'connection' listener on a tls or http2 secure
// server left that connection unanswered; and a socket 'close' listener that
// threw at server.close() ended the process too.
//
// Each scenario prints what the throwing listener's connection got, the
// uncaught messages, and what the next connection got. Clients are the
// runtime's own, one at a time; nothing timed is printed.
import http from "node:http";
import http2 from "node:http2";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";

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

const uncaught = [];
process.on("uncaughtException", (e) => uncaught.push(e.message));
const tick = (ms) => new Promise((r) => setTimeout(r, ms));
const hard = (p) => Promise.race([p, tick(4000).then(() => "HUNG")]);
const listen = (server) => new Promise((r) => server.listen(0, "127.0.0.1", r));

// A client that reads what the server sends until the connection closes.
const streamClient = (secure) => (port) =>
  new Promise((resolve) => {
    let got = "";
    const c = secure
      ? tls.connect({ host: "127.0.0.1", port, ca: CA, servername: "localhost" })
      : net.connect(port, "127.0.0.1");
    c.setEncoding("utf8");
    c.on("data", (d) => (got += d));
    c.on("error", (e) => resolve("error " + e.code));
    c.on("close", () => resolve(got || "nothing"));
  });
const httpsClient = (port) =>
  new Promise((resolve) => {
    const q = https.get({ host: "127.0.0.1", port, path: "/", ca: CA, servername: "localhost", agent: false }, (res) => {
      let b = "";
      res.setEncoding("utf8");
      res.on("data", (d) => (b += d));
      res.on("end", () => resolve(res.statusCode + " " + b));
    });
    q.on("error", (e) => resolve("error " + e.code));
  });

const scenarios = [
  ["net 'connection'", () => net.createServer((s) => s.end("hi")), "connection", streamClient(false)],
  ["tls 'connection'", () => tls.createServer({ key: KEY, cert: CERT }, (s) => s.end("hi")), "connection", streamClient(true)],
  ["tls 'secureConnection'", () => tls.createServer({ key: KEY, cert: CERT }, (s) => s.end("hi")), "secureConnection", streamClient(true)],
  [
    "http2 secure 'connection'",
    () => http2.createSecureServer({ key: KEY, cert: CERT, allowHTTP1: true }, (_q, res) => res.end("hi")),
    "connection",
    httpsClient,
  ],
  [
    "http2 secure 'secureConnection'",
    () => http2.createSecureServer({ key: KEY, cert: CERT, allowHTTP1: true }, (_q, res) => res.end("hi")),
    "secureConnection",
    httpsClient,
  ],
];
for (const [name, make, event, client] of scenarios) {
  uncaught.length = 0;
  let first = true;
  const server = make();
  server.on(event, () => {
    if (first) {
      first = false;
      throw new Error(name + " boom");
    }
  });
  await listen(server);
  const port = server.address().port;
  const a = await hard(client(port));
  await tick(50);
  const b = await hard(client(port));
  console.log(name + ": its connection got " + a + ", uncaught " + JSON.stringify(uncaught) + ", the next got " + b);
  server.close();
}

// A socket 'close' listener that throws when server.close() ends the connection.
{
  uncaught.length = 0;
  const server = http.createServer((_q, res) => res.end("ok"));
  server.on("connection", (socket) =>
    socket.on("close", () => {
      throw new Error("close boom");
    }),
  );
  await listen(server);
  const port = server.address().port;
  const c = net.connect(port, "127.0.0.1", () => c.write("GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
  c.on("error", () => {});
  await new Promise((r) => c.once("data", r));
  let closed = false;
  server.close(() => (closed = true));
  for (let i = 0; i < 160 && !(closed && uncaught.length); i++) await tick(25);
  console.log("http socket 'close' at server.close(): uncaught " + JSON.stringify(uncaught) + ", close(cb) called " + closed);
  c.destroy();
}

process.exit(0);
