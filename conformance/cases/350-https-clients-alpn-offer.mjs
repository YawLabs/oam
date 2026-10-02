// What node's https clients offer by ALPN, against an h2-capable origin
// (http2.createSecureServer with allowHTTP1): fetch() offers `http/1.1`
// alone, as undici's connector does unless a dispatcher sets allowH2, and
// http.request's https.Agent offers no ALPN at all -- so both are served
// over HTTP/1.1, and the server sees `http/1.1` and `false`. A keep-alive
// agent and fetch to the same origin each keep to their own connections.
//
// Regression guard (#176): oam's transport offered `h2, http/1.1` to every
// origin, so fetch and https.request were served over HTTP/2 -- and
// https.request's response still reported httpVersion 1.1.
//
// The server is a separate `node` process (the harness's oracle, on PATH)
// in BOTH runs, so only the clients differ. undici.request and an allowH2
// dispatcher need the undici package, which node does not ship; the e2e
// test `https_clients_offer_what_nodes_offer_by_alpn` covers them.
//
// Fixtures: conformance case 143's throwaway P-256 CA (valid 2025-2125) and
// the localhost leaf it signed.
import https from "node:https";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

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

const here = fileURLToPath(import.meta.url);
const CHILD_TIMEOUT = 30000;

// The clients, in a child of the runtime under test that trusts the CA
// through NODE_EXTRA_CA_CERTS (fetch has no `ca` option of its own, and a
// dispatcher with one would make the connection through tls.connect, not
// through the transport). Each line is what the server saw for one request
// and, for http.request, what the response reported.
if (process.env.OAM_CASE_350 === "clients") {
  const port = process.env.OAM_CASE_350_PORT;
  const url = (path) => "https://localhost:" + port + path;
  const viaFetch = async (label, path, init) => {
    try {
      const res = await fetch(url(path), init);
      console.log(label + ": " + (await res.text()));
    } catch (e) {
      console.log(label + ": " + e.message + " " + (e.cause && e.cause.code));
    }
  };
  const viaGet = (label, path, opts) => new Promise((resolve) => {
    https.get(url(path), opts, (res) => {
      let body = "";
      res.setEncoding("utf8");
      res.on("data", (d) => { body += d; });
      res.on("end", () => {
        console.log(label + ": " + body + " res.httpVersion=" + res.httpVersion);
        resolve();
      });
    }).on("error", (e) => {
      console.log(label + ": " + e.code);
      resolve();
    });
  });
  const keepAlive = new https.Agent({ keepAlive: true });
  // An https.Agent's connection first, then fetch to the same origin, then
  // the agent again: each client keeps to connections that offered what it
  // offers, whichever opened one first.
  await viaGet("https.get keepAlive agent", "/a", { agent: keepAlive });
  await viaFetch("fetch GET", "/b");
  await viaFetch("fetch POST", "/c", { method: "POST", body: "x" });
  await viaGet("https.get keepAlive agent again", "/d", { agent: keepAlive });
  await viaGet("https.get global agent", "/e", {});
  await viaGet("https.get agent:false", "/f", { agent: false });
  keepAlive.destroy();
  process.exit(0);
}

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 50000).unref();

// The server: real Node in both runs, so only the clients differ. It
// answers with the protocol version of the request and the ALPN protocol
// its connection negotiated (false: the client offered none).
const SERVER = `
import http2 from "node:http2";
const [cert, key] = JSON.parse(process.argv[1]);
const server = http2.createSecureServer({ cert, key, allowHTTP1: true }, (req, res) => {
  req.resume();
  req.on("end", () => res.end(req.method + " " + req.url + " httpVersion=" + req.httpVersion +
    " alpn=" + req.socket.alpnProtocol));
});
server.listen(0, "127.0.0.1", () => console.log(server.address().port));
`;

const server = spawn("node", ["--input-type=module", "-e", SERVER, JSON.stringify([CERT, KEY])], {
  stdio: ["ignore", "pipe", "inherit"],
});
const port = await new Promise((resolve) => {
  let out = "";
  server.stdout.setEncoding("utf8");
  server.stdout.on("data", (d) => {
    out += d;
    if (out.includes("\n")) resolve(out.trim());
  });
});

const caFile = path.join(os.tmpdir(), "oam-case-350-ca-" + process.pid + ".pem");
fs.writeFileSync(caFile, CA);
try {
  const out = execFileSync(process.execPath, [here], {
    env: { ...process.env, NODE_EXTRA_CA_CERTS: caFile, OAM_CASE_350: "clients", OAM_CASE_350_PORT: port },
    encoding: "utf8",
    timeout: CHILD_TIMEOUT,
  });
  console.log(out.trim());
} finally {
  fs.unlinkSync(caFile);
  server.kill();
}
process.exit(0);
