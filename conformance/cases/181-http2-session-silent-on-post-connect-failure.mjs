// An http2.connect session that fails AFTER it has emitted 'connect' is
// destroyed silently: the failure is carried by its streams, not re-surfaced
// on the session, so no 'error' (which, unhandled, would crash the process)
// and no 'close' fire on the session (#197). A failure BEFORE 'connect' -- a
// refused connection, a reset or an EOF during the handshake -- still reaches
// the session as 'error' then 'close', the only place a caller sees it.
//
// The mTLS case: a TLS 1.3 server that requires a client certificate sends a
// fatal `certificate_required` alert after the handshake to a client that
// sent none. Node's session emits nothing; the pending stream carries
// `ERR_SSL_TLSV13_ALERT_CERTIFICATE_REQUIRED` (that code is #196). Up to
// 0.16.4 oam's session emitted `'error'` and `'close'` here, so a program
// that handled the request's error -- as it would on Node, having no reason
// to listen on the session -- was taken down by the session's instead.
//
// Each scenario is run with a session 'error' listener and without one; a
// process-level `uncaughtException` guard records an unhandled session error
// as `UNCAUGHT` rather than crashing, so both runtimes reach exit 0. Measured
// on node v22.22.2.
import http2 from "node:http2";
import net from "node:net";

const watchdog = setTimeout(() => { console.log("WATCHDOG"); process.exit(9); }, 30000);

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

// Run one client attempt against `authority`, with or without a session
// 'error' listener; record the session's, the stream's and any uncaught
// events in order, then settle.
function attempt(authority, options, installErr, waitMs) {
  return new Promise((resolve) => {
    const ev = [];
    const onUncaught = (e) => ev.push("uncaught:" + (e.code || e.message));
    process.once("uncaughtException", onUncaught);
    const session = http2.connect(authority, options);
    session.on("connect", () => ev.push("s.connect"));
    if (installErr) session.on("error", (e) => ev.push("s.error:" + (e.code || e.message)));
    session.on("close", () => ev.push("s.close"));
    let stream;
    try {
      stream = session.request({ ":path": "/" });
      stream.on("error", (e) => ev.push("st.error:" + (e.code || e.message)));
      stream.on("close", () => ev.push("st.close"));
      stream.on("response", () => ev.push("st.response"));
    } catch (e) {
      ev.push("req-throw:" + (e.code || e.message));
    }
    setTimeout(() => {
      process.removeListener("uncaughtException", onUncaught);
      try { session.destroy(); } catch {}
      resolve(ev.join(" | "));
    }, waitMs);
  });
}

const listen = (server) => new Promise((r) => server.listen(0, "127.0.0.1", () => r(server.address().port)));

// ---- mTLS: the server sends certificate_required after the handshake.
{
  const server = http2.createSecureServer({ cert: CERT, key: KEY, ca: [CA], requestCert: true, rejectUnauthorized: true }, () => {});
  server.on("session", () => {});
  const port = await listen(server);
  for (const errL of [false, true]) {
    const line = await attempt(`https://localhost:${port}`, { ca: CA }, errL, 1200);
    console.log("mTLS " + (errL ? "with-error-listener" : "no-error-listener") + ": " + line);
  }
  await new Promise((r) => server.close(r));
}

// ---- before connect: nothing is listening, the connection is refused.
{
  const probe = net.createServer();
  const port = await listen(probe);
  await new Promise((r) => probe.close(r));
  for (const errL of [false, true]) {
    const line = await attempt(`https://127.0.0.1:${port}`, {}, errL, 900);
    console.log("refused " + (errL ? "with-error-listener" : "no-error-listener") + ": " + line);
  }
}

clearTimeout(watchdog);
process.exit(0);
