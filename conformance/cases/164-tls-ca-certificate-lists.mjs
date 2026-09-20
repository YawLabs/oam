// tls.rootCertificates and tls.getCACertificates([type]).
//
// rootCertificates is the root store the TLS client trusts when it is given
// no `ca`, as PEM strings (a frozen array, the same one each time, behind a
// getter); getCACertificates('bundled') is that array, 'extra' the
// NODE_EXTRA_CA_CERTS certificates, 'system' the operating system's store,
// and 'default' (the default type) the bundled roots followed by the extra
// ones. The usual way to trust a private CA as well as the public roots is
// `ca: [...tls.rootCertificates, privateCA]`.
//
// The lists' lengths are the runtimes' own (node bundles its release of the
// Mozilla store, oam the one its client trusts), so they are not printed.
//
// Regression guard: oam's rootCertificates was an empty array and
// getCACertificates was missing, so `ca: [...tls.rootCertificates,
// privateCA]` -- which https.request honours -- trusted the private CA
// alone and every public site failed.
//
// Fixtures: the case 141 throwaway P-256 CA and its localhost leaf; the case
// 163 root and intermediate as the extra certificates.
import tls from "node:tls";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { X509Certificate } from "node:crypto";

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
const EXTRA = [`-----BEGIN CERTIFICATE-----
MIIBbzCCARWgAwIBAgIBCzAKBggqhkjOPQQDAjAeMRwwGgYDVQQDDBNvYW0gY2hh
aW4gdGVzdCByb290MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAxMDAwMDAwWjAe
MRwwGgYDVQQDDBNvYW0gY2hhaW4gdGVzdCByb290MFkwEwYHKoZIzj0CAQYIKoZI
zj0DAQcDQgAE0nhjwFbnEztYVr8y1dSDKCEkU4MIkP+piRZ5XMYbmZ9V9h/DSdI6
vXny7ztREvpnZCM8W9UROrkhfqVKwVkD/6NCMEAwDwYDVR0TAQH/BAUwAwEB/zAO
BgNVHQ8BAf8EBAMCAQYwHQYDVR0OBBYEFJdj6PKqiwenVoPtxFDOzSB1OMpkMAoG
CCqGSM49BAMCA0gAMEUCIGeHjvxGlLulZ8zLFPykdn7yAqW9zwejyn+sZpbzzWLF
AiEApPFKNRlypD0jcdsUbWPwir6+R/24IuDgLpGFZJm3GqA=
-----END CERTIFICATE-----
`, `-----BEGIN CERTIFICATE-----
MIIBmTCCAT6gAwIBAgIBDDAKBggqhkjOPQQDAjAeMRwwGgYDVQQDDBNvYW0gY2hh
aW4gdGVzdCByb290MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUwMTAxMDAwMDAwWjAm
MSQwIgYDVQQDDBtvYW0gY2hhaW4gdGVzdCBpbnRlcm1lZGlhdGUwWTATBgcqhkjO
PQIBBggqhkjOPQMBBwNCAAST5X7K5jLLkFSvxbaXgU+S9JLq2CN7/qzWAwpy2NL6
43d3Uvaav8XM8pN4F5+I0qLymVY99CFCjMDX41XKg4lLo2MwYTAPBgNVHRMBAf8E
BTADAQH/MA4GA1UdDwEB/wQEAwIBBjAdBgNVHQ4EFgQUsqhJF2Evx32JhYJDphzU
4gf/0mAwHwYDVR0jBBgwFoAUl2Po8qqLB6dWg+3EUM7NIHU4ymQwCgYIKoZIzj0E
AwIDSQAwRgIhANqCuSTUrs9MkaO50wpawZCDH7JGy1OlM152b1yCEpN8AiEApNE3
9dHUrWXG03i8g7AVEh0Rsh65oCYmUf3PkZjqfnw=
-----END CERTIFICATE-----
`];

// ---- 1. the lists, in this process (no NODE_EXTRA_CA_CERTS).
{
  const d = Object.getOwnPropertyDescriptor(tls, "rootCertificates");
  console.log("rootCertificates " + JSON.stringify({ get: typeof d.get, set: typeof d.set, enumerable: d.enumerable, configurable: d.configurable }));
  console.log("getCACertificates " + typeof tls.getCACertificates + " length " + tls.getCACertificates.length +
    " name " + tls.getCACertificates.name);
  for (const type of ["default", "bundled", "system", "extra"]) {
    const list = tls.getCACertificates(type);
    const pem = list.every((x) => typeof x === "string" && x.startsWith("-----BEGIN CERTIFICATE-----\n") &&
      /\n-----END CERTIFICATE-----\n?$/.test(x));
    // What the operating system's store holds is the host's, not the
    // runtime's: only its shape is printed.
    const contents = type === "system" ? "" : " empty " + (list.length === 0) +
      " newline-terminated " + list.some((x) => x.endsWith("\n"));
    console.log(type + ": array " + Array.isArray(list) + " frozen " + Object.isFrozen(list) +
      " cached " + (list === tls.getCACertificates(type)) + " PEM " + pem + contents);
  }
  console.log("bundled is rootCertificates " + (tls.getCACertificates("bundled") === tls.rootCertificates));
  console.log("no type is default " + (tls.getCACertificates() === tls.getCACertificates("default")));
  console.log("default is bundled " + (JSON.stringify(tls.getCACertificates("default")) === JSON.stringify(tls.rootCertificates)));
  console.log("every root a CA certificate " + tls.rootCertificates.every((p) => new X509Certificate(p).ca));
  for (const bad of ["x", "", "Bundled", 1, null, {}]) {
    try {
      tls.getCACertificates(bad);
      console.log("type " + JSON.stringify(bad) + ": no throw");
    } catch (e) {
      console.log("type " + JSON.stringify(bad) + ": " + e.name + " " + e.code + " " + JSON.stringify(e.message));
    }
  }
  try {
    tls.rootCertificates = [];
    console.log("assigned");
  } catch (e) {
    console.log("assignment: " + e.name);
  }
  try {
    tls.rootCertificates.push(CA);
    console.log("pushed");
  } catch (e) {
    console.log("push: " + e.name);
  }
}

// ---- 2. the public roots and a private CA, as one `ca`.
{
  const server = tls.createServer({ key: KEY, cert: CERT }, (s) => { s.on("error", () => {}); s.end("hello"); });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  const connect = (label, ca) => new Promise((resolve) => {
    const s = tls.connect({ host: "127.0.0.1", port, servername: "localhost", ca }, () => {
      console.log(label + ": authorized " + s.authorized);
      s.destroy();
      resolve();
    });
    s.on("error", (e) => { console.log(label + ": " + e.code); resolve(); });
  });
  await connect("ca [...rootCertificates, CA]", [...tls.rootCertificates, CA]);
  await connect("ca [...getCACertificates(), CA]", [...tls.getCACertificates(), CA]);
  await connect("ca rootCertificates", tls.rootCertificates);
  server.close();
}

// ---- 3. NODE_EXTRA_CA_CERTS, in a child of this runtime.
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-conformance-ca-"));
  const bundle = path.join(dir, "extra.pem");
  fs.writeFileSync(bundle, EXTRA.join(""));
  const script = path.join(dir, "child.mjs");
  fs.writeFileSync(script, `
import tls from "node:tls";
const extra = tls.getCACertificates("extra");
const expected = ${JSON.stringify(EXTRA)};
const def = tls.getCACertificates("default");
console.log("extra " + extra.length + " as written " + extra.every((x, i) => x === expected[i]));
console.log("default is bundled then extra " + (def.length === tls.rootCertificates.length + extra.length) +
  " " + def.slice(-extra.length).every((x, i) => x === extra[i]) +
  " " + tls.rootCertificates.every((x, i) => x === def[i]));
console.log("bundled unchanged " + (tls.getCACertificates("bundled") === tls.rootCertificates));
`);
  const out = await new Promise((resolve) => {
    const child = spawn(process.execPath, [script], {
      env: { ...process.env, NODE_EXTRA_CA_CERTS: bundle },
      stdio: ["ignore", "pipe", "inherit"],
    });
    let text = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (d) => { text += d; });
    child.on("close", () => resolve(text));
  });
  process.stdout.write(out);
  fs.rmSync(dir, { recursive: true, force: true });
}
