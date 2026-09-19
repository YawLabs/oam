// An http.Agent's socket pool, over sockets an agent's createConnection
// returns: keepAlive reuse keyed by agent.getName(), maxSockets queueing,
// maxFreeSockets, the 'free' event, freeSockets / sockets / requests
// bookkeeping, what a response's framing and Connection / Keep-Alive headers
// decide, the Connection header node sends, a pooled socket the server
// closes, agent.destroy(), a request destroyed while queued, the global
// agent reused by a request whose socket is watched (got's shape), https,
// and end()'s callback. Measured on node v22.22.2.
//
// A raw server (the same code under both runtimes) counts connections and
// records each request's Connection header. Pool counts are printed once
// destroyed sockets have closed: oam's net.Socket emits 'close' from
// destroy() at once, node's in a later loop phase.
import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";

// The test CA and a leaf for localhost / 127.0.0.1 (as in case 107).
const CA = `-----BEGIN CERTIFICATE-----
MIIDHzCCAgegAwIBAgIUX5ir308lg8m4hQdNnUz0UdN33DwwDQYJKoZIhvcNAQEL
BQAwFjEUMBIGA1UEAwwLb2FtIHRlc3QgQ0EwIBcNMjYwOTE0MTExMTI0WhgPMjEy
NjA4MjExMTExMjRaMBYxFDASBgNVBAMMC29hbSB0ZXN0IENBMIIBIjANBgkqhkiG
9w0BAQEFAAOCAQ8AMIIBCgKCAQEA5oXf7XNg5MHjC511VA64HF8kdBHebuI207US
fCQg9EYTe3hzOBACwsn78SNXFfmDw5E7hlF2xTuZmD3OJx9a0Ax54EoF67Z4Bigw
My6GF1oKNsmeCGn9nv62+7jm9UspForbmWE8/rC3bM37BbvS87FoogEdXQS5uNQz
4AuGbduhr27IXlScHsub4paSIrW6etllby5Ja+81NpVmwuZ32QNk+s0bwcLq8YIq
5zpemaKeTGDBbG3mIt3vYsfjg8zUTdCdkjOs8q0+BSB8OkhGpe888d5JyUxd1WiK
qiTpfG3+2Pbr0eK7pzIzeT+HDfzUInFfr7lu6lBtfjQRWSZWGwIDAQABo2MwYTAd
BgNVHQ4EFgQUKmakijzWE71HQeyNAwaTnq9/xyMwHwYDVR0jBBgwFoAUKmakijzW
E71HQeyNAwaTnq9/xyMwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYw
DQYJKoZIhvcNAQELBQADggEBACX/zgcVyya26/+5t6Be9duAAJs1X0VSKSzXP/Au
A+ngqWqFBPDhIzorx84d+siuRKVLOZUjObba245P4oiaJwNSz3Ihix5V3FHGZTVM
vHpVP8V7tzKpoEz89vfhueFOB0u2TVJe/099DAHrjaaza0zWa1zfxucrBAFQiQIA
2GK95UN3sSv9/rl3QlxQx8ld5QlpIjjhQL7N1JWWKcuBqDHgbfN1qwB2CWSB+v3g
YyTiYg/yyeFi173xPPS3CoiyyVyO+6ySfhwvopDJVkTdZafDpV5/d1o+AssKYX3R
Jd1U3J1YgXh3HzZEI9Yeo2jZzDogNzNObNoYdXRUoDVPMXo=
-----END CERTIFICATE-----`;

const LEAF = `-----BEGIN CERTIFICATE-----
MIIDRzCCAi+gAwIBAgIUXMdiPT0RoKd1ynyNQq5kRcwrF9UwDQYJKoZIhvcNAQEL
BQAwFjEUMBIGA1UEAwwLb2FtIHRlc3QgQ0EwIBcNMjYwOTE0MTExMTI1WhgPMjEy
NjA4MjExMTExMjVaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcN
AQEBBQADggEPADCCAQoCggEBALtUgW8legRgDaIObCQ75gb63jPvvGLkgrmfvL+z
zuIpFr6McD3Em6aX0fje4x8SjVF10F1HTa8pLDy4G6T/UiBuATovjMsEIqk1MLW2
F6/KfQLO35pVC6PeUCYW8UkqymVifxsPQuzdV+Hbp9VDaamHtCFhJN0sl0TAbc37
xp4WZwI1HTSQ4q+ReLSslNQiK+bwJQeKdiL7u6jzXqkb0uTxOJ2bSS2BhpPbPiNR
fZObJiFr6wtURUvy0AY9AmbNJwuWkuM0aJlOibaVIPPgVGDtZJCd8gQEdV4pKIMZ
avTN3AbNeIMmn3nZehk5jvEHxL+tjTXG8no5f5X2KFlMwi0CAwEAAaOBjDCBiTAa
BgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMC
BaAwEwYDVR0lBAwwCgYIKwYBBQUHAwEwHQYDVR0OBBYEFJpXOwzKMtLLnbaIViTA
QsTBV5+8MB8GA1UdIwQYMBaAFCpmpIo81hO9R0HsjQMGk56vf8cjMA0GCSqGSIb3
DQEBCwUAA4IBAQCsP5gsrw1RHvEN9oBR1Pf+CXylfpH7It7ZMWDFW73rdhuC3Zxr
22zgG04mRt2Gd4Ufq4FCjqELVoecWx5U/hv2v/4KmVqegJkcTnMOmQ3Bs391XXa9
C+07yxnaDXE19agNm4ZACwmdf30LPaSqeVp3Y3aw8lH+5KeWrrVBpi7m8NMyHThC
Yn0a/DcxRET01zHZb6AEve5eJT6Lm0YF/DF6r4+YfGehLX892VDoWgrNCz7DpDuC
1ALfON7I9FSAJGh3iBvTbX9R7xVuKd8Za2f8Xwr/t7jK/zYxLAT9oyTH1FXIFAnP
H5shelNOFfKjeO2TTJ9u7hMSzF9fWd4EOB7u
-----END CERTIFICATE-----`;

const LEAF_KEY = `-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC7VIFvJXoEYA2i
DmwkO+YG+t4z77xi5IK5n7y/s87iKRa+jHA9xJuml9H43uMfEo1RddBdR02vKSw8
uBuk/1IgbgE6L4zLBCKpNTC1thevyn0Czt+aVQuj3lAmFvFJKsplYn8bD0Ls3Vfh
26fVQ2mph7QhYSTdLJdEwG3N+8aeFmcCNR00kOKvkXi0rJTUIivm8CUHinYi+7uo
816pG9Lk8Tidm0ktgYaT2z4jUX2TmyYha+sLVEVL8tAGPQJmzScLlpLjNGiZTom2
lSDz4FRg7WSQnfIEBHVeKSiDGWr0zdwGzXiDJp952XoZOY7xB8S/rY01xvJ6OX+V
9ihZTMItAgMBAAECggEADSEodpMjMNilRqJ0JJCo1/xlQ9vy/DYVONAKo/UE9Fz6
Nx4TZSuOgpKe04Prr0CBnx/+xqA6FaNxHPWvxP9le4MPmvW84c3HECJQ6QDQ5YVF
AG63b/2zSdJJvncFL6JMJTxODvt22VskzwkHg68B4jFHXWo4Rzgvh1C6tsvavoxS
DA/J/Pl+saC6iccDtLp4lbJaMzCGGRDPjb13hqBcHoPEjF5JtN9I1bCVUZn/QFbY
7PHRptS2SDuAcoPiC8SlqZff7PSMakZzBT7Ng7kSdW3mFapJkN2NM5IsmIlTyG83
1GfTXCH1o00HXpoJ8N5YundxoG5FWlCIgcrEO1jNEQKBgQDbCJapOXVz6ULoTevi
SzdQH38UH1Ckd1rp0QYxm/MWXXyupWnBBt2iBekbFygT2bRwhtIA33CEusYmh4sN
nal6ERh5wbYYzngPaO0sHX4QVzBYleu344/pkgZxCEpG8G5oxjggj/ds15TsgLVS
KEsvXnodKmVsvDqfDFWD+ZnEzwKBgQDa8ijtlbO2ro9HgvqAr88kS/8nVlnZdXE5
9YT/DEYVsLQzduIze9G4uzI/dgSn8UtUatCvgREFB2CkQUvSeUEF4LIH6zhiI2eu
yJzhAR3tU6hXWsJSLSMildlv6ooWngdNmQg9pXTNbUjJ4dtfn1Rip5A/FKzsLxDG
/mjx6R3AQwKBgHTfA0zuXM5pY4sCsN+BVNVKyQranrPy/66NGqnz1WRUo8eoeWJG
oJHoZ3ZOB9N3sYDtXzaaAra/1iUO49JzEtAQOSgWhWx9FrDaQtrsLazYaPKLpEft
g4eUpB1B2Cg7+B2tzpsJVnNcIJmFH7rjxyJSXgQb8Bxx3zGoaiTOVQ8fAoGAST2G
iWtxkaO1FEPxTkkBbu/pK5yMM91AghXqZnMRosHYlfqn0ncSAczFE0uEZTWncFbG
9l6jdd4w6uFY3tBm+vNeOp3p35JeZa6AJBh+jVxVzNr0dA7bWP9tnC2GAejdIo0V
n6GQgAOVvMrL2qHu1Y2eCCv/aIaaAycprfrAVAcCgYBF6Hs47CZ4RPMzUnlMV8F+
F7McNeFuVRqpneXVSNB7UDuID2ttb7RTchZaG2hc84LWRV0/yjElLrG6yPxyGFIq
hnNgLVJt6pGXwWKx6CgqUvijJFPNwDhZRYtLfyCWXHDQ4E9T3C5DO7T+8lafH6NO
lAvLJ1NDDacIcdciXw6fZg==
-----END PRIVATE KEY-----`;

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

function rawServer(respond, secure) {
  const stats = { connections: 0, requests: [] };
  const onConn = (c) => {
    const conn = ++stats.connections;
    let buf = Buffer.alloc(0);
    let n = 0;
    c.on("error", () => {});
    c.on("data", (d) => {
      buf = Buffer.concat([buf, d]);
      for (;;) {
        const idx = buf.indexOf("\r\n\r\n");
        if (idx === -1) return;
        const lines = buf.subarray(0, idx).toString("latin1").split("\r\n");
        const headers = {};
        for (const l of lines.slice(1)) {
          const i = l.indexOf(":");
          headers[l.slice(0, i).toLowerCase()] = l.slice(i + 1).trim();
        }
        const len = +(headers["content-length"] || 0);
        if (buf.length < idx + 4 + len) return;
        buf = buf.subarray(idx + 4 + len);
        n++;
        stats.requests.push({ conn, n, connection: headers.connection });
        const method = lines[0].split(" ")[0];
        const out = respond(c, method);
        c.write(out.data ?? out);
        if (out.end) c.end();
      }
    });
  };
  const server = secure
    ? tls.createServer({ key: LEAF_KEY, cert: LEAF }, onConn)
    : net.createServer(onConn);
  return new Promise((resolve) =>
    server.listen(0, "127.0.0.1", () => resolve({ server, stats, port: server.address().port })),
  );
}

const OK = (extra = "") => `HTTP/1.1 200 OK\r\nContent-Length: 2\r\n${extra}\r\nok`;

class OwnAgent extends http.Agent {
  createConnection(options, cb) {
    return net.createConnection(options, cb);
  }
}
class OwnHttpsAgent extends https.Agent {
  createConnection(options, cb) {
    return tls.connect(options, cb);
  }
}

function get(port, options = {}, onReq) {
  return new Promise((resolve) => {
    const req = (options.secure ? https : http).request(
      { host: "127.0.0.1", port, path: "/", ...options.req },
      (res) => {
        res.resume();
        res.on("end", () => resolve({ req, res, reused: req.reusedSocket, keep: req.shouldKeepAlive }));
      },
    );
    req.on("error", (e) => resolve({ req, error: `${e.code} ${e.message}` }));
    if (onReq) onReq(req);
    req.end();
  });
}
const tick = () => new Promise((r) => setImmediate(r));
const settle = () => new Promise((r) => setTimeout(r, 50));
let PORT = 0;
const named = (s) => s.split(`:${PORT}:`).join(":PORT:");
function pool(agent) {
  const count = (o) => {
    const out = {};
    for (const [k, v] of Object.entries(o)) out[named(k)] = v.length;
    return JSON.stringify(out);
  };
  return `free=${count(agent.freeSockets)} sockets=${count(agent.sockets)} requests=${count(agent.requests)} total=${agent.totalSocketCount}`;
}
const conns = (stats) => stats.requests.map((r) => `${r.conn}.${r.n}:${r.connection}`).join(" ");
const headersSeen = (stats) => [...new Set(stats.requests.map((r) => r.connection))].join(",");
async function serve(respond, secure) {
  const s = await rawServer(respond, secure);
  PORT = s.port;
  return s;
}

{
  const { server, stats, port } = await serve(() => OK());
  const agent = new OwnAgent({ keepAlive: true });
  let frees = 0;
  agent.on("free", () => frees++);
  const seen = [];
  for (let i = 0; i < 4; i++) {
    const r = await get(port, { req: { agent } });
    seen.push(`${r.reused}/${r.keep}/${r.res.socket === null}`);
  }
  const free = Object.values(agent.freeSockets)[0]?.[0];
  console.log(`keepAlive, 4 in turn: conns=${stats.connections} [${conns(stats)}] reused/keep/res.socket-null=${seen} frees=${frees} ${pool(agent)} timeout=${free?.timeout}`);
  agent.destroy();
  server.close();
}

{
  const { server, stats, port } = await serve(() => OK());
  const agent = new OwnAgent();
  let frees = 0;
  agent.on("free", () => frees++);
  const seen = [];
  for (let i = 0; i < 3; i++) {
    const r = await get(port, { req: { agent } });
    seen.push(`${r.reused}/${r.keep}`);
  }
  await settle();
  console.log(`no keepAlive: conns=${stats.connections} [${conns(stats)}] ${seen} frees=${frees} ${pool(agent)}`);
  server.close();
}

{
  const { server, stats, port } = await serve(() => OK());
  const agent = new OwnAgent({ keepAlive: true, maxSockets: 1 });
  const all = [0, 1, 2].map(() => get(port, { req: { agent } }));
  console.log(`maxSockets 1, 3 at once, queued: ${pool(agent)}`);
  const rs = await Promise.all(all);
  await settle();
  console.log(`maxSockets 1: conns=${stats.connections} [${conns(stats)}] reused=${rs.map((r) => r.reused)} ${pool(agent)}`);
  agent.destroy();
  server.close();
}

{
  const { server, stats, port } = await serve(() => OK());
  const agent = new OwnAgent({ maxSockets: 2 });
  const rs = await Promise.all([0, 1, 2, 3].map(() => get(port, { req: { agent } })));
  await settle();
  console.log(`maxSockets 2 without keepAlive, 4 at once: conns=${stats.connections} requests=${stats.requests.length} sent=${headersSeen(stats)} keep=${rs.map((r) => r.keep)} ${pool(agent)}`);
  server.close();
}

{
  const { server, stats, port } = await serve(() => OK());
  const agent = new OwnAgent({ keepAlive: true, maxFreeSockets: 1 });
  await Promise.all([0, 1, 2].map(() => get(port, { req: { agent } })));
  await settle();
  console.log(`maxFreeSockets 1, 3 at once: conns=${stats.connections} ${pool(agent)}`);
  const rs = await Promise.all([0, 1, 2].map(() => get(port, { req: { agent } })));
  await settle();
  console.log(`maxFreeSockets 1, 3 more: conns=${stats.connections} reused=${rs.map((r) => r.reused)} ${pool(agent)}`);
  agent.destroy();
  server.close();
}

for (const [label, head] of [
  ["Connection: close", "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"],
  ["Keep-Alive: timeout=1", OK("Keep-Alive: timeout=1\r\n")],
  ["Keep-Alive: timeout=5", OK("Keep-Alive: timeout=5\r\n")],
  ["HTTP/1.0", "HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok"],
  ["HTTP/1.0 keep-alive", "HTTP/1.0 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok"],
  ["no length", { data: "HTTP/1.1 200 OK\r\n\r\nok", end: true }],
  ["chunked", "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n"],
  ["204", "HTTP/1.1 204 No Content\r\n\r\n"],
  ["304", "HTTP/1.1 304 Not Modified\r\n\r\n"],
]) {
  const { server, stats, port } = await serve(() => head);
  const agent = new OwnAgent({ keepAlive: true, timeout: 7000 });
  const seen = [];
  for (let i = 0; i < 2; i++) {
    const r = await get(port, { req: { agent } });
    seen.push(r.error || `${r.res.statusCode}/${r.reused}/${r.keep}`);
  }
  await settle();
  const free = Object.values(agent.freeSockets)[0]?.[0];
  console.log(`response ${label}: conns=${stats.connections} ${seen} ${pool(agent)} timeout=${free?.timeout}`);
  agent.destroy();
  server.close();
}

{
  const { server, stats, port } = await serve((c, method) =>
    method === "HEAD" ? "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n" : OK(),
  );
  const agent = new OwnAgent({ keepAlive: true });
  const r = await get(port, { req: { agent, method: "HEAD" } });
  const r2 = await get(port, { req: { agent, method: "HEAD" } });
  console.log(`HEAD: conns=${stats.connections} reused=${r.reused},${r2.reused}`);
  agent.destroy();
  server.close();
}

{
  const { server, stats, port } = await serve((c) => {
    setTimeout(() => c.end(), 30);
    return OK();
  });
  const agent = new OwnAgent({ keepAlive: true });
  let closes = 0;
  const r1 = await get(port, { req: { agent } });
  r1.req.socket.on("close", () => closes++);
  await tick();
  const before = pool(agent);
  await new Promise((r) => setTimeout(r, 200));
  const after = pool(agent);
  const r2 = await get(port, { req: { agent } });
  console.log(`server closes a pooled socket: before=${before} after=${after} closes=${closes} conns=${stats.connections} reused=${r2.reused}`);
  agent.destroy();
  server.close();
}

for (const [label, apply] of [
  ["caller Connection: close", (req) => req.setHeader("connection", "close")],
  ["caller Connection: keep-alive, no keepAlive", (req) => req.setHeader("connection", "keep-alive")],
  ["Connection header removed", (req) => req.removeHeader("connection")],
]) {
  const { server, stats, port } = await serve(() => OK());
  const agent = new OwnAgent({ keepAlive: !label.includes("no keepAlive") });
  const seen = [];
  for (let i = 0; i < 2; i++) {
    const r = await get(port, { req: { agent } }, apply);
    seen.push(`${r.reused}/${r.keep}`);
  }
  await settle();
  console.log(`${label}: conns=${stats.connections} [${conns(stats)}] ${seen} ${pool(agent)}`);
  agent.destroy();
  server.close();
}

{
  const { server, port } = await serve(() => OK());
  const agent = new OwnAgent({ keepAlive: true });
  const log = [];
  agent.on("free", () => log.push(`agent free ${pool(agent)}`));
  await new Promise((resolve) => {
    const req = http.get({ host: "127.0.0.1", port, agent }, (res) => {
      req.socket.on("free", () => log.push("socket free"));
      res.on("end", () => {
        log.push(`res end socket=${res.socket === null ? "null" : "set"}`);
        process.nextTick(() => log.push("nextTick after end"));
        Promise.resolve().then(() => log.push(`microtask after end ${pool(agent)}`));
        setImmediate(() => {
          log.push("immediate after end");
          resolve();
        });
      });
      res.resume();
    });
    req.on("close", () => log.push(`req close destroyed=${req.destroyed}`));
    req.on("finish", () => log.push("req finish"));
  });
  console.log(`order: ${log.join(" | ")}`);
  agent.destroy();
  server.close();
}

{
  const { server, port } = await serve(() => OK());
  const agent = new OwnAgent({ keepAlive: true });
  const r = await get(port, { req: { agent } });
  await tick();
  let closed = false;
  r.req.socket.on("close", () => (closed = true));
  agent.destroy();
  await settle();
  console.log(`agent.destroy(): closed=${closed} ${pool(agent)}`);
  server.close();
}

{
  const { server, stats, port } = await serve(() => OK());
  const r = await get(port, { req: { agent: undefined, createConnection: (o, cb) => net.createConnection(o, cb) } });
  console.log(`createConnection option: [${conns(stats)}] keep=${r.keep}`);
  server.close();
}

for (const how of ["destroy", "abort"]) {
  const { server, stats, port } = await serve(() => OK());
  const agent = new OwnAgent({ keepAlive: true, maxSockets: 1 });
  const events = [];
  const p1 = get(port, { req: { agent } });
  let r2;
  const p2 = get(port, { req: { agent } }, (req) => {
    r2 = req;
    req.on("close", () => events.push("close"));
    req.on("error", (e) => events.push(`error ${e.code}`));
    req.on("abort", () => events.push("abort"));
  });
  const p3 = get(port, { req: { agent } });
  r2[how]();
  const done = await Promise.all([p1, p3]);
  await settle();
  console.log(`${how}() while queued: conns=${stats.connections} [${conns(stats)}] r1=${done[0].res.statusCode} r3 reused=${done[1].reused} r2 events=${events} ${pool(agent)}`);
  agent.destroy();
  server.close();
  void p2;
}

{
  const { server, stats, port } = await serve(() => OK());
  const seen = [];
  for (let i = 0; i < 3; i++) {
    const r = await get(port, {}, (req) => {
      req.on("socket", (s) => {
        s.once("connect", () => seen.push(`connect${i}`));
      });
    });
    seen.push(`${r.reused}`);
  }
  await settle();
  console.log(`global agent, socket watched: conns=${stats.connections} [${conns(stats)}] ${seen} ${pool(http.globalAgent)}`);
  http.globalAgent.destroy();
  server.close();
}

{
  const { server, stats, port } = await serve(() => OK(), true);
  const agent = new OwnHttpsAgent({ keepAlive: true, ca: CA });
  const seen = [];
  for (let i = 0; i < 3; i++) {
    const r = await get(port, { secure: true, req: { agent, servername: "localhost" } });
    seen.push(r.error || `${r.reused}`);
  }
  console.log(`https keepAlive: conns=${stats.connections} [${conns(stats)}] ${seen}`);
  agent.destroy();
  server.close();
}

{
  const { server, stats, port } = await serve(() => OK());
  const agent = new OwnAgent({ keepAlive: true });
  const seen = [];
  for (let i = 0; i < 3; i++) {
    const r = await new Promise((resolve) => {
      const events = [];
      const req = http.request({ host: "127.0.0.1", port, method: "POST", agent }, (res) => {
        events.push("response");
        res.resume();
        res.on("end", () => resolve({ reused: req.reusedSocket, events }));
      });
      req.on("finish", () => events.push("finish"));
      req.end("hello body " + i, function () {
        events.push(`end callback args=${arguments.length}`);
      });
    });
    seen.push(`${r.reused}:${r.events.join(">")}`);
  }
  console.log(`POST keepAlive: conns=${stats.connections} [${conns(stats)}] ${seen}`);
  agent.destroy();
  server.close();
}
console.log("done");
