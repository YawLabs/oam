// What an https.Server does with node:tls's server options: requestCert /
// rejectUnauthorized / ca (a client with no certificate, one from an
// unknown CA, one the CA signed; TLS 1.3 and 1.2), what req.socket reports
// about the handshake, ALPN (https's ['http/1.1'] default), a cleartext
// client, handshakeTimeout, passphrase / pfx, setSecureContext() and
// requestCert changed on a listening server -- the server under test, real
// Node as every client.
//
// The clients are separate `node` processes (the harness's oracle, on PATH)
// in BOTH runs, so the only thing that differs between the two runs is the
// server. Server-side events are collected per server and printed once its
// client is done.
//
// Regression guard: oam's https server ignored requestCert,
// rejectUnauthorized and ca -- it never asked for a client certificate, so
// a server that required one served every client -- and req.socket carried
// none of the handshake (authorized, authorizationError,
// getPeerCertificate()). It also ignored ALPNProtocols, passphrase and pfx,
// read a key it could not use only at listen(), and raised no
// 'tlsClientError' / 'clientError' for a failed handshake.
//
// Fixtures: the case 141 throwaway P-256 CA (valid 2025-2125), the
// localhost leaf it signed, a client leaf it signed, a self-signed "rogue"
// client certificate, and (from case 144) the leaf's key encrypted as
// PKCS#8 and the pair as a PKCS#12 bundle, passphrase "hunter2".
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

const ENC_PKCS8_AES256_SHA256 = `-----BEGIN ENCRYPTED PRIVATE KEY-----
MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBD/NfAlfZ9uL1N7wVVt
wnhUAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQ3SzJ6yMRKmFSVr12
3lg/JgSBkCYCOxV1KkXDgOlVCVslFI3opLMP2/WqW9SV8O6w22TT1dvuQZCA2M/h
fBFdD8vg+8BI5E/79HcL+4pcD/K3fC2Zt0bKlKaHfpKGGJT2McuRaLFX9QUvEv+K
neMEQmUOsMDcb+pfmQDQuVe8pUFvDFXp3IdCpdj7USyaW01vPEjygoSuwG2QRMZV
PIcp7mnM0w==
-----END ENCRYPTED PRIVATE KEY-----
`;

const PFX_AES256 =
  "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";

// ---- the client side: one node process runs a list of connections, one
// after the other, and prints one JSON line per connection. A TLS client
// sends a GET as soon as it is connected and reports the status line and
// body it gets back. A connection dropped after the handshake reads as a
// reset or as a plain end depending on timing (in Node too), so those two
// are one outcome, "closed".
const CLIENT = `
import tls from "node:tls";
import net from "node:net";
const ops = JSON.parse(process.argv[1]);
const settle = (ms) => new Promise((r) => setTimeout(r, ms));
function tlsOp(op) {
  return new Promise((resolve) => {
    const out = { secure: false };
    let data = "";
    let error;
    const s = tls.connect({
      host: "127.0.0.1", port: op.port, servername: "localhost", ca: op.ca,
      rejectUnauthorized: op.reject !== false,
      ALPNProtocols: op.alpn, cert: op.cert, key: op.key, maxVersion: op.maxVersion,
    }, () => {
      out.secure = true;
      out.alpn = s.alpnProtocol;
      out.server = s.getPeerCertificate().subject.CN;
      s.write("GET /x HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n");
    });
    s.setEncoding("utf8");
    s.on("data", (d) => { data += d; });
    // A refusal of the client's certificate is an alert the server sends
    // after reading the flight that carried it (under TLS 1.3, once this
    // side's handshake is already done), and it closes right behind the
    // alert with the rest of that flight, or the request, unread: a reset,
    // which can pre-empt the alert in this client's read. Which of the
    // alert, the reset and a plain end comes first is timing -- Node's own
    // client against Node's own server prints any of them under load -- so
    // the line records that the client was refused, and the server's lines
    // say why. An alert answering the ClientHello alone (ALPN) is read
    // before a clean close, and stays.
    s.on("error", (e) => {
      if (/ALERT_(HANDSHAKE_FAILURE|CERTIFICATE_REQUIRED|UNKNOWN_CA|BAD_CERTIFICATE|CERTIFICATE_UNKNOWN|DECRYPT_ERROR)/.test(e.code)) return;
      error = e.code;
    });
    s.on("close", () => {
      if (data) {
        out.response = data.split("\\r\\n")[0] + " " + data.split("\\r\\n\\r\\n")[1];
      } else {
        out.refused = error && error !== "ECONNRESET" ? error : "closed";
      }
      resolve(out);
    });
    setTimeout(() => { out.timeout = true; s.destroy(); }, 4000).unref();
  });
}
function rawOp(op) {
  return new Promise((resolve) => {
    const chunks = [];
    const out = {};
    const s = net.connect(op.port, "127.0.0.1", () => { if (op.send) s.write(op.send, "latin1"); if (op.close) s.end(); });
    s.on("data", (d) => chunks.push(d));
    s.on("error", () => {});
    s.on("close", () => { out.bytesBack = Buffer.concat(chunks).length; resolve(out); });
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

// What a request's socket reports about its connection's handshake.
function describeSocket(req) {
  const s = req.socket;
  const peer = s.getPeerCertificate();
  const detailed = s.getPeerCertificate(true);
  const cipher = s.getCipher();
  return "authorized=" + s.authorized +
    " authorizationError=" + s.authorizationError +
    " peer=" + (peer.subject ? peer.subject.CN + " (issuer " + peer.issuer.CN + ")" : JSON.stringify(peer)) +
    " issuerCertificate=" + (detailed.issuerCertificate ? detailed.issuerCertificate.subject.CN : "none") +
    " alpn=" + JSON.stringify(s.alpnProtocol) +
    " servername=" + JSON.stringify(s.servername) +
    " encrypted=" + s.encrypted +
    " protocol=" + s.getProtocol() +
    " cipher=" + cipher.name + "/" + cipher.version +
    " req.client=" + (req.client === s);
}

// A handshake failure as the server sees it: node's code, and the socket's
// state (destroyed, whether it still knows its peer).
function describeFailure(err, socket) {
  const message = err.code === "ECONNRESET" || err.code === "ERR_TLS_HANDSHAKE_TIMEOUT"
    ? " " + JSON.stringify(err.message) : "";
  return err.code + message + " destroyed=" + socket.destroyed + " remoteAddress=" + typeof socket.remoteAddress;
}

// A server that records what happens to each connection, and resolves
// `settled` once a request arrived or a handshake failed.
function recordingServer(options, withClientError) {
  const events = [];
  let settle;
  const settled = new Promise((r) => { settle = r; });
  const server = https.createServer(options, (req, res) => {
    events.push("request " + req.method + " " + req.url + " " + describeSocket(req));
    res.end("hello");
    settle();
  });
  server.on("tlsClientError", (err) => {
    events.push("tlsClientError " + err.code);
    settle();
  });
  if (withClientError) {
    server.on("clientError", (err, socket) => {
      events.push("clientError " + describeFailure(err, socket));
      socket.destroy();
    });
  }
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
    const rec = recordingServer(c.server, c.clientError !== false);
    const port = await listen(rec.server);
    servers.push(rec);
    ops.push({ kind: "tls", ...c.client, port });
  }
  const lines = await runClients(ops);
  for (let i = 0; i < cases.length; i++) {
    await within(servers[i].settled, 3000);
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
    tlsServer: s instanceof tls.Server,
    setSecureContext: typeof s.setSecureContext,
    httpAllowHalfOpen: s.httpAllowHalfOpen,
    maxHeadersCount: s.maxHeadersCount,
  });
  console.log("defaults " + shape(https.createServer({ key: KEY, cert: CERT })));
  console.log("explicit " + shape(https.createServer({ key: KEY, cert: CERT, requestCert: true, rejectUnauthorized: false, ALPNProtocols: ["h2", "http/1.1"] })));
  console.log("truthy non-booleans " + shape(https.createServer({ key: KEY, cert: CERT, requestCert: 1, rejectUnauthorized: 0 })));
  console.log("ALPNCallback " + shape(https.createServer({ key: KEY, cert: CERT, ALPNCallback: () => "http/1.1" })));
  const attempt = (label, fn) => {
    try {
      fn();
      console.log(label + ": created");
    } catch (e) {
      console.log(label + ": " + e.name + " " + e.code + " " + JSON.stringify(e.message) +
        " library=" + e.library + " reason=" + e.reason);
    }
  };
  attempt("key that is not a key", () => https.createServer({ key: "not a key", cert: CERT }));
  attempt("key of another certificate", () => https.createServer({ key: CLIENT_KEY, cert: CERT }));
  attempt("cert that is not a certificate", () => https.createServer({ key: KEY, cert: "not a certificate" }));
  attempt("encrypted key, wrong passphrase", () => https.createServer({ key: ENC_PKCS8_AES256_SHA256, passphrase: "nope", cert: CERT }));
  attempt("handshakeTimeout not a number", () => https.createServer({ key: KEY, cert: CERT, handshakeTimeout: "10" }));
  attempt("ALPNCallback with ALPNProtocols", () => https.createServer({ key: KEY, cert: CERT, ALPNProtocols: ["h2"], ALPNCallback: () => "h2" }));
  attempt("SNICallback not a function", () => https.createServer({ key: KEY, cert: CERT, SNICallback: 1 }));
  attempt("options a string", () => https.createServer("options"));
  attempt("bad maxHeaderSize and bad key", () => https.createServer({ key: "not a key", cert: CERT, maxHeaderSize: "x" }));
}

// ---- 2. client certificates, over TLS 1.3 and TLS 1.2.
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
          client: { ca: CA, maxVersion, ...copts },
        });
      }
    }
    await scenario("client certificate " + maxVersion, cases);
  }
  // With no 'clientError' listener the server destroys the socket itself.
  await scenario("no clientError listener", [
    { label: "no certificate", clientError: false, server: { key: KEY, cert: CERT, requestCert: true, ca: [CA] }, client: { ca: CA } },
    { label: "self-signed certificate", clientError: false, server: { key: KEY, cert: CERT, requestCert: true, ca: [CA] }, client: { ca: CA, cert: ROGUE_CERT, key: ROGUE_KEY } },
  ]);
}

// ---- 3. ALPN: https offers http/1.1 unless told otherwise.
{
  const cases = [];
  for (const [slabel, server] of [
    ["server default", { key: KEY, cert: CERT }],
    ["server ALPNProtocols undefined", { key: KEY, cert: CERT, ALPNProtocols: undefined }],
    ["server [h2,http/1.1]", { key: KEY, cert: CERT, ALPNProtocols: ["h2", "http/1.1"] }],
  ]) {
    for (const [clabel, alpn] of [
      ["client none", undefined],
      ["client [h2]", ["h2"]],
      ["client [h2,http/1.1]", ["h2", "http/1.1"]],
      ["client [foo]", ["foo"]],
    ]) {
      cases.push({ label: slabel + ", " + clabel, server, client: { ca: CA, alpn } });
    }
  }
  await scenario("ALPN", cases);
}

// ---- 4. a client that does not speak TLS gets no answer at all.
await scenario("cleartext", [
  { label: "HTTP/1.1 request", server: { key: KEY, cert: CERT }, client: { kind: "raw", send: "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n" } },
  { label: "HTTP/1.1 request, no clientError listener", clientError: false, server: { key: KEY, cert: CERT }, client: { kind: "raw", send: "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n" } },
  { label: "closes at once", server: { key: KEY, cert: CERT }, client: { kind: "raw", close: true } },
]);

// ---- 5. handshakeTimeout, and a silent client holds up no one else.
{
  let served = 0;
  let timedOut;
  const timeout = new Promise((r) => { timedOut = r; });
  const server = https.createServer({ key: KEY, cert: CERT, handshakeTimeout: 1500 }, (req, res) => {
    served++;
    res.end("hello");
  });
  server.on("clientError", (err, socket) => {
    timedOut("clientError " + describeFailure(err, socket));
    socket.destroy();
  });
  const port = await listen(server);
  const silent = net.connect(port, "127.0.0.1");
  silent.on("error", () => {});
  await new Promise((r) => setTimeout(r, 50));
  const [line] = await runClients([{ kind: "tls", port, ca: CA }]);
  console.log("served while a client is silent: " + line + " requests=" + served);
  console.log("the silent client: " + await within(timeout, 5000));
  silent.destroy();
  server.close();
}

// ---- 6. the key as an encrypted PEM with its passphrase, or a pfx.
for (const [label, options] of [
  ["encrypted key + passphrase", { key: ENC_PKCS8_AES256_SHA256, passphrase: "hunter2", cert: CERT }],
  ["pfx + passphrase", { pfx: Buffer.from(PFX_AES256, "base64"), passphrase: "hunter2" }],
]) {
  const server = https.createServer(options, (req, res) => res.end("hello"));
  const port = await listen(server);
  const [line] = await runClients([{ kind: "tls", port, ca: CA }]);
  console.log(label + ": " + line);
  server.close();
}

// ---- 7. a listening server's options are read for each new connection:
// setSecureContext() (a certificate rotation), then requestCert and
// rejectUnauthorized switched.
{
  const events = [];
  const server = https.createServer({ key: KEY, cert: CERT, ca: [CA] }, (req, res) => {
    events.push("request authorized=" + req.socket.authorized + " authorizationError=" + req.socket.authorizationError);
    res.end("hello");
  });
  server.on("clientError", (err, socket) => {
    events.push("clientError " + describeFailure(err, socket));
    socket.destroy();
  });
  const port = await listen(server);
  const lines = [];
  lines.push(...await runClients([{ kind: "tls", port, ca: CA }]));
  server.setSecureContext({ key: ROGUE_KEY, cert: ROGUE_CERT, ca: [CA] });
  lines.push(...await runClients([{ kind: "tls", port, reject: false }]));
  server.setSecureContext({ key: KEY, cert: CERT, ca: [CA] });
  server.requestCert = true;
  lines.push(...await runClients([
    { kind: "tls", port, ca: CA },
    { kind: "tls", port, ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY },
  ]));
  server.rejectUnauthorized = false;
  lines.push(...await runClients([{ kind: "tls", port, ca: CA }]));
  await new Promise((r) => setTimeout(r, 100));
  server.close();
  for (const [i, label] of [
    "before setSecureContext",
    "after setSecureContext",
    "requestCert on, no certificate",
    "requestCert on, the CA's certificate",
    "rejectUnauthorized off, no certificate",
  ].entries()) {
    console.log("listening server | " + label + ": " + lines[i]);
  }
  for (const e of events) console.log("  server " + e);
}

// ---- 8. the rest of the handshake a request's socket reports.
{
  const server = https.createServer({ key: KEY, cert: CERT, requestCert: true, ca: [CA] }, (req, res) => {
    const s = req.socket;
    const x509 = s.getPeerX509Certificate();
    const peer = s.getPeerCertificate();
    console.log("peer X509Certificate: " + x509.subject.replace(/\n/g, ", ") + " / " + x509.issuer.replace(/\n/g, ", "));
    console.log("peer fingerprint256 matches: " + (peer.fingerprint256 === x509.fingerprint256));
    console.log("getCipher: " + JSON.stringify(s.getCipher()));
    console.log("getEphemeralKeyInfo: " + JSON.stringify(s.getEphemeralKeyInfo()));
    res.end("hello");
  });
  const port = await listen(server);
  await runClients([{ kind: "tls", port, ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY }]);
  server.close();
}

// ---- 9. a session made while the server let any client certificate in,
// resumed after it turned rejectUnauthorized on: the certificate the
// session carries is judged again and the connection refused before any
// request is read, as a new handshake with it is. Over TLS 1.3 (a ticket)
// and TLS 1.2.
{
  const RESUME = `
import tls from "node:tls";
const [port, ca, cert, key, maxVersion, session64] = JSON.parse(process.argv[1]);
const out = { secure: false, data: "" };
let session = null;
const s = tls.connect({
  host: "127.0.0.1", port, servername: "localhost", ca, cert, key, maxVersion,
  session: session64 ? Buffer.from(session64, "base64") : undefined,
}, () => {
  out.secure = true;
  out.reused = s.isSessionReused();
  s.write("GET /x HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n");
});
s.setEncoding("utf8");
s.on("session", (next) => { session = next; });
s.on("data", (d) => { out.data += d; });
s.on("error", (e) => { if (e.code !== "ECONNRESET") out.error = e.code; });
s.on("close", () => setTimeout(() => {
  out.data = out.data ? out.data.split("\\r\\n")[0] + " " + out.data.split("\\r\\n\\r\\n")[1] : "";
  process.stdout.write(JSON.stringify({ out, session: session && session.toString("base64") }));
}, 20));
setTimeout(() => { out.timeout = true; s.destroy(); }, 4000).unref();
`;
  const client = (port, maxVersion, session) => new Promise((resolve) => {
    const child = spawn("node", ["--input-type=module", "-e", RESUME,
      JSON.stringify([port, CA, ROGUE_CERT, ROGUE_KEY, maxVersion, session])], { stdio: ["ignore", "pipe", "inherit"] });
    let text = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (d) => { text += d; });
    child.on("close", () => resolve(JSON.parse(text)));
  });
  for (const maxVersion of ["TLSv1.3", "TLSv1.2"]) {
    const events = [];
    const server = https.createServer({ key: KEY, cert: CERT, requestCert: true, rejectUnauthorized: false, ca: [CA] }, (req, res) => {
      events.push("request authorized=" + req.socket.authorized + " authorizationError=" + req.socket.authorizationError);
      res.end("hello");
    });
    server.on("tlsClientError", (e) => events.push("tlsClientError " + e.code));
    const port = await listen(server);
    const first = await client(port, maxVersion);
    console.log("stricter server " + maxVersion + " | first: " + JSON.stringify(first.out) + " session=" + !!first.session);
    server.rejectUnauthorized = true;
    const resumed = await client(port, maxVersion, first.session);
    console.log("stricter server " + maxVersion + " | resumed: " + JSON.stringify(resumed.out));
    const fresh = await client(port, maxVersion);
    console.log("stricter server " + maxVersion + " | new handshake: " + JSON.stringify(fresh.out));
    await new Promise((r) => setTimeout(r, 100));
    server.close();
    for (const e of events) console.log("  server " + e);
  }
}
