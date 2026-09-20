// A server's 'connection' event: the first thing an http, https or tls
// server hears about a connection, before a byte is read off it and, on a
// TLS server, before the handshake. An application that decides there
// which clients it will serve -- an allow list that destroys the sockets
// it refuses -- must have decided before anything else happens on the
// connection: nothing it refused may reach the request handler, or
// 'secureConnection', or the client as an answer on the wire.
//
// Also here: a listener that throws. Node raises 'uncaughtException' and
// the server goes on serving; a server that stopped taking connections
// because one listener threw would be a denial of service of its own.
//
// Regression guard: oam's servers used to go straight from the accept to
// serving, emitting no 'connection' at all, so the listener above never
// ran and a client the application would have refused was served instead
// -- on http and https servers alike, and on a tls server the refused
// client reached 'secureConnection'. A throwing listener ended the
// server's accept loop, after which every connection was accepted and
// then never dispatched.
//
// The clients are separate `node` processes (the harness's oracle, on
// PATH) in BOTH runs, so the only thing that differs between the two runs
// is the server. Each client sends its request from a timer rather than
// inline in its connect callback: a request pipelined into the first
// flight can already be in the server's read buffer when the listener
// destroys the socket. Off that flight the refusal is deterministic on
// both runtimes.
//
// Fixtures: the case 141 throwaway P-256 CA (valid 2025-2125) and the
// localhost leaf it signed.
import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";
import { spawn } from "node:child_process";

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

// The uncaught exceptions the throwing scenarios raise, by message only:
// the stack is each runtime's own.
const uncaught = [];
process.on("uncaughtException", (err) => uncaught.push("uncaughtException " + err.message));

// ---- the client side: one node process per connection, printing one JSON
// line. A connection dropped before or during the first flight reads as a
// reset, a plain end or a TLS read error depending on timing (in Node
// too), so every one of those is the single outcome "closed".
const CLIENT = `
import net from "node:net";
import tls from "node:tls";
const op = JSON.parse(process.argv[1]);
const out = {};
let data = "";
const ready = () => setTimeout(() => {
  if (s.destroyed) return;
  s.write("GET /one HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n");
}, 120);
const s = op.tls
  ? tls.connect({ host: "127.0.0.1", port: op.port, servername: "localhost", ca: op.ca }, ready)
  : net.connect(op.port, "127.0.0.1", ready);
s.setEncoding("utf8");
s.on("data", (d) => { data += d; });
s.on("error", () => {});
s.on("close", () => {
  if (data) { out.bytes = data.length; out.first = data.split("\\r\\n")[0]; }
  else out.refused = "closed";
  console.log(JSON.stringify(out));
});
setTimeout(() => { out.timeout = true; s.destroy(); }, 5000).unref();
`;

function runClient(op) {
  return new Promise((resolve) => {
    const child = spawn("node", ["--input-type=module", "-e", CLIENT, JSON.stringify(op)], {
      stdio: ["ignore", "pipe", "inherit"],
    });
    let out = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (d) => { out += d; });
    child.on("close", () => resolve(out.trim()));
  });
}

const settle = (ms) => new Promise((r) => setTimeout(r, ms));

// What the plain socket 'connection' hands out reports. Node's is a
// net.Socket nothing has been read from yet.
function describe(s) {
  return "isNetSocket=" + (s instanceof net.Socket) +
    " remote=" + s.remoteAddress + " remotePort=" + (typeof s.remotePort) +
    " localPort=" + (typeof s.localPort) +
    " destroyed=" + s.destroyed;
}

// One server, `clients` connections one after the other, everything the
// server saw. `verdict` is the application's own decision in the
// 'connection' listener: returning false destroys the socket, and nothing
// it refuses may reach the handler.
async function scenario(label, make, clients, verdict) {
  const events = [];
  const server = make((req, res) => {
    events.push("request " + req.url);
    res.end("hello");
  });
  server.on("connection", (s) => {
    events.push("connection " + describe(s));
    if (verdict === "throw") throw new Error("boom from the connection listener");
    if (verdict(s) === false) {
      events.push("destroy");
      s.destroy();
    }
  });
  if (server instanceof tls.Server && !(server instanceof https.Server)) {
    // A tls server has no request handler: what it would have served is
    // one answer written on the socket 'secureConnection' hands out, and
    // then the connection ends.
    server.on("secureConnection", (s) => {
      events.push("secureConnection authorized=" + s.authorized);
      s.end("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret");
    });
  }
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  const results = [];
  for (const c of clients) results.push(await runClient({ ...c, port, ca: CA }));
  await settle(250);
  console.log("== " + label);
  for (const e of events) console.log("   " + e);
  for (const r of results) console.log("   client " + r);
  for (const u of uncaught.splice(0)) console.log("   " + u);
  server.close();
  await settle(60);
}

const plain = (handler) => http.createServer(handler);
const secure = (handler) => https.createServer({ cert: CERT, key: KEY }, handler);
const tlsServer = () => tls.createServer({ cert: CERT, key: KEY });

const one = [{}];
const oneTls = [{ tls: true }];
const three = [{}, {}, {}];
const threeTls = [{ tls: true }, { tls: true }, { tls: true }];
// The documented decision: refuse every client, so nothing that reaches
// the handler can be mistaken for a client the listener admitted.
const refuseAll = () => false;
const admitAll = () => true;

await scenario("http, a 'connection' listener that refuses the client", plain, one, refuseAll);
await scenario("http, a 'connection' listener that admits the client", plain, one, admitAll);
await scenario("https, a 'connection' listener that refuses the client", secure, oneTls, refuseAll);
await scenario("https, a 'connection' listener that admits the client", secure, oneTls, admitAll);
await scenario("tls, a 'connection' listener that refuses the client", tlsServer, oneTls, refuseAll);
await scenario("tls, a 'connection' listener that admits the client", tlsServer, oneTls, admitAll);

// A listener that throws: the exception is the application's to handle,
// and the server keeps taking connections.
await scenario("http, a 'connection' listener that throws on every connection",
  plain, three, "throw");
await scenario("https, a 'connection' listener that throws on every connection",
  secure, threeTls, "throw");

process.exit(0);
