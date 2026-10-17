// A TLS client destroyed in its 'secureConnect' listener never sends its
// last handshake flight. node reports the handshake done from OpenSSL's
// handshake-done callback, before it writes out the client's Finished, so
// destroy() there drops it: over TLS 1.3 the server's handshake never
// completes, and the server sees the connection close mid-handshake
// ('tlsClientError', ECONNRESET "socket hang up") with no
// 'secureConnection' at all. Measured on Node v22.22.2 through a
// record-counting proxy: the client sends its ClientHello, then a FIN.
//
// oam's client used to flush that flight before 'secureConnect', so its
// server peer finished the handshake, handed out a socket and failed the
// first write on the reset -- an unhandled 'error' that killed case 183
// under load. Over TLS 1.2 the client's Finished precedes the server's, so
// the server's handshake is done and 'secureConnection' fires (its socket
// then closes; a write's reset is caught, never printed).
//
// Both transports: a socket tls.connect dials itself, and one over a
// net.Socket ({ socket }). Five connections each, one at a time.
import net from "node:net";
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

const ROUNDS = 5;

async function run(label, maxVersion, wrapped) {
  const seen = [];
  let settle;
  const server = tls.createServer({ cert: CERT, key: KEY, maxVersion }, (s) => {
    s.on("error", () => {});
    s.on("close", () => { seen.push("secureConnection"); settle(); });
    s.end("hi");
  });
  server.on("tlsClientError", (e) => { seen.push("tlsClientError " + e.code + " " + e.message); settle(); });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  for (let i = 0; i < ROUNDS; i++) {
    const serverDone = new Promise((r) => { settle = r; });
    const clientDone = new Promise((resolve) => {
      const options = { host: "127.0.0.1", servername: "localhost", ca: [CA], maxVersion };
      if (wrapped) options.socket = net.connect(port, "127.0.0.1");
      else options.port = port;
      const s = tls.connect(options, () => {
        seen.push("client secureConnect " + s.getProtocol() + " authorized=" + s.authorized);
        s.destroy();
      });
      s.on("error", (e) => seen.push("client error " + e.code));
      s.on("close", resolve);
    });
    await Promise.all([serverDone, clientDone]);
  }
  await new Promise((r) => server.close(r));
  // Each connection's client line, then its server line: the two ends are
  // scheduled independently, so they are printed sorted, not as they came.
  const counts = new Map();
  for (const line of seen) counts.set(line, (counts.get(line) || 0) + 1);
  for (const [line, n] of [...counts].sort()) console.log(label + ": " + n + "x " + line);
}

await run("TLSv1.3 direct", "TLSv1.3", false);
await run("TLSv1.3 { socket }", "TLSv1.3", true);
await run("TLSv1.2 direct", "TLSv1.2", false);
await run("TLSv1.2 { socket }", "TLSv1.2", true);

clearTimeout(watchdog);
process.exit(0);
