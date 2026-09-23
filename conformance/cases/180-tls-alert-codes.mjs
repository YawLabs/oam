// Every fatal alert a server can answer the ClientHello with, sent by a raw
// TCP server as one alert record (level 2, description N), and the error
// each client reports for it (#196):
//
// - tls.connect with nothing queued reports node's name for the alert --
//   `ERR_SSL_` and OpenSSL's reason, uppercased: `SSL/TLS` for the SSL3-era
//   alerts (OpenSSL 3's spelling), `TLSV1` / `TLSV13` for the rest, five of
//   them without the word ALERT -- with `library` and `reason` ahead of
//   `code`.
// - tls.connect with a write queued behind the handshake reports the failed
//   write, `write EPROTO` (errno, code, syscall), on the write's callback and
//   then on the socket; https.request and the shared transport's https.get,
//   whose request head is such a write, report the same EPROTO.
// - A description node cannot name (255), and `close_notify` sent as fatal,
//   are, on a socket with nothing queued, the disconnect: ECONNRESET,
//   "Client network socket disconnected before secure TLS connection was
//   established", carrying the options dialled with (code, path, host, port,
//   localAddress; an agent dials with `path: null`) -- and EPROTO once a
//   write was queued.
// - A warning-level alert is ignored, and the close behind it is the
//   disconnect, as a bare EOF is, with or without a write queued. The queued
//   write's callback then gets `write ECANCELED` (errno, code, syscall),
//   after the socket's error; a second queued write gets the socket's error.
// - http2.connect's session fails with the alert's own error (its preface
//   is written only once the handshake is done), and the disconnect for an
//   EOF.
// - The other ends of a handshake with a write queued behind it: a version
//   range with nothing to offer fails the write with a detail-less `write
//   EPROTO` before the socket's ERR_SSL_NO_PROTOCOLS_AVAILABLE; a certificate
//   the verifier refuses fails it with `write EBADF` after the socket's
//   UNABLE_TO_VERIFY_LEAF_SIGNATURE. And `authorizationError` stays null
//   after an alert.
//
// Up to 0.16.4 oam reported `EIO` for every alert but `protocol_version`, and
// ERR_SOCKET_CLOSED_BEFORE_CONNECTION on a queued write whatever ended the
// handshake. Every value measured on node v22.22.2 (OpenSSL 3.5.5). Never
// printed: errno's value (platform-specific) and the messages (OpenSSL's
// diagnostics carry its build path; docs/node-divergences.md entry 34). Not
// covered: a server answering with something that is not TLS (node
// ERR_SSL_PACKET_LENGTH_TOO_LONG, oam EIO; entry 34), and a fatal alert
// sent AFTER the handshake over the shared transport (hyper collapses it to
// a connection-closed, so oam reports ECONNRESET where node names the alert;
// entry 34). `fetch` is not one of the four clients here: it keeps the
// alert's code as its `cause` rather than the failed write, which conformance
// case 178 (#144) pins.
//
// Fixtures: the case 141 throwaway localhost leaf and its key (its CA is not
// given to the client, which is the point).
import http2 from "node:http2";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";

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

let section = "start";
const watchdog = setTimeout(() => {
  console.log("WATCHDOG " + section);
  process.exit(9);
}, 60000);

// RFC 8446 s6 and RFC 5246 s7.2, every description OpenSSL names.
const ALERTS = [
  [10, "unexpected_message"], [20, "bad_record_mac"], [21, "decryption_failed"],
  [22, "record_overflow"], [30, "decompression_failure"], [40, "handshake_failure"],
  [41, "no_certificate"], [42, "bad_certificate"], [43, "unsupported_certificate"],
  [44, "certificate_revoked"], [45, "certificate_expired"], [46, "certificate_unknown"],
  [47, "illegal_parameter"], [48, "unknown_ca"], [49, "access_denied"], [50, "decode_error"],
  [51, "decrypt_error"], [60, "export_restriction"], [70, "protocol_version"],
  [71, "insufficient_security"], [80, "internal_error"], [86, "inappropriate_fallback"],
  [90, "user_canceled"], [100, "no_renegotiation"], [109, "missing_extension"],
  [110, "unsupported_extension"], [111, "certificate_unobtainable"], [112, "unrecognized_name"],
  [113, "bad_certificate_status_response"], [114, "bad_certificate_hash_value"],
  [115, "unknown_psk_identity"], [116, "certificate_required"], [120, "no_application_protocol"],
];
const alertRecord = (level, description) => Buffer.from([0x15, 0x03, 0x03, 0x00, 0x02, level, description]);

// A raw server: on the ClientHello, `answer` the connection and close it.
function rawServer(answer) {
  const server = net.createServer((c) => {
    c.on("error", () => {});
    c.once("data", () => answer(c));
  });
  return new Promise((r) => server.listen(0, "127.0.0.1", () => r(server)));
}

let port = 0;
// The error as it matters: code, syscall, its own keys, whether errno is a
// negative number -- and, for the disconnect, the options it carries (the
// port as its type: http2.connect's session carries the URL's string).
function shape(e) {
  let s = e.code + (e.syscall ? ":" + e.syscall : "") + " " + Object.keys(e).join(",");
  if ("errno" in e) s += (typeof e.errno === "number" && e.errno < 0 ? " errno<0" : " errno=" + e.errno);
  if (e.code === "ECONNRESET") {
    s += " path=" + JSON.stringify(e.path) + " host=" + JSON.stringify(e.host) +
      " port=" + (String(e.port) === String(port) ? typeof e.port + " PORT" : JSON.stringify(e.port)) +
      " localAddress=" + JSON.stringify(e.localAddress);
  }
  return s;
}
function tlsClient(write, opts) {
  return new Promise((resolve) => {
    const events = [];
    const s = tls.connect({ host: "127.0.0.1", port, rejectUnauthorized: false, ...opts }, () => events.push("secureConnect"));
    if (write) {
      s.write("x", (e) => events.push("cb " + (e ? shape(e) : "ok")));
      s.write("y", (e) => events.push("cb2 " + (e ? e.code : "ok")));
    }
    s.on("error", (e) => events.push("error " + shape(e) + " authorizationError=" + JSON.stringify(s.authorizationError)));
    s.on("close", () => resolve(events.join(" | ")));
  });
}
function request() {
  return new Promise((resolve) => {
    const req = https.request({ host: "127.0.0.1", port, path: "/", rejectUnauthorized: false, method: "POST" });
    req.on("error", (e) => resolve(shape(e)));
    req.end();
  });
}
function shared() {
  return new Promise((resolve) => {
    // No TLS option of its own: the shared transport.
    const req = https.get("https://127.0.0.1:" + port + "/");
    req.on("error", (e) => resolve(shape(e)));
  });
}
function h2() {
  return new Promise((resolve) => {
    const events = [];
    const session = http2.connect("https://127.0.0.1:" + port, { rejectUnauthorized: false });
    session.on("connect", () => events.push("connect"));
    session.on("error", (e) => events.push("error " + shape(e)));
    session.on("close", () => resolve(events.join(" ")));
  });
}

async function scenario(label, answer, clients) {
  section = label;
  const server = await rawServer(answer);
  port = server.address().port;
  const out = [];
  for (const [name, run] of clients) out.push(name + "=" + (await run()));
  await new Promise((r) => server.close(r));
  console.log(label + ": " + out.join(" | "));
}

const every = [["tls", () => tlsClient(false)], ["tlsWrite", () => tlsClient(true)], ["request", request], ["shared", shared]];
const withH2 = [...every, ["h2", h2]];

for (const [description, name] of ALERTS) {
  await scenario("fatal " + description + " " + name, (c) => c.end(alertRecord(2, description)),
    description === 40 || description === 70 || description === 116 ? withH2 : every);
}
await scenario("fatal 255 (no name)", (c) => c.end(alertRecord(2, 255)), withH2);
await scenario("fatal 0 close_notify", (c) => c.end(alertRecord(2, 0)), every);
await scenario("warning 40 then close", (c) => c.end(alertRecord(1, 40)), withH2);
await scenario("eof", (c) => c.end(), withH2);
await scenario("no protocols", (c) => c.end(), [["tlsWrite", () => tlsClient(true, { minVersion: "TLSv1.3", maxVersion: "TLSv1.2" })]]);

// A certificate the verifier refuses, with a write queued behind the
// handshake: a real TLS server whose CA the client was not given.
{
  section = "untrusted";
  const server = tls.createServer({ cert: CERT, key: KEY }, (c) => c.on("error", () => {}));
  server.on("tlsClientError", () => {});
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  port = server.address().port;
  console.log("untrusted: tlsWrite=" + (await tlsClient(true, { rejectUnauthorized: true })));
  await new Promise((r) => server.close(r));
}

clearTimeout(watchdog);
process.exit(0);
