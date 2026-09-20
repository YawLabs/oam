// The per-request socket an http / https server hands the handler, as node's
// net.Socket reports it: `readable` and `writable` true while the connection
// is up, `destroyed` false, and the same object on req.socket, req.connection,
// req.client, res.socket and res.connection.
//
// on-finished (express, body-parser, morgan, serve-static, finalhandler) is
// the reason this matters: isFinished(req) is `!socket.readable`, so a socket
// without the flag looks FINISHED, and body-parser 2 answers "body already
// parsed" and leaves req.body undefined. Every express 5 express.json() and
// express.urlencoded() request lost its body that way, on http and https
// alike. isFinished(res) needs res.socket and the deprecated res.finished.
import http from "node:http";
import https from "node:https";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

// on-finished 2.4.1's isFinished(), verbatim.
function isFinished(msg) {
  const socket = msg.socket;
  if (typeof msg.finished === "boolean") {
    return Boolean(msg.finished || (socket && !socket.writable));
  }
  if (typeof msg.complete === "boolean") {
    return Boolean(msg.upgrade || !socket || !socket.readable || (msg.complete && !msg.readable));
  }
  return undefined;
}

function flags(s) {
  return `readable=${s.readable} writable=${s.writable} destroyed=${s.destroyed}`;
}

const out = [];
function handler(scheme) {
  return (req, res) => {
    out.push(`${scheme} entry ${flags(req.socket)}`);
    out.push(`${scheme} entry isFinished(req)=${isFinished(req)} complete=${req.complete}`);
    out.push(
      `${scheme} entry isFinished(res)=${isFinished(res)} finished=${res.finished}` +
        ` writableEnded=${res.writableEnded}`,
    );
    out.push(
      `${scheme} same object req.connection=${req.connection === req.socket}` +
        ` req.client=${req.client === req.socket}` +
        ` res.socket=${res.socket === req.socket}` +
        ` res.connection=${res.connection === req.socket}`,
    );
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      out.push(`${scheme} body ${JSON.stringify(Buffer.concat(chunks).toString())}`);
      out.push(`${scheme} after end ${flags(req.socket)}`);
      out.push(
        `${scheme} after end isFinished(req)=${isFinished(req)} complete=${req.complete}` +
          ` req.readable=${req.readable}`,
      );
      res.end("ok");
      out.push(`${scheme} after res.end finished=${res.finished} writableEnded=${res.writableEnded}`);
      out.push(`${scheme} after res.end isFinished(res)=${isFinished(res)}`);
      out.push(`${scheme} after res.end ${flags(req.socket)}`);
    });
  };
}

// Fixtures: the case 141 / 154 throwaway P-256 CA (valid 2025-2125) and the
// localhost leaf it signed.
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

function post(mod, port, options) {
  return new Promise((resolve) => {
    const req = mod.request({ host: "127.0.0.1", port, method: "POST", path: "/p", ...options }, (res) => {
      const parts = [];
      res.on("data", (c) => parts.push(c));
      res.on("end", () => resolve(`${res.statusCode} ${Buffer.concat(parts).toString()}`));
    });
    req.on("error", (e) => resolve(`ERR ${e.code || e.message}`));
    req.end("a=1&b=2");
  });
}

const hs = http.createServer(handler("http"));
await new Promise((r) => hs.listen(0, "127.0.0.1", r));
out.push(`http client ${await post(http, hs.address().port, { agent: false })}`);
hs.close();

const ss = https.createServer({ key: KEY, cert: CERT }, handler("https"));
await new Promise((r) => ss.listen(0, "127.0.0.1", r));
out.push(
  `https client ${await post(https, ss.address().port, { agent: false, ca: CA, servername: "localhost" })}`,
);
ss.close();

// A handler that destroys the connection: node's socket then reports itself
// neither readable nor writable.
const ds = http.createServer((req, res) => {
  req.socket.destroy();
  out.push(`http destroyed ${flags(req.socket)}`);
  out.push(`http destroyed isFinished(req)=${isFinished(req)}`);
  void res;
});
await new Promise((r) => ds.listen(0, "127.0.0.1", r));
out.push(`http destroy client ${await post(http, ds.address().port, { agent: false })}`);
ds.close();

console.log(out.join("\n"));
