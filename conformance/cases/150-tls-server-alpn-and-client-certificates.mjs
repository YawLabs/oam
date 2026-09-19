// What a tls.Server does with ALPNProtocols, requestCert / rejectUnauthorized
// / ca, a cleartext client, a client that never finishes its handshake, and
// options it cannot use -- the server under test, real Node as every client.
//
// The clients are separate `node` processes (the harness's oracle, on PATH)
// in BOTH runs, so the only thing that differs between the two runs is the
// server, and every client-side line is what Node's own TLS stack makes of
// it. Server-side events are collected per server and printed once its
// client is done.
//
// Regression guard: oam's tls.Server ignored ALPNProtocols (nothing was ever
// negotiated), requestCert / rejectUnauthorized / ca (a client certificate
// was never asked for, so a server that required one admitted every
// client), and handshakeTimeout; its handshakes ran one at a time, so a
// client that never sent a ClientHello stalled every later connection; a
// key it could not use was only discovered per connection; and a cleartext
// client was answered with a TLS alert.
//
// Fixtures: a throwaway P-256 CA (valid 2025-2125), the localhost leaf it
// signed (SAN DNS:localhost, IP:127.0.0.1, serverAuth), a client leaf it
// signed (clientAuth), and a self-signed "rogue" client certificate.
import tls from "node:tls";
import net from "node:net";
import { spawn } from "node:child_process";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 50000).unref();

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
// after the other, and prints one JSON line per connection.
const CLIENT = `
import tls from "node:tls";
import net from "node:net";
const ops = JSON.parse(process.argv[1]);
const settle = (ms) => new Promise((r) => setTimeout(r, ms));
function tlsOp(op) {
  return new Promise((resolve) => {
    const out = { secure: false, data: "", end: false };
    const s = tls.connect({
      host: "127.0.0.1", port: op.port, servername: "localhost", ca: op.ca,
      ALPNProtocols: op.alpn, cert: op.cert, key: op.key, maxVersion: op.maxVersion,
    }, () => {
      out.secure = true;
      out.alpn = s.alpnProtocol;
      if (op.write) s.write(op.write);
    });
    s.setEncoding("utf8");
    s.on("data", (d) => { out.data += d; });
    s.on("end", () => { out.end = true; });
    s.on("error", (e) => { out.error = e.code; });
    s.on("close", () => resolve(out));
    setTimeout(() => { out.timeout = true; s.destroy(); }, 4000).unref();
  });
}
function rawOp(op) {
  return new Promise((resolve) => {
    const chunks = [];
    const out = {};
    const s = net.connect(op.port, "127.0.0.1", () => { if (op.send) s.write(op.send, "latin1"); if (op.close) s.end(); });
    s.on("data", (d) => chunks.push(d));
    s.on("error", (e) => { out.error = e.code; });
    s.on("close", () => { out.bytes = Buffer.concat(chunks).toString("hex"); resolve(out); });
    if (op.hold) setTimeout(() => { out.held = true; s.destroy(); }, op.hold).unref();
    setTimeout(() => { out.timeout = true; s.destroy(); }, 4000).unref();
  });
}
for (const op of ops) {
  const out = op.kind === "raw" ? await rawOp(op) : await tlsOp(op);
  console.log(JSON.stringify(out));
  await settle(20);
}
`;

function runClients(ops) {
  return new Promise((resolve) => {
    const child = spawn("node", ["--input-type=module", "-e", CLIENT, JSON.stringify(ops)], {
      stdio: ["ignore", "pipe", "inherit"],
    });
    let out = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (d) => { out += d; });
    child.on("close", () => resolve(out.trim().split("\n").filter(Boolean)));
  });
}

// A server that records what happens to each connection, and resolves
// `settled` once a connection it accepted is secured or has failed.
function recordingServer(options) {
  const events = [];
  let settle;
  const settled = new Promise((r) => { settle = r; });
  const server = tls.createServer(options, (socket) => {
    const peer = socket.getPeerCertificate();
    events.push(
      "secureConnection alpn=" + JSON.stringify(socket.alpnProtocol) +
        " authorized=" + socket.authorized +
        " authorizationError=" + socket.authorizationError +
        " peer=" + (peer && peer.subject ? peer.subject.CN : JSON.stringify(peer)) +
        " servername=" + JSON.stringify(socket.servername),
    );
    socket.end("hello");
    settle();
  });
  server.on("tlsClientError", (err, socket) => {
    events.push("tlsClientError " + err.code + " socket=" + (socket instanceof tls.TLSSocket));
    settle();
  });
  return { server, events, settled };
}
function listen(server) {
  return new Promise((r) => server.listen(0, "127.0.0.1", () => r(server.address().port)));
}
const within = (p, ms) => Promise.race([p, new Promise((r) => setTimeout(r, ms))]);

// Run each case on its own server, all clients in one node process.
async function scenario(title, cases) {
  const servers = [];
  const ops = [];
  for (const c of cases) {
    const rec = recordingServer(c.server);
    const port = await listen(rec.server);
    servers.push(rec);
    ops.push({ ...c.client, port });
  }
  const lines = await runClients(ops);
  for (let i = 0; i < cases.length; i++) {
    await within(servers[i].settled, 3000);
    // A late ECONNRESET for a connection the client already closed.
    await new Promise((r) => setTimeout(r, 30));
    servers[i].server.close();
    console.log(title + " | " + cases[i].label);
    console.log("  client " + lines[i]);
    for (const e of servers[i].events) console.log("  server " + e);
  }
}

// ---- 1. the server's own view of its options, and the ones Node refuses.
{
  const shape = (s) => JSON.stringify({
    requestCert: s.requestCert,
    rejectUnauthorized: s.rejectUnauthorized,
    ALPNProtocols: s.ALPNProtocols === undefined ? "undefined" : [...s.ALPNProtocols],
  });
  console.log("defaults " + shape(tls.createServer({ key: KEY, cert: CERT })));
  console.log("truthy non-booleans " + shape(tls.createServer({ key: KEY, cert: CERT, requestCert: 1, rejectUnauthorized: 0, ALPNProtocols: ["h2", "http/1.1"] })));
  console.log("explicit " + shape(tls.createServer({ key: KEY, cert: CERT, requestCert: true, rejectUnauthorized: false, ALPNProtocols: Buffer.from([2, 104, 50]) })));
  console.log("string ALPN is ignored " + shape(tls.createServer({ key: KEY, cert: CERT, ALPNProtocols: "h2" })));
  const attempt = (label, options) => {
    try {
      tls.createServer(options);
      console.log(label + ": created");
    } catch (e) {
      console.log(label + ": " + e.name + " " + e.code + " " + JSON.stringify(e.message) +
        " library=" + e.library + " reason=" + e.reason);
    }
  };
  attempt("ALPN name over 255 bytes", { key: KEY, cert: CERT, ALPNProtocols: ["x".repeat(256)] });
  attempt("ALPNCallback with ALPNProtocols", { key: KEY, cert: CERT, ALPNProtocols: ["h2"], ALPNCallback: () => "h2" });
  attempt("handshakeTimeout not a number", { key: KEY, cert: CERT, handshakeTimeout: "10" });
  attempt("key that is not a key", { key: "not a key", cert: CERT });
  attempt("key of another certificate", { key: CLIENT_KEY, cert: CERT });
  attempt("cert that is not a certificate", { key: KEY, cert: "not a certificate" });
  attempt("ca that is not a certificate", { key: KEY, cert: CERT, ca: "not a certificate" });
  attempt("key without cert", { key: KEY });
}

// ---- 2. ALPN: the server's order among what the client offers.
{
  const cases = [];
  for (const [slabel, salpn] of [
    ["server none", undefined],
    ["server [h2,http/1.1]", ["h2", "http/1.1"]],
    ["server [http/1.1,h2]", ["http/1.1", "h2"]],
    ["server wire [h2,http/1.1]", Buffer.from([2, 104, 50, 8, ...Buffer.from("http/1.1")])],
  ]) {
    for (const [clabel, calpn] of [
      ["client none", undefined],
      ["client [h2]", ["h2"]],
      ["client [http/1.1,h2]", ["http/1.1", "h2"]],
      ["client [foo]", ["foo"]],
    ]) {
      cases.push({
        label: slabel + ", " + clabel,
        server: { key: KEY, cert: CERT, ALPNProtocols: salpn },
        client: { kind: "tls", ca: CA, alpn: calpn },
      });
    }
  }
  await scenario("ALPN", cases);
}

// ---- 3. client certificates, over TLS 1.3 and TLS 1.2.
{
  const clients = {
    "no certificate": {},
    "certificate the CA signed": { cert: CLIENT_CERT, key: CLIENT_KEY },
    "self-signed certificate": { cert: ROGUE_CERT, key: ROGUE_KEY },
  };
  const servers = {
    "requestCert, rejectUnauthorized, ca": { requestCert: true, ca: [CA] },
    "requestCert, rejectUnauthorized false, ca": { requestCert: true, rejectUnauthorized: false, ca: CA },
    "requestCert, rejectUnauthorized, no ca": { requestCert: true },
    "no requestCert, ca": { ca: [CA] },
  };
  for (const maxVersion of ["TLSv1.3", "TLSv1.2"]) {
    const cases = [];
    for (const [slabel, sopts] of Object.entries(servers)) {
      for (const [clabel, copts] of Object.entries(clients)) {
        cases.push({
          label: slabel + " / " + clabel,
          server: { key: KEY, cert: CERT, ...sopts },
          client: { kind: "tls", ca: CA, maxVersion, ...copts },
        });
      }
    }
    await scenario("client certificate " + maxVersion, cases);
  }
}

// ---- 4. a client that does not speak TLS gets no answer at all.
await scenario("cleartext", [
  { label: "HTTP/1.1 request", server: { key: KEY, cert: CERT }, client: { kind: "raw", send: "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n" } },
  { label: "HTTP/2 preface", server: { key: KEY, cert: CERT }, client: { kind: "raw", send: "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" } },
  { label: "proxy CONNECT", server: { key: KEY, cert: CERT }, client: { kind: "raw", send: "CONNECT localhost:443 HTTP/1.1\r\n\r\n" } },
  { label: "closes at once", server: { key: KEY, cert: CERT }, client: { kind: "raw", close: true } },
]);

// ---- 5. handshakeTimeout, and a silent client holds up no one else.
{
  let secured = 0;
  let timedOut;
  const timeout = new Promise((r) => { timedOut = r; });
  const server = tls.createServer({ key: KEY, cert: CERT, handshakeTimeout: 1500 }, (socket) => {
    secured++;
    socket.end("hello");
  });
  server.on("tlsClientError", (err) => timedOut(err.code + " " + JSON.stringify(err.message)));
  const port = await listen(server);
  const silent = net.connect(port, "127.0.0.1");
  silent.on("error", () => {});
  await new Promise((r) => setTimeout(r, 50));
  const [line] = await runClients([{ kind: "tls", port, ca: CA }]);
  console.log("served while a client is silent: " + line + " secureConnection=" + secured);
  console.log("the silent client: tlsClientError " + await within(timeout, 5000));
  silent.destroy();
  server.close();
}

// ---- 6. a resumed session keeps its client certificate's verdict.
{
  const events = [];
  const server = tls.createServer({ key: KEY, cert: CERT, requestCert: true, ca: [CA] }, (socket) => {
    const peer = socket.getPeerCertificate();
    events.push("secureConnection authorized=" + socket.authorized + " peer=" + (peer.subject ? peer.subject.CN : "none"));
    socket.end("hello");
  });
  server.on("tlsClientError", (e) => events.push("tlsClientError " + e.code));
  const port = await listen(server);
  const RESUMING = `
import tls from "node:tls";
const [port, ca, cert, key, maxVersion] = JSON.parse(process.argv[1]);
let session;
for (let i = 0; i < 2; i++) {
  const out = await new Promise((resolve) => {
    const s = tls.connect({ host: "127.0.0.1", port, servername: "localhost", ca, cert, key, session, maxVersion }, () => s.resume());
    s.on("session", (sess) => { session = sess; });
    s.on("end", () => resolve("reused=" + s.isSessionReused()));
    s.on("error", (e) => resolve("error " + e.code));
  });
  console.log(out);
}
`;
  for (const maxVersion of ["TLSv1.3", "TLSv1.2"]) {
    const out = await new Promise((resolve) => {
      const child = spawn("node", ["--input-type=module", "-e", RESUMING, JSON.stringify([port, CA, CLIENT_CERT, CLIENT_KEY, maxVersion])], {
        stdio: ["ignore", "pipe", "inherit"],
      });
      let text = "";
      child.stdout.setEncoding("utf8");
      child.stdout.on("data", (d) => { text += d; });
      child.on("close", () => resolve(text.trim().split("\n").join(", ")));
    });
    console.log("resumption " + maxVersion + ": " + out);
  }
  await new Promise((r) => setTimeout(r, 100));
  server.close();
  for (const e of events) console.log("  server " + e);
}
