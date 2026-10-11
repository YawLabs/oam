// https.Server over a pipe (#282): a Windows named pipe, a Unix domain
// socket elsewhere. listen(path) binds and names the server by the path, a
// TLS client completes its handshake and a POST with a body is served over
// it, close() drains, and a NUL in the path is refused asynchronously
// (EINVAL, syscall listen, port -1) -- the same lines print on Node and
// oam, and the oracle is that they match.
//
// The client dials with tls.connect({ socket }) over a net.connect
// ({ path }) and hands the secure socket to the request through
// createConnection: oam's client-side tls.connect({ path }) -- which
// https.get({ socketPath }) rides -- is still the recorded refusal
// (docs/node-divergences.md, entry 50). This case pins the SERVER; the {
// socket } form runs on both engines.
import https from "node:https";
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
  ? String.raw`\\.\pipe\oam-case-390-` + process.pid
  : path.join(os.tmpdir(), "oam-case-390-" + process.pid + ".sock");
const lines = [];
// The path embeds the pid; the log scrubs it (case 375's convention), in
// both its raw and JSON-escaped spellings.
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

const server = new https.Server({ cert: CERT, key: KEY }, (req, res) => {
  log("req " + req.method + " " + req.url);
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    res.writeHead(200, { "content-type": "text/plain" });
    res.end(Buffer.concat(chunks).toString());
  });
});
server.on("tlsClientError", (e) => log("tlsClientError " + (e.code ?? "none")));
server.on("error", (e) => log("server error " + e.code));
await new Promise((r) => server.listen(P, r));
log("listening address=" + JSON.stringify(server.address()));

// A POST with a body, over the pipe, encrypted.
{
  const out = await new Promise((resolve) => {
    const raw = net.connect({ path: P });
    raw.on("error", () => resolve("error ECONNREFUSED"));
    raw.on("connect", () => {
      const secure = tls.connect({ socket: raw, rejectUnauthorized: false });
      secure.on("error", (e) => resolve("error " + e.code));
      const req = https.request(
        { createConnection: () => secure, path: "/post", method: "POST" },
        (res) => {
          let body = "";
          res.on("data", (c) => (body += c));
          res.on("end", () => resolve(res.statusCode + " " + body));
        },
      );
      req.on("error", (e) => resolve("error " + e.code));
      req.end("posted securely");
    });
  });
  log("post " + out);
}

// close() drains; the server keeps its name after.
await new Promise((r) => server.close(r));
log("drained address=" + JSON.stringify(server.address()));

// A NUL in the path: the refusal is ASYNC on both engines (an 'error'
// event, not a synchronous throw), with the bind-shaped fields. Only the
// fields that are node's contract print; the message text does not.
{
  const bad = new https.Server({ cert: CERT, key: KEY });
  bad.on("error", (e) => {
    log("nul " + e.code + " syscall=" + e.syscall + " port=" + e.port);
    finish();
  });
  bad.listen("a\0b");
}
