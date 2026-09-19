// http2.connect(authority, options) runs its session over one socket, as
// node does: options.createConnection(authority, options) when that is a
// function, else net.connect for http: and tls.connect (offering h2 by ALPN)
// for https:. So the `lookup` option, a replaced dns.lookup, the socket's
// 'lookup' event, `family`, `ca`, `servername`, `checkServerIdentity` and
// `rejectUnauthorized` apply to it as they do to that socket, a refusal
// fails the session with its own error and cancels the pending streams
// naming it, and session.socket reports the real addresses. Up to 0.16.2
// the options were ignored and every stream went out on oam's shared
// transport. (The servers print no :authority: oam's http2 server does not
// report it yet.) Measured on node v22.22.2.
import dns from "node:dns";
import http2 from "node:http2";
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

const log = (...a) => console.log(...a);
const redact = (s) => String(s).replace(/:\d{4,5}\b/g, ":P");
const errShape = (e) =>
  e ? `${e.constructor.name} code=${e.code} msg=${redact(e.message)}` : String(e);
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

// An h2c server that answers every stream with its tag and path.
function h2cServer(tag) {
  const state = { hits: 0, port: 0, server: http2.createServer() };
  state.server.on("stream", (stream, headers) => {
    state.hits++;
    stream.respond({ ":status": 200 });
    stream.end(tag + " " + headers[":path"]);
  });
  return new Promise((r) =>
    state.server.listen(0, "127.0.0.1", () => {
      state.port = state.server.address().port;
      r(state);
    }),
  );
}

// TLS offering h2 by ALPN in front of an h2c server: an https: origin.
function tlsFront(upstreamPort) {
  const server = tls.createServer({ key: LEAF_KEY, cert: LEAF, ALPNProtocols: ["h2"] }, (s) => {
    s.on("error", () => {});
    const up = net.connect(upstreamPort, "127.0.0.1");
    up.on("error", () => s.destroy());
    s.pipe(up);
    up.pipe(s);
    s.on("close", () => up.destroy());
  });
  return new Promise((r) => server.listen(0, "127.0.0.1", () => r({ server, port: server.address().port })));
}

// One request on `session`; logs the answer or the stream's error.
function once(session, path, label) {
  return new Promise((resolve) => {
    const stream = session.request({ ":path": path });
    let body = "";
    stream.setEncoding("utf8");
    stream.on("response", (h) => log(label, "status", h[":status"]));
    stream.on("data", (d) => (body += d));
    stream.on("end", () => log(label, "body", body));
    stream.on("error", (e) =>
      log(label, "stream error", errShape(e), "cause=" + (e.cause ? errShape(e.cause) : "-")),
    );
    stream.on("close", () => {
      session.close();
      resolve();
    });
  });
}

const A = await h2cServer("A");
const B = await h2cServer("B");
const front = await tlsFront(A.port);

// 1. The lookup option resolves the name the session dials, with node's
// arguments; the socket's 'lookup' event and addresses follow.
{
  const calls = [];
  const s = http2.connect(`http://guard.test:${A.port}`, {
    lookup(host, opts, cb) {
      calls.push([host, JSON.stringify(opts)]);
      setTimeout(() => cb(null, [{ address: "127.0.0.1", family: 4 }]), 5);
    },
  });
  log("1 lookup calls at connect()", JSON.stringify(calls));
  s.socket.on("lookup", (err, address, family, host) => log("1 socket lookup", err, address, family, host));
  s.on("connect", (session, socket) =>
    log(
      "1 connect",
      session === s,
      "remote=" + socket.remoteAddress,
      "family=" + socket.remoteFamily,
      "session.socket.remote=" + s.socket.remoteAddress,
      "remotePort ok=" + (s.socket.remotePort === A.port),
      "alpn=" + s.alpnProtocol,
      "encrypted=" + s.encrypted,
    ),
  );
  await once(s, "/one", "1");
}

// 2. A lookup that refuses fails the session with its own error and cancels
// the pending stream naming it; nothing reaches the server.
{
  const before = A.hits;
  const denial = Object.assign(new Error("blocked by guard"), { code: "EBLOCKED" });
  const s = http2.connect(`http://guard.test:${A.port}`, {
    lookup(host, opts, cb) {
      setTimeout(() => cb(denial), 5);
    },
  });
  s.on("error", (e) => log("2 session error", errShape(e), "same=" + (e === denial)));
  s.on("close", () => log("2 session close"));
  s.on("connect", () => log("2 connect"));
  const st = s.request({ ":path": "/two" });
  st.on("error", (e) => log("2 stream error", errShape(e), "cause same=" + (e.cause === denial)));
  st.on("close", () => log("2 stream close rstCode=" + st.rstCode));
  await wait(300);
  log("2 server hits", A.hits - before, "destroyed=" + s.destroyed);
}

// 3. A lookup that refuses synchronously, and one that throws.
{
  const denial = Object.assign(new Error("sync blocked"), { code: "EBLOCKED" });
  const s = http2.connect(`http://guard.test:${A.port}`, {
    lookup(host, opts, cb) {
      cb(denial);
    },
  });
  log("3 connect returned");
  s.on("error", (e) => log("3 session error", errShape(e), "same=" + (e === denial)));
  await wait(100);
  const boom = new Error("hook threw");
  try {
    http2.connect(`http://guard.test:${A.port}`, {
      lookup() {
        throw boom;
      },
    });
    log("3 throwing lookup: connect returned");
  } catch (e) {
    log("3 throwing lookup: connect threw", errShape(e), "same=" + (e === boom));
  }
  try {
    http2.connect(`http://guard.test:${A.port}`, { lookup: "nope" });
    log("3 lookup not a function: connect returned");
  } catch (e) {
    log("3 lookup not a function: connect threw", errShape(e));
  }
}

// 4. An IP literal is not looked up; a lookup answering a non-IP fails the
// session; `family` reaches the lookup's options.
{
  let called = 0;
  const s = http2.connect(`http://127.0.0.1:${A.port}`, {
    lookup(h, o, cb) {
      called++;
      cb(null, [{ address: "127.0.0.1", family: 4 }]);
    },
  });
  await once(s, "/four", "4");
  log("4 lookup calls for an IP", called);
  const bad = http2.connect(`http://guard.test:${A.port}`, {
    lookup(h, o, cb) {
      setTimeout(() => cb(null, [{ address: "nope", family: 4 }]), 1);
    },
  });
  bad.on("error", (e) => log("4 bad address", errShape(e)));
  await wait(150);
  const fam = http2.connect(`http://guard.test:${A.port}`, {
    family: 4,
    lookup(h, o, cb) {
      log("4 family lookup", h, JSON.stringify(o));
      cb(null, "127.0.0.1", 4);
    },
  });
  await once(fam, "/four-family", "4");
}

// 5. A replaced dns.lookup is what the session's socket resolves with.
{
  const orig = dns.lookup;
  const seen = [];
  dns.lookup = function (h, o, cb) {
    seen.push(h);
    return orig.call(this, h, o, cb);
  };
  const s = http2.connect(`http://localhost:${A.port}`);
  s.on("error", (e) => log("5 session error", errShape(e)));
  await once(s, "/five", "5");
  dns.lookup = orig;
  log("5 replaced dns.lookup saw", JSON.stringify(seen));
}

// 6. createConnection gets the authority URL and the options, and the socket
// it returns carries the session wherever it goes (here: server B); lookup
// is then not called by http2.
{
  let args;
  let lookups = 0;
  const opts = {
    createConnection(authority, options) {
      args = {
        isURL: authority instanceof URL,
        href: redact(authority.href),
        sameOptions: options === opts,
        keys: Object.keys(options).join(","),
      };
      return net.connect(B.port, "127.0.0.1");
    },
    lookup(h, o, cb) {
      lookups++;
      cb(null, [{ address: "127.0.0.1", family: 4 }]);
    },
    custom: 1,
  };
  const beforeA = A.hits;
  const beforeB = B.hits;
  const s = http2.connect(`http://a.test:${A.port}`, opts);
  log("6 createConnection args", JSON.stringify(args));
  s.on("connect", (session, socket) =>
    log("6 connect", "to B=" + (socket.remotePort === B.port), "session.socket to B=" + (s.socket.remotePort === B.port)),
  );
  await once(s, "/six", "6");
  log("6 hits A", A.hits - beforeA, "B", B.hits - beforeB, "lookups", lookups);
}

// 7. createConnection that throws throws from connect(); a socket it returns
// that a guard destroys on its 'lookup' event fails the session with that
// error (the request-filtering-agent shape).
{
  const boom = new Error("refused by createConnection");
  try {
    http2.connect(`http://a.test:${A.port}`, {
      createConnection() {
        throw boom;
      },
    });
    log("7 connect returned");
  } catch (e) {
    log("7 connect threw", errShape(e), "same=" + (e === boom));
  }
  const before = A.hits;
  const denial = Object.assign(new Error("destination refused"), { code: "EBLOCKED" });
  const s = http2.connect(`http://a.test:${A.port}`, {
    createConnection() {
      const sock = net.connect(A.port, "localhost");
      sock.on("lookup", () => sock.destroy(denial));
      return sock;
    },
  });
  s.on("error", (e) => log("7 session error", errShape(e), "same=" + (e === denial)));
  s.on("close", () => log("7 session close"));
  const st = s.request({ ":path": "/seven" });
  st.on("error", (e) => log("7 stream error", errShape(e), "cause same=" + (e.cause === denial)));
  await wait(300);
  log("7 server hits", A.hits - before);
}

// 8. A socket already connected when createConnection returns it.
{
  const pre = net.connect(A.port, "127.0.0.1");
  await new Promise((r) => pre.once("connect", r));
  const s = http2.connect(`http://a.test:${A.port}`, {
    createConnection() {
      return pre;
    },
  });
  s.on("connect", () => log("8 connect"));
  await once(s, "/eight", "8");
  try {
    http2.connect(`http://a.test:${A.port}`, {
      createConnection() {
        return pre;
      },
    });
    log("8 second session on the socket: returned");
  } catch (e) {
    log("8 second session on the socket:", errShape(e));
  }
}

// 9. https: the session's socket is tls.connect's, with `ca`, `servername`,
// `checkServerIdentity`, `rejectUnauthorized` and `lookup` applied to it.
{
  const s = http2.connect(`https://localhost:${front.port}`, { ca: CA });
  s.on("connect", () =>
    log("9 verified connect", "encrypted=" + s.encrypted, "authorized=" + s.socket.authorized, "remote=" + s.socket.remoteAddress),
  );
  await once(s, "/nine", "9");

  const named = http2.connect(`https://guard.test:${front.port}`, {
    ca: CA,
    servername: "localhost",
    lookup(h, o, cb) {
      log("9 tls lookup", h);
      cb(null, [{ address: "127.0.0.1", family: 4 }]);
    },
  });
  await once(named, "/nine-named", "9 named");

  const denial = new Error("tls lookup refused");
  const refused = http2.connect(`https://guard.test:${front.port}`, {
    ca: CA,
    lookup(h, o, cb) {
      setTimeout(() => cb(denial), 1);
    },
  });
  refused.on("error", (e) => log("9 refused session error", errShape(e), "same=" + (e === denial)));
  await once(refused, "/nine-refused", "9 refused");

  const untrusted = http2.connect(`https://localhost:${front.port}`);
  untrusted.on("error", (e) => log("9 untrusted session error", errShape(e)));
  await once(untrusted, "/nine-untrusted", "9 untrusted");

  const pinned = new Error("pin mismatch");
  let identityArgs;
  const pin = http2.connect(`https://localhost:${front.port}`, {
    ca: CA,
    checkServerIdentity(host, cert) {
      identityArgs = host + " " + typeof cert.subject;
      return pinned;
    },
  });
  pin.on("error", (e) => log("9 pin session error", errShape(e), "same=" + (e === pinned)));
  await once(pin, "/nine-pin", "9 pin");
  log("9 checkServerIdentity args", identityArgs);

  const insecure = http2.connect(`https://localhost:${front.port}`, { rejectUnauthorized: false });
  insecure.on("connect", () => log("9 insecure connect authorized=" + insecure.socket.authorized));
  await once(insecure, "/nine-insecure", "9 insecure");
}

// 10. createConnection returning tls.connect over a socket it opened itself
// (a tunnel's shape) carries the session.
{
  const s = http2.connect(`https://localhost:${front.port}`, {
    createConnection(authority) {
      const raw = net.connect(front.port, "127.0.0.1");
      return tls.connect({ socket: raw, servername: authority.hostname, ca: CA, ALPNProtocols: ["h2"] });
    },
  });
  s.on("connect", () => log("10 connect authorized=" + s.socket.authorized, "encrypted=" + s.encrypted));
  await once(s, "/ten", "10");
}

// 11. What the ClientHello offers by ALPN: h2 for an https: session (and
// http/1.1 too with allowHTTP1), and ALPNProtocols over a wrapped socket.
{
  // The ALPN extension (16) of the first ClientHello a connection sends.
  const alpnOffer = (hello) => {
    let at = 5 + 4 + 2 + 32;
    at += 1 + hello[at];
    at += 2 + hello.readUInt16BE(at);
    at += 1 + hello[at];
    const end = at + 2 + hello.readUInt16BE(at);
    at += 2;
    while (at + 4 <= end) {
      const type = hello.readUInt16BE(at);
      const len = hello.readUInt16BE(at + 2);
      if (type === 16) {
        const names = [];
        for (let i = at + 6; i < at + 4 + len; i += 1 + hello[i]) {
          names.push(hello.toString("latin1", i + 1, i + 1 + hello[i]));
        }
        return names;
      }
      at += 4 + len;
    }
    return null;
  };
  const hellos = [];
  const sniffer = net.createServer((c) => {
    let buf = Buffer.alloc(0);
    c.on("data", (d) => {
      buf = Buffer.concat([buf, d]);
      if (buf.length >= 5 && buf.length >= 5 + buf.readUInt16BE(3)) {
        hellos.push(JSON.stringify(alpnOffer(buf)));
        c.destroy();
      }
    });
    c.on("error", () => {});
  });
  await new Promise((r) => sniffer.listen(0, "127.0.0.1", r));
  const port = sniffer.address().port;
  const offer = async (label, authority, options) => {
    const s = http2.connect(authority, options);
    s.on("error", () => {});
    await new Promise((r) => s.on("close", r));
    log("11", label, hellos.shift());
  };
  await offer("https", `https://localhost:${port}`, {});
  await offer("allowHTTP1", `https://localhost:${port}`, { allowHTTP1: true });
  await offer("wrapped", `https://localhost:${port}`, {
    createConnection() {
      return tls.connect({ socket: net.connect(port, "127.0.0.1"), servername: "localhost", ALPNProtocols: ["h2", "x-test"] });
    },
  });
  sniffer.close();
}

A.server.close();
B.server.close();
front.server.close();
