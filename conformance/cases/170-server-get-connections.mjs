// A server is a net.Server, and getConnections() reports its live
// connections. In node every server in this family inherits from one --
// `http.Server extends net.Server`, `tls.Server extends net.Server`,
// `https.Server extends tls.Server` -- so all four answer
// `instanceof net.Server`, all four carry getConnections(), and the count
// reaches its callback on a later tick rather than synchronously.
//
// It is what library code tests for. A graceful-shutdown wrapper
// (stoppable, http-terminator, server-destroy) polls getConnections() to
// decide when a drain has finished, and middleware checks the instance to
// decide what it was handed.
//
// Regression guard: oam builds each server on its own native server, so
// none of them was `instanceof net.Server` and only net.Server had
// getConnections() at all -- and that one was a stub answering 0 whatever
// was connected, which is the worse half: a drain loop written against it
// exits at once instead of throwing something a caller would notice.
//
// What this case deliberately does NOT assert: the count AFTER the
// connections close. oam holds a connection's record until its keep-alive
// idle timeout even when the exchange was close-delimited (about 6 s
// against node's immediate release), which is a defect in the connection's
// lifetime rather than in the counter, and it is tracked separately.
// Everything here is about a server whose connections are still open.
import http from "node:http";
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

const line = (k, v) => console.log(k + "=" + v);
const count = (server) =>
  new Promise((resolve, reject) =>
    server.getConnections((err, n) => (err ? reject(err) : resolve(n))));

// ---- the shape every one of them has -------------------------------------
const servers = {
  net: net.createServer(),
  http: http.createServer((_q, s) => s.end("ok")),
  tls: tls.createServer({ cert: CERT, key: KEY }),
  https: https.createServer({ cert: CERT, key: KEY }, (_q, s) => s.end("ok")),
};
for (const name of ["net", "http", "tls", "https"]) {
  line(name + ".instanceof net.Server", servers[name] instanceof net.Server);
  line(name + ".typeof getConnections", typeof servers[name].getConnections);
}

// A subclass of one of them is still a net.Server, and a plain object is not.
class MyServer extends http.Server {}
line("subclass.instanceof net.Server", new MyServer(() => {}) instanceof net.Server);
line("plain object.instanceof net.Server", {} instanceof net.Server);
line("net.Socket.instanceof net.Server", new net.Socket() instanceof net.Server);

// node hands the count to process.nextTick; it never arrives synchronously.
let stillInCall = true;
servers.http.getConnections(() => line("callback arrived synchronously", stillInCall));
stillInCall = false;
await new Promise((r) => setTimeout(r, 20));

// A server that has never listened has none.
for (const name of ["net", "http", "tls", "https"]) {
  line(name + ".connections when idle", await count(servers[name]));
}
for (const s of Object.values(servers)) { try { s.close(); } catch {} }

// ---- the live count, while the connections are open ----------------------
async function live(kind) {
  const isTls = kind === "https";
  const server = isTls
    ? https.createServer({ cert: CERT, key: KEY }, (_q, s) => s.end("ok"))
    : http.createServer((_q, s) => s.end("ok"));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  const mod = isTls ? https : http;
  const agent = new mod.Agent({ keepAlive: true, maxSockets: 2 });
  const get = () =>
    new Promise((resolve, reject) => {
      const req = mod.request(
        { host: "127.0.0.1", port, path: "/", agent, rejectUnauthorized: false },
        (res) => { res.resume(); res.on("end", resolve); },
      );
      req.on("error", reject);
      req.end();
    });

  await get();
  line(kind + ".connections after one keep-alive request", await count(server));
  await Promise.all([get(), get()]);
  line(kind + ".connections with two requests in flight at least two",
    (await count(server)) >= 2);
  agent.destroy();
  server.close();
}
await live("http");
await live("https");

// A net server counts the connections it accepted itself.
{
  const server = net.createServer((s) => s.resume());
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  const a = net.connect(port, "127.0.0.1");
  await new Promise((r) => a.on("connect", r));
  await new Promise((r) => setTimeout(r, 50));
  line("net.connections after one accept", await count(server));
  const b = net.connect(port, "127.0.0.1");
  await new Promise((r) => b.on("connect", r));
  await new Promise((r) => setTimeout(r, 50));
  line("net.connections after two accepts", await count(server));
  a.destroy();
  b.destroy();
  server.close();
}

process.exit(0);
