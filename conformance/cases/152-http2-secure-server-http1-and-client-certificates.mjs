// http2.createSecureServer's other clients, driven by real Node: an HTTPS
// client that negotiates no ALPN or http/1.1 (a curl-style request) --
// served as HTTP/1.1 under allowHTTP1, handed to an 'unknownProtocol'
// listener, or answered with Node's 403 -- and a server that requires a
// client certificate.
//
// Every client is a separate `node` process (the harness's oracle, on PATH)
// in BOTH runs, so only the server differs between them.
//
// Regression guard: oam's createSecureServer served everything in
// cleartext; it had no allowHTTP1, no 'unknownProtocol', and ignored
// requestCert / rejectUnauthorized / ca, so a server that required a
// client certificate admitted every client.
//
// Fixtures: a throwaway P-256 CA (valid 2025-2125), the localhost leaf it
// signed, a client leaf it signed, and a self-signed "rogue" client
// certificate.
import http from "node:http";
import http2 from "node:http2";
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

const CLIENT = `
import http2 from "node:http2";
import https from "node:https";
import tls from "node:tls";
const ops = JSON.parse(process.argv[1]);
function httpsOp(op) {
  return new Promise((resolve) => {
    const req = https.get({
      host: "127.0.0.1", port: op.port, path: op.path || "/", servername: "localhost", ca: op.ca,
      agent: false,
    }, (res) => {
      let body = "";
      res.setEncoding("utf8");
      res.on("data", (d) => { body += d; });
      res.on("end", () => resolve({
        status: res.statusCode, httpVersion: res.httpVersion,
        server: res.headers["x-served-by"], type: res.headers["content-type"], body,
      }));
    });
    req.on("error", (e) => resolve({ error: e.code }));
  });
}
function alpnOp(op) {
  return new Promise((resolve) => {
    const s = tls.connect({
      host: "127.0.0.1", port: op.port, servername: "localhost", ca: op.ca, ALPNProtocols: op.alpn,
    }, () => { const alpn = s.alpnProtocol; s.end(); resolve({ alpn }); });
    s.on("error", (e) => resolve({ error: e.code }));
  });
}
function h2Op(op) {
  return new Promise((resolve) => {
    const out = {};
    const session = http2.connect("https://localhost:" + op.port, {
      ca: op.ca, cert: op.cert, key: op.key, maxVersion: op.maxVersion,
    });
    // A refused handshake may end the session with or without an error,
    // and the stream with it: whichever ends first reports. A connection
    // dropped after its handshake reads as a reset or as a plain close
    // depending on timing (in Node too), so those two are one outcome.
    const done = () => {
      session.destroy();
      if (out.status === undefined) {
        const code = out.streamError || out.sessionError;
        resolve({ refused: code && code !== "ECONNRESET" ? code : "connection closed" });
      } else {
        resolve(out);
      }
    };
    session.on("error", (e) => { out.sessionError = e.code; });
    session.on("close", done);
    const req = session.request({ ":path": op.path || "/" }, { endStream: true });
    req.on("response", (h) => { out.status = h[":status"]; });
    let body = "";
    req.setEncoding("utf8");
    req.on("data", (d) => { body += d; });
    req.on("error", (e) => { out.streamError = e.code; });
    req.on("close", () => { if (out.status !== undefined) out.body = body; done(); });
  });
}
for (const op of ops) {
  const out = op.kind === "https" ? await httpsOp(op) : op.kind === "alpn" ? await alpnOp(op) : await h2Op(op);
  console.log(JSON.stringify(out));
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
function listen(server) {
  return new Promise((r) => server.listen(0, "127.0.0.1", () => r(server.address().port)));
}
const settle = (ms) => new Promise((r) => setTimeout(r, ms));

function handler(events) {
  return (req, res) => {
    const socket = req.socket;
    events.push(
      "request " + req.url + " httpVersion=" + req.httpVersion +
        " IncomingMessage=" + (req instanceof http.IncomingMessage) +
        " ServerResponse=" + (res instanceof http.ServerResponse) +
        " alpn=" + socket.alpnProtocol + " encrypted=" + socket.encrypted +
        " authorized=" + socket.authorized + " authorizationError=" + socket.authorizationError +
        " peer=" + (socket.getPeerCertificate().subject ? socket.getPeerCertificate().subject.CN : "none"),
    );
    res.setHeader("x-served-by", "http" + req.httpVersion);
    res.end("hello over " + req.httpVersion);
  };
}

// ---- 1. without allowHTTP1: Node's 403, and ALPN that offers no h2.
for (const variant of ["no listener", "unknownProtocol listener"]) {
  const events = [];
  const server = http2.createSecureServer({ key: KEY, cert: CERT }, handler(events));
  server.on("session", () => events.push("session"));
  server.on("secureConnection", (socket) => events.push("secureConnection alpn=" + socket.alpnProtocol));
  server.on("tlsClientError", (e) => events.push("tlsClientError " + e.code));
  if (variant === "unknownProtocol listener") {
    server.on("unknownProtocol", (socket) => {
      events.push("unknownProtocol alpn=" + socket.alpnProtocol);
      socket.end("HTTP/1.1 421 Misdirected Request\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno");
    });
  }
  const port = await listen(server);
  const lines = await runClients([
    { kind: "https", port, ca: CA },
    { kind: "alpn", port, ca: CA, alpn: ["http/1.1"] },
    { kind: "alpn", port, ca: CA, alpn: ["foo", "h2"] },
    { kind: "h2", port, ca: CA },
  ]);
  await settle(150);
  server.close();
  console.log("without allowHTTP1, " + variant);
  console.log("  https.get: " + lines[0]);
  console.log("  ALPN [http/1.1]: " + lines[1]);
  console.log("  ALPN [foo,h2]: " + lines[2]);
  console.log("  http2: " + lines[3]);
  for (const e of events) console.log("  server " + e);
}

// ---- 2. allowHTTP1: the same handler serves both.
{
  const events = [];
  const server = http2.createSecureServer({ key: KEY, cert: CERT, allowHTTP1: true }, handler(events));
  console.log("allowHTTP1 server: timeout=" + server.timeout + " headersTimeout=" + server.headersTimeout +
    " requestTimeout=" + server.requestTimeout + " ALPNProtocols=" + JSON.stringify([...server.ALPNProtocols]));
  server.on("session", () => events.push("session"));
  const port = await listen(server);
  const lines = await runClients([
    { kind: "https", port, ca: CA, path: "/one" },
    { kind: "alpn", port, ca: CA, alpn: ["http/1.1", "h2"] },
    { kind: "h2", port, ca: CA, path: "/two" },
  ]);
  await settle(100);
  server.close();
  console.log("  https.get: " + lines[0]);
  console.log("  ALPN [http/1.1,h2]: " + lines[1]);
  console.log("  http2: " + lines[2]);
  for (const e of events) console.log("  server " + e);
}

// ---- 3. client certificates, required or requested.
const clients = {
  "no certificate": {},
  "certificate the CA signed": { cert: CLIENT_CERT, key: CLIENT_KEY },
  "self-signed certificate": { cert: ROGUE_CERT, key: ROGUE_KEY },
};
for (const [label, options] of [
  ["requestCert, rejectUnauthorized", { requestCert: true, ca: [CA] }],
  ["requestCert, rejectUnauthorized false", { requestCert: true, rejectUnauthorized: false, ca: [CA] }],
]) {
  const events = [];
  const server = http2.createSecureServer({ key: KEY, cert: CERT, ...options }, handler(events));
  server.on("session", () => events.push("session"));
  server.on("tlsClientError", (e) => events.push("tlsClientError " + e.code));
  const port = await listen(server);
  const ops = Object.values(clients).map((c) => ({ kind: "h2", port, ca: CA, maxVersion: "TLSv1.3", path: "/cert", ...c }));
  const lines = await runClients(ops);
  await settle(150);
  server.close();
  console.log(label);
  Object.keys(clients).forEach((name, i) => console.log("  " + name + ": " + lines[i]));
  for (const e of events) console.log("  server " + e);
}
