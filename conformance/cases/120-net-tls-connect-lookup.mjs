// net.connect and tls.connect honour the `lookup` connect option and emit
// the socket's 'lookup' event, as node's lookupAndConnect does (measured on
// node v22.22.2):
//  - the hook is called synchronously inside connect() with (host, {family,
//    hints[, all]}, cb); `all: true` unless a family, a localAddress or
//    autoSelectFamily:false asks for one address;
//  - an IP-literal host is never looked up; no host is 'localhost';
//  - a refusal fails the socket with the hook's own error object, a hook that
//    throws makes connect() throw, and a non-function `lookup` throws for a
//    name (and is ignored for a literal);
//  - every answered address is emitted as 'lookup' before anything is
//    dialled, and a 'lookup' listener that destroys the socket stops the
//    connect -- nothing reaches the server;
//  - an empty, unusable or never-arriving answer never falls back to system
//    DNS;
//  - a replaced dns.lookup is what connect resolves through.
// oam used to ignore the option entirely and resolve through getaddrinfo, so a
// lookup that refused a host was skipped. The default resolver's answers
// depend on the host's resolver configuration (AI_ADDRCONFIG off Windows), so
// those cases print only that 'lookup' fired and how the connect ended.
// Server ports are redacted; `connectionAttempt` events (not emitted by oam)
// are not recorded, and neither is where a hook's callback returns relative to
// a veto's 'error' (oam's net.Socket emits 'error' and 'close' from destroy()
// synchronously, node on the next tick).
import dns from "node:dns";
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
let accepted = 0;
const ports = [];
const P = (s) => {
  let out = String(s);
  for (const p of ports) out = out.replaceAll(String(p), "PORT");
  return out;
};
function desc(e) {
  if (!e) return "none";
  const base = `${e.constructor && e.constructor.name} code=${e.code} msg=${j(P(e.message))} keys=${j(Object.keys(e))}`;
  if (e.errors) return `${base} errors=${j(e.errors.map((x) => `${x.code}@${x.address}`))}`;
  return base;
}
const listen = (server) => new Promise((r) => server.listen(0, "127.0.0.1", r));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const srv = net.createServer((c) => {
  accepted++;
  c.on("error", () => {});
  c.end();
});
await listen(srv);
const port = srv.address().port;
const tsrv = tls.createServer({ key: LEAF_KEY, cert: LEAF }, (c) => {
  accepted++;
  c.on("error", () => {});
  c.end();
});
tsrv.on("tlsClientError", () => {});
await listen(tsrv);
const tport = tsrv.address().port;
const probe = net.createServer();
await listen(probe);
const closed = probe.address().port;
await new Promise((r) => probe.close(r));
ports.push(port, tport, closed);

// A lookup hook that records how it was called and answers as `answer` says.
function mkLookup(events, answer, mode, logCb) {
  return function lookup(host, opts, cb) {
    events.push(`CALL lookup(${j(host)}, ${j(opts)}, ${typeof cb}) argc=${arguments.length} keys=${j(Object.keys(opts || {}))}`);
    const a = answer(host, opts);
    const fire = () => {
      if (a.throwSync) throw a.throwSync;
      try {
        if (a.err) cb(a.err);
        else if (opts && opts.all) cb(null, a.all);
        else cb(null, a.one[0], a.one[1]);
        if (logCb) events.push("cb returned");
      } catch (e) {
        events.push(`cb THREW ${desc(e)}`);
      }
    };
    if (mode === "sync") fire();
    else if (mode !== "never") setTimeout(fire, 5);
  };
}

async function run(label, api, opts, extra = {}) {
  const events = [];
  const before = accepted;
  // Default-resolver answers vary by host: record only that 'lookup' fired.
  const system = !extra.lookup && opts.lookup == null;
  if (extra.lookup) opts.lookup = mkLookup(events, extra.lookup, extra.mode, !extra.destroyOnLookup);
  let s;
  try {
    s = api === "tls" ? tls.connect(opts) : net.connect(opts);
  } catch (e) {
    console.log(label, "THROW", desc(e), "| same", e === extra.err, "| events", j(events));
    return;
  }
  events.push("returned");
  let outcome;
  const settled = new Promise((resolve) => {
    const done = (what) => {
      if (outcome === undefined) {
        outcome = what;
        resolve();
      }
    };
    s.on("lookup", (...a) => {
      const text = system
        ? "EV lookup"
        : `EV lookup(${a.map((x) => (x instanceof Error ? `Error:${x.code}` : j(x))).join(", ")}) connecting=${s.connecting} remote=${s.remoteAddress}`;
      if (events[events.length - 1] !== text) events.push(text);
      if (extra.destroyOnLookup) s.destroy(Object.assign(new Error("guard refused"), { code: "EGUARD" }));
    });
    s.on("connect", () => events.push(`EV connect remote=${s.remoteAddress}/${s.remoteFamily} local=${s.localAddress}`));
    s.on(api === "tls" ? "secureConnect" : "connect", () => done(api === "tls" ? `CONNECTED authorized=${s.authorized}` : "CONNECTED"));
    s.on("error", (e) => {
      events.push(`EV error same=${e === extra.err}`);
      done(`ERROR ${desc(e)}`);
    });
    s.on("close", () => events.push("EV close"));
    if (extra.pendingAfter) {
      setTimeout(() => done(`PENDING connecting=${s.connecting} destroyed=${s.destroyed}`), extra.pendingAfter);
    }
  });
  await settled;
  await sleep(50);
  console.log(label, outcome, "| accepted+", accepted - before, "| events", j(events));
  s.destroy();
  await sleep(10);
}

const deny = Object.assign(new Error("blocked"), { code: "EBLOCKED" });
const ok4 = () => ({ all: [{ address: "127.0.0.1", family: 4 }], one: ["127.0.0.1", 4] });
const two = () => ({ all: [{ address: "::1", family: 6 }, { address: "127.0.0.1", family: 4 }] });

for (const api of ["net", "tls"]) {
  const Pt = api === "tls" ? tport : port;
  const t = api === "tls" ? { rejectUnauthorized: false } : {};
  const g = (more) => ({ host: "guard.test", port: Pt, ...t, ...more });
  console.log(`==== ${api}`);
  await run(`${api} hook ok async`, api, g(), { lookup: ok4 });
  await run(`${api} hook ok sync`, api, g(), { lookup: ok4, mode: "sync" });
  await run(`${api} hook deny async`, api, g(), { lookup: () => ({ err: deny }), err: deny });
  await run(`${api} hook deny sync`, api, g(), { lookup: () => ({ err: deny }), mode: "sync", err: deny });
  await run(`${api} hook throws sync`, api, g(), { lookup: () => ({ throwSync: deny }), mode: "sync", err: deny });
  await run(`${api} hook empty answer`, api, g(), { lookup: () => ({ all: [] }), pendingAfter: 200 });
  await run(`${api} hook bad ip`, api, g(), { lookup: () => ({ all: [{ address: "nope", family: 4 }] }) });
  await run(`${api} hook bad family`, api, g(), { lookup: () => ({ all: [{ address: "127.0.0.1", family: 5 }] }) });
  await run(`${api} hook two addrs`, api, g(), { lookup: two });
  await run(`${api} hook mixed bad`, api, g(), { lookup: () => ({ all: [{ address: "x", family: 4 }, { address: "127.0.0.1", family: 4 }] }) });
  await run(`${api} hook string answer`, api, g(), { lookup: () => ({ all: "127.0.0.1" }) });
  await run(`${api} single bad ip`, api, g({ family: 4 }), { lookup: () => ({ one: ["nope", 4] }) });
  await run(`${api} single bad family`, api, g({ family: 4 }), { lookup: () => ({ one: ["127.0.0.1", 7] }) });
  await run(`${api} family:4`, api, g({ family: 4 }), { lookup: ok4 });
  await run(`${api} family:6`, api, g({ family: 6 }), { lookup: ok4 });
  await run(`${api} family:IPv4`, api, g({ family: "IPv4" }), { lookup: ok4 });
  await run(`${api} autoSelectFamily:false`, api, g({ autoSelectFamily: false }), { lookup: ok4 });
  await run(`${api} localAddress`, api, g({ localAddress: "127.0.0.1" }), { lookup: ok4 });
  await run(`${api} hints:32`, api, g({ hints: 32 }), { lookup: ok4 });
  await run(`${api} IP literal`, api, { host: "127.0.0.1", port: Pt, ...t }, { lookup: ok4 });
  await run(`${api} no host`, api, { port: Pt, ...t }, { lookup: ok4 });
  await run(`${api} lookup not a function`, api, g({ lookup: "nope" }));
  await run(`${api} lookup not a function, IP host`, api, { host: "127.0.0.1", port: Pt, lookup: "nope", ...t });
  await run(`${api} autoSelectFamily not a boolean`, api, g({ autoSelectFamily: 1 }), { lookup: ok4 });
  await run(`${api} lookup null`, api, { host: "localhost", port: Pt, lookup: null, ...t });
  await run(`${api} veto in 'lookup' (hook)`, api, g(), { lookup: ok4, destroyOnLookup: true });
  await run(`${api} veto in 'lookup' (dns)`, api, { host: "localhost", port: Pt, ...t }, { destroyOnLookup: true });
  await run(`${api} default dns`, api, { host: "localhost", port: Pt, ...t });
  await run(`${api} hook never answers`, api, g(), { lookup: ok4, mode: "never", pendingAfter: 200 });
  await run(`${api} hook ok, closed port`, api, g({ port: closed }), { lookup: ok4 });
  await run(`${api} hook two, closed port`, api, g({ port: closed }), { lookup: two });

  // destroy() right after connect() to a literal: nothing is dialled.
  {
    const before = accepted;
    const s = api === "tls"
      ? tls.connect({ host: "127.0.0.1", port: Pt, ...t })
      : net.connect({ host: "127.0.0.1", port: Pt });
    s.on("error", () => {});
    s.destroy();
    await sleep(150);
    console.log(`${api} destroy right after connect(literal) | accepted+`, accepted - before);
  }

  // A replaced dns.lookup is what connect resolves through; the original,
  // put back, is oam's resolver again.
  {
    const original = dns.lookup;
    const calls = [];
    dns.lookup = function (host, opts, cb) {
      calls.push(`${host} ${j(opts)}`);
      cb(deny);
    };
    await run(`${api} replaced dns.lookup`, api, g(), { err: deny });
    dns.lookup = original;
    await run(`${api} restored dns.lookup`, api, { host: "localhost", port: Pt, ...t });
    console.log(`${api} replaced dns.lookup calls`, j(calls));
  }
}

// The server name comes from servername || host, never from the hook's
// answer: a verifying connect to 'localhost' through a hook succeeds.
await run("tls verify, hook, host=localhost", "tls", { host: "localhost", port: tport, ca: CA }, { lookup: ok4 });

srv.close();
tsrv.close();
