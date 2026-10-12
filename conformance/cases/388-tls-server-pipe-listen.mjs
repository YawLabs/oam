// tls.Server over a pipe (#282): a Windows named pipe, a Unix domain socket
// elsewhere. listen(path) binds and names the server by the path (address()
// answers it, while listening and after close()), a plain net client
// arrives as 'connection' with the address fields undefined (a pipe has
// none), a TLS client completes its handshake and echoes over it, and the
// refusal for a NUL in the path is measured, not guessed -- the same lines
// print on Node and oam, and the oracle is that they match.
//
// The TLS client dials with tls.connect({ socket }) over a net.connect
// ({ path }): oam's tls.connect({ path }) is still the recorded client-side
// refusal (docs/node-divergences.md, entry 50) -- this case pins the
// SERVER. The { socket } form runs on both engines.
//
// Each side's events are captured and then printed after BOTH ends have
// reported: the differential compares line order, and the two ends of one
// socket are scheduled independently, so 'connection' (server) vs 'connect'
// (client) and 'secureConnection' (server) vs the secure callback (client)
// flip order between engines and runs (measured: darwin/node put the server
// line first where win/node and win/oam put the client line first -- the
// 2026-10-12 mac release leg died on exactly that). Each pair is awaited on
// both ends and logged in one fixed order, so the printed sequence is this
// case's on every platform.
import tls from "node:tls";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const CERT = `-----BEGIN CERTIFICATE-----
MIIDCTCCAfGgAwIBAgIUJscRiMbEzxV45KtAxD+Lly4dJrQwDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDYxNTEyMzAwN1oXDTI3MDYx
NTEyMzAwN1owFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAoQ5a/fh4J3VW0MPpngEpN+yRUdJtlmY6aBhV/984yEIm
ng9/MGoK0ZRdB8YYGqx4awK1z82ECwtmmdVO/77WA4q6N0CJRzmAF6BN9RgzoyKV
2w1ltowPFyB6SrVqcW1MHqA/9NX/gw/ckvcjcuazYeI857joWulUmR/iWIpSNuBJ
c6odEIkfXG9W6/GyZwlutQXnKaa8eClLqCm+hDnkPBHx+doGWxezFVeOfFAdQM8w
NXT7mj4QN3fiHFDQHI6UkSnVttu7lAAEHY978gjnVyixAPX2dY9mB/Ed4R5eSOpJ
eTR7bXH6+QmUcDJSaDblM5vB3fb3zhitEGLo/APdQQIDAQABo1MwUTAdBgNVHQ4E
FgQUPffw9cdyC1LQ2PLrzN7IZjkpKmMwHwYDVR0jBBgwFoAUPffw9cdyC1LQ2PLr
zN7IZjkpKmMwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAWtdW
V/jSdVB5cN4GOwYXTHhh3dkYDtAPvFPCXbYacelaQe8mlRWv2BBHAhOZdmoJ3ai/
kNRw0D6pKqjcF4p17of9S07ZFCRaQGBAsDEd9jNY156AlEXu4Z8yp/kXE3fvznib
WHrQjdlDcmC2H/Ao+S7f4BkmbvsabyDbUoo+0Drk4MDvqga2azrFDdljqXQxzrEH
/mEwoi9pfukgFnFnhDE+WEqNsZQF9Yxa5QEX6d5tgbOcxS2NpKDug4xSgkpAQ0l6
XKpI59mdGTahOy9zGuNfTqVTHvrFoSXudnNHUjkfHK7Mh/VrNz9ZGpwDt5fGFD4x
E13+0jp6In545LYu+A==
-----END CERTIFICATE-----`;
const KEY = `-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQChDlr9+HgndVbQ
w+meASk37JFR0m2WZjpoGFX/3zjIQiaeD38wagrRlF0HxhgarHhrArXPzYQLC2aZ
1U7/vtYDiro3QIlHOYAXoE31GDOjIpXbDWW2jA8XIHpKtWpxbUweoD/01f+DD9yS
9yNy5rNh4jznuOha6VSZH+JYilI24Elzqh0QiR9cb1br8bJnCW61Becpprx4KUuo
Kb6EOeQ8EfH52gZbF7MVV458UB1AzzA1dPuaPhA3d+IcUNAcjpSRKdW227uUAAQd
j3vyCOdXKLEA9fZ1j2YH8R3hHl5I6kl5NHttcfr5CZRwMlJoNuUzm8Hd9vfOGK0Q
Yuj8A91BAgMBAAECgf9+I0AgqPlx7fSQjN/rX/1oT1+BNc2efXJBFM5GGA3gye50
3K5AvMy8V/aEoCFAwtOM/BJpLgy8mbFByk6U/mGfZIdzvpfFsMMhvetQiiPnIK89
YMDIt+kZs9YTrQIw0+lKEzgECZaUj1exwt2AoC7d+tK4qZlRmm0ngFFGBw9c6g4E
bKpZPPb62HjVAPcPPNJzj0ULTCkFQ7CPhgyz7q6UQUQJ0kM/8DWbnI9qbOWkv0qN
TafdX50piyHstcXGNFelOXmUMw1qQvbPo28qpzkxH7bDU8pShzsnySJL2HL5Wxbr
PbzZ94WOOLXfD3OmT5oW9kHDpH/zUd8pKlbYkNkCgYEA1ygCE4ZSNm0B7U8iwgBK
0Aszxpnf9f4aKKfb9CsmLaf4rxmAAqRaxT4Eki2yebqRX15Ctzlf1ryddKxNAAdh
gCcc+KAdwoJOO3pkwac+r/jsmvWqXHHi/Jn9Bhj886n5NkDGfKiCBL3rUHonwOjN
7y61ExJIy46kOM81Pm88B90CgYEAv6E6owvvAjFs1eoWD5oyUnO1HeAhKvBClDjZ
dcoY965ak5RFM4Da/HcnXAho4+pJY+4O48PIi8nQeZugm8DvpOKfivxYTI3ISDmz
CG0m7N9jJiYOPyt8dpn7Yl7R8OqFvfZAd/KkJ1wBMpsKy1MGU1pdny9mVaAVjxei
fnxNprUCgYEAn2onT6wgUe8mlFwkFrX8uHT0Ydw1EqC5ZRIqaJln6kAghCxSqqJ4
FtjCrkRpjsPrXkwLBpLeLc8GoyHe03ykgz13u8d3BV1i9bLT4KA4VE4NkSsglOpV
EnBOByyQj0GLQuVvq4F3BGhrZ+96cPaNTwC+bWkIwrnnd6gffSkRw4kCgYEAsAEE
mzZdunTs0nii9IeaipJNmnf93rM3Y23nhUEut2ZDOOLowEosV8+UrfnnZNYNvCOt
N1LeAk5FFTx0QjntoVKoWH43F3DtsDCWmDmwk8UFCsfPNAPb2A7LjekrCAxO9E+V
nNWWIbRmQTWXr3G9EJeh/5AIfMKAqqF5lJTUuTUCgYEAtzMfzgUekShhJoGov7uH
MyykhATJv+3ZlR0BCuEjgb7Lu6tu/pbgD1SkhpQ3QbM+XF5DgNJWxQATcgPWP6wy
C7rRXUYQtUTmtwTetACx3EEz7k2ixAxxdDCUPJIxGcVIPVKt6sTovr3yGLMuc4f7
I5PYIZ3kyY8EsQqX4JpTtbY=
-----END PRIVATE KEY-----`;

const P = process.platform === "win32"
  ? String.raw`\\.\pipe\oam-case-388-` + process.pid
  : path.join(os.tmpdir(), "oam-case-388-" + process.pid + ".sock");
const lines = [];
// The path embeds the pid; the log scrubs it (case 375's convention), so the
// printed lines are byte-comparable across runs and engines. Both spellings:
// the raw path (`\\.\pipe\...`, in error fields) and the JSON-escaped one
// (`\\\\.\\pipe\\...`, since address() is logged through JSON.stringify).
const scrub = (s) => {
  if (typeof s !== "string") return s;
  return s.split(JSON.stringify(P)).join('"PIPE"').split(P).join("PIPE");
};
const log = (...parts) => lines.push(scrub(parts.join(" ")));
const watchdog = setTimeout(() => {
  console.log(lines.join("\n"));
  console.log("WATCHDOG");
  process.exit(9);
}, 30000);
const finish = () => {
  clearTimeout(watchdog);
  console.log(lines.join("\n"));
  process.exit(0);
};

// Cross-end slot: a server event fills its slot with the line, a reader takes
// it (or waits for it). Each step then prints every line itself, in one fixed
// order -- arrival order is the scheduler's and is never printed directly.
const slots = {};
const fill = (k, line) => {
  const s = (slots[k] = slots[k] || {});
  s.line = line;
  if (s.resolve) s.resolve(s.line);
};
const take = (k) =>
  new Promise((resolve) => {
    const s = (slots[k] = slots[k] || {});
    if (s.line !== undefined) resolve(s.line);
    else s.resolve = resolve;
  });

// 1. listen(path), and the server's name while it listens.
const server = new tls.Server({ cert: CERT, key: KEY });
// Which connection step is dialing: the two dials are strictly sequential
// (each step awaits the previous one to completion), so one variable is
// enough to route each accepted connection to its step's slot.
let dialStep = 0;
server.on("secureConnection", (s) => {
  fill("secure3", "secureConnection remote=" + JSON.stringify(s.remoteAddress));
  s.on("data", (d) => s.write(d));
});
server.on("connection", (s) => {
  s.on("error", () => {});
  fill("conn" + dialStep, "connection remote=" + JSON.stringify(s.remoteAddress));
});
server.on("tlsClientError", (e) => fill("tcerr", "tlsClientError " + (e.code ?? "none")));
server.on("error", (e) => log("server error " + e.code));
await new Promise((r) => server.listen(P, r));
log("listening address=" + JSON.stringify(server.address()));

// 2. A plain net client: 'connection' fires, the address fields are
//    undefined, and the client goes away before any TLS happens -- which
//    the server reports as a tlsClientError. Every line here is printed by
//    this step AFTER both ends have reported, in the fixed order below:
//    which end reports first is the scheduler's, not node's contract, and
//    it flipped between engines, platforms and runs (measured: darwin/node
//    put the server line first where win/node and win/oam put the client
//    line first 12/12 -- the 2026-10-12 mac release leg died on exactly
//    that). The tlsClientError line comes last: it can only fire after the
//    server accepted the connection, and the step waits for it anyway.
{
  dialStep = 2;
  const dial = new Promise((r) => {
    const s = net.connect({ path: P });
    s.once("connect", () => {
      s.destroy();
      r("plain client connected");
    });
  });
  const [conn, tle] = await Promise.all([take("conn2"), take("tcerr"), dial]);
  log(conn);
  log("plain client connected");
  log(tle);
}

// 3. A TLS client over the pipe socket: the handshake, one echo, the end,
//    and the server's name through both closes (while listening, after the
//    client is gone, after the drain). Same fixed-order rule as step 2: the
//    server's accept ('connection'), the server's handshake event
//    ('secureConnection') and the client's handshake callback all race each
//    other on arrival, so every line is gathered first and printed in one
//    order below. close() and the NUL refusal are then driven from THIS
//    flow, after the batch -- never from an event handler -- so no line's
//    place depends on whether an engine drains its nextTick queue before
//    its promise microtasks.
{
  dialStep = 3;
  const raw = net.connect({ path: P });
  await new Promise((r) => raw.once("connect", r));
  const conn3 = take("conn3");
  const c = tls.connect({ socket: raw, rejectUnauthorized: false }, () => {
    fill("clientSecure3", "client secure protocol=" + c.getProtocol());
    c.write("ping");
  });
  c.on("data", (d) => {
    fill("echo3", "client echo=" + d.toString());
    c.end();
  });
  c.on("error", (e) => log("client error " + e.code));
  c.on("close", () => fill("gone3", "client gone address=" + JSON.stringify(server.address())));
  const [conn, secure, clientSecure, echo, gone] = await Promise.all([
    conn3,
    take("secure3"),
    take("clientSecure3"),
    take("echo3"),
    take("gone3"),
  ]);
  log(conn);
  log(secure);
  log(clientSecure);
  log(echo);
  log(gone);

  await new Promise((r) => server.close(r));
  log("drained address=" + JSON.stringify(server.address()));

  // 4. A NUL in the path: the refusal is ASYNC on both engines (an
  //    'error' event, not a synchronous throw), with the bind-shaped
  //    fields -- EINVAL, syscall listen, port -1. Only the fields that
  //    are node's contract print; the message text is not compared.
  const bad = new tls.Server({ cert: CERT, key: KEY });
  bad.on("error", (e) => fill("nul", "nul " + e.code + " syscall=" + e.syscall + " port=" + e.port));
  bad.listen("a\0b");
  log(await take("nul"));
  finish();
}
