// An http or https server holds each connection to node's timeouts:
//
// - headersTimeout / requestTimeout: a request that is not in within the
//   time is answered `408 Request Timeout` (unless a response head already
//   went out) and its connection is closed, by a check that runs every
//   connectionsCheckingInterval. The clock starts at the accept and again at
//   each request's first byte; a request whose body is in is not checked.
// - keepAliveTimeout + keepAliveTimeoutBuffer: an idle keep-alive connection
//   is closed, or handed to a server 'timeout' listener.
// - server.timeout, server.setTimeout, req.setTimeout, res.setTimeout: the
//   socket timeout, with node's 'timeout' events and its destroy-by-default.
// - https's handshakeTimeout.
//
// oam's server had none of them: the options were stored and ignored, so a
// connection that sent nothing, or stopped half way, or sat idle between
// requests, held one of the server's connection slots for good.
//
// Times are printed as ranges generous enough for a loaded machine.
import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";

const RSA_CERT = `-----BEGIN CERTIFICATE-----
MIIDCzCCAfOgAwIBAgIUJscRiMbEzxV45KtAxD+Lly4dJrQwDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI1MDEwMTAwMDAwMFoYDzIxMjUw
MTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwggEiMA0GCSqGSIb3DQEB
AQUAA4IBDwAwggEKAoIBAQChDlr9+HgndVbQw+meASk37JFR0m2WZjpoGFX/3zjI
QiaeD38wagrRlF0HxhgarHhrArXPzYQLC2aZ1U7/vtYDiro3QIlHOYAXoE31GDOj
IpXbDWW2jA8XIHpKtWpxbUweoD/01f+DD9yS9yNy5rNh4jznuOha6VSZH+JYilI2
4Elzqh0QiR9cb1br8bJnCW61Becpprx4KUuoKb6EOeQ8EfH52gZbF7MVV458UB1A
zzA1dPuaPhA3d+IcUNAcjpSRKdW227uUAAQdj3vyCOdXKLEA9fZ1j2YH8R3hHl5I
6kl5NHttcfr5CZRwMlJoNuUzm8Hd9vfOGK0QYuj8A91BAgMBAAGjUzBRMB0GA1Ud
DgQWBBQ99/D1x3ILUtDY8uvM3shmOSkqYzAfBgNVHSMEGDAWgBQ99/D1x3ILUtDY
8uvM3shmOSkqYzAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQAH
jdkIGOWczuu/hZMJ3QZcynx2Uxm7qK3cox56uFpnO7RPkv4jBu9SAOl7WVWwOlYb
MlL7Iamv+A+NdghDucbEiDyw9798hVQggIe1cjMjkdpeXD1vBp2XWzVcFUCkQFjP
kWeSUzzcsaC/rFV5xASqP2RQK+aGAPeUfhgin1lCtuC0zfhfsQVTSD5HrPkTKYMj
7GKUJqmLqTPreCVblP9BCm9S4Og5mvNx1SFwBUIHIcuosk9daaf7JZQynYv1yJRp
E3ybo/8zVvpZ3durGRT9NLilVC289CbYbRWAkh2zmHNA5nZsgxU9/JSLAjgtmLEt
laiOwbm2oOPQ9QfligDD
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
const show = (label, fn) => {
  try {
    log(label, JSON.stringify(fn()));
  } catch (e) {
    log(label, `throws ${e.name} ${e.code} ${JSON.stringify(e.message)}`);
  }
};
const within = (ms, lo, hi) => (ms >= lo && ms < hi ? `in [${lo}, ${hi})` : `OUT of [${lo}, ${hi}): ${ms}`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- the options, as node validates and stores them ----
const props = (s) => ({
  timeout: s.timeout,
  keepAliveTimeout: s.keepAliveTimeout,
  keepAliveTimeoutBuffer: s.keepAliveTimeoutBuffer,
  headersTimeout: s.headersTimeout,
  requestTimeout: s.requestTimeout,
  connectionsCheckingInterval: s.connectionsCheckingInterval,
  own: Object.keys(s).filter((k) => /imeout|nterval/i.test(k)),
});
show("http defaults", () => props(http.createServer()));
show("https defaults", () => props(https.createServer({ cert: RSA_CERT, key: RSA_KEY })));
for (const [name, values] of Object.entries({
  headersTimeout: [0, 1000, -1, 1.5, "1", null, Infinity],
  requestTimeout: [0, 1000, -1, "1"],
  keepAliveTimeout: [0, 1000, -1, "1"],
  keepAliveTimeoutBuffer: [0, 250, -1, "1"],
  connectionsCheckingInterval: [0, 100, -1, 1.5],
})) {
  for (const value of values) {
    const shown = typeof value === "string" ? JSON.stringify(value) : String(value);
    show(`{${name}: ${shown}}`, () => props(http.createServer({ [name]: value })));
  }
}
show("{headersTimeout: 5000, requestTimeout: 1000}", () => props(http.createServer({ headersTimeout: 5000, requestTimeout: 1000 })));
show("{headersTimeout: 0, requestTimeout: 1000}", () => props(http.createServer({ headersTimeout: 0, requestTimeout: 1000 })));
show("https {headersTimeout: 5000, requestTimeout: 1000}", () => props(https.createServer({ cert: RSA_CERT, key: RSA_KEY, headersTimeout: 5000, requestTimeout: 1000 })));
show("https {handshakeTimeout: '1'}", () => https.createServer({ cert: RSA_CERT, key: RSA_KEY, handshakeTimeout: "1" }) && "created");
show("setTimeout(500, cb)", () => {
  const s = http.createServer();
  return [s.setTimeout(500, () => {}) === s, s.timeout, s.listenerCount("timeout")];
});
show("req.setTimeout / res.setTimeout", () => [typeof http.IncomingMessage.prototype.setTimeout, typeof http.ServerResponse.prototype.setTimeout]);

// ---- the connection side, over raw sockets ----

// Connect, write `writes` ([delay ms, bytes] in turn), and record what comes
// back until the server closes or `hold` ms pass.
function connect(port, { writes = [], hold = 4000, secure = false } = {}) {
  return new Promise((resolve) => {
    const started = Date.now();
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
      const statuses = [...raw.matchAll(/HTTP\/1\.1 \d{3}[^\r\n]*/g)].map((m) => m[0]);
      resolve({ statuses: statuses.join(" + ") || "(nothing)", closed, elapsed: Date.now() - started, raw });
    };
    const timer = setTimeout(() => finish(false), hold);
    socket.on("data", (d) => (raw += d.toString("latin1")));
    socket.on("close", () => finish(true));
    socket.on("error", () => {});
    (async () => {
      for (const [delay, bytes] of writes) {
        await sleep(delay);
        if (!socket.destroyed) socket.write(bytes);
      }
    })();
  });
}

async function serve(options, handler, setup) {
  const server = http.createServer(options, handler || ((req, res) => {
    req.resume();
    req.on("end", () => res.end("ok"));
  }));
  if (setup) setup(server);
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return server;
}

const GET = "GET / HTTP/1.1\r\nHost: x\r\n\r\n";
const FAST = { headersTimeout: 500, requestTimeout: 1000, connectionsCheckingInterval: 50 };

{
  const server = await serve(FAST);
  const r = await connect(server.address().port);
  log("silent connection", r.statuses, `closed ${r.closed}`, within(r.elapsed, 500, 2500));
  log("the answer", JSON.stringify(r.raw));
  server.close();
}
{
  // The clock starts again at the first byte: part of a head, from 300 ms
  // on. (The client stops writing before the answer, so the server's close
  // never meets bytes it has not read.)
  const server = await serve(FAST);
  const r = await connect(server.address().port, {
    writes: [[300, "G"], [100, "ET / HT"], [100, "TP/1.1\r\n"]],
  });
  log("dripped head", r.statuses, `closed ${r.closed}`, within(r.elapsed, 800, 2800));
  server.close();
}
{
  const events = [];
  const server = await serve(FAST, (req, res) => {
    for (const e of ["aborted", "close"]) req.on(e, () => events.push(`req ${e}`));
    req.on("error", (err) => events.push(`req error ${err.code} ${err.message}`));
    res.on("close", () => events.push(`res close finished=${res.writableFinished}`));
    req.resume();
  });
  const r = await connect(server.address().port, { writes: [[0, "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nab"]] });
  await sleep(50);
  log("body never finishes", r.statuses, `closed ${r.closed}`, within(r.elapsed, 1000, 3000), events.sort().join(", "));
  server.close();
}
{
  const events = [];
  const server = await serve(FAST, (req, res) => {
    res.writeHead(200, { "content-type": "text/plain" });
    res.flushHeaders();
    req.on("aborted", () => events.push("req aborted"));
    res.on("close", () => events.push(`res close finished=${res.writableFinished}`));
    req.resume();
  });
  const r = await connect(server.address().port, { writes: [[0, "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nab"]] });
  await sleep(50);
  log("body never finishes, response started", r.statuses, `closed ${r.closed}`, events.sort().join(", "));
  server.close();
}
{
  const server = await serve(FAST, (req, res) => {
    req.resume();
    req.on("end", () => setTimeout(() => res.end("late"), 1500));
  });
  const r = await connect(server.address().port, { writes: [[0, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"]] });
  log("handler slower than requestTimeout", r.statuses, `closed ${r.closed}`);
  server.close();
}
{
  const server = await serve({ keepAliveTimeout: 300, keepAliveTimeoutBuffer: 200 });
  const r = await connect(server.address().port, { writes: [[0, GET]] });
  log("idle keep-alive", r.statuses, `closed ${r.closed}`, within(r.elapsed, 500, 2000));
  server.close();
}
{
  const server = await serve({ keepAliveTimeout: 0 });
  const r = await connect(server.address().port, { writes: [[0, GET]], hold: 1500 });
  log("keepAliveTimeout 0", r.statuses, `closed ${r.closed}`);
  server.close();
}
{
  // Set after listen() returned, the usual place.
  const server = http.createServer((req, res) => res.end("ok"));
  server.listen(0, "127.0.0.1");
  server.keepAliveTimeout = 200;
  server.keepAliveTimeoutBuffer = 0;
  await new Promise((r) => server.once("listening", r));
  const r = await connect(server.address().port, { writes: [[0, GET]] });
  log("keepAliveTimeout set after listen()", r.statuses, `closed ${r.closed}`, within(r.elapsed, 200, 1500));
  server.close();
}
{
  let calls = 0;
  const server = await serve({ keepAliveTimeout: 200, keepAliveTimeoutBuffer: 0 }, null, (s) =>
    s.on("timeout", (socket) => {
      calls += 1;
      log("keep-alive 'timeout' listener", typeof socket, socket.remoteAddress);
    }),
  );
  const r = await connect(server.address().port, { writes: [[0, GET]], hold: 1200 });
  log("keep-alive with a 'timeout' listener", r.statuses, `closed ${r.closed}`, `calls ${calls}`);
  server.close();
}
{
  // The second request's clock starts at its first byte.
  const server = await serve({ ...FAST, keepAliveTimeout: 700, keepAliveTimeoutBuffer: 0 });
  const r = await connect(server.address().port, {
    writes: [[0, GET], [300, "G"], [100, "ET /2 HT"], [100, "TP/1.1\r\n"]],
  });
  log("dripped second request", r.statuses, `closed ${r.closed}`, within(r.elapsed, 800, 2800));
  server.close();
}
{
  const server = await serve({ headersTimeout: 3000, requestTimeout: 4000, connectionsCheckingInterval: 50 });
  server.timeout = 300;
  const r = await connect(server.address().port);
  log("server.timeout, silent connection", r.statuses, `closed ${r.closed}`, within(r.elapsed, 300, 2000));
  server.close();
}
{
  let seen = "";
  const server = await serve({ headersTimeout: 800, requestTimeout: 1000, connectionsCheckingInterval: 50 }, null, (s) =>
    s.setTimeout(300, (socket) => {
      seen = `${typeof socket} destroyed=${socket.destroyed} ${socket.remoteAddress}`;
    }),
  );
  const r = await connect(server.address().port);
  log("server.setTimeout(ms, cb)", seen, r.statuses, `closed ${r.closed}`, within(r.elapsed, 800, 2800));
  server.close();
}
{
  let called = false;
  const server = await serve({}, (req, res) => {
    req.setTimeout(300, () => {
      called = true;
    });
    req.resume();
  });
  const r = await connect(server.address().port, { writes: [[0, GET]] });
  log("req.setTimeout on a complete request", `callback ${called}`, r.statuses, `closed ${r.closed}`, within(r.elapsed, 300, 2000));
  server.close();
}
{
  const events = [];
  const server = await serve({}, (req, res) => {
    res.setTimeout(300);
    res.on("close", () => events.push("res close"));
    req.resume();
  });
  const r = await connect(server.address().port, { writes: [[0, GET]] });
  await sleep(50);
  log("res.setTimeout without a listener", r.statuses, `closed ${r.closed}`, within(r.elapsed, 300, 2000), events.join(", "));
  server.close();
}
{
  const events = [];
  const server = await serve({}, (req, res) => {
    res.setTimeout(300, () => {
      events.push("res timeout");
      res.end("after timeout");
    });
    req.resume();
  });
  const r = await connect(server.address().port, { writes: [[0, "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"]] });
  log("res.setTimeout with a callback", r.statuses, `closed ${r.closed}`, events.join(", "));
  server.close();
}
{
  // https: the TLS handshake has its own limit, then the same timeouts.
  const server = https.createServer({ cert: RSA_CERT, key: RSA_KEY, handshakeTimeout: 300, ...FAST }, (req, res) => res.end("ok"));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  const silent = await connect(port);
  log("https, no handshake", silent.statuses, `closed ${silent.closed}`, within(silent.elapsed, 300, 2000));
  const idle = await connect(port, { secure: true });
  log("https, silent after the handshake", idle.statuses, `closed ${idle.closed}`, within(idle.elapsed, 500, 2500));
  server.close();
}
{
  // A request pipelined behind one whose body the handler never read is
  // held to its own timeouts: its body never comes, so it is answered 408.
  const server = await serve(FAST, (req, res) => {
    if (req.url === "/a") res.end("nope");
  });
  const r = await connect(server.address().port, {
    writes: [
      [0, "POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nab"],
      [200, "cdePOST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n"],
    ],
  });
  log("pipelined request after an unread body", r.statuses, `closed ${r.closed}`, within(r.elapsed, 1000, 3500));
  server.close();
}
{
  // Uploads the client abandons mid-body: each response closes.
  let closes = 0;
  const server = await serve({}, (req, res) => {
    res.on("close", () => closes++);
    req.resume();
    req.on("end", () => res.end("ok"));
  });
  for (let i = 0; i < 3; i++) {
    await new Promise((resolve) => {
      const socket = net.connect(server.address().port, "127.0.0.1", () => {
        socket.write("POST /u HTTP/1.1\r\nHost: x\r\nContent-Length: 100000\r\n\r\n" + "x".repeat(1000));
        setTimeout(() => {
          socket.destroy();
          resolve();
        }, 100);
      });
      socket.on("error", () => {});
    });
  }
  await sleep(300);
  log("uploads abandoned mid-body", `response closes ${closes}`);
  server.close();
}
for (const how of ["end", "destroy"]) {
  // The response handed over before the socket goes is still delivered.
  const server = await serve({}, (req, res) => {
    res.end(`body-${how}`);
    req.socket[how]();
  });
  let delivered = 0;
  for (let i = 0; i < 10; i++) {
    const r = await connect(server.address().port, { writes: [[0, GET]], hold: 2000 });
    if (r.raw.includes(`body-${how}`) && r.closed) delivered++;
  }
  log(`res.end() then socket.${how}()`, `delivered and closed ${delivered}/10`);
  server.close();
}
