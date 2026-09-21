// An http2.createSecureServer's connection events, held to node the way
// cases 168 and 169 hold https and tls: 'connection' once per connection,
// before the handshake, with the plain net.Socket; 'secureConnection' once
// the handshake is done, with the TLSSocket that negotiated h2; and a
// listener on either that destroys the socket refuses the client -- no
// stream reaches the server, and the client is answered nothing. Also that
// the server is a net.Server and a tls.Server, and that getConnections()
// counts an open session's connection and lets go of it when the client
// closes.
//
// Where 'session' falls: node's Http2SecureServer is a tls.Server whose
// own 'secureConnection' listener -- connectionListener, registered by
// createSecureServer() -- sets up the session and emits 'session'. So an
// application's 'secureConnection' listener, added after it, runs after
// 'session' (and before any 'stream'), and one prepended in front of it
// runs before the session exists. Both orders are held below.
//
// Regression guard: up to 0.16.3 this server emitted no 'connection' at
// all, so a listener refusing clients there never ran and the client it
// would have refused was answered 200; and the server was not
// `instanceof net.Server` and had no getConnections().
//
// The client is the runtime's own http2.connect, in process. Only its
// outcome -- answered, with the status and body, or refused -- is printed,
// so what is compared is the server's side. It asks for /one only once the
// server's 'secureConnection' listeners have run for its connection, and
// 120 ms after that: off the handshake flight, so the request is never in
// the server's hands while a listener decides, and the refusal is
// deterministic on both runtimes. (The HTTP/2 preface and SETTINGS go out
// on 'connect' as they always do; they carry no request.)
//
// What this case leaves out, because oam 0.16.4 differs there (verified by
// probe against node v22.22.2, reported rather than bent to fit):
//   - a socket destroyed in 'connection' closes before its handshake, and
//     node reports that as 'tlsClientError' ECONNRESET "socket hang up"
//     (and, on this server, 'clientError'); oam raises neither. No listener
//     for them is added here.
//   - a prepended 'secureConnection' listener that destroys the socket:
//     node's connectionListener still runs after it and emits 'session'
//     (then 'session close') for the dead socket; oam emits neither. The
//     client is refused on both.
//
// Fixtures: the case 141 throwaway P-256 CA (valid 2025-2125) and the
// localhost leaf it signed.
import http2 from "node:http2";
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

const settle = (ms) => new Promise((r) => setTimeout(r, ms));
const bounded = (p, ms) => Promise.race([p, settle(ms)]);
const closed = (emitter) => new Promise((r) => emitter.once("close", r));
const listen = (server) => new Promise((r) => server.listen(0, "127.0.0.1", r));
const deferred = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };

// ---- the client: the runtime's own http2.connect. `decided` settles
// once the server's 'secureConnection' listeners have run for this
// connection; the request goes out 120 ms after that (see the top). A
// refused client sees a reset, an I/O error or a plain close depending on
// the runtime and on when it was refused, so every one of those is the
// single outcome "refused".
function h2Client(port, decided, keepOpen) {
  return new Promise((resolve) => {
    const out = {};
    let done = false;
    const finish = () => { if (!done) { done = true; resolve({ out, session }); } };
    const session = http2.connect("https://localhost:" + port, { ca: CA });
    session.on("error", () => {});
    session.on("connect", async () => {
      await decided;
      await settle(120);
      if (session.destroyed || session.closed) return;
      let req;
      try { req = session.request({ ":path": "/one" }); } catch { return; }
      req.on("response", (h) => { out.status = h[":status"]; });
      req.setEncoding("utf8");
      let body = "";
      req.on("data", (d) => { body += d; });
      req.on("error", () => {});
      req.on("close", () => {
        if (out.status !== undefined) out.body = body;
        if (keepOpen) finish();
        else session.close();
      });
      req.end();
    });
    session.on("close", finish);
    setTimeout(() => { out.timeout = true; session.destroy(); finish(); }, 5000).unref();
  });
}
const outcome = (out) =>
  out.timeout ? "timeout" : out.status !== undefined ? "answered " + out.status + " " + JSON.stringify(out.body) : "refused";

// What 'connection' hands out: the plain net.Socket, not a TLSSocket.
function describePlain(s) {
  return "isNetSocket=" + (s instanceof net.Socket) +
    " isTLSSocket=" + (s instanceof tls.TLSSocket) +
    " encrypted=" + s.encrypted +
    " remote=" + s.remoteAddress + " localPort=" + (typeof s.localPort) +
    " destroyed=" + s.destroyed;
}
// What 'secureConnection' hands out: the TLSSocket, with the handshake done
// and h2 negotiated.
function describeSecure(s) {
  return "isTLSSocket=" + (s instanceof tls.TLSSocket) +
    " alpnProtocol=" + JSON.stringify(s.alpnProtocol) +
    " encrypted=" + s.encrypted +
    " protocol=" + s.getProtocol() +
    " servername=" + JSON.stringify(s.servername) +
    " destroyed=" + s.destroyed;
}

// One server, `clients` connections one after the other, everything the
// server saw about them in order. `onConnection` / `onSecure` are the
// application's own decisions: returning false destroys the socket.
// `prependSecure` puts the application's 'secureConnection' listener in
// front of the server's own.
async function scenario(label, { clients = 1, onConnection, onSecure, prependSecure = false }) {
  const events = [];
  const plains = [];
  const secures = [];
  const sessions = [];
  let decided = deferred();
  const server = http2.createSecureServer({ cert: CERT, key: KEY });
  server.on("connection", (s) => {
    plains.push(closed(s));
    events.push("connection " + describePlain(s));
    if (onConnection && onConnection(s) === false) {
      events.push("destroy");
      s.destroy();
    }
  });
  const secureListener = (s) => {
    secures.push(closed(s));
    events.push("secureConnection " + describeSecure(s));
    if (onSecure && onSecure(s) === false) {
      events.push("destroy");
      s.destroy();
    }
    decided.resolve();
  };
  if (prependSecure) server.prependListener("secureConnection", secureListener);
  else server.on("secureConnection", secureListener);
  server.on("session", (session) => {
    sessions.push(closed(session));
    events.push("session alpnProtocol=" + JSON.stringify(session.alpnProtocol) + " encrypted=" + session.encrypted);
    session.on("close", () => events.push("session close"));
  });
  server.on("sessionError", (err) => events.push("sessionError " + err.code));
  server.on("stream", (stream, headers) => {
    events.push("stream " + headers[":path"]);
    stream.respond({ ":status": 200 });
    stream.end("hello");
  });
  await listen(server);
  const port = server.address().port;
  const results = [];
  for (let i = 0; i < clients; i++) {
    const { out } = await h2Client(port, decided.promise, false);
    results.push(outcome(out));
    // Everything the server does with this connection is over before the
    // next one arrives: its sockets and its session have closed.
    await bounded(Promise.all([...plains, ...secures, ...sessions]), 3000);
    decided = deferred();
  }
  await settle(100);
  console.log("== " + label);
  for (const e of events) console.log("   " + e);
  for (const r of results) console.log("   client " + r);
  // Shutdown wrappers track a server's connections from these events and
  // drop each one on its socket's 'close' (server-destroy 1.0.1 from
  // 'connection'; stoppable 1.1.0 from 'connection', or on an https server
  // 'secureConnection'), so every socket they were handed has to emit it.
  const count = async (list) => (await Promise.all(list.map((p) => bounded(p.then(() => 1), 1000)))).filter(Boolean).length;
  console.log("   'connection' sockets closed " + (await count(plains)) + "/" + plains.length);
  console.log("   'secureConnection' sockets closed " + (await count(secures)) + "/" + secures.length);
  server.close();
  await settle(60);
}

const admit = () => true;
const refuse = () => false;

// 'connection' fires once per connection, before the handshake, with the
// plain socket; 'session' is emitted from the server's own first
// 'secureConnection' listener -- node's connectionListener, the one
// createSecureServer() registers -- so an application's 'secureConnection'
// listener, added after it, runs after 'session' and before any 'stream'.
await scenario("'connection' and 'secureConnection' listeners that admit two clients, one after the other",
  { clients: 2, onConnection: admit, onSecure: admit });
// Refused before the handshake: no TLS, no session, no answer.
await scenario("a 'connection' listener that refuses the client",
  { onConnection: refuse, onSecure: admit });
// Refused after the handshake: the session the server already set up goes
// with its socket, before any stream, and the client is answered nothing.
await scenario("a 'secureConnection' listener that refuses the client",
  { onConnection: admit, onSecure: refuse });
// Put in front of the server's own listener, the application's runs before
// the session exists.
await scenario("a 'secureConnection' listener prepended in front of the server's own, admitting the client",
  { onSecure: admit, prependSecure: true });

// ---- a net.Server and a tls.Server, counting its connections --------------
{
  const server = http2.createSecureServer({ cert: CERT, key: KEY });
  console.log("== the server");
  console.log("   instanceof net.Server=" + (server instanceof net.Server) +
    " instanceof tls.Server=" + (server instanceof tls.Server) +
    " typeof getConnections=" + typeof server.getConnections);
  server.on("stream", (stream) => {
    stream.respond({ ":status": 200 });
    stream.end("hello");
  });
  const count = () =>
    new Promise((resolve, reject) =>
      server.getConnections((err, n) => (err ? reject(err) : resolve(n))));
  // The count once it has settled at 0, or what it was after 3 s -- a
  // runtime that never lets go prints that number instead of 0.
  async function drained() {
    let n = -1;
    for (let i = 0; i < 120; i++) {
      n = await count();
      if (n === 0) return 0;
      await settle(25);
    }
    return n;
  }
  await listen(server);
  console.log("   connections when idle=" + (await count()));
  const { out, session } = await h2Client(server.address().port, Promise.resolve(), true);
  console.log("   client " + outcome(out));
  console.log("   connections with the client's session open=" + (await count()));
  session.close();
  console.log("   connections after the client closes its session=" + (await drained()));
  server.close();
}

process.exit(0);
