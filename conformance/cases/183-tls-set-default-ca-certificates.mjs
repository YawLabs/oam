// tls.setDefaultCACertificates (Node 22.15+) replaces the process default
// trust store (#199). The argument is validated as Node validates it, an
// empty array is accepted and trusts nothing, duplicates are dropped, and a
// connection made after it with no `ca` of its own verifies against the
// override; getCACertificates('default') reads it back.
//
// Counts are printed relative to the input, never the absolute bundled size
// (Node's Mozilla store and oam's webpki-roots hold different numbers --
// docs/node-divergences.md entry 34), and the store is left replaced only at
// the end. Measured on Node v22.22.2.
import tls from "node:tls";

const watchdog = setTimeout(() => { console.log("WATCHDOG"); process.exit(9); }, 20000);

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

const err = (fn) => { try { fn(); return "ok"; } catch (e) { return e.code + " | " + e.message; } };

console.log("types set=" + typeof tls.setDefaultCACertificates + " get=" + typeof tls.getCACertificates);
console.log("string:   " + err(() => tls.setDefaultCACertificates("pem")));
console.log("number:   " + err(() => tls.setDefaultCACertificates([123])));
console.log("null:     " + err(() => tls.setDefaultCACertificates(null)));
console.log("nocert:   " + err(() => tls.setDefaultCACertificates(["not a cert"])));

const bundledBefore = tls.getCACertificates("bundled").length;

console.log("empty:    " + err(() => tls.setDefaultCACertificates([])));
console.log("afterEmpty default=" + tls.getCACertificates("default").length);

console.log("setCA:    " + err(() => tls.setDefaultCACertificates([CA])));
const d = tls.getCACertificates("default");
console.log("afterCA default=" + d.length + " roundTrips=" + (d.length === 1 && d[0].replace(/\r/g, "") === CA) +
  " bundledStable=" + (tls.getCACertificates("bundled").length === bundledBefore));

console.log("dupes:    " + err(() => tls.setDefaultCACertificates([CA, CA])));
console.log("afterDupes default=" + tls.getCACertificates("default").length);

console.log("buffer:   " + err(() => tls.setDefaultCACertificates([Buffer.from(CA, "utf8")])));
console.log("afterBuffer default=" + tls.getCACertificates("default").length);

// A connection with no `ca` of its own verifies against the override.
tls.setDefaultCACertificates([CA]);
const server = tls.createServer({ cert: CERT, key: KEY }, (s) => s.end("hi"));
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
await new Promise((resolve) => {
  const s = tls.connect({ port, host: "127.0.0.1", servername: "localhost" }, () => {
    console.log("live authorized=" + s.authorized);
    s.destroy();
  });
  s.on("error", (e) => console.log("live error " + e.code));
  s.on("close", resolve);
});
await new Promise((r) => server.close(r));

clearTimeout(watchdog);
process.exit(0);
