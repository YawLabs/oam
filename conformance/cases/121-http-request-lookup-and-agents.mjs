// http.request and https.request honour the `lookup` connect option (the
// request's and the agent's -- the agent's wins, as node's createSocket
// merges them), a replaced dns.lookup, and an agent's own createConnection:
// the request goes over the socket that agent returns. Measured on node
// v22.22.2:
//  - a lookup hook runs inside http.get(), a refusal fails the request with
//    the hook's own error object, and a hook that throws makes http.get()
//    throw it;
//  - req.socket is null until 'socket', a 'lookup' or 'connect' listener
//    attached there that destroys the socket stops the request before a byte
//    is written, and the response names the socket it came on;
//  - the Agent protocol: createConnection may throw, call back with an error
//    or a socket later, or return a socket that connects somewhere other than
//    the request's host (the Host header still names the host); a duck agent
//    hands the request its socket with onSocket(); `agent: {}` is refused;
//    http.Agent is an ES5 constructor; the global agents are real instances
//    whose patched methods are honoured.
// Guard replicas after request-filtering-agent 3.2.1 / 1.1.2 and
// ssrf-req-filter 1.1.1 (all MIT) run against 127.0.0.1, [::1] and a name an
// asynchronous request lookup maps to 127.0.0.1: node refuses every one, and
// so must oam. oam used to send every such request through its fetch
// transport, which ignored the hook and the agent and reached the target.
// Ports are redacted; servers bind 127.0.0.1 and ::1 separately.
import dns from "node:dns";
import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

// Fixtures from case 107: a throwaway CA and the localhost leaf it signed.
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


const j = (v) => JSON.stringify(v);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const ports = [];
const P = (s) => {
  let out = String(s);
  for (const p of ports) out = out.replaceAll(String(p), "PORT");
  return out;
};
function desc(e) {
  if (!e) return "none";
  return `${e.constructor && e.constructor.name} code=${e.code} msg=${j(P(e.message))}`;
}

let hits = 0;
const handler = (req, res) => {
  hits++;
  res.end(`hit ${req.headers.host}`);
};
const listen = (server, host) => new Promise((r) => server.listen(0, host, r));
const srv4 = http.createServer(handler);
await listen(srv4, "127.0.0.1");
const srv6 = http.createServer(handler);
await listen(srv6, "::1");
const tsrv4 = https.createServer({ key: LEAF_KEY, cert: LEAF }, handler);
await listen(tsrv4, "127.0.0.1");
const P4 = srv4.address().port;
const P6 = srv6.address().port;
const T4 = tsrv4.address().port;
ports.push(P4, P6, T4);

// One request, start to finish: what it threw, or how it ended, the events
// on the way, and how many requests reached a server.
async function run(label, start, expect = {}) {
  const before = hits;
  const events = [];
  let req;
  try {
    req = start(events);
  } catch (e) {
    console.log(label, "THROW", desc(e), "| same", e === expect.err, "| events", j(events));
    return;
  }
  events.push(`returned socket=${req.socket === null ? "null" : typeof req.socket}`);
  let timer;
  const outcome = await new Promise((resolve) => {
    req.on("socket", (s) => events.push(`socket ${s.constructor.name} remote=${s.remoteAddress}`));
    req.on("response", (res) => {
      // The socket's facts at 'response' (after 'end', node has handed a
      // keep-alive socket back to its pool and detached it).
      let facts = `remote=${res.socket.remoteAddress} same-socket=${res.socket === req.socket}`;
      if (expect.tls) {
        facts += ` authorized=${res.socket.authorized} cn=${res.socket.getPeerCertificate().subject.CN}`;
      }
      let body = "";
      res.setEncoding("utf8");
      res.on("data", (d) => (body += d));
      res.on("end", () => resolve(`RESPONSE ${res.statusCode} ${j(P(body))} ${facts}`));
    });
    req.on("error", (e) => resolve(`ERROR ${desc(e)} same=${e === expect.err}`));
    timer = setTimeout(() => resolve("PENDING"), expect.wait || 5000);
  });
  clearTimeout(timer);
  await sleep(30);
  console.log(label, outcome, "| hits+", hits - before, "| events", j(events));
  req.destroy();
  await sleep(10);
}

const deny = Object.assign(new Error("blocked"), { code: "EBLOCKED" });
// A lookup hook that records its call and answers as told.
function hook(events, answer, sync) {
  return function lookup(host, opts, cb) {
    events.push(`lookup(${j(host)}, ${j(opts)})`);
    const fire = () => {
      const a = answer();
      if (a.throwSync) throw a.throwSync;
      if (a.err) cb(a.err);
      else if (opts.all) cb(null, a.all);
      else cb(null, a.all[0].address, a.all[0].family);
    };
    if (sync) fire();
    else setTimeout(fire, 5);
  };
}
const to127 = () => ({ all: [{ address: "127.0.0.1", family: 4 }] });
const refuse = () => ({ err: deny });

console.log("==== lookup");
await run("opts.lookup ok async", (ev) =>
  http.get({ host: "guard.test", port: P4, agent: new http.Agent(), lookup: hook(ev, to127) }));
await run("opts.lookup ok sync", (ev) =>
  http.get({ host: "guard.test", port: P4, agent: new http.Agent(), lookup: hook(ev, to127, true) }));
await run("opts.lookup deny async", (ev) =>
  http.get({ host: "guard.test", port: P4, agent: new http.Agent(), lookup: hook(ev, refuse) }), { err: deny });
await run("opts.lookup deny sync", (ev) =>
  http.get({ host: "guard.test", port: P4, agent: new http.Agent(), lookup: hook(ev, refuse, true) }), { err: deny });
await run("opts.lookup throws", (ev) =>
  http.get({ host: "guard.test", port: P4, agent: new http.Agent(), lookup: hook(ev, () => ({ throwSync: deny }), true) }), { err: deny });
await run("opts.lookup bad ip", (ev) =>
  http.get({ host: "guard.test", port: P4, agent: new http.Agent(), lookup: hook(ev, () => ({ all: [{ address: "nope", family: 4 }] })) }));
await run("opts.lookup family:4", (ev) =>
  http.get({ host: "guard.test", port: P4, family: 4, agent: new http.Agent(), lookup: hook(ev, to127) }));
await run("opts.lookup IP literal", (ev) =>
  http.get({ host: "127.0.0.1", port: P4, agent: new http.Agent(), lookup: hook(ev, refuse) }));
await run("opts.lookup not a function", () =>
  http.get({ host: "guard.test", port: P4, agent: new http.Agent(), lookup: "nope" }));
await run("agent lookup deny", (ev) =>
  http.get({ host: "guard.test", port: P4, agent: new http.Agent({ lookup: hook(ev, refuse) }) }), { err: deny });
{
  const called = [];
  await run("agent and request lookup: the agent's wins", () =>
    http.get({
      host: "guard.test",
      port: P4,
      agent: new http.Agent({ lookup: (h, o, cb) => { called.push("agent"); cb(null, [{ address: "127.0.0.1", family: 4 }]); } }),
      lookup: (h, o, cb) => { called.push("request"); cb(deny); },
    }));
  console.log("  called", j(called));
}
{
  const original = dns.lookup;
  const called = [];
  dns.lookup = (h, o, cb) => { called.push(h); cb(deny); };
  await run("replaced dns.lookup", () => http.get({ host: "guard.test", port: P4 }), { err: deny });
  dns.lookup = original;
  await run("restored dns.lookup", () => http.get({ host: "localhost", port: P4, family: 4, agent: new http.Agent() }));
  console.log("  called", j(called));
}

console.log("==== listeners on req.socket");
await run("veto in 'lookup' (hook)", (ev) => {
  const req = http.get({ host: "guard.test", port: P4, agent: new http.Agent(), lookup: hook(ev, to127) });
  req.on("socket", (s) => s.once("lookup", () => s.destroy(deny)));
  return req;
}, { err: deny });
await run("veto in 'lookup' (dns)", () => {
  const req = http.get({ host: "localhost", port: P4, agent: new http.Agent() });
  req.on("socket", (s) => s.once("lookup", () => s.destroy(deny)));
  return req;
}, { err: deny });
await run("veto in 'connect' (name)", () => {
  const req = http.get({ host: "localhost", port: P4, family: 4, agent: new http.Agent() });
  req.on("socket", (s) => s.once("connect", () => s.destroy(deny)));
  return req;
}, { err: deny });
await run("veto in 'connect' (literal, stock agent)", () => {
  const req = http.get({ host: "127.0.0.1", port: P4, agent: new http.Agent() });
  req.on("socket", (s) => s.once("connect", () => s.destroy(deny)));
  return req;
}, { err: deny });
await run("a 'connect' listener that lets it through", (ev) => {
  const req = http.get({ host: "127.0.0.1", port: P4, agent: new http.Agent() });
  req.on("socket", (s) => s.once("connect", () => ev.push(`connect ${s.remoteAddress}`)));
  return req;
});
await run("destroy in 'socket'", () => {
  const req = http.get({ host: "127.0.0.1", port: P4, agent: new http.Agent() });
  req.on("socket", (s) => s.destroy(deny));
  return req;
}, { err: deny });

console.log("==== the agent protocol");
class Throws extends http.Agent { createConnection() { throw deny; } }
await run("createConnection throws", () => http.get({ host: "guard.test", port: P4, agent: new Throws() }), { err: deny });
class CallsBackError extends http.Agent { createConnection(o, cb) { cb(deny); } }
await run("createConnection cb(err)", () => http.get({ host: "guard.test", port: P4, agent: new CallsBackError() }), { err: deny });
class CallsBackLater extends http.Agent {
  createConnection(o, cb) { setTimeout(() => cb(null, net.connect(o.port, "127.0.0.1")), 5); }
}
await run("createConnection calls back later", () => http.get({ host: "guard.test", port: P4, agent: new CallsBackLater() }));
class Elsewhere extends http.Agent { createConnection(o) { return net.connect(o.port, "127.0.0.1"); } }
await run("createConnection ignores the host", () => http.get({ host: "guard.test", port: P4, agent: new Elsewhere() }));
await run("createConnection option, no agent", () =>
  http.get({ host: "guard.test", port: P4, createConnection: (o) => net.connect(o.port, "127.0.0.1") }));
{
  const keys = [];
  const duck = { addRequest(req, opts) { keys.push(...Object.keys(opts).sort()); req.onSocket(net.connect(opts.port, "127.0.0.1")); } };
  await run("duck agent", () => http.get({ host: "guard.test", port: P4, agent: duck }));
  console.log("  duck addRequest keys", j(keys));
}
await run("agent {}", () => http.get({ host: "guard.test", port: P4, agent: {} }));
{
  function A5(options) { http.Agent.call(this, options); }
  Object.setPrototypeOf(A5.prototype, http.Agent.prototype);
  Object.setPrototypeOf(A5, http.Agent);
  await run("ES5 subclass", () => http.get({ host: "127.0.0.1", port: P4, agent: new A5() }));
}
console.log("globalAgent is an Agent", http.globalAgent instanceof http.Agent, "keepAlive", http.globalAgent.keepAlive);
console.log("https.globalAgent is an https.Agent", https.globalAgent instanceof https.Agent, https.globalAgent instanceof http.Agent, https.Agent === http.Agent);
console.log("Agent.prototype", j(Object.getOwnPropertyNames(http.Agent.prototype).sort()));
{
  const calls = [];
  const original = http.globalAgent.createConnection;
  http.globalAgent.createConnection = function (options, cb) {
    calls.push(options.host);
    return original.call(this, options, cb);
  };
  await run("patched globalAgent.createConnection", () => http.get({ host: "127.0.0.1", port: P4 }));
  delete http.globalAgent.createConnection;
  console.log("  calls", j(calls));
}

console.log("==== guard replicas");
// The private-address check, reduced to the loopback ranges used here.
const isPrivate = (ip) => ip === "::1" || String(ip).startsWith("127.");
// request-filtering-agent 3.2.1: a lookup wrapper, and an error socket for a
// private literal.
class LookupWrapAgent extends http.Agent {
  constructor(options = {}) { super(options); this.allow = options.allow === true; }
  createConnection(options, connectionListener) {
    if (!this.allow && net.isIP(options.host) && isPrivate(options.host)) {
      const socket = new net.Socket();
      connectionListener(new Error(`refused literal ${options.host}`), socket);
      return socket;
    }
    const lookup = options.lookup || dns.lookup;
    const allow = this.allow;
    return super.createConnection({
      ...options,
      lookup: (host, opts, cb) => lookup(host, opts, (err, addresses, family) => {
        if (err) return cb(err);
        const list = opts.all ? addresses : [{ address: addresses, family }];
        for (const { address } of list) {
          if (!allow && isPrivate(address)) return cb(new Error(`refused lookup ${host} ${address}`));
        }
        return opts.all ? cb(null, addresses) : cb(null, addresses, family);
      }),
    }, connectionListener);
  }
}
// ssrf-req-filter 1.1.1: an instance patch that throws for a private literal
// and destroys the socket on a private 'lookup'.
function eventGuard(agent) {
  const createConnection = agent.createConnection;
  agent.createConnection = function (options, fn) {
    if (net.isIP(options.host) && isPrivate(options.host)) throw new Error(`blocked literal ${options.host}`);
    const socket = createConnection.call(this, options, fn);
    socket.on("lookup", (err, address) => {
      if (!err && isPrivate(address)) socket.destroy(new Error(`blocked lookup ${address}`));
    });
    return socket;
  };
  return agent;
}
// request-filtering-agent 1.1.2: the literal checked when the socket
// connects, a name on its 'lookup'.
class ConnectCheckAgent extends http.Agent {
  createConnection(options, connectionListener) {
    const socket = super.createConnection(options, connectionListener);
    socket.on("lookup", (err, address) => {
      if (!err && isPrivate(address)) socket.destroy(new Error(`refused lookup ${address}`));
    });
    socket.once("connect", () => {
      if (net.isIP(options.host) && isPrivate(options.host)) socket.destroy(new Error(`refused on connect ${options.host}`));
    });
    return socket;
  }
}
const guards = [
  ["lookup wrapper", () => new LookupWrapAgent()],
  ["lookup event", () => eventGuard(new http.Agent())],
  ["connect check", () => new ConnectCheckAgent()],
];
const targets = [
  ["127.0.0.1", (ev) => ({ host: "127.0.0.1", port: P4 })],
  ["[::1]", (ev) => ({ host: "::1", port: P6 })],
  ["guard.test via async lookup", (ev) => ({ host: "guard.test", port: P4, lookup: hook(ev, to127) })],
];
for (const [gname, make] of guards) {
  for (const [tname, target] of targets) {
    await run(`${gname} -> ${tname}`, (ev) => http.get({ ...target(ev), agent: make() }));
  }
}
await run("lookup wrapper, allowed -> guard.test", (ev) =>
  http.get({ host: "guard.test", port: P4, lookup: hook(ev, to127), agent: new LookupWrapAgent({ allow: true }) }));

console.log("==== https");
await run("https lookup, verified against ca", (ev) =>
  https.get({ host: "localhost", port: T4, ca: CA, lookup: hook(ev, to127), agent: new https.Agent() }), { tls: true });
await run("https lookup deny", (ev) =>
  https.get({ host: "localhost", port: T4, ca: CA, lookup: hook(ev, refuse), agent: new https.Agent() }), { err: deny });
await run("https rejectUnauthorized:false", () =>
  https.get({ host: "127.0.0.1", port: T4, rejectUnauthorized: false, agent: new https.Agent() }));
await run("https rejectUnauthorized:false, agent lookup deny", (ev) =>
  https.get({ host: "localhost", port: T4, rejectUnauthorized: false, agent: new https.Agent({ lookup: hook(ev, refuse) }) }), { err: deny });
await run("https veto in 'connect'", () => {
  const req = https.get({ host: "127.0.0.1", port: T4, rejectUnauthorized: false, agent: new https.Agent() });
  req.on("socket", (s) => s.once("connect", () => s.destroy(deny)));
  return req;
}, { err: deny });
await run("https agent with a patched createConnection", (ev) => {
  const agent = new https.Agent({ rejectUnauthorized: false });
  const original = agent.createConnection;
  agent.createConnection = function (options, cb) {
    ev.push(`createConnection ${options.host} servername=${j(options.servername)}`);
    return original.call(this, options, cb);
  };
  return https.get({ host: "127.0.0.1", port: T4, agent });
});

console.log("==== redirects are not followed");
{
  // A 3xx is the response: node's http client never follows it, so a hop the
  // caller never named is never dialled -- with or without a guard that the
  // literal first host never needed.
  const redirector = http.createServer((req, res) => {
    res.writeHead(302, { location: `http://localhost:${P4}/internal` });
    res.end();
  });
  await listen(redirector, "127.0.0.1");
  const R = redirector.address().port;
  ports.push(R);
  await run("302, no guard", () => http.get({ host: "127.0.0.1", port: R, agent: new http.Agent() }));
  await run("302, request lookup", (ev) =>
    http.get({ host: "127.0.0.1", port: R, agent: new http.Agent(), lookup: hook(ev, refuse) }));
  await run("302, default agent", () => http.get(`http://127.0.0.1:${R}/start`));
  redirector.close();
}

console.log("==== listeners attached later");
await run("veto in 'lookup' after an await in 'socket'", () => {
  const req = http.get({ host: "localhost", port: P4, agent: new http.Agent() });
  req.on("socket", async (s) => {
    await null;
    await null;
    s.once("lookup", () => s.destroy(deny));
  });
  return req;
}, { err: deny });

console.log("==== a wrapped socket layer");
{
  const calls = [];
  const original = net.Socket.prototype.connect;
  net.Socket.prototype.connect = function (...args) {
    calls.push("connect");
    return original.apply(this, args);
  };
  await run("net.Socket.prototype.connect wrapped", () =>
    http.get({ host: "127.0.0.1", port: P4, agent: new http.Agent() }));
  net.Socket.prototype.connect = original;
  console.log("  calls", j(calls));
}
{
  const calls = [];
  const original = net.createConnection;
  net.createConnection = function (...args) {
    calls.push(`createConnection ${args[0].host}`);
    return original.apply(this, args);
  };
  await run("net.createConnection wrapped", () =>
    http.get({ host: "127.0.0.1", port: P4, agent: new http.Agent() }));
  net.createConnection = original;
  console.log("  calls", j(calls));
}
{
  const calls = [];
  const original = tls.connect;
  tls.connect = function (...args) {
    calls.push(`tls.connect ${args[0].host}`);
    throw deny;
  };
  await run("tls.connect wrapped, refusing", () =>
    https.get({ host: "localhost", port: T4, ca: CA, agent: new https.Agent() }), { err: deny });
  tls.connect = original;
  console.log("  calls", j(calls));
}
{
  const calls = [];
  const original = tls.TLSSocket.prototype.connect;
  tls.TLSSocket.prototype.connect = function (...args) {
    calls.push(typeof args[0] === "object" ? `TLSSocket connect ${args[0].host}` : "TLSSocket connect");
    return original.apply(this, args);
  };
  await run("TLSSocket.prototype.connect wrapped", () =>
    https.get({ host: "localhost", port: T4, ca: CA, agent: new https.Agent() }));
  tls.TLSSocket.prototype.connect = original;
  console.log("  calls", j(calls));
}

console.log("==== TLS identity");
const pinMismatch = Object.assign(new Error("pin mismatch"), { code: "EPIN" });
await run("https checkServerIdentity refuses", (ev) =>
  https.get({
    host: "localhost",
    port: T4,
    ca: CA,
    agent: new https.Agent(),
    checkServerIdentity: (host, cert) => {
      ev.push(`checkServerIdentity(${j(host)}, ${cert.subject.CN})`);
      return pinMismatch;
    },
  }), { err: pinMismatch });
await run("https checkServerIdentity accepts", (ev) =>
  https.get({
    host: "localhost",
    port: T4,
    ca: CA,
    agent: new https.Agent(),
    checkServerIdentity: (host, cert) => {
      ev.push(`checkServerIdentity(${j(host)}, ${cert.subject.CN})`);
      return undefined;
    },
  }), { tls: true });
await run("https servername the certificate does not name", () =>
  https.get({ host: "localhost", port: T4, ca: CA, servername: "other.example", agent: new https.Agent() }));
{
  const events = [];
  const outcome = await new Promise((resolve) => {
    const s = tls.connect({
      host: "localhost",
      port: T4,
      ca: CA,
      checkServerIdentity: (host) => {
        events.push(`checkServerIdentity(${j(host)})`);
        return pinMismatch;
      },
    });
    s.on("secureConnect", () => resolve("secureConnect"));
    s.on("error", (e) => resolve(`error same=${e === pinMismatch} authorized=${s.authorized} authorizationError=${s.authorizationError}`));
  });
  console.log("tls.connect checkServerIdentity refuses", outcome, j(events));
}

// A host node's resolver refuses is refused on oam too: the URL parser
// would rewrite these spellings to an address (percent-escapes, octal and
// zero-padded IPv4, a trailing dot, a tab, a space, a port, fullwidth digits
// -- which node refuses at the Host header), and a request must not reach an
// address node never dials. What each resolves to is the platform's
// getaddrinfo's answer on both runtimes; the server is not reached unless it
// resolves.
for (const host of [
  "%31%32%37.0.0.1", "127.0.0.1.", "0177.0.0.1", "127.000.000.001", "127.0.0.1	", " 127.0.0.1",
  "127.0.0.1:80", "%6c%6f%63%61%6c%68%6f%73%74", "１２７.0.0.1", "[::1]", "LocalHost",
]) {
  const before = hits;
  const outcome = await new Promise((resolve) => {
    let req;
    try {
      req = http.get({ host, port: P4, path: "/spelling" }, (res) => {
        res.resume();
        res.on("end", () => resolve(`RESPONSE ${res.statusCode}`));
      });
    } catch (e) {
      resolve(`THROW ${e.code} ${e.message}`);
      return;
    }
    req.on("error", (e) => resolve(`ERROR ${e.code}`));
  });
  console.log(`host ${j(host)}: ${outcome} | reached=${hits > before}`);
}

srv4.close();
srv6.close();
tsrv4.close();
