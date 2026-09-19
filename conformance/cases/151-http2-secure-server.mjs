// http2.createSecureServer, driven by real Node clients: the server serves
// HTTP/2 over TLS -- 'secureConnection', 'session', 'stream' and the
// compatibility API's 'request', in Node's order and shapes -- and a client
// that does not speak TLS gets no answer at all.
//
// Every client is a separate `node` process (the harness's oracle, on PATH)
// in BOTH runs, so only the server differs between them; each client line is
// what Node's own http2 / tls / net stack made of the server.
//
// Regression guard: oam's createSecureServer ignored its key and cert and
// served cleartext HTTP/2 (h2c) and HTTP/1.1 on the port: a cleartext client
// was answered, a TLS client was not, and the (req, res) handler was handed
// (stream, headers) instead.
//
// Fixtures: a throwaway P-256 CA (valid 2025-2125) and the localhost leaf it
// signed (SAN DNS:localhost, IP:127.0.0.1).
import http2 from "node:http2";
import tls from "node:tls";
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

// ---- the client side: one node process, a list of operations run one
// after the other, one JSON line each.
const CLIENT = `
import http2 from "node:http2";
import net from "node:net";
const ops = JSON.parse(process.argv[1]);
// date varies; te is a request header node also echoes on a response and
// hyper does not send.
const clean = (h) => { const o = { ...h }; delete o.date; delete o.te; return o; };
function h2Session(op) {
  const authority = (op.cleartext ? "http://127.0.0.1:" : "https://localhost:") + op.port;
  return http2.connect(authority, { ca: op.ca, cert: op.cert, key: op.key });
}
function h2Request(session, spec) {
  return new Promise((resolve) => {
    const out = {};
    const req = session.request(spec.headers, spec.endStream ? { endStream: true } : undefined);
    req.on("response", (h) => { out.headers = clean(h); });
    req.setEncoding("utf8");
    out.body = "";
    req.on("data", (d) => { out.body += d; });
    req.on("error", (e) => { out.error = e.code; });
    req.on("close", () => { out.rstCode = req.rstCode; resolve(out); });
    if (!spec.endStream) {
      for (const chunk of spec.body || []) req.write(chunk);
      req.end();
    }
  });
}
async function h2Op(op) {
  const session = h2Session(op);
  const out = { session: [] };
  session.on("error", (e) => out.session.push("error " + e.code));
  // A graceful close may take one GOAWAY or two (RFC 9113 6.8's two-step
  // shutdown); what matters is the code.
  session.on("goaway", (code) => { if (!out.session.includes("goaway " + code)) out.session.push("goaway " + code); });
  const closed = new Promise((r) => session.on("close", () => { out.session.push("close"); r(); }));
  await new Promise((r) => { session.on("connect", () => { out.alpn = session.alpnProtocol; r(); }); session.on("close", r); });
  if (op.parallel) {
    out.results = await Promise.all(op.requests.map((spec) => h2Request(session, spec)));
  } else {
    out.results = [];
    for (const spec of op.requests || []) out.results.push(await h2Request(session, spec));
  }
  if (!op.keepOpen) session.close();
  await Promise.race([closed, new Promise((r) => setTimeout(r, 3000))]);
  return out;
}
function rawOp(op) {
  return new Promise((resolve) => {
    const chunks = [];
    const out = {};
    const s = net.connect(op.port, "127.0.0.1", () => s.write(op.send, "latin1"));
    s.on("data", (d) => chunks.push(d));
    s.on("error", (e) => { out.error = e.code; });
    s.on("close", () => { out.bytes = Buffer.concat(chunks).toString("latin1"); resolve(out); });
    setTimeout(() => { out.timeout = true; s.destroy(); }, 4000).unref();
  });
}
for (const op of ops) {
  const out = op.kind === "raw" ? await rawOp(op) : await h2Op(op);
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

// ---- 1. one session, the compatibility API: what each side sees.
{
  const events = [];
  const server = http2.createSecureServer({ key: KEY, cert: CERT }, (req, res) => {
    const sock = req.socket;
    events.push(
      "request " + req.method + " " + req.url + " httpVersion=" + req.httpVersion +
        " scheme=" + req.scheme + " authority is localhost:port=" + /^localhost:\d+$/.test(req.authority) +
        " " + req.constructor.name + "/" + res.constructor.name +
        " socket: encrypted=" + sock.encrypted + " alpn=" + sock.alpnProtocol +
        " TLSSocket=" + (sock instanceof tls.TLSSocket) + " peer=" + JSON.stringify(sock.getPeerCertificate()),
    );
    let body = "";
    req.setEncoding("utf8");
    req.on("data", (chunk) => { body += chunk; });
    req.on("end", () => {
      if (req.url === "/missing") {
        res.statusCode = 404;
        res.end();
        return;
      }
      if (req.url === "/nothing") {
        res.writeHead(204, { "x-empty": "yes" });
        res.end();
        return;
      }
      res.setHeader("content-type", "text/plain");
      res.setHeader("set-cookie", ["a=1", "b=2"]);
      res.setHeader("x-multi", ["one", "two"]);
      if (req.url === "/chunks") {
        res.write("one,");
        res.write("two,");
        res.end("three");
        return;
      }
      res.end(req.method + " " + req.url + " body=" + JSON.stringify(body));
    });
  });
  server.on("secureConnection", (socket) => events.push("secureConnection alpn=" + socket.alpnProtocol));
  server.on("session", (session) => {
    const s = session.socket;
    events.push(
      "session type=" + session.type + " alpn=" + session.alpnProtocol + " encrypted=" + session.encrypted +
        " socket: TLSSocket=" + (s instanceof tls.TLSSocket) + " alpn=" + s.alpnProtocol +
        " protocol=" + s.getProtocol() + " remote=" + s.remoteAddress + " servername=" + s.servername,
    );
    let manipulation;
    try { s.write("x"); } catch (e) { manipulation = e.code; }
    events.push("session.socket.write: " + manipulation);
    session.on("close", () => events.push("session close"));
  });
  server.on("stream", (stream, headers, flags, rawHeaders) => {
    events.push(
      "stream " + headers[":method"] + " " + headers[":path"] + " id=" + stream.id +
        " headers=" + JSON.stringify(Object.keys(headers).sort()) + " flags=" + flags +
        " endAfterHeaders=" + stream.endAfterHeaders + " rawHeaders=" + rawHeaders.length +
        " prototype=" + (Object.getPrototypeOf(headers) === null) + " x-in=" + headers["x-in"],
    );
  });
  server.on("tlsClientError", (e) => events.push("tlsClientError " + e.code));
  const port = await listen(server);
  const [line] = await runClients([{
    kind: "h2", port, ca: CA, requests: [
      { headers: { ":path": "/a?b=1", "x-in": ["p", "q"] }, endStream: true },
      { headers: { ":path": "/post", ":method": "POST" }, body: ["pay", "load"] },
      { headers: { ":path": "/chunks" }, endStream: true },
      { headers: { ":path": "/missing" }, endStream: true },
      { headers: { ":path": "/nothing" }, endStream: true },
      { headers: { ":path": "/head", ":method": "HEAD" }, endStream: true },
    ],
  }]);
  await settle(100);
  server.close();
  const out = JSON.parse(line);
  console.log("client alpn=" + out.alpn + " session=" + JSON.stringify(out.session));
  for (const r of out.results) console.log("client " + JSON.stringify(r));
  for (const e of events) console.log("server " + e);
}

// ---- 2. the stream API: respond(), and what respond() refuses.
{
  const events = [];
  const server = http2.createSecureServer({ key: KEY, cert: CERT });
  server.on("stream", (stream, headers) => {
    const attempt = (label, fn) => {
      try {
        fn();
        events.push(label + ": ok");
      } catch (e) {
        events.push(label + ": " + e.name + " " + e.code + " " + JSON.stringify(e.message));
      }
    };
    if (headers[":path"] === "/refusals") {
      attempt("status 99", () => stream.respond({ ":status": 99 }));
      attempt("status 600", () => stream.respond({ ":status": 600 }));
      attempt("connection header", () => stream.respond({ connection: "close" }));
      attempt("transfer-encoding header", () => stream.respond({ "transfer-encoding": "chunked" }));
      attempt("te: trailers", () => stream.respond({ ":status": 200, te: "trailers", "x-ok": "1" }, { endStream: true }));
      attempt("second respond", () => stream.respond({ ":status": 200 }));
      events.push("headersSent=" + stream.headersSent + " sentHeaders[x-ok]=" + stream.sentHeaders["x-ok"]);
      return;
    }
    stream.on("close", () => events.push("stream close rstCode=" + stream.rstCode));
    stream.respond({ ":status": 201, "content-type": "application/json", "x-list": ["1", "2"] });
    stream.write('{"a":');
    stream.end("1}");
  });
  const port = await listen(server);
  const [line] = await runClients([{
    kind: "h2", port, ca: CA, requests: [
      { headers: { ":path": "/refusals" }, endStream: true },
      { headers: { ":path": "/json" }, endStream: true },
    ],
  }]);
  await settle(100);
  server.close();
  const out = JSON.parse(line);
  for (const r of out.results) console.log("client " + JSON.stringify(r));
  for (const e of events) console.log("server " + e);
}

// ---- 3. many streams at once on one session.
{
  let requests = 0;
  const server = http2.createSecureServer({ key: KEY, cert: CERT }, (req, res) => {
    requests++;
    setTimeout(() => res.end("r" + req.url.slice(1)), 5);
  });
  const port = await listen(server);
  const specs = [];
  for (let i = 0; i < 20; i++) specs.push({ headers: { ":path": "/" + i }, endStream: true });
  const [line] = await runClients([{ kind: "h2", port, ca: CA, parallel: true, requests: specs }]);
  server.close();
  const out = JSON.parse(line);
  console.log("parallel: requests=" + requests + " bodies=" + out.results.map((r) => r.body).join(","));
}

// ---- 4. clients that do not speak TLS get nothing back.
{
  const events = [];
  const server = http2.createSecureServer({ key: KEY, cert: CERT }, (req, res) => {
    events.push("request!");
    res.end("leaked");
  });
  server.on("session", () => events.push("session!"));
  server.on("tlsClientError", (e, socket) => events.push("tlsClientError " + e.code + " socket=" + (socket instanceof tls.TLSSocket)));
  const port = await listen(server);
  const lines = await runClients([
    { kind: "h2", port, cleartext: true, requests: [{ headers: { ":path": "/" }, endStream: true }] },
    { kind: "raw", port, send: "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n" },
    { kind: "raw", port, send: "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n" },
  ]);
  await settle(100);
  server.close();
  console.log("h2c client: " + lines[0]);
  console.log("cleartext HTTP/1.1: " + lines[1]);
  console.log("cleartext HTTP/2 preface: " + lines[2]);
  for (const e of events) console.log("server " + e);
}

// ---- 5. session.close() from the server: the stream in flight finishes.
{
  const events = [];
  const server = http2.createSecureServer({ key: KEY, cert: CERT }, (req, res) => {
    const session = req.stream.session;
    session.close(() => events.push("close callback"));
    events.push("closed=" + session.closed + " destroyed=" + session.destroyed);
    setTimeout(() => res.end("finished after close"), 20);
  });
  const port = await listen(server);
  const [line] = await runClients([{
    kind: "h2", port, ca: CA, keepOpen: true, requests: [{ headers: { ":path": "/" }, endStream: true }],
  }]);
  await settle(100);
  server.close();
  const out = JSON.parse(line);
  console.log("client " + JSON.stringify(out.results[0]) + " session=" + JSON.stringify(out.session));
  for (const e of events) console.log("server " + e);
}
