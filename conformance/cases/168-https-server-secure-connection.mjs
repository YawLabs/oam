// An https server's connection events: 'secureConnection' with the
// connection's socket, where it fires relative to the handshake and the
// first request, what the socket reports by then, which object every
// request on the connection carries as req.socket / res.socket, and what a
// listener that destroys the socket does.
//
// Node documents a mutual-TLS pattern the application decides for itself:
// the server is created with `requestCert: true` and
// `rejectUnauthorized: false`, and a 'secureConnection' listener reads
// `authorized`, `authorizationError` and `getPeerCertificate()` and
// destroys the clients it refuses. This case holds that pattern to Node's
// behaviour end to end -- a refused client's request must never reach the
// handler, and an accepted client's must.
//
// Regression guard: oam's https server terminates TLS natively and used to
// go straight from the handshake to serving HTTP. It emitted no
// 'secureConnection' at all, so the listener above never ran and a client
// the application would have refused was served instead; and each request
// got its own socket object rather than the connection's one.
//
// The clients are separate `node` processes (the harness's oracle, on PATH)
// in BOTH runs, so the only thing that differs between the two runs is the
// server. Each client sends its request from a timer rather than inline in
// its own 'secureConnect' callback: a request pipelined into the handshake
// flight can already be in the server's read buffer when the listener
// destroys the socket, and Node parses what it has buffered (the client is
// still answered nothing). Off the handshake flight the refusal is
// deterministic on both runtimes.
//
// Fixtures: the case 141 throwaway P-256 CA (valid 2025-2125), the
// localhost leaf it signed, a client leaf it signed, and a self-signed
// "rogue" client certificate.
import https from "node:https";
import tls from "node:tls";
import net from "node:net";
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
const CLIENT_CERT = `-----BEGIN CERTIFICATE-----
MIIBoTCCAUigAwIBAgIUOy7BLDqzc+0IZz2NWG95hnXgrd8wCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAVMRMwEQYDVQQDDApvYW0gY2xpZW50MFkwEwYHKoZIzj0C
AQYIKoZIzj0DAQcDQgAEulhTChDco8oZzXpPqo3iqtybv/nUXKwS67GiGZ23ra4b
5Ta8McX1MVv2p0WA1/JYyncszN9kbKwE1oeV0Q0lTKNvMG0wCQYDVR0TBAIwADAL
BgNVHQ8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwIwHQYDVR0OBBYEFCk7s+uR
ZEuahksP0Vn6QPqJ+TmIMB8GA1UdIwQYMBaAFDpSKOjuLSBTYw+71yVedRVNgy0H
MAoGCCqGSM49BAMCA0cAMEQCIFOOnRBbxbAbIOcU15I7xnKlD5QXj7P2ZHQbxax0
goFxAiBEgrUhNh9pzkHEQGCdqJNAtqjNUURu8GVWs9re4QYI7A==
-----END CERTIFICATE-----
`;
const CLIENT_KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgT9C7qt8YZkF23ize
WR5qGRqTlTsUdSjwsf/UmBhdckihRANCAAS6WFMKENyjyhnNek+qjeKq3Ju/+dRc
rBLrsaIZnbetrhvlNrwxxfUxW/anRYDX8ljKdyzM32RsrATWh5XRDSVM
-----END PRIVATE KEY-----
`;
const ROGUE_CERT = `-----BEGIN CERTIFICATE-----
MIIBmTCCAUCgAwIBAgIUGlc1ru8HwNb+Q/Mu7dMCNPfAocswCgYIKoZIzj0EAwIw
FzEVMBMGA1UEAwwMcm9ndWUgY2xpZW50MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUw
MTAxMDAwMDAwWjAXMRUwEwYDVQQDDAxyb2d1ZSBjbGllbnQwWTATBgcqhkjOPQIB
BggqhkjOPQMBBwNCAAQ9RwUp47J8lOK3t92HA7gbn6/in8YmMNmd1xdfCfmgKTMz
lDyFz1VM0dwI6KtooNv0ubR8/KmhnJS0yFCugC2Lo2gwZjAdBgNVHQ4EFgQU/7mR
Dyd3M2+3xDFCFAhtl1mUL9kwHwYDVR0jBBgwFoAU/7mRDyd3M2+3xDFCFAhtl1mU
L9kwDwYDVR0TAQH/BAUwAwEB/zATBgNVHSUEDDAKBggrBgEFBQcDAjAKBggqhkjO
PQQDAgNHADBEAiAccfsRWDhnWobD+9J8R2fydTxf4E/ePbkue9NfHCHqcQIgRVGb
asDZ6pyGf669FP4nlBCxQetAmb5ZvRlSGk6/Cus=
-----END CERTIFICATE-----
`;
const ROGUE_KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgkuM3bXg9HbW/PUGA
46UkXUruDFYrEDyq5MRBWtc1XD6hRANCAAQ9RwUp47J8lOK3t92HA7gbn6/in8Ym
MNmd1xdfCfmgKTMzlDyFz1VM0dwI6KtooNv0ubR8/KmhnJS0yFCugC2L
-----END PRIVATE KEY-----
`;

// ---- the client side: one node process runs a list of connections, one
// after the other, and prints one JSON line per connection. A connection
// dropped after the handshake reads as a reset or as a plain end depending
// on timing (in Node too), so those two are one outcome, "closed".
const CLIENT = `
import tls from "node:tls";
const ops = JSON.parse(process.argv[1]);
const settle = (ms) => new Promise((r) => setTimeout(r, ms));
function tlsOp(op) {
  return new Promise((resolve) => {
    const out = {};
    let data = "";
    let error;
    const s = tls.connect({
      host: "127.0.0.1", port: op.port, servername: "localhost", ca: op.ca,
      cert: op.cert, key: op.key,
      minVersion: op.version, maxVersion: op.version,
    }, () => {
      // Off the handshake flight: what the server's listener decides is
      // settled before these bytes are written.
      setTimeout(() => {
        if (s.destroyed) return;
        s.write("GET /one HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: " +
          (op.keepAlive ? "keep-alive" : "close") + "\\r\\n\\r\\n");
      }, 120);
    });
    s.setEncoding("utf8");
    s.on("data", (d) => {
      data += d;
      if (op.keepAlive && data.split("HTTP/1.1").length === 2) {
        s.write("GET /two HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n");
      }
    });
    s.on("error", (e) => { error = e.code; });
    s.on("close", () => {
      out.answers = data ? data.split("HTTP/1.1").length - 1 : 0;
      if (data) out.status = data.split("\\r\\n")[0];
      else out.refused = error && error !== "ECONNRESET" ? error : "closed";
      resolve(out);
    });
    setTimeout(() => { out.timeout = true; s.destroy(); }, 5000).unref();
  });
}
for (const op of ops) {
  console.log(JSON.stringify(await tlsOp(op)));
  await settle(20);
}
`;

function runClient(op) {
  return new Promise((resolve) => {
    const child = spawn("node", ["--input-type=module", "-e", CLIENT, JSON.stringify([op])], {
      stdio: ["ignore", "pipe", "inherit"],
    });
    let out = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (d) => { out += d; });
    child.on("close", () => resolve(out.trim()));
  });
}

// What the connection's socket reports when 'secureConnection' hands it out.
function describe(s) {
  const peer = s.getPeerCertificate();
  const detailed = s.getPeerCertificate(true);
  const cipher = s.getCipher();
  return "authorized=" + s.authorized +
    " authorizationError=" + s.authorizationError +
    " peer=" + (peer.subject ? peer.subject.CN : JSON.stringify(peer)) +
    " issuerCertificate=" + (detailed.issuerCertificate ? detailed.issuerCertificate.subject.CN : "none") +
    " encrypted=" + s.encrypted +
    " protocol=" + s.getProtocol() +
    " cipher=" + cipher.name +
    " alpn=" + JSON.stringify(s.alpnProtocol) +
    " servername=" + JSON.stringify(s.servername) +
    " isTLSSocket=" + (s instanceof tls.TLSSocket) +
    " isNetSocket=" + (s instanceof net.Socket) +
    " readable=" + s.readable + " writable=" + s.writable + " destroyed=" + s.destroyed +
    " remote=" + s.remoteAddress + " localPort=" + (typeof s.localPort) +
    " address=" + JSON.stringify(s.address().address);
}

// One server, one client connection, everything the server saw about it.
// `verdict` is the application's own decision in the 'secureConnection'
// listener: it may destroy the socket, and nothing it refuses may reach the
// handler.
async function scenario(label, options, op, verdict) {
  const events = [];
  let connSocket = null;
  let closed = false;
  const server = https.createServer(
    { cert: CERT, key: KEY, ...options },
    (req, res) => {
      events.push("request " + req.url +
        " sameAsSecureConnection=" + (req.socket === connSocket) +
        " res.socket===req.socket=" + (res.socket === req.socket) +
        " req.client===req.socket=" + (req.client === req.socket) +
        " authorized=" + req.socket.authorized);
      res.end("hello");
    },
  );
  server.on("secureConnection", (s) => {
    connSocket = s;
    events.push("secureConnection " + describe(s));
    s.once("close", () => { closed = true; });
    if (verdict && verdict(s) === false) {
      events.push("destroy");
      s.destroy();
    }
  });
  server.on("connection", (s) => events.push(
    "connection encrypted=" + s.encrypted +
    " remote=" + s.remoteAddress + " localPort=" + (typeof s.localPort)));
  server.on("tlsClientError", (err) => events.push("tlsClientError " + err.code));
  server.on("clientError", (err) => events.push("clientError " + err.code));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const client = await runClient({ ...op, port: server.address().port, ca: CA });
  await new Promise((r) => setTimeout(r, 250));
  console.log("== " + label);
  // 'connection' carries the plain socket, before the handshake: it is the
  // first thing the server hears about a connection, and it is where an
  // application filtering clients by address decides.
  for (const e of events) console.log("   " + e);
  console.log("   client " + client);
  console.log("   socket closed after the connection ended: " + closed);
  server.close();
  await new Promise((r) => setTimeout(r, 50));
}

const MTLS = { requestCert: true, rejectUnauthorized: false, ca: CA };
const good = { cert: CLIENT_CERT, key: CLIENT_KEY };
const rogue = { cert: ROGUE_CERT, key: ROGUE_KEY };
// The documented decision: serve the clients this CA vouched for, refuse
// the rest.
const onlyAuthorized = (s) => s.authorized;

for (const version of ["TLSv1.3", "TLSv1.2"]) {
  await scenario(version + " mTLS, a client the CA signed -- accepted",
    MTLS, { ...good, version }, onlyAuthorized);
  await scenario(version + " mTLS, a client from an unknown CA -- refused",
    MTLS, { ...rogue, version }, onlyAuthorized);
  await scenario(version + " mTLS, a client that sent no certificate -- refused",
    MTLS, { version }, onlyAuthorized);
}

// The connection's socket is one object: a keep-alive client's second
// request carries the same one, with the handshake it was admitted on.
await scenario("mTLS keep-alive, two requests on one connection",
  MTLS, { ...good, version: "TLSv1.3", keepAlive: true }, onlyAuthorized);

// No certificate asked for: the listener still runs, and sees a handshake
// with no peer.
await scenario("no requestCert, a client that sent a certificate anyway",
  {}, { ...good, version: "TLSv1.3" }, null);

// The server refuses on its own (0.16.3): no 'secureConnection' at all.
await scenario("requestCert + rejectUnauthorized, no client certificate",
  { requestCert: true, rejectUnauthorized: true, ca: CA }, { version: "TLSv1.2" }, null);

process.exit(0);
