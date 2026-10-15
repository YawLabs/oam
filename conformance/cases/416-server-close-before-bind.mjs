// close() made before listen()'s bind has landed (#387, #369). node binds a
// listen() without a host synchronously, so a close() in the same tick
// closes a bound listener: no 'listening', 'close' on a later tick, close(cb)
// called with nothing, and the port free again. A listen() with a host looks
// the name up first, and a close() during the lookup abandons it: nothing is
// bound, no 'error' even for a port that is taken, and cb gets
// ERR_SERVER_NOT_RUNNING. oam binds every server asynchronously and its
// close() used to act only on a server already bound, so the bind landed
// afterwards and the server listened and served (net, http, https, http2)
// or kept the port bound with nobody accepting (tls), and close(cb) never
// called back. Each server kind (net, tls, http, https, h2c http2) is run
// through:
//   - listen(port) + close(cb) in one tick, then the port re-bound by a
//     fresh server (it is free again);
//   - listen(port, '127.0.0.1') + close(cb) in one tick, on a port another
//     server holds;
//   - listen(port) + close(cb) in one tick on a port another server holds:
//     node's bind fails inside listen(), so 'error' then 'close', and cb
//     gets ERR_SERVER_NOT_RUNNING;
//   - listen, close, and in close's callback listen + close again (the
//     re-listen window);
//   - close(cb) on a server that never listened, and listen() twice;
//   - close() twice before the bind lands, with and without a host: the
//     second close()'s 'close' waits for the bind to let the port go;
//   - listen(), close() and listen() again on the same server, in one tick
//     (with and without a host) and on the next turn of the loop, and two
//     listen(port, host) in one tick: the second bind waits for the first
//     to let the port go, where it used to fail with EADDRINUSE;
//   - a failed same-tick bind with no 'error' listener: an uncaught
//     exception, then 'close' and cb.
// The server's own `listening` and address() between listen() and the bind
// are left out: oam answers false and null there (docs/node-divergences.md).
// Self-signed pair; nothing here connects, so nothing is verified.
import net from "node:net";
import tls from "node:tls";
import http from "node:http";
import https from "node:https";
import http2 from "node:http2";

const cert = `-----BEGIN CERTIFICATE-----
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
const key = `-----BEGIN PRIVATE KEY-----
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

const kinds = [
  ["net", () => net.createServer()],
  ["tls", () => tls.createServer({ key, cert })],
  ["http", () => http.createServer()],
  ["https", () => https.createServer({ key, cert })],
  ["http2", () => http2.createServer()],
];

const log = (...a) => console.log(...a);
const describe = (e) => (e === undefined ? "none" : e.code + " " + JSON.stringify(e.message));

function watch(server, tag) {
  server.on("listening", () => log(tag, "'listening'"));
  server.on("error", (e) => log(tag, "'error'", e.code));
  server.on("close", () => log(tag, "'close'"));
}

// A port nothing listens on now: bound and released by a plain server.
function freePort() {
  return new Promise((resolve) => {
    const s = net.createServer();
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

// A server of `make`'s kind listening on `port` (no host), or the error code.
function bindAgain(make, port) {
  return new Promise((resolve) => {
    const s = make();
    s.once("error", (e) => resolve("rebind " + e.code));
    s.listen(port, () => s.close(() => resolve("rebind ok")));
  });
}

// A plain server holding `port`, for the binds that must fail.
function hold(port) {
  return new Promise((resolve) => {
    const s = net.createServer();
    s.listen(port, () => resolve(s));
  });
}

async function sameTick(name, make) {
  const tag = name + " same-tick:";
  const port = await freePort();
  const s = make();
  watch(s, tag);
  await new Promise((resolve) => {
    s.listen(port, () => log(tag, "listen callback"));
    s.close((...args) => {
      log(tag, "close callback, args", args.length);
      resolve();
    });
  });
  log(tag, await bindAgain(make, port));
}

async function sameTickHost(name, make) {
  const tag = name + " same-tick host:";
  const port = await freePort();
  const holder = await hold(port);
  const s = make();
  watch(s, tag);
  await new Promise((resolve) => {
    s.listen(port, "127.0.0.1", () => log(tag, "listen callback"));
    s.close((e) => {
      log(tag, "close callback", describe(e));
      resolve();
    });
  });
  await new Promise((resolve) => holder.close(resolve));
}

async function sameTickInUse(name, make) {
  const tag = name + " same-tick in use:";
  const port = await freePort();
  const holder = await hold(port);
  const s = make();
  watch(s, tag);
  await new Promise((resolve) => {
    s.listen(port, () => log(tag, "listen callback"));
    s.close((e) => {
      log(tag, "close callback", describe(e));
      resolve();
    });
  });
  await new Promise((resolve) => holder.close(resolve));
}

async function relisten(name, make) {
  const tag = name + " re-listen:";
  const port = await freePort();
  const s = make();
  watch(s, tag);
  await new Promise((resolve) => {
    s.listen(port, () => {
      log(tag, "first listen callback");
      s.close(() => {
        log(tag, "first close callback");
        s.listen(port, () => log(tag, "second listen callback"));
        s.close((...args) => {
          log(tag, "second close callback, args", args.length);
          resolve();
        });
      });
    });
  });
  log(tag, await bindAgain(make, port));
}

async function misuse(name, make) {
  const tag = name + " misuse:";
  const never = make();
  watch(never, tag);
  await new Promise((resolve) => {
    never.close((e) => {
      log(tag, "never-listening close callback", describe(e));
      resolve();
    });
  });
  const twice = make();
  twice.listen(0);
  try {
    twice.listen(0);
    log(tag, "second listen() returned");
  } catch (e) {
    log(tag, "second listen() threw", describe(e));
  }
  await new Promise((resolve) => twice.close(resolve));
}

// listen(port[, host]) and close() twice in one tick: the second close()
// finds no handle, so its cb gets ERR_SERVER_NOT_RUNNING at the first
// 'close' and a second 'close' follows -- and the port is free by the first
// close()'s cb (the fresh server re-binds it from there). oam's second
// close() used to emit its 'close' before the bind had landed and let its
// listener go, so the re-bind failed with EADDRINUSE.
async function doubleClose(name, make, host) {
  const tag = name + " double close" + (host ? " host:" : ":");
  const port = await freePort();
  const s = make();
  watch(s, tag);
  const rebound = await new Promise((resolve) => {
    if (host) s.listen(port, host);
    else s.listen(port);
    s.close((e) => {
      log(tag, "first close callback", describe(e));
      resolve(bindAgain(make, port));
    });
    s.close((e) => log(tag, "second close callback", describe(e)));
  });
  await new Promise((resolve) => setImmediate(resolve));
  log(tag, rebound);
}

// listen(port, host), close() and listen(port, host) again in one tick: node
// abandons the first lookup, so 'close' (cb gets ERR_SERVER_NOT_RUNNING),
// then the second bind listens on the port. oam's second bind used to race
// the first one's listener for the port and fail with EADDRINUSE.
async function relistenHost(name, make) {
  const tag = name + " re-listen same tick host:";
  const port = await freePort();
  const s = make();
  watch(s, tag);
  await new Promise((resolve) => {
    s.listen(port, "127.0.0.1");
    s.close((e) => log(tag, "close callback", describe(e)));
    s.listen(port, "127.0.0.1", () => {
      log(tag, "second listen callback, same port", s.address().port === port);
      s.close(() => {
        log(tag, "second close callback");
        resolve();
      });
    });
  });
  log(tag, await bindAgain(make, port));
}

// The same with no host. node's bind is synchronous, so its first
// 'listening' tick already finds the second listener and 'listening' comes
// before the first 'close'; oam's comes after it (docs/node-divergences.md,
// 52). What both runtimes do -- once each, the right callbacks, the port
// re-bound -- is printed once everything has come, not in arrival order.
async function relistenSameTick(name, make) {
  const tag = name + " re-listen same tick:";
  const port = await freePort();
  const s = make();
  const seen = { listening: 0, close: 0, error: 0 };
  s.on("listening", () => seen.listening++);
  s.on("close", () => seen.close++);
  s.on("error", () => seen.error++);
  let first;
  let samePort;
  await new Promise((resolve) => {
    let left = 2;
    const done = () => --left === 0 && resolve();
    s.listen(port);
    s.close((e) => {
      first = describe(e);
      done();
    });
    s.listen(port, () => {
      samePort = s.address().port === port;
      s.close(done);
    });
  });
  await new Promise((resolve) => setImmediate(resolve));
  log(tag, "first close callback", first, "second listen on the same port", samePort);
  log(tag, "'listening'", seen.listening, "'close'", seen.close, "'error'", seen.error);
  log(tag, await bindAgain(make, port));
}

// listen(port) + close(), and listen(port) again on the next turn of the
// loop (setImmediate): the port is free by then in node; in oam it was
// still bound by the first bind's listener.
async function relistenImmediate(name, make) {
  const tag = name + " re-listen immediate:";
  const port = await freePort();
  const s = make();
  watch(s, tag);
  await new Promise((resolve) => {
    s.listen(port);
    s.close((e) => log(tag, "close callback", describe(e)));
    setImmediate(() => {
      s.listen(port, () => {
        log(tag, "second listen callback");
        s.close(() => {
          log(tag, "second close callback");
          resolve();
        });
      });
    });
  });
}

// listen(port, host) twice in one tick: the second supersedes the first's
// lookup, and listens on the port; oam's two binds used to race for it.
async function supersede(name, make) {
  const tag = name + " supersede:";
  const port = await freePort();
  const s = make();
  watch(s, tag);
  await new Promise((resolve) => {
    s.listen(port, "127.0.0.1", () => log(tag, "first listen callback"));
    s.listen(port, "127.0.0.1", () => {
      log(tag, "second listen callback");
      s.close(() => {
        log(tag, "close callback");
        resolve();
      });
    });
  });
}

// listen(port) + close(cb) in one tick on a port another server holds, with
// no 'error' listener: the 'error' is an uncaught exception, and 'close'
// and cb still come after it. oam's 'error' threw inside the bind's promise
// reaction, an unhandled rejection, and 'close' and cb never came.
async function inUseUnhandled(name, make) {
  const tag = name + " same-tick in use, no 'error' listener:";
  const port = await freePort();
  const holder = await hold(port);
  const s = make();
  s.on("close", () => log(tag, "'close'"));
  const uncaught = (e) => log(tag, "uncaughtException", e.code);
  process.on("uncaughtException", uncaught);
  await new Promise((resolve) => {
    s.listen(port);
    s.close((e) => {
      log(tag, "close callback", describe(e));
      resolve();
    });
  });
  process.removeListener("uncaughtException", uncaught);
  await new Promise((resolve) => holder.close(resolve));
}

for (const [name, make] of kinds) {
  await sameTick(name, make);
  await sameTickHost(name, make);
  await sameTickInUse(name, make);
  await relisten(name, make);
  await misuse(name, make);
  await doubleClose(name, make);
  await doubleClose(name, make, "127.0.0.1");
  await relistenHost(name, make);
  await relistenSameTick(name, make);
  await relistenImmediate(name, make);
  await supersede(name, make);
  await inUseUnhandled(name, make);
}
log("done");
