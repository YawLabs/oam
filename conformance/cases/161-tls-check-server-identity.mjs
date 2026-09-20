// tls.checkServerIdentity(hostname, cert) and a connection's
// `checkServerIdentity` option.
//
// The function is node's lib/tls.js: the certificate's subjectAltName DNS
// names (wildcards in the leftmost label only, never for a two-label name
// or an A-label), its IP addresses (compared in canonical form), and its
// subject CN only when it has neither; undefined for a match, else an
// ERR_TLS_CERT_ALTNAME_INVALID carrying reason, host and cert. A name
// printed as a JSON string literal is read back as one name: a URI or an
// e-mail address that holds ", DNS:victim.test" does not make the
// certificate valid for victim.test.
//
// On a connection, node calls `checkServerIdentity` -- tls.checkServerIdentity
// unless the options bring their own, which must be a function -- once the
// chain is trusted, with the host name and the peer's certificate; an
// Error it returns makes the socket unauthorized, and with
// rejectUnauthorized the connection fails with it. A function of the
// caller's own is what decides the name: one that accepts a certificate for
// another name connects.
//
// Regression guard: oam's tls.checkServerIdentity returned undefined for
// every certificate, so code that checks the name itself (after
// rejectUnauthorized:false, or around a pin) accepted any certificate; and
// oam checked the name natively even when the caller brought a function,
// which it then did not call on a mismatch.
//
// Fixtures: ./fixtures/tls-names.mjs, and the case 141 throwaway P-256 CA
// and its localhost / 127.0.0.1 leaf for the connections.
import tls from "node:tls";
import https from "node:https";
import { spawn } from "node:child_process";
import { X509Certificate } from "node:crypto";
import { CERTS } from "./fixtures/tls-names.mjs";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 50000).unref();

// ---- 1. the function on the fixtures' certificates.
const describe = (r, cert) => r === undefined
  ? "match"
  : r.code + " reason=" + JSON.stringify(r.reason) + " host=" + JSON.stringify(r.host) + " cert=" + (r.cert === cert);
for (const [name, pem] of Object.entries(CERTS)) {
  const cert = new X509Certificate(pem).toLegacyObject();
  console.log(name);
  for (const host of ["victim.test", "other.test", "a.example.test", "foo.wild.test", "fo.wild.test",
    "127.0.0.1", "::1", "0:0::1", "1.2.3.4", "::1.2.3.4", "::ffff:1.2.3.4", "2001:db8::1", "VICTIM.TEST."]) {
    console.log("  " + host + " -> " + describe(tls.checkServerIdentity(host, cert), cert));
  }
}

// ---- 2. hand-built certificate objects.
const check = (label, host, cert) => {
  let out;
  try {
    const r = tls.checkServerIdentity(host, cert);
    out = r === undefined ? "match" : r.code + " " + JSON.stringify(r.reason);
  } catch (e) {
    out = "throws " + e.name + " " + e.code + " " + JSON.stringify(e.message);
  }
  console.log(label + ": " + out);
};
check("CN only", "a.test", { subject: { CN: "a.test" } });
check("CN array", "b.test", { subject: { CN: ["a.test", "b.test"] } });
check("CN array, no match", "c.test", { subject: { CN: ["a.test", "b.test"] } });
check("empty CN", "a.test", { subject: { CN: "" } });
check("no subject", "a.test", {});
check("CN ignored beside a DNS name", "a.test", { subject: { CN: "a.test" }, subjectaltname: "DNS:b.test" });
check("CN used beside a URI only", "a.test", { subject: { CN: "a.test" }, subjectaltname: "URI:http://b.test" });
check("empty altnames", "a.test", { subject: { CN: "a.test" }, subjectaltname: "" });
check("upper case", "A.TEST", { subjectaltname: "DNS:a.Test" });
check("wildcard", "x.a.test", { subjectaltname: "DNS:*.A.test" });
check("wildcard, one label", "a.test", { subjectaltname: "DNS:*.test" });
check("wildcard, two labels deep", "x.y.a.test", { subjectaltname: "DNS:*.a.test" });
check("wildcard, partial", "fooxbar.a.test", { subjectaltname: "DNS:foo*bar.a.test" });
check("wildcard, two stars", "ab.a.test", { subjectaltname: "DNS:*b*.a.test" });
check("wildcard, A-label", "xn--x.a.test", { subjectaltname: "DNS:xn--*.a.test" });
check("empty label", "a..test", { subjectaltname: "DNS:a..test" });
check("non-ASCII pattern", "é.test", { subjectaltname: "DNS:é.test" });
check("trailing dot", "a.test.", { subjectaltname: "DNS:a.test" });
check("IP, trailing dot", "1.2.3.4.", { subjectaltname: "IP Address:1.2.3.4" });
check("IP with a zone", "fe80::1%eth0", { subjectaltname: "IP Address:FE80:0:0:0:0:0:0:1" });
check("IP against DNS", "1.2.3.4", { subjectaltname: "DNS:1.2.3.4" });
check("IP against CN", "1.2.3.4", { subject: { CN: "1.2.3.4" } });
check("number host", 5, { subjectaltname: "DNS:5" });
check("escaped comma", "a,b.test", { subjectaltname: 'DNS:"a\\u002cb.test"' });
check("no space after the comma", "b.test", { subjectaltname: "DNS:a.test,DNS:b.test" });
check("quoted, no space after the comma", "b.test", { subjectaltname: 'DNS:"a.test",DNS:b.test' });
check("unterminated quote", "a.test", { subjectaltname: 'DNS:"a.test' });
check("undefined cert", "a.test", undefined);
console.log("length " + tls.checkServerIdentity.length + " name " + tls.checkServerIdentity.name);
{
  const e = tls.checkServerIdentity("c.test", { subjectaltname: "DNS:b.test" });
  console.log("error " + e.name + " " + (e instanceof Error) + " keys=" + JSON.stringify(Object.keys(e)) +
    " " + String(e));
}

// ---- 3. connections: a node server with a certificate for localhost and
// 127.0.0.1, the client under test.
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
const SERVER = `
import tls from "node:tls";
import https from "node:https";
const CERT = \`-----BEGIN CERTIFICATE-----
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
\`;
const KEY = \`-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgQLidYpqFITu5wno8
Fw5b5Ahrg5eTwH0UqA7RU57egNKhRANCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0b
j0ngz5dpnyIRlajsUptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1V
-----END PRIVATE KEY-----
\`;
const t = tls.createServer({ key: KEY, cert: CERT }, (s) => { s.on("error", () => {}); s.end("hello"); });
t.on("tlsClientError", () => {});
const h = https.createServer({ key: KEY, cert: CERT }, (q, s) => s.end("served"));
await new Promise((r) => t.listen(0, "127.0.0.1", r));
await new Promise((r) => h.listen(0, "127.0.0.1", r));
console.log(JSON.stringify({ tls: t.address().port, https: h.address().port }));
process.stdin.on("data", () => {});
process.stdin.on("end", () => process.exit(0));
`;
const server = spawn("node", ["--input-type=module", "-e", SERVER], { stdio: ["pipe", "pipe", "inherit"] });
const ports = await new Promise((resolve) => {
  let buffered = "";
  server.stdout.on("data", (d) => {
    buffered += d;
    if (buffered.includes("\n")) resolve(JSON.parse(buffered));
  });
});

const calls = [];
const recorded = (fn) => (host, cert) => {
  calls.push(host + " " + cert.subject.CN + (cert.issuerCertificate ? " <- " + cert.issuerCertificate.subject.CN : ""));
  return fn(host, cert);
};
function connect(label, options) {
  calls.length = 0;
  return new Promise((resolve) => {
    let socket;
    const done = (text) => {
      console.log(label + ": " + text + (calls.length ? " calls=" + JSON.stringify(calls) : ""));
      resolve();
    };
    try {
      socket = tls.connect({ host: "127.0.0.1", port: ports.tls, ca: CA, ...options }, () => {
        const peer = socket.getPeerCertificate();
        const manual = options.manual ? " manual=" + describe(tls.checkServerIdentity(options.servername, peer), peer) : "";
        socket.destroy();
        done("secure authorized=" + socket.authorized + " authorizationError=" + socket.authorizationError + manual);
      });
    } catch (e) {
      done("throws " + e.name + " " + e.code + " " + JSON.stringify(e.message));
      return;
    }
    socket.on("error", (e) => done("error " + e.code + " " + JSON.stringify(e.message) +
      (e.host !== undefined ? " host=" + e.host + " cert=" + typeof e.cert : "")));
  });
}
const acceptAll = () => undefined;
const pin = (fingerprint) => (host, cert) => {
  if (cert.fingerprint256 !== fingerprint) {
    const e = new Error("certificate pin mismatch for " + host);
    e.code = "PIN_MISMATCH";
    return e;
  }
  return undefined;
};
await connect("name matches (host)", {});
await connect("name matches (servername)", { servername: "localhost" });
await connect("another name", { servername: "wrong.test" });
await connect("another name, trailing dot", { servername: "wrong.test." });
await connect("another name, rejectUnauthorized false", { servername: "wrong.test", rejectUnauthorized: false, manual: true });
await connect("another name, tls.checkServerIdentity", { servername: "wrong.test", checkServerIdentity: recorded(tls.checkServerIdentity) });
await connect("another name, accepts every name", { servername: "wrong.test", checkServerIdentity: recorded(acceptAll) });
await connect("pin that fails", { servername: "localhost", checkServerIdentity: recorded(pin("00")) });
await connect("pin that fails, rejectUnauthorized false", { servername: "localhost", rejectUnauthorized: false, checkServerIdentity: pin("00") });
await connect("function returning an Error with no code", { servername: "localhost", rejectUnauthorized: false, checkServerIdentity: () => new Error("no") });
await connect("untrusted chain: not called", { servername: "wrong.test", ca: undefined, checkServerIdentity: recorded(acceptAll) });
await connect("untrusted chain, rejectUnauthorized false: not called", { servername: "localhost", ca: undefined, rejectUnauthorized: false, checkServerIdentity: recorded(acceptAll) });
await connect("checkServerIdentity undefined", { checkServerIdentity: undefined });
await connect("checkServerIdentity null", { checkServerIdentity: null });
await connect("checkServerIdentity a string", { checkServerIdentity: "x" });
{
  const original = tls.checkServerIdentity;
  tls.checkServerIdentity = recorded(acceptAll);
  await connect("tls.checkServerIdentity replaced", { servername: "wrong.test" });
  tls.checkServerIdentity = original;
}

// ---- 4. an https request's checkServerIdentity.
function get(label, options) {
  calls.length = 0;
  return new Promise((resolve) => {
    https.get({ host: "127.0.0.1", port: ports.https, ca: CA, agent: false, path: "/", ...options }, (res) => {
      let body = "";
      res.on("data", (d) => { body += d; });
      res.on("end", () => {
        console.log(label + ": " + res.statusCode + " " + body + (calls.length ? " calls=" + JSON.stringify(calls) : ""));
        resolve();
      });
    }).on("error", (e) => {
      console.log(label + ": error " + e.code + (calls.length ? " calls=" + JSON.stringify(calls) : ""));
      resolve();
    });
  });
}
await get("https, another name", { servername: "wrong.test" });
await get("https, another name, accepts every name", { servername: "wrong.test", checkServerIdentity: recorded(acceptAll) });
await get("https, Host header names it", { headers: { host: "wrong.test" }, checkServerIdentity: recorded(tls.checkServerIdentity) });
await get("https, pin that fails", { checkServerIdentity: pin("00") });

server.stdin.end();
