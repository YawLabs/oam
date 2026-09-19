// A ClientRequest's 'finish', as node's (measured on node v22.22.2): it
// fires once the request has been written to its socket -- so after the
// socket's 'connect' / 'secureConnect' for a request sent over a socket the
// agent creates, with req.writableFinished false until then -- and never for
// a request whose socket refused or that was destroyed before it was written.
// oam used to emit it on the tick after end() on every path, before the
// socket had even connected: timing tools that measure the request phase
// from 'connect' to 'finish' (got's http-timer) got NaN. On oam's own
// transport (no agent socket) it still follows end() at once, as node's does
// for a socket that is already connected.
//
// Only the order of events is printed.
import http from "node:http";
import https from "node:https";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

// Fixtures from case 107: the localhost leaf the throwaway CA signed.
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

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const handler = (req, res) => {
  let n = 0;
  req.on("data", (d) => (n += d.length));
  req.on("end", () => res.end(`got ${n}`));
};
const listen = (server) => new Promise((r) => server.listen(0, "127.0.0.1", r));
const srv = http.createServer(handler);
await listen(srv);
const tsrv = https.createServer({ key: LEAF_KEY, cert: LEAF }, handler);
await listen(tsrv);
const P = srv.address().port;
const T = tsrv.address().port;
const closed = net.createServer();
await listen(closed);
const CLOSED = closed.address().port;
await new Promise((r) => closed.close(r));

// Agents whose createConnection is the stock one, called through a subclass
// (the guard packages' shape): the request goes over the socket it returns.
class WrappingAgent extends http.Agent {
  createConnection(options, cb) {
    return super.createConnection(options, cb);
  }
}
class WrappingHttpsAgent extends https.Agent {
  createConnection(options, cb) {
    return super.createConnection(options, cb);
  }
}
const lookup = (host, opts, cb) => {
  if (opts && opts.all) cb(null, [{ address: "127.0.0.1", family: 4 }]);
  else cb(null, "127.0.0.1", 4);
};

async function run(label, mod, options, drive, extra = {}) {
  const events = [];
  const req = mod.request(options);
  req.on("socket", (s) => {
    events.push(`socket reused=${req.reusedSocket}`);
    if (extra.watch !== false) {
      s.on("lookup", () => events.push("lookup"));
      s.on("connect", () => events.push(`connect finished=${req.writableFinished}`));
      s.on("secureConnect", () => events.push(`secureConnect finished=${req.writableFinished}`));
    }
  });
  req.on("finish", () => events.push("finish"));
  req.on("close", () => events.push("close"));
  const outcome = await new Promise((resolve) => {
    req.on("response", (res) => {
      events.push(`response finished=${req.writableFinished}`);
      let body = "";
      res.on("data", (d) => (body += d));
      res.on("end", () => {
        events.push("end");
        resolve(`${res.statusCode} ${body}`);
      });
    });
    req.on("error", (e) => {
      events.push(`error ${e.code}`);
      resolve(`ERROR ${e.code}`);
    });
    drive(req, events);
  });
  await sleep(30);
  console.log(`${label}: ${outcome} | ${events.join(", ")}`);
}

const get = (req) => req.end();
const cbEnd = (req, events) => req.end(() => events.push("end callback"));
const post = (req) => {
  req.write("abc");
  req.end("def");
};
const streamed = (req, events) => {
  req.write("abc");
  setTimeout(() => {
    events.push("end()");
    req.end("def");
  }, 100);
};
const big = (req) => req.end(Buffer.alloc(300000, 97));
// 300000 bytes in three writes 50 ms apart: the body streams (chunked) and
// ends after the socket has connected.
const bigStreamed = (req, events) => {
  let n = 0;
  const more = () => {
    req.write(Buffer.alloc(100000, 98));
    if (++n < 3) {
      setTimeout(more, 50);
      return;
    }
    setTimeout(() => {
      events.push("end()");
      req.end();
    }, 50);
  };
  more();
};

const base = { host: "127.0.0.1", port: P, path: "/" };
const tbase = { host: "127.0.0.1", port: T, path: "/", rejectUnauthorized: false };

await run("agent socket GET", http, { ...base, agent: new WrappingAgent() }, get);
await run("agent socket GET, end callback", http, { ...base, agent: new WrappingAgent() }, cbEnd);
await run("agent socket GET, lookup", http, { ...base, host: "example.test", lookup, agent: new WrappingAgent() }, get);
await run("agent socket POST", http, { ...base, method: "POST", agent: new WrappingAgent() }, post);
await run("agent socket POST, end later", http, { ...base, method: "POST", agent: new WrappingAgent() }, streamed);
await run("agent socket POST 300000", http, { ...base, method: "POST", agent: new WrappingAgent() }, big);
await run("agent socket POST 300000 streamed", http, { ...base, method: "POST", agent: new WrappingAgent() }, bigStreamed);
await run("agent socket https GET", https, { ...tbase, agent: new WrappingHttpsAgent() }, get);
await run("agent socket https POST", https, { ...tbase, method: "POST", agent: new WrappingHttpsAgent() }, post);
await run("agent socket https POST 300000 streamed", https, { ...tbase, method: "POST", agent: new WrappingHttpsAgent() }, bigStreamed);
await run("watched socket GET", http, { ...base }, get);
await run("watched socket POST (pooled)", http, { ...base, method: "POST" }, post);
await run("watched socket https GET", https, { ...tbase }, get);
await run("unwatched GET", http, { ...base, agent: new http.Agent() }, get, { watch: false });
await run("unwatched POST", http, { ...base, method: "POST", agent: new http.Agent() }, post, { watch: false });
const keep = new WrappingAgent({ keepAlive: true });
await run("keep-alive first", http, { ...base, agent: keep }, get);
await run("keep-alive reused", http, { ...base, agent: keep }, get);
await run("keep-alive reused POST", http, { ...base, method: "POST", agent: keep }, post);
keep.destroy();
await run("agent socket refused", http, { ...base, port: CLOSED, agent: new WrappingAgent() }, get);
await run("agent socket destroyed on 'socket'", http, { ...base, agent: new WrappingAgent() }, (req) => {
  req.on("socket", () => req.destroy());
  req.end();
});
await run("unwatched destroyed at once", http, { ...base, agent: new http.Agent() }, (req) => {
  req.end();
  req.destroy();
}, { watch: false });
await run("destroyed on 'socket'", http, { ...base, agent: new http.Agent() }, (req) => {
  req.on("socket", () => req.destroy());
  req.end();
}, { watch: false });

// An upgrade: 'finish' once the head is written, before 'upgrade'.
{
  const events = [];
  const up = http.createServer();
  up.on("upgrade", (req, socket) => {
    socket.end("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n");
  });
  await listen(up);
  await new Promise((resolve) => {
    const req = http.request({
      host: "127.0.0.1",
      port: up.address().port,
      headers: { Connection: "Upgrade", Upgrade: "x" },
      agent: new WrappingAgent(),
    });
    req.on("socket", (s) => {
      events.push("socket");
      s.on("connect", () => events.push(`connect finished=${req.writableFinished}`));
    });
    req.on("finish", () => events.push("finish"));
    req.on("upgrade", (res, socket) => {
      events.push(`upgrade ${res.statusCode} finished=${req.writableFinished}`);
      socket.destroy();
      resolve();
    });
    req.on("error", (e) => {
      events.push(`error ${e.code}`);
      resolve();
    });
    req.end();
  });
  up.close();
  console.log(`upgrade: ${events.join(", ")}`);
}

// got's http-timer: the request phase is 'finish' time minus 'connect' time,
// which needs 'connect' first.
{
  const t = {};
  await new Promise((resolve) => {
    const req = http.request({ ...base, agent: new WrappingAgent() });
    req.on("socket", (s) => s.once("connect", () => (t.connect = true)));
    req.prependOnceListener("finish", () => (t.uploadAfterConnect = t.connect === true));
    req.on("response", (res) => {
      res.resume();
      res.on("end", resolve);
    });
    req.end();
  });
  console.log("http-timer request phase measurable:", t.uploadAfterConnect);
}

srv.close();
tsrv.close();
