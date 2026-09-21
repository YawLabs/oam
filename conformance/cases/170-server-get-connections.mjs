// A server is a net.Server, and getConnections() reports the connections it
// is holding. In node every server in this family inherits from one --
// `http.Server extends net.Server`, `tls.Server extends net.Server`,
// `https.Server extends tls.Server` -- so all four answer
// `instanceof net.Server`, all four carry getConnections(), and the count
// reaches its callback on a later tick rather than synchronously.
//
// It is what library code relies on. A graceful-shutdown wrapper (stoppable,
// http-terminator, server-destroy) polls getConnections() until it reads 0,
// so the count has to go UP with every connection -- a websocket an upgrade
// took over included -- and come back DOWN when each one ends, however it
// ends.
//
// Regression guard: oam builds each server on its own native server, so none
// of them was `instanceof net.Server`, only net.Server had getConnections()
// at all, and that one was a stub answering 0 whatever was connected -- the
// worse half, because a drain loop written against it finished at once. Then,
// once counted: an upgraded or CONNECT connection dropped out of the count at
// the handover while its socket was still open, and a net or tls socket left
// the count only from a 'close' listener, so one whose listeners were removed
// (or whose destroy(err) threw at its 'error') stayed counted for good.
//
// What is deliberately NOT asserted: the http count after a KEEP-ALIVE agent
// is destroyed. Over oam's own HTTP transport the pooled connection outlives
// the JS agent until the server's keep-alive timeout, a client-side gap
// rather than a server one. Every other way a connection ends is held to 0.
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
const tick = (ms) => new Promise((r) => setTimeout(r, ms));
// The count once it has settled at 0, or what it was after 3 s -- a runtime
// that never lets go prints that number instead of 0.
async function drained(server) {
  let n = -1;
  for (let i = 0; i < 120; i++) {
    n = await count(server);
    if (n === 0) return 0;
    await tick(25);
  }
  return n;
}
const listen = (server) => new Promise((r) => server.listen(0, "127.0.0.1", r));

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
class MyServer extends http.Server {}
line("subclass.instanceof net.Server", new MyServer(() => {}) instanceof net.Server);
line("plain object.instanceof net.Server", {} instanceof net.Server);
line("net.Socket.instanceof net.Server", new net.Socket() instanceof net.Server);

let stillInCall = true;
servers.http.getConnections(() => line("callback arrived synchronously", stillInCall));
stillInCall = false;
await tick(20);
for (const name of ["net", "http", "tls", "https"]) {
  line(name + ".connections when idle", await count(servers[name]));
}
for (const s of Object.values(servers)) { try { s.close(); } catch {} }

// ---- http and https: up with each connection, down when it ends ----------
async function web(kind) {
  const isTls = kind === "https";
  const mod = isTls ? https : http;
  const server = isTls
    ? https.createServer({ cert: CERT, key: KEY }, (_q, s) => s.end("ok"))
    : http.createServer((_q, s) => s.end("ok"));
  await listen(server);
  const port = server.address().port;
  const get = (agent) =>
    new Promise((resolve, reject) => {
      const req = mod.request(
        { host: "127.0.0.1", port, path: "/", agent, rejectUnauthorized: false },
        (res) => { res.resume(); res.on("end", resolve); },
      );
      req.on("error", reject);
      req.end();
    });

  // A one-off agent frames the request close: the server lets go at once.
  await get(false);
  line(kind + ".connections after an agent:false request ends", await drained(server));

  const agent = new mod.Agent({ keepAlive: true, maxSockets: 2 });
  await get(agent);
  line(kind + ".connections after one keep-alive request", await count(server));
  await Promise.all([get(agent), get(agent)]);
  line(kind + ".connections after two concurrent keep-alive requests", await count(server));
  agent.destroy();
  if (isTls) line(kind + ".connections after the keep-alive agent is destroyed", await drained(server));
  server.close();
}
await web("http");
await web("https");

// ---- net: its own accepts, and every way a socket can end ----------------
{
  const accepted = [];
  const server = net.createServer((s) => { accepted.push(s); s.on("error", () => {}); s.resume(); });
  await listen(server);
  const port = server.address().port;
  const dial = () => new Promise((r) => { const c = net.connect(port, "127.0.0.1", () => r(c)); c.on("error", () => {}); });
  const a = await dial();
  const b = await dial();
  while (accepted.length < 2) await tick(10);
  line("net.connections after two accepts", await count(server));
  a.destroy();
  b.destroy();
  line("net.connections after both clients close", await drained(server));

  // A socket whose 'close' listeners were all removed still leaves the count.
  const c = await dial();
  while (accepted.length < 3) await tick(10);
  accepted[2].removeAllListeners("close");
  accepted[2].destroy();
  line("net.connections after a listener-less socket is destroyed", await drained(server));

  // destroy(err) with no 'error' listener: the error surfaces as an uncaught
  // exception -- node emits it on a later tick; oam, divergently, throws it
  // out of destroy() itself, before 'close' -- and the socket leaves the count
  // either way, because it goes in the socket's own teardown.
  const surfaced = [];
  const onUncaught = (e) => surfaced.push(e.message);
  process.on("uncaughtException", onUncaught);
  const d = await dial();
  while (accepted.length < 4) await tick(10);
  accepted[3].removeAllListeners("error");
  try { accepted[3].destroy(new Error("boom")); } catch (e) { surfaced.push(e.message); }
  line("net.connections after destroy(err) with no error listener", await drained(server));
  await tick(20);
  line("net.destroy(err) error surfaced", surfaced.join(","));
  process.off("uncaughtException", onUncaught);
  c.destroy();
  d.destroy();
  server.close();
}

// ---- tls: counted from the accept until the TLSSocket's own teardown ------
{
  let secured = 0;
  const kept = [];
  const server = tls.createServer({ cert: CERT, key: KEY }, (s) => { secured++; kept.push(s); s.on("error", () => {}); s.resume(); });
  await listen(server);
  const port = server.address().port;
  const dial = () =>
    new Promise((r) => {
      const c = tls.connect({ host: "127.0.0.1", port, rejectUnauthorized: false }, () => r(c));
      c.on("error", () => {});
    });
  const a = await dial();
  while (secured < 1) await tick(10);
  line("tls.connections after one handshake", await count(server));
  const b = await dial();
  while (secured < 2) await tick(10);
  line("tls.connections after two handshakes", await count(server));
  a.destroy();
  b.destroy();
  line("tls.connections after both clients close", await drained(server));
  const c = await dial();
  while (secured < 3) await tick(10);
  kept[2].removeAllListeners("close");
  kept[2].destroy();
  line("tls.connections after a listener-less socket is destroyed", await drained(server));
  c.destroy();
  server.close();
}

// ---- a client that never finishes the handshake still counts, then leaves --
// Port scanners and TCP health checks connect to a TLS port and never speak
// TLS. The connection counts from the accept; it must leave the count when
// the client hangs up, or when what it sent is refused as not TLS.
async function halfHandshake(kind) {
  const server = kind === "https"
    ? https.createServer({ cert: CERT, key: KEY }, (_q, s) => s.end("ok"))
    : tls.createServer({ cert: CERT, key: KEY });
  server.on("tlsClientError", () => {});
  server.on("clientError", () => {});
  await listen(server);
  const port = server.address().port;
  // Connects and says nothing.
  const silent = net.connect(port, "127.0.0.1");
  silent.on("error", () => {});
  await new Promise((r) => silent.on("connect", r));
  let n = 0;
  for (let i = 0; i < 40 && n < 1; i++) { n = await count(server); if (n < 1) await tick(25); }
  line(kind + ".connections with a client that never handshakes", n);
  silent.destroy();
  line(kind + ".connections after that client hangs up", await drained(server));
  // Connects and sends bytes that are not TLS.
  const junk = net.connect(port, "127.0.0.1", () => junk.write("GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
  junk.on("error", () => {});
  await new Promise((r) => junk.on("close", r));
  line(kind + ".connections after a non-TLS client is refused", await drained(server));
  server.close();
}
await halfHandshake("tls");
await halfHandshake("https");

// ---- an upgrade or CONNECT keeps its connection counted until it closes ---
{
  const held = [];
  const server = http.createServer((_q, s) => s.end("ok"));
  server.on("upgrade", (_req, socket) => {
    socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: probe\r\nConnection: Upgrade\r\n\r\n");
    held.push(socket);
  });
  server.on("connect", (_req, socket) => {
    socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    held.push(socket);
  });
  await listen(server);
  const port = server.address().port;
  const raw = (head) =>
    new Promise((r) => {
      const c = net.connect(port, "127.0.0.1", () => c.write(head));
      c.on("error", () => {});
      c.once("data", () => r(c));
    });
  const up = await raw("GET /socket HTTP/1.1\r\nHost: x\r\nUpgrade: probe\r\nConnection: Upgrade\r\n\r\n");
  line("upgrade.connections while the upgraded socket is open", await count(server));
  held[0].destroy();
  up.destroy();
  line("upgrade.connections after it closes", await drained(server));
  const tunnel = await raw("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n");
  line("connect.connections while the tunnel is open", await count(server));
  held[1].destroy();
  tunnel.destroy();
  line("connect.connections after it closes", await drained(server));
  server.close();
}

process.exit(0);
