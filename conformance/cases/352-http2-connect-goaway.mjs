// http2.connect when the server sends GOAWAY after answering one of three
// concurrent streams (#185): the session's 'goaway' reports the frame
// (code, last stream id, debug data) after the answered stream's response
// and before anything else; the two streams above the last stream id close
// with an ERR_HTTP2_STREAM_ERROR naming NGHTTP2_REFUSED_STREAM (rstCode 7),
// so an application knows the server never processed them and can send
// them again; the answered stream reads to its end; and the session closes
// (a NO_ERROR GOAWAY) or is destroyed with ERR_HTTP2_SESSION_ERROR (any
// other code).
//
// Regression guard: oam's session never heard of the GOAWAY. hyper reported
// the refused streams as the connection failing and the graceful end as an
// EOF, so every stream -- the answered one, its response unread, included --
// closed silently with NGHTTP2_CANCEL, and no 'goaway' came.
//
// The server is a separate `node` process (the harness's oracle, on PATH)
// in BOTH runs, so only the client differs.
//
// Fixtures: conformance case 143's throwaway P-256 CA (valid 2025-2125) and
// the localhost leaf it signed.
import http2 from "node:http2";
import { spawn } from "node:child_process";

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

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 50000).unref();

// The server: real Node in both runs. It answers the first stream it
// handles on a session, then sends a GOAWAY with argv's code -- through
// session.close() for NO_ERROR (a graceful close: one GOAWAY whose last
// stream id is the answered stream), through session.goaway(code, 1, data)
// otherwise -- and leaves every later stream unanswered.
const SERVER = `
import http2 from "node:http2";
const [cert, key, mode] = JSON.parse(process.argv[1]);
const server = http2.createSecureServer({ cert, key });
server.on("stream", (stream) => {
  stream.on("error", () => {});
  const session = stream.session;
  if (session.answered) return;
  session.answered = true;
  stream.respond({ ":status": 200 });
  stream.end("ok");
  if (mode === "close") session.close();
  else session.goaway(11, 1, Buffer.from("calm"));
});
server.on("session", (session) => session.on("error", () => {}));
server.listen(0, "127.0.0.1", () => console.log(server.address().port));
`;

function startServer(mode) {
  const server = spawn("node", ["--input-type=module", "-e", SERVER, JSON.stringify([CERT, KEY, mode])], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  return new Promise((resolve) => {
    let out = "";
    server.stdout.setEncoding("utf8");
    server.stdout.on("data", (d) => {
      out += d;
      if (out.includes("\n")) resolve({ server, port: out.trim() });
    });
  });
}

// Three requests at once on one session. Which of them the server answers
// first is the order they reach it, so the lines name no stream: what
// matters is what each kind of stream sees, and in what order.
async function run(mode) {
  const { server, port } = await startServer(mode);
  const lines = [];
  const session = http2.connect("https://localhost:" + port, { ca: CA });
  session.on("error", (e) => lines.push("session error " + e.code + " " + e.message));
  session.on("goaway", (code, last, data) => lines.push(
    "session goaway code=" + code + " lastStreamID=" + last +
    " opaqueData=" + (data === undefined ? "undefined" : JSON.stringify(data.toString())) +
    " closed=" + session.closed + " destroyed=" + session.destroyed,
  ));
  session.on("close", () => lines.push("session close"));
  const one = () => new Promise((resolve) => {
    const req = session.request({ ":path": "/" });
    req.on("response", (h) => lines.push("stream response " + h[":status"]));
    req.on("error", (e) => lines.push("stream error " + e.code + " " + e.message));
    req.on("data", (d) => lines.push("stream data " + d));
    req.on("end", () => lines.push("stream end"));
    req.on("close", () => {
      lines.push("stream close rstCode=" + req.rstCode);
      resolve();
    });
    req.end();
  });
  await Promise.all([one(), one(), one()]);
  await new Promise((r) => setTimeout(r, 100));
  try {
    session.request({ ":path": "/" });
    lines.push("request after: created");
  } catch (e) {
    lines.push("request after: " + e.code);
  }
  session.destroy();
  server.kill();
  console.log(mode === "close" ? "session.close():" : "session.goaway(11, 1, 'calm'):");
  for (const line of lines) console.log("  " + line);
}

await run("close");
await run("goaway");
process.exit(0);
