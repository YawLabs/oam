// tls.connect / tls.createServer (and https.request / https.createServer)
// honour minVersion, maxVersion and secureProtocol (#144). Node negotiates the
// highest protocol both peers allow, pins it when asked, throws synchronously
// (a TypeError) at tls.connect() / createServer() / https.request() for an
// invalid version or method or a secureProtocol+min/max conflict, and errors
// asynchronously when the effective range has no usable version or the peer's
// highest version is below the client's floor. oam used to ignore all three
// options and always ran TLS 1.3 whenever the peer allowed it.
//
// Every value here was measured on Node v22.22.2, including the edges: a
// truthy secureProtocol conflicts with any non-null min/max ('' included),
// only a string names a method ('' is unknown, a number is ignored), the
// SSLv2/SSLv3 families have their own message, and messages render the value
// with %j (a number bare, a string quoted). A server range with nothing to
// offer still binds and fails each handshake: the client sees the
// protocol_version alert, the server a per-connection tlsClientError.
//
// What is printed is only what is byte-identical to Node: the negotiated
// getProtocol() string, the error codes, and the synchronous messages. The
// negotiated 1.2 cipher name (rustls prefers AES-256, OpenSSL AES-128) and the
// OpenSSL async error messages (which carry OpenSSL's build path) diverge by
// design and are never printed -- see docs/node-divergences.md.
import tls from "node:tls";
import https from "node:https";

const CERT = `-----BEGIN CERTIFICATE-----
MIIDCTCCAfGgAwIBAgIUJscRiMbEzxV45KtAxD+Lly4dJrQwDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDYxNTEyMzAwN1oXDTI3MDYx
NTEyMzAwN1owFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAoQ5a/fh4J3VW0MPpngEpN+yRUdJtlmY6aBhV/984yEIm
ng9/MGoK0ZRdB8YYGqx4awK1z82ECwtmmdVO/77WA4q6N0CJRzmAF6BN9RgzoyKV
2w1ltowPFyB6SrVqcW1MHqA/9NX/gw/ckvcjcuazYeI857joWulUmR/iWIpSNuBJ
c6odEIkfXG9W6/GyZwlutQXnKaa8eClLqCm+hDnkPBHx+doGWxezFVeOfFAdQM8w
NXT7mj4QN3fiHFDQHI6UkSnVttu7lAAEHY978gjnVyixAPX2dY9mB/Ed4R5eSOpJ
eTR7bXH6+QmUcDJSaDblM5vB3fb3zhitEGLo/APdQQIDAQABo1MwUTAdBgNVHQ4E
FgQUPffw9cdyC1LQ2PLrzN7IZjkpKmMwHwYDVR0jBBgwFoAUPffw9cdyC1LQ2PLr
zN7IZjkpKmMwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAWtdW
V/jSdVB5cN4GOwYXTHhh3dkYDtAPvFPCXbYacelaQe8mlRWv2BBHAhOZdmoJ3ai/
kNRw0D6pKqjcF4p17of9S07ZFCRaQGBAsDEd9jNY156AlEXu4Z8yp/kXE3fvznib
WHrQjdlDcmC2H/Ao+S7f4BkmbvsabyDbUoo+0Drk4MDvqga2azrFDdljqXQxzrEH
/mEwoi9pfukgFnFnhDE+WEqNsZQF9Yxa5QEX6d5tgbOcxS2NpKDug4xSgkpAQ0l6
XKpI59mdGTahOy9zGuNfTqVTHvrFoSXudnNHUjkfHK7Mh/VrNz9ZGpwDt5fGFD4x
E13+0jp6In545LYu+A==
-----END CERTIFICATE-----`;
const KEY = `-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQChDlr9+HgndVbQ
w+meASk37JFR0m2WZjpoGFX/3zjIQiaeD38wagrRlF0HxhgarHhrArXPzYQLC2aZ
1U7/vtYDiro3QIlHOYAXoE31GDOjIpXbDWW2jA8XIHpKtWpxbUweoD/01f+DD9yS
9yNy5rNh4jznuOha6VSZH+JYilI24Elzqh0QiR9cb1br8bJnCW61Becpprx4KUuo
Kb6EOeQ8EfH52gZbF7MVV458UB1AzzA1dPuaPhA3d+IcUNAcjpSRKdW227uUAAQd
j3vyCOdXKLEA9fZ1j2YH8R3hHl5I6kl5NHttcfr5CZRwMlJoNuUzm8Hd9vfOGK0Q
Yuj8A91BAgMBAAECgf9+I0AgqPlx7fSQjN/rX/1oT1+BNc2efXJBFM5GGA3gye50
3K5AvMy8V/aEoCFAwtOM/BJpLgy8mbFByk6U/mGfZIdzvpfFsMMhvetQiiPnIK89
YMDIt+kZs9YTrQIw0+lKEzgECZaUj1exwt2AoC7d+tK4qZlRmm0ngFFGBw9c6g4E
bKpZPPb62HjVAPcPPNJzj0ULTCkFQ7CPhgyz7q6UQUQJ0kM/8DWbnI9qbOWkv0qN
TafdX50piyHstcXGNFelOXmUMw1qQvbPo28qpzkxH7bDU8pShzsnySJL2HL5Wxbr
PbzZ94WOOLXfD3OmT5oW9kHDpH/zUd8pKlbYkNkCgYEA1ygCE4ZSNm0B7U8iwgBK
0Aszxpnf9f4aKKfb9CsmLaf4rxmAAqRaxT4Eki2yebqRX15Ctzlf1ryddKxNAAdh
gCcc+KAdwoJOO3pkwac+r/jsmvWqXHHi/Jn9Bhj886n5NkDGfKiCBL3rUHonwOjN
7y61ExJIy46kOM81Pm88B90CgYEAv6E6owvvAjFs1eoWD5oyUnO1HeAhKvBClDjZ
dcoY965ak5RFM4Da/HcnXAho4+pJY+4O48PIi8nQeZugm8DvpOKfivxYTI3ISDmz
CG0m7N9jJiYOPyt8dpn7Yl7R8OqFvfZAd/KkJ1wBMpsKy1MGU1pdny9mVaAVjxei
fnxNprUCgYEAn2onT6wgUe8mlFwkFrX8uHT0Ydw1EqC5ZRIqaJln6kAghCxSqqJ4
FtjCrkRpjsPrXkwLBpLeLc8GoyHe03ykgz13u8d3BV1i9bLT4KA4VE4NkSsglOpV
EnBOByyQj0GLQuVvq4F3BGhrZ+96cPaNTwC+bWkIwrnnd6gffSkRw4kCgYEAsAEE
mzZdunTs0nii9IeaipJNmnf93rM3Y23nhUEut2ZDOOLowEosV8+UrfnnZNYNvCOt
N1LeAk5FFTx0QjntoVKoWH43F3DtsDCWmDmwk8UFCsfPNAPb2A7LjekrCAxO9E+V
nNWWIbRmQTWXr3G9EJeh/5AIfMKAqqF5lJTUuTUCgYEAtzMfzgUekShhJoGov7uH
MyykhATJv+3ZlR0BCuEjgb7Lu6tu/pbgD1SkhpQ3QbM+XF5DgNJWxQATcgPWP6wy
C7rRXUYQtUTmtwTetACx3EEz7k2ixAxxdDCUPJIxGcVIPVKt6sTovr3yGLMuc4f7
I5PYIZ3kyY8EsQqX4JpTtbY=
-----END PRIVATE KEY-----`;

let section = "start";
const watchdog = setTimeout(() => {
  console.log("WATCHDOG " + section);
  process.exit(9);
}, 30000);

// A tls server; `serverErr` resolves with the code of its first
// tlsClientError, so a refused handshake can be reported from both ends.
function listen(serverOpts) {
  let onErr;
  const serverErr = new Promise((r) => { onErr = r; });
  const server = tls.createServer({ cert: CERT, key: KEY, ...serverOpts }, (s) => {
    s.resume();
    s.on("error", () => {});
  });
  server.on("tlsClientError", (e) => onErr(e.code));
  server.on("error", () => {});
  return new Promise((r) => server.listen(0, "127.0.0.1", () => r({ server, serverErr })));
}

function connectTo(port, clientOpts) {
  return new Promise((resolve) => {
    const c = tls.connect(
      { host: "127.0.0.1", port, rejectUnauthorized: false, servername: "localhost", ...clientOpts },
      () => { const p = c.getProtocol(); c.destroy(); resolve("-> " + p); },
    );
    c.on("error", (e) => resolve("ERROR " + e.code));
  });
}

// The negotiated protocol for a client range against a server range. Prints
// only getProtocol(), never the cipher (which diverges on the 1.2 suites).
async function negotiated(label, serverOpts, clientOpts) {
  section = label;
  const { server } = await listen(serverOpts);
  console.log(label + " " + (await connectTo(server.address().port, clientOpts)));
  await new Promise((r) => server.close(r));
}

// A refused handshake, reported from both ends: the client's code and the
// server's per-connection tlsClientError code.
async function refused(label, serverOpts, clientOpts) {
  section = label;
  const { server, serverErr } = await listen(serverOpts);
  const client = await connectTo(server.address().port, clientOpts);
  const serverCode = await Promise.race([serverErr, new Promise((r) => setTimeout(() => r("none"), 2000))]);
  console.log(label + " client " + client + " | server tlsClientError " + serverCode);
  await new Promise((r) => server.close(r));
}

// A synchronous throw: code, message and class, all byte-identical.
function throws(label, fn) {
  section = label;
  try {
    const r = fn();
    if (r && r.close) r.close();
    if (r && r.destroy) r.destroy();
    console.log(label + " NO THROW");
  } catch (e) {
    console.log(label + " " + e.code + " | " + e.message + " | " + (e instanceof TypeError) + " " + e.name + " | " + String(e));
  }
}
const connectThrows = (label, opts) => throws(label, () => tls.connect({ host: "127.0.0.1", port: 1, ...opts }, () => {}));

// ---- protocol negotiation: client-side pins
await negotiated("default", {}, {});
await negotiated("min1.3", {}, { minVersion: "TLSv1.3" });
await negotiated("max1.2", {}, { maxVersion: "TLSv1.2" });
await negotiated("pin1.2", {}, { minVersion: "TLSv1.2", maxVersion: "TLSv1.2" });
await negotiated("min1.1", {}, { minVersion: "TLSv1.1" });
await negotiated("sp1.2", {}, { secureProtocol: "TLSv1_2_method" });
await negotiated("sp1.2client", {}, { secureProtocol: "TLSv1_2_client_method" });
await negotiated("spTLS", {}, { secureProtocol: "TLS_method" });
// SSLv23_* is capped at TLS 1.2 in Node (the name predates 1.3); TLS_* is not.
await negotiated("spSSLv23", {}, { secureProtocol: "SSLv23_method" });
await negotiated("spSSLv23client", {}, { secureProtocol: "SSLv23_client_method" });
await negotiated("spTLSclient", {}, { secureProtocol: "TLS_client_method" });
// Node's edges: a falsy or non-string secureProtocol is ignored; an explicit
// null version is the default.
await negotiated("spFalsy+min1.3", {}, { secureProtocol: 0, minVersion: "TLSv1.3" });
await negotiated("spNumber", {}, { secureProtocol: 12345 });
await negotiated("minNull", {}, { minVersion: null });
await negotiated("sp1.2+maxNull", {}, { secureProtocol: "TLSv1_2_method", maxVersion: null });
// ---- server-side pins
await negotiated("serverMax1.2", { maxVersion: "TLSv1.2" }, {});
await negotiated("serverMin1.1", { minVersion: "TLSv1.1" }, {});

// ---- synchronous throws at tls.connect()
connectThrows("badMin", { minVersion: "TLSv9" });
connectThrows("badMax", { maxVersion: "bogus" });
connectThrows("badMinNumber", { minVersion: 771 });
connectThrows("badMaxBoolean", { maxVersion: true });
connectThrows("badMinEmpty", { minVersion: "" });
connectThrows("badMethod", { secureProtocol: "no_such_method" });
connectThrows("methodEmpty", { secureProtocol: "" });
connectThrows("methodEmpty+min1.3", { secureProtocol: "", minVersion: "TLSv1.3" });
connectThrows("methodTLSv1_3", { secureProtocol: "TLSv1_3_method" });
connectThrows("methodSSLv3", { secureProtocol: "SSLv3_method" });
connectThrows("methodSSLv2client", { secureProtocol: "SSLv2_client_method" });
connectThrows("conflict", { secureProtocol: "TLSv1_2_method", minVersion: "TLSv1.2" });
connectThrows("conflictMax", { secureProtocol: "TLSv1_2_method", maxVersion: "TLSv1.3" });
connectThrows("conflictBoth", { secureProtocol: "TLSv1_2_method", minVersion: "TLSv1.2", maxVersion: "TLSv1.3" });
connectThrows("conflictMinEmpty", { secureProtocol: "TLSv1_2_method", minVersion: "" });
connectThrows("conflictNumberSp", { secureProtocol: 12345, minVersion: "TLSv1.3" });
// ---- and at tls.createServer() / https.createServer() / https.request()
throws("serverBadMin", () => tls.createServer({ cert: CERT, key: KEY, minVersion: "TLSv9" }));
throws("serverConflict", () => tls.createServer({ cert: CERT, key: KEY, secureProtocol: "TLSv1_2_method", maxVersion: "TLSv1.3" }));
throws("httpsServerBadMin", () => https.createServer({ cert: CERT, key: KEY, minVersion: "TLSv9" }));
throws("httpsRequestBadMin", () => https.request({ host: "127.0.0.1", port: 1, minVersion: "TLSv9" }, () => {}));
throws("httpsRequestBadMethod+rejectFalse", () => https.request({ host: "127.0.0.1", port: 1, rejectUnauthorized: false, secureProtocol: "bogus_method" }, () => {}));
throws("httpsRequestConflict", () => https.request({ host: "127.0.0.1", port: 1, secureProtocol: "TLSv1_2_method", maxVersion: "TLSv1.3" }, () => {}));

// ---- async connection errors (codes only)
// Client floor above the server's ceiling: the server sends a protocol_version
// alert; the client reports ERR_SSL_TLSV1_ALERT_PROTOCOL_VERSION.
await negotiated("mismatch", { maxVersion: "TLSv1.2" }, { minVersion: "TLSv1.3" });
// An effective range with no version to offer, on the client: the SSL
// context has no protocols, reported once the transport is up. An explicit
// sub-1.2 range is the same: OpenSSL cannot build a legacy hello either.
await negotiated("noProtocols", {}, { maxVersion: "TLSv1.1" });
await negotiated("noProtocolsExplicit", {}, { minVersion: "TLSv1", maxVersion: "TLSv1.1" });
await negotiated("noProtocolsSp1.1", {}, { secureProtocol: "TLSv1_1_method" });
// The same on the server: it binds, and each handshake is refused with the
// alert -- the client's code and the server's tlsClientError code.
await refused("serverMax1.1", { maxVersion: "TLSv1.1" }, {});
await refused("serverMin1.3Max1.2", { minVersion: "TLSv1.3", maxVersion: "TLSv1.2" }, {});

// ---- https: a server pin, and the pin on a non-verifying request
section = "httpsServerMax1.2";
{
  const server = https.createServer({ cert: CERT, key: KEY, maxVersion: "TLSv1.2" }, (req, res) => res.end("ok"));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  console.log("httpsServerMax1.2 " + (await connectTo(server.address().port, {})));
  await new Promise((r) => server.close(r));
}
section = "httpsServerMax1.1";
{
  const server = https.createServer({ cert: CERT, key: KEY, maxVersion: "TLSv1.1" }, (req, res) => res.end("ok"));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  console.log("httpsServerMax1.1 " + (await connectTo(server.address().port, {})));
  await new Promise((r) => server.close(r));
}
section = "httpsRequestRejectFalseMax1.2";
{
  // A tls server speaking just enough HTTP/1.1 to answer, reporting the
  // protocol it negotiated with the https client.
  const server = tls.createServer({ cert: CERT, key: KEY }, (s) => {
    let seen = "";
    s.on("data", (d) => {
      seen += d;
      if (seen.includes("\r\n\r\n")) {
        const body = "proto=" + s.getProtocol();
        s.end("HTTP/1.1 200 OK\r\nContent-Length: " + body.length + "\r\nConnection: close\r\n\r\n" + body);
      }
    });
    s.on("error", () => {});
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  const body = await new Promise((resolve) => {
    const r = https.request(
      { host: "127.0.0.1", port, path: "/", rejectUnauthorized: false, maxVersion: "TLSv1.2" },
      (res) => { let b = ""; res.on("data", (d) => (b += d)); res.on("end", () => resolve(b)); },
    );
    r.on("error", (e) => resolve("ERROR " + e.code));
    r.end();
  });
  console.log("httpsRequestRejectFalseMax1.2 " + body);
  await new Promise((r) => server.close(r));
}

clearTimeout(watchdog);
