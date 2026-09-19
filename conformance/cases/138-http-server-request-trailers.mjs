// Trailer fields of a chunked request body, on http and https servers:
// req.trailers / req.rawTrailers hold them once the body has ended (empty
// before, and for a body without a trailer section), repeated fields
// combined as node combines headers; and a trailer section carrying
// Content-Length or Transfer-Encoding -- framing fields, which a front end
// may act on -- is answered 400 and the connection closed, as node's parser
// refuses it, so nothing after it is read as a request.
//
// oam dropped the trailers (req.trailers was undefined), read framing
// fields in them as ordinary ones, and went on to serve the next request on
// the connection.
//
// Names in rawTrailers are printed lowercased: oam's parser lowercases them
// (divergence 40).
import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";

const RSA_CERT = `-----BEGIN CERTIFICATE-----
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
const RSA_KEY = `-----BEGIN PRIVATE KEY-----
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

const TRAILERS = {
  "no trailer section": "",
  "two fields": "X-T: 1\r\nX-U: 2\r\n",
  "a repeated field": "X-T: 1\r\nx-t: 2\r\n",
  "set-cookie twice": "Set-Cookie: a\r\nSet-Cookie: b\r\n",
  "cookie twice": "Cookie: a=1\r\nCookie: b=2\r\n",
  "a single-value field twice": "ETag: one\r\nETag: two\r\n",
  "an empty value": "X-E:\r\n",
  "whitespace around a value": "X-O:   v  \r\n",
  "Host, Trailer and Connection": "Host: y\r\nTrailer: x\r\nConnection: keep-alive\r\n",
  "Content-Length": "Content-Length: 50\r\n",
  "content-length 0": "content-length: 0\r\n",
  "Transfer-Encoding": "Transfer-Encoding: chunked\r\n",
  "Transfer-Encoding identity": "X-A: 1\r\nTransfer-Encoding: identity\r\n",
  "a space in a name": "X T: 1\r\n",
  "obs-fold": "X-F: a\r\n b\r\n",
};

const events = [];
const handler = (req, res) => {
  const before = JSON.stringify([req.trailers, req.rawTrailers]);
  req.resume();
  req.on("end", () => {
    events.push(
      `${req.url} end, before ${before}, trailers ${JSON.stringify(req.trailers)}, raw ${JSON.stringify(
        req.rawTrailers.map((v, i) => (i % 2 === 0 ? v.toLowerCase() : v)),
      )}`,
    );
    res.end("ok");
  });
  req.on("error", (e) => events.push(`${req.url} error ${e.code}`));
};

function send(port, bytes, secure) {
  return new Promise((resolve) => {
    const socket = secure
      ? tls.connect({ port, host: "127.0.0.1", rejectUnauthorized: false })
      : net.connect(port, "127.0.0.1");
    let raw = "";
    let settled = false;
    const done = (closed) => {
      if (settled) return;
      settled = true;
      clearTimeout(backstop);
      socket.destroy();
      const statuses = [...raw.matchAll(/HTTP\/1\.1 \d{3}[^\r\n]*/g)].map((m) => m[0]);
      resolve(`${statuses.join(" + ") || "(nothing)"}${closed ? ", closed" : ""}`);
    };
    const backstop = setTimeout(() => done(false), 800);
    socket.on("data", (d) => (raw += d.toString("latin1")));
    socket.on("close", () => done(true));
    socket.on("error", () => {});
    socket.once(secure ? "secureConnect" : "connect", () => socket.write(bytes));
  });
}

for (const secure of [false, true]) {
  const server = secure
    ? https.createServer({ cert: RSA_CERT, key: RSA_KEY }, handler)
    : http.createServer(handler);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port;
  for (const [name, trailers] of Object.entries(TRAILERS)) {
    events.length = 0;
    const request =
      "POST /t HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n" +
      trailers +
      "\r\nGET /next HTTP/1.1\r\nHost: x\r\n\r\n";
    const status = await send(port, request, secure);
    await new Promise((r) => setTimeout(r, 30));
    // (An https handler is not shown a body refused as malformed: oam's
    // https server reads a body before the handler runs -- divergence 40.)
    const shown = secure && status.includes(" 400 ") ? "" : ` | ${events.join(" ; ")}`;
    console.log(`${secure ? "https" : "http"}, ${name} | ${status}${shown}`);
  }
  // A body that is not chunked has no trailers.
  events.length = 0;
  await send(port, "POST /cl HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc", secure);
  await new Promise((r) => setTimeout(r, 30));
  console.log(`${secure ? "https" : "http"}, Content-Length body | ${events.join(" ; ")}`);
  server.close();
}
