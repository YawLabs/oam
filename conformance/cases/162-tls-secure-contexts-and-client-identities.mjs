// tls.createSecureContext() and the secure context a client connection is
// made with.
//
// node's createSecureContext reads the key, cert and pfx options when it
// builds the context: a key it cannot decrypt (a wrong or missing
// passphrase) throws ERR_OSSL_BAD_DECRYPT there, one it cannot read
// ERR_OSSL_UNSUPPORTED, a cert with no certificate in it
// ERR_OSSL_PEM_NO_START_LINE, a key that is not the certificate's
// ERR_OSSL_X509_KEY_VALUES_MISMATCH, and a pfx whose MAC does not verify
// "mac verify failure". tls.connect() builds its context the same way,
// synchronously, so it throws the same errors. It returns a SecureContext
// whose `context` is the native half.
//
// A connection given `secureContext` is made with that context: its `ca`,
// its certificate and its version range, whatever the connect options say
// about them (https.request passes the option through). A client's
// certificate may come as a passphrase-protected key (the passphrase
// option, or `key: [{ pem, passphrase }]`) or a pfx.
//
// Regression guard: oam's createSecureContext copied its options and
// checked nothing, tls.connect read neither `secureContext`, `passphrase`
// nor `pfx` (an encrypted client key failed at the handshake with no code,
// and a secureContext's `ca` was replaced by the default store or the
// options' own `ca`).
//
// Fixtures: the case 141 throwaway P-256 CA, its localhost leaf and client
// leaf, the self-signed "rogue" certificate (as a CA that signed neither),
// the case 155 triple-DES leaf key and bundle, and the client key encrypted
// and bundled here, passphrase "hunter2".
import tls from "node:tls";
import https from "node:https";
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
const ENC_TRAD_DES3 = `-----BEGIN EC PRIVATE KEY-----
Proc-Type: 4,ENCRYPTED
DEK-Info: DES-EDE3-CBC,13845B1516F673B0

QqcIwLLpGeXMk9Qp9GDpdtoKCHRRhfvnW8SYur02hROCEclq8NOeiOV/x4gPyNAd
hpfeGVGj+CY7cwuDJIyB8BvrO1CQTnWwMRW00/L5qCg2VYcWEsRSIBeZzxAPkYNr
mu17GYfMbvfl7SMLQntzcA3Us039ATbzn9FUeDGxA9w=
-----END EC PRIVATE KEY-----
`;
const PFX_3DES = Buffer.from(
  "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", "base64");
const CLIENT_KEY_AES = `-----BEGIN ENCRYPTED PRIVATE KEY-----
MIH0MF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBAGu9yanYZduWJrLmVs
ehDnAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQlsAlgxZucD8Y+E6O
fT/XrgSBkLOJDgi+589SodmBhP/o/kK3xeYSzaLSugD4n+NqFe1tXulNlQ3uOKpU
LkeVQ7SsxM620kp7AHjtAsxtLlMJgKfuQYXLjw3DNo21Knw/nx2MpNqv9cQMY9h+
KFym+TydagRZVKgdvwEfJYs7aTVzRAZJFg3cGZdZB54geDc8pzYfxr3e+nvBRctH
7g//dycOQg==
-----END ENCRYPTED PRIVATE KEY-----
`;
const CLIENT_KEY_DES3 = `-----BEGIN ENCRYPTED PRIVATE KEY-----
MIHrMFYGCSqGSIb3DQEFDTBJMDEGCSqGSIb3DQEFDDAkBBBKTUim8AOMiZKlYoK4
LaBhAgIIADAMBggqhkiG9w0CCQUAMBQGCCqGSIb3DQMHBAjbXnNDE1VW2gSBkFvH
iWU3++IcS4VwGh2qnAmcYId71Iabbb2GMMq6cmhpzDBeIgnMEbUD0Hf+YQmMjs/6
DTfsySqVdG4VZojHnwmcvUqhAvSwgZkxWi1vhw/C9bcevnIUmotsLzQmDDYdzLwX
7XhjP2hNgTAs+ElF/I3NKvhbI0QsFdN+MjrxSzw7zN5c5xntZ7PQINaGL6FxqA==
-----END ENCRYPTED PRIVATE KEY-----
`;
const CLIENT_PFX = Buffer.from(
  "MIIEPAIBAzCCA/IGCSqGSIb3DQEHAaCCA+MEggPfMIID2zCCAooGCSqGSIb3DQEHBqCCAnswggJ3" +
  "AgEAMIICcAYJKoZIhvcNAQcBMF8GCSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBCr8x0unWxy" +
  "+MBh3K6pmLBQAgIIADAMBggqhkiG9w0CCQUAMB0GCWCGSAFlAwQBKgQQJejqkBYbYkb8rYugmouP" +
  "B4CCAgDnRjCf2eg0R4//6IFsKFCpznegF/X9cKxrsjq7hXDDV69lcwRQLN5fiCfOyBYcobuW4buL" +
  "7dMlOpbPNnhTtG2z8rIHlYg/JZM0FeVQrhQY2IFA4CMVf+BI/Yo4enrgYlGL7Jd60tBDCYROXbJh" +
  "id5LBqMMjIsz1fMokvoUnpKPI15w22om2c8CEMFD9qPSB3IVJ/iyxLAsdZrYwir9YDqxTNnybM1T" +
  "cyxMtgLdicM7AbvrmkTwVQFUdounomt5+P76imvTHss+NYIL0lH0fGmQkeCxkb8C091RcJiIvKLT" +
  "lzUGXl1rwRH8ibnZ5fr2lhO3nN6zzLf/bKCxMkN71DXxRvaf77bH9kej4crjAO6v8sYrg9cvuAhE" +
  "6mjBwuJ09ORBS2QpX5CddeIpZLy6pxi+WPCxAd5f6SWcS9/BXAN6kuIhxtZL5tqK7Y2PUV1k85Ji" +
  "jOmvYa7+dskWEYbA0G/FmNxsnqvnLZG/PjfITZ/oJ5f/TffiXTC6QGAXyQlIqTx1TC517DQyxypp" +
  "TGsDynChz07Znrh37DDmkD8cGyhpwzoX+PsGGizbEmrh10NWkkdyoQhM/NFMYX9wGLL3K3+Y1ZEO" +
  "0kYT/CKnVd8J2CVGFzMDL6Yp0ZxXMo63g9gHGPLNtCftWv7I7tcRgU6z6e4WYID8YGovGRZjINmm" +
  "Zle2TTCCAUkGCSqGSIb3DQEHAaCCAToEggE2MIIBMjCCAS4GCyqGSIb3DQEMCgECoIH3MIH0MF8G" +
  "CSqGSIb3DQEFDTBSMDEGCSqGSIb3DQEFDDAkBBCCMz8XWi/5udbaLBfKIYMRAgIIADAMBggqhkiG" +
  "9w0CCQUAMB0GCWCGSAFlAwQBKgQQRn8ApVCt6kZ75HEEyRSbygSBkPVvt3ynEXPdK3W+phxB2Uej" +
  "udEHT2etao3vghWYwa0XZwspTAPuzCsk97sfPtZWWRLnrvuGiQ8oUy2vYQcl/SRY3zCM1MfqhTSq" +
  "lZntO1IMJGsf694l3nn+LigjgOYAYPAHut7Tk5l8HLpvTx5yyRUJIHeHhbon5uwjddJ0ESX0Rxjm" +
  "2gVet1v/WPYLacR9yTElMCMGCSqGSIb3DQEJFTEWBBRjAJaothGZJ5OwthmRqJ458PPxhzBBMDEw" +
  "DQYJYIZIAWUDBAIBBQAEIJ07VxkUt7WCPiGEk7odfsqE/BxWX/SlPBTBYsiLyEy0BAgJPDno9sQW" +
  "zgICCAA=", "base64");
const CLIENT_PFX_WITH_CA = Buffer.from(
  "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" +
  "COCQ+JDjWYMKAgIIAA==", "base64");

const failure = (e) => e.name + " " + e.code + " " + JSON.stringify(e.message) +
  (e.library !== undefined ? " library=" + e.library + " reason=" + e.reason : "");

// ---- 1. the SecureContext surface.
{
  const sc = tls.createSecureContext({ ca: CA });
  console.log("keys " + JSON.stringify(Object.keys(sc)) + " instanceof " + (sc instanceof tls.SecureContext) +
    " context " + typeof sc.context + " " + sc.context.constructor.name);
  console.log("lengths " + tls.SecureContext.length + " " + tls.createSecureContext.length + " " + tls.createSecureContext.name);
  console.log("new SecureContext " + JSON.stringify(Object.keys(new tls.SecureContext())) + " called " +
    JSON.stringify(Object.keys(tls.SecureContext())));
  for (const [label, options] of [["undefined", undefined], ["null", null], ["{}", {}], ["a string", "x"], ["an array", []],
    ["a number", 5], ["minVersion TLSv9", { minVersion: "TLSv9" }], ["secureProtocol with maxVersion", { secureProtocol: "TLSv1_2_method", maxVersion: "TLSv1.3" }]]) {
    try {
      const made = tls.createSecureContext(options);
      console.log(label + ": " + (made instanceof tls.SecureContext ? "created" : "?"));
    } catch (e) {
      console.log(label + ": " + failure(e));
    }
  }
}

// ---- 2. keys, certificates and bundles, read when the context is built.
const identities = [
  ["plain key", { cert: CERT, key: KEY }],
  ["key only", { key: KEY }],
  ["cert only", { cert: CERT }],
  ["triple-DES key, passphrase", { cert: CERT, key: ENC_TRAD_DES3, passphrase: "hunter2" }],
  ["triple-DES key, wrong passphrase", { cert: CERT, key: ENC_TRAD_DES3, passphrase: "wrong" }],
  ["triple-DES key, no passphrase", { cert: CERT, key: ENC_TRAD_DES3 }],
  ["AES key as {pem, passphrase}", { cert: CLIENT_CERT, key: [{ pem: CLIENT_KEY_AES, passphrase: "hunter2" }] }],
  ["AES key as {pem}, wrong passphrase option", { cert: CLIENT_CERT, key: [{ pem: CLIENT_KEY_AES }], passphrase: "wrong" }],
  ["truncated key", { cert: CERT, key: ENC_TRAD_DES3.slice(0, 180) + "\n-----END EC PRIVATE KEY-----\n", passphrase: "hunter2" }],
  ["garbage key", { cert: CERT, key: "not a key" }],
  ["garbage cert", { cert: "not a certificate", key: KEY }],
  ["key of another certificate", { cert: CERT, key: CLIENT_KEY }],
  ["key not a string", { cert: CERT, key: 5 }],
  ["passphrase not a string", { cert: CERT, key: KEY, passphrase: 5 }],
  ["pfx, passphrase", { pfx: PFX_3DES, passphrase: "hunter2" }],
  ["pfx, wrong passphrase", { pfx: PFX_3DES, passphrase: "wrong" }],
  ["pfx, no passphrase", { pfx: PFX_3DES }],
  ["pfx as [{buf, passphrase}]", { pfx: [{ buf: PFX_3DES, passphrase: "hunter2" }] }],
  ["pfx garbage", { pfx: Buffer.from("not a bundle") }],
  ["ca garbage", { ca: "not a certificate" }],
];
for (const [label, options] of identities) {
  let made;
  try {
    tls.createSecureContext(options);
    made = "created";
  } catch (e) {
    made = failure(e);
  }
  // tls.connect builds the same context, synchronously, before it opens
  // anything (port 1: nothing listens there).
  let connected;
  try {
    const socket = tls.connect({ host: "127.0.0.1", port: 1, ...options });
    socket.on("error", () => {});
    socket.destroy();
    connected = "no throw";
  } catch (e) {
    connected = e.code || e.message;
  }
  console.log(label + ": " + made + " | tls.connect " + connected);
}

// ---- 3. connections, to node servers that ask for a client certificate.
const SERVER = `
import tls from "node:tls";
import https from "node:https";
const CA = \`${CA}\`;
const CERT = \`${CERT}\`;
const KEY = \`${KEY}\`;
const describe = (s) => {
  const peer = s.getPeerCertificate(true);
  const chain = [];
  for (let p = peer, i = 0; p && p.subject && i < 4; p = p.issuerCertificate === p ? null : p.issuerCertificate, i++) chain.push(p.subject.CN);
  return "authorized=" + s.authorized + " error=" + s.authorizationError + " peer=" + JSON.stringify(chain) +
    " protocol=" + s.getProtocol();
};
const options = { key: KEY, cert: CERT, ca: CA, requestCert: true, rejectUnauthorized: false };
const t = tls.createServer(options, (s) => { s.on("error", () => {}); s.end(describe(s)); });
t.on("tlsClientError", () => {});
const h = https.createServer(options, (q, s) => s.end(describe(q.socket)));
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
function connect(label, options) {
  return new Promise((resolve) => {
    let data = "";
    let socket;
    try {
      socket = tls.connect({ host: "127.0.0.1", port: ports.tls, servername: "localhost", ...options });
    } catch (e) {
      console.log(label + ": throws " + (e.code || e.message));
      resolve();
      return;
    }
    socket.setEncoding("utf8");
    socket.on("data", (d) => { data += d; });
    socket.on("error", (e) => { data = data || "error " + e.code; });
    socket.on("close", () => {
      console.log(label + ": " + data);
      resolve();
    });
  });
}
await connect("no certificate", { ca: CA });
await connect("plain key", { ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY });
await connect("AES key, passphrase", { ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY_AES, passphrase: "hunter2" });
await connect("triple-DES key, passphrase", { ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY_DES3, passphrase: "hunter2" });
await connect("key as [{pem, passphrase}]", { ca: CA, cert: CLIENT_CERT, key: [{ pem: CLIENT_KEY_AES, passphrase: "hunter2" }] });
await connect("pfx", { ca: CA, pfx: CLIENT_PFX, passphrase: "hunter2" });
await connect("pfx carrying the CA, no ca option", { pfx: CLIENT_PFX_WITH_CA, passphrase: "hunter2" });
await connect("AES key, wrong passphrase", { ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY_AES, passphrase: "wrong" });
await connect("secureContext {ca}", { secureContext: tls.createSecureContext({ ca: CA }) });
await connect("secureContext {ca}, options ca another", { ca: ROGUE_CERT, secureContext: tls.createSecureContext({ ca: CA }) });
await connect("secureContext {ca: another}, options ca", { ca: CA, secureContext: tls.createSecureContext({ ca: ROGUE_CERT }) });
await connect("secureContext with the certificate", { secureContext: tls.createSecureContext({ ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY_AES, passphrase: "hunter2" }) });
await connect("options certificate, secureContext without", { ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY, secureContext: tls.createSecureContext({ ca: CA }) });
await connect("secureContext maxVersion TLSv1.2, options minVersion TLSv1.3", { minVersion: "TLSv1.3", secureContext: tls.createSecureContext({ ca: CA, maxVersion: "TLSv1.2" }) });
await connect("secureContext reused", { secureContext: tls.createSecureContext({ ca: CA, pfx: CLIENT_PFX, passphrase: "hunter2" }) });
await connect("secureContext not one", { secureContext: { context: {} } });
await connect("secureContext a string", { secureContext: "x" });
function get(label, options) {
  return new Promise((resolve) => {
    https.get({ host: "127.0.0.1", port: ports.https, servername: "localhost", agent: false, path: "/", ...options }, (res) => {
      let body = "";
      res.on("data", (d) => { body += d; });
      res.on("end", () => {
        console.log(label + ": " + res.statusCode + " " + body);
        resolve();
      });
    }).on("error", (e) => {
      console.log(label + ": error " + e.code);
      resolve();
    });
  });
}
await get("https secureContext {ca, certificate}", { secureContext: tls.createSecureContext({ ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY }) });
await get("https secureContext {ca: another}, options ca", { ca: CA, secureContext: tls.createSecureContext({ ca: ROGUE_CERT }) });
await get("https pfx", { ca: CA, pfx: CLIENT_PFX, passphrase: "hunter2" });
await get("https key, passphrase", { ca: CA, cert: CLIENT_CERT, key: CLIENT_KEY_DES3, passphrase: "hunter2" });

server.stdin.end();
