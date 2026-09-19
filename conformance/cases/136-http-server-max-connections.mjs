// net.Server's maxConnections on http and https servers, and its 'drop'
// event. Unset (node's default) there is no limit at all: the server takes
// connections for as long as the OS gives them. Set, a connection that
// arrives while `connections >= maxConnections` is closed at once without a
// byte, and the server emits 'drop' with a null-prototype record of its two
// ends. A value set after listen() holds from the next connection; one that
// is not a number compares as NaN and refuses nothing.
//
// oam's servers had no maxConnections and a fixed limit of 256 connections
// instead, past which every new connection was dropped.
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

const log = (...parts) => console.log(parts.join(" | "));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Connect, send `bytes`, and report the first status line (if any) and
// whether the server closed the connection within `hold` ms.
function exchange(port, bytes, { hold = 1500, secure = false } = {}) {
  return new Promise((resolve) => {
    const socket = secure
      ? tls.connect({ port, host: "127.0.0.1", rejectUnauthorized: false })
      : net.connect(port, "127.0.0.1");
    let raw = "";
    let settled = false;
    const finish = (closed) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      resolve(`${raw.split("\r\n")[0] || "(nothing)"}, ${closed ? "closed" : "open"}`);
    };
    const timer = setTimeout(() => finish(false), hold);
    socket.on("data", (d) => (raw += d.toString("latin1")));
    socket.on("close", () => finish(true));
    socket.on("error", () => {});
    socket.once(secure ? "secureConnect" : "connect", () => socket.write(bytes));
  });
}

// Plain sockets that connect and say nothing, held open.
async function silent(port, n) {
  const sockets = [];
  for (let i = 0; i < n; i++) {
    const socket = net.connect(port, "127.0.0.1");
    socket.on("error", () => {});
    await new Promise((r) => socket.once("connect", r));
    sockets.push(socket);
  }
  await sleep(150);
  return sockets;
}

async function closeAll(sockets) {
  for (const s of sockets) s.destroy();
  await sleep(500);
}

const describeDrop = (server, drop) =>
  [
    Object.getPrototypeOf(drop) === null ? "null prototype" : "object",
    Object.keys(drop).join(","),
    drop.localAddress,
    drop.localFamily,
    drop.localPort === server.address().port ? "server port" : `port ${drop.localPort}`,
    drop.remoteAddress,
    drop.remoteFamily,
    typeof drop.remotePort,
  ].join(" ");

const GET = "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

async function listen(server) {
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return server.address().port;
}

// The default: no limit.
{
  const server = http.createServer((req, res) => res.end("ok"));
  log("default", String(server.maxConnections), String(server.maxConnections == null));
  const drops = [];
  server.on("drop", (d) => drops.push(d));
  const port = await listen(server);
  const held = await silent(port, 20);
  log("unset, 20 held", await exchange(port, GET), `drops ${drops.length}`);
  await closeAll(held);
  server.close();
}

// A limit set before listen().
{
  const server = http.createServer((req, res) => res.end("ok"));
  server.maxConnections = 2;
  log("set", String(server.maxConnections));
  const drops = [];
  server.on("drop", (d) => drops.push(d));
  const port = await listen(server);
  const held = await silent(port, 2);
  log("2 of 2 held", await exchange(port, GET));
  await sleep(100);
  log("drops", String(drops.length));
  if (drops[0]) log("drop", describeDrop(server, drops[0]));
  await closeAll(held);
  log("after they close", await exchange(port, GET));
  const one = await silent(port, 1);
  log("1 of 2 held", await exchange(port, GET));
  await closeAll(one);
  log("drops", String(drops.length));
  server.close();
}

// Set after listen(), changed, and values that are not numbers.
{
  const server = http.createServer((req, res) => res.end("ok"));
  const drops = [];
  server.on("drop", () => drops.push(1));
  const port = await listen(server);
  server.maxConnections = 0;
  log("0 after listen", await exchange(port, GET));
  server.maxConnections = 1;
  log("1", await exchange(port, GET));
  for (const value of ["x", null, undefined, "1", true, -1, 1.5]) {
    server.maxConnections = value;
    const held = await silent(port, 1);
    log(`${typeof value} ${String(value)}`, String(server.maxConnections), await exchange(port, GET));
    await closeAll(held);
  }
  log("drops", String(drops.length));
  server.close();
}

// https: the limit counts TCP connections, handshake or not.
{
  const server = https.createServer({ cert: RSA_CERT, key: RSA_KEY }, (req, res) => res.end("ok"));
  server.maxConnections = 1;
  const drops = [];
  server.on("drop", (d) => drops.push(d));
  const port = await listen(server);
  const held = await silent(port, 1);
  const refused = await exchange(port, GET, { secure: true });
  log("https 1 of 1 held", refused.startsWith("(nothing)") ? "nothing" : refused);
  await sleep(100);
  log("https drops", String(drops.length));
  if (drops[0]) log("https drop", describeDrop(server, drops[0]));
  await closeAll(held);
  log("https after it closes", await exchange(port, GET, { secure: true }));
  server.close();
}
