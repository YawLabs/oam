// An http / https server reports each request's connection as node does:
// req.socket carries the TCP peer's remoteAddress / remotePort /
// remoteFamily and the accepted socket's localAddress / localPort /
// localFamily, address() is the local end, and req.connection is the same
// object. oam used to report every client as remoteAddress '127.0.0.1' with
// no port or family, whatever address it connected from (an IPv6 client
// included), which is what loopback-only routes, `trust proxy` settings and
// per-IP limits read.
//
// Loopback servers only, so the output is the same on every host: IPv4 on
// 127.0.0.1 over http and https, IPv6 on ::1 over http. Each connection
// sends two keep-alive requests from a raw client whose own local port is
// known, so the reported ports are compared rather than printed.
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

function describe(label, req, server, clientPort) {
  const s = req.socket;
  const a = typeof s.address === "function" ? s.address() : "no address()";
  console.log(
    label,
    JSON.stringify({
      remoteAddress: s.remoteAddress,
      remoteFamily: s.remoteFamily,
      remotePortIsClientPort: s.remotePort === clientPort,
      localAddress: s.localAddress,
      localFamily: s.localFamily,
      localPortIsServerPort: s.localPort === server.address().port,
      address: a && { address: a.address, family: a.family, portIsServerPort: a.port === server.address().port },
      connectionIsSocket: req.connection === s,
      encrypted: s.encrypted,
    }),
  );
}

// Two requests on one connection; resolves once both responses are in.
function exchange(proto, host, server, label) {
  return new Promise((resolve, reject) => {
    const port = server.address().port;
    const opts = { host, port };
    const sock =
      proto === "https"
        ? tls.connect({ ...opts, rejectUnauthorized: false, servername: "localhost" })
        : net.connect(opts);
    const ready = proto === "https" ? "secureConnect" : "connect";
    let seen = 0;
    let buf = "";
    server.on("request", (req, res) => {
      seen++;
      describe(`${label} #${seen}`, req, server, sock.localPort);
      res.setHeader("content-length", "2");
      res.end("ok");
    });
    sock.on(ready, () => {
      sock.write("GET /1 HTTP/1.1\r\nHost: x\r\n\r\n");
    });
    sock.on("data", (d) => {
      buf += d;
      const done = (buf.match(/HTTP\/1\.1 200/g) || []).length;
      if (done === 1 && !sock.secondSent) {
        sock.secondSent = true;
        sock.write("GET /2 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
      } else if (done === 2) {
        sock.destroy();
        server.close();
        resolve();
      }
    });
    sock.on("error", reject);
  });
}

function listen(server, host) {
  return new Promise((resolve) => server.listen(0, host, () => resolve(server)));
}

await exchange("http", "127.0.0.1", await listen(http.createServer(), "127.0.0.1"), "http 127.0.0.1");
await exchange("http", "::1", await listen(http.createServer(), "::1"), "http ::1");
await exchange(
  "https",
  "127.0.0.1",
  await listen(https.createServer({ cert: RSA_CERT, key: RSA_KEY }), "127.0.0.1"),
  "https 127.0.0.1",
);
await exchange(
  "https",
  "::1",
  await listen(https.createServer({ cert: RSA_CERT, key: RSA_KEY }), "::1"),
  "https ::1",
);

// An upgrade hands the listener the connection's own socket, carrying the
// same addresses (oam's used to say family 'IPv4' for an IPv6 client).
await new Promise((resolve, reject) => {
  const server = http.createServer();
  server.on("upgrade", (req, socket) => {
    console.log(
      "upgrade ::1",
      JSON.stringify({
        remoteAddress: socket.remoteAddress,
        remoteFamily: socket.remoteFamily,
        remotePortIsClientPort: socket.remotePort === client.localPort,
        localAddress: socket.localAddress,
        localFamily: socket.localFamily,
        localPortIsServerPort: socket.localPort === server.address().port,
        reqSocketIsSocket: req.socket === socket,
      }),
    );
    socket.end("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n");
  });
  let client;
  server.listen(0, "::1", () => {
    client = net.connect({ host: "::1", port: server.address().port }, () => {
      client.write("GET / HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n");
    });
    client.on("data", () => {});
    client.on("end", () => {
      client.destroy();
      server.close();
      resolve();
    });
    client.on("error", reject);
  });
});
