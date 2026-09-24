// tls.connect against a server whose certificate is trusted but issued to a
// different name is refused with ERR_TLS_CERT_ALTNAME_INVALID, and Node hands
// the peer certificate to the error as `err.cert` -- the same object
// getPeerCertificate(true) returns: the leaf's subject, issuer, altnames,
// key, validity, fingerprints, and its `issuerCertificate` linked from the
// store (#198). A chain-build failure (no `ca`) refuses with
// UNABLE_TO_VERIFY_LEAF_SIGNATURE and leaves `err.cert` absent (the property is
// never set; `err.cert === undefined`, own-keys `[code]`), as Node does.
//
// Printed: the error's own keys (order matters), and every stable field of
// err.cert -- never the fingerprints' or raw bytes' values (they are the
// fixture's, not a divergence) but that they are present and strings.
// Measured on node v22.22.2.
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

const server = tls.createServer({ cert: CERT, key: KEY }, (s) => s.end("hi"));
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

// A cert field's presence and type, so the fixture's own bytes (fingerprints,
// raw, pubkey, modulus) never enter the output while their presence does.
const shown = (c) => {
  if (!c || typeof c !== "object") return c;
  const strOf = (k) => (typeof c[k] === "string" ? "str" : c[k] === undefined ? "absent" : typeof c[k]);
  return {
    subject: c.subject, issuer: c.issuer, subjectaltname: c.subjectaltname, ca: c.ca,
    bits: c.bits, exponent: c.exponent, valid_from: typeof c.valid_from, valid_to: typeof c.valid_to,
    fingerprint: strOf("fingerprint"), fingerprint256: strOf("fingerprint256"), fingerprint512: strOf("fingerprint512"),
    modulus: strOf("modulus"), pubkey: (c.pubkey && c.pubkey.constructor && c.pubkey.constructor.name) || "absent",
    ext_key_usage: c.ext_key_usage, serialNumber: strOf("serialNumber"), raw: (c.raw && c.raw.constructor && c.raw.constructor.name) || "absent",
    issuerCN: (c.issuerCertificate && c.issuerCertificate.subject || {}).CN,
    issuerSelf: c.issuerCertificate === c.issuerCertificate?.issuerCertificate,
  };
};

function attempt(label, opts) {
  return new Promise((resolve) => {
    const s = tls.connect({ host: "127.0.0.1", port, ...opts }, () => { s.destroy(); resolve(); });
    s.on("error", (e) => {
      console.log(label + " code=" + e.code + " keys=" + JSON.stringify(Object.keys(e)));
      console.log(label + " cert=" + JSON.stringify(shown(e.cert)));
    });
    s.on("close", resolve);
  });
}

await attempt("altname", { servername: "wrong.example", ca: CA });
await attempt("untrusted", { servername: "localhost" });

await new Promise((r) => server.close(r));
clearTimeout(watchdog);
process.exit(0);
