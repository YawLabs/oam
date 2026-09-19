// tls.connect({ socket }) runs TLS over a socket it did not open, as node
// does: a connected net.Socket (a CONNECT tunnel, the STARTTLS shape) with no
// 'connect' of its own and that socket's addresses; one still connecting,
// whose 'connect' it hands on; a plain JS Duplex (through node's
// JSStreamSocket, which `new tls.TLSSocket(stream)._handle._parentWrap`
// reaches); a TLSSocket (TLS in TLS: an https proxy); a certificate the
// wrapped handshake refuses; the transport closing with the TLS socket; and
// an https request through a CONNECT tunnel an agent opens by hand. Also the
// net.Socket reading those tunnels need: paused mode ('readable' + read()),
// data held once the last 'readable' listener goes, and push() into a socket
// with no handle. Up to 0.16.2 a wrapped socket failed with
// ERR_FEATURE_UNAVAILABLE_ON_PLATFORM and net.Socket had no read() or
// push(). Measured on node v22.22.2.
import https from "node:https";
import net from "node:net";
import tls from "node:tls";
import { Duplex, PassThrough } from "node:stream";

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

// A TLS server that echoes one line per connection and ends.
function echoServer() {
  const server = tls.createServer({ key: LEAF_KEY, cert: LEAF }, (s) => {
    s.on("error", () => {});
    let buf = "";
    s.on("data", (d) => {
      buf += d;
      if (buf.includes("\r\n\r\n")) s.end("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello");
      else if (buf.endsWith("\n")) s.end("echo:" + buf);
    });
  });
  return new Promise((r) => server.listen(0, "127.0.0.1", () => r({ server, port: server.address().port })));
}
// A CONNECT proxy, plain or over TLS, that tunnels to what it is asked.
function connectProxy(secure) {
  const connects = [];
  const onConn = (c) => {
    c.on("error", () => {});
    let head = "";
    const onData = (d) => {
      head += d.toString("latin1");
      const idx = head.indexOf("\r\n\r\n");
      if (idx === -1) return;
      c.removeListener("data", onData);
      const line = head.slice(0, head.indexOf("\r\n"));
      connects.push(line);
      const [host, port] = line.split(" ")[1].split(":");
      const up = net.connect(+port, host === "localhost" ? "127.0.0.1" : host, () => {
        c.write("HTTP/1.1 200 Connection established\r\n\r\n");
        c.pipe(up);
        up.pipe(c);
      });
      up.on("error", () => c.destroy());
      c.on("close", () => up.destroy());
    };
    c.on("data", onData);
  };
  const server = secure ? tls.createServer({ key: LEAF_KEY, cert: LEAF }, onConn) : net.createServer(onConn);
  return new Promise((r) => server.listen(0, "127.0.0.1", () => r({ server, connects, port: server.address().port })));
}
// Open a CONNECT tunnel over `raw` and call back once the proxy answered.
function tunnel(raw, host, port, cb) {
  raw.write(`CONNECT ${host}:${port} HTTP/1.1\r\nHost: ${host}:${port}\r\n\r\n`);
  let head = "";
  const onData = (d) => {
    head += d.toString("latin1");
    if (!head.includes("\r\n\r\n")) return;
    raw.removeListener("data", onData);
    cb(head.slice(0, head.indexOf("\r\n")));
  };
  raw.on("data", onData);
}
async function exchange(s, line) {
  s.end(line);
  let got = "";
  s.on("data", (d) => (got += d));
  await new Promise((r) => s.once("close", r));
  return got;
}

const { server: target, port: TPORT } = await echoServer();

{
  const log = [];
  const raw = net.connect(TPORT, "127.0.0.1");
  await new Promise((r) => raw.once("connect", r));
  raw.on("close", () => log.push("raw close"));
  const s = tls.connect({ socket: raw, servername: "localhost", ca: CA });
  for (const ev of ["connect", "ready", "secureConnect", "secure", "end", "finish"]) s.on(ev, () => log.push(ev));
  s.on("close", (hadError) => log.push(`close ${hadError}`));
  s.on("error", (e) => log.push(`error ${e.code}`));
  log.push(`at once: connecting=${s.connecting} encrypted=${s.encrypted} remote=${s.remoteAddress} port=${s.remotePort === TPORT} readyState=${s.readyState} handle=${typeof s._handle} parentWrap=${s._handle._parentWrap === raw}`);
  await new Promise((r) => s.once("secureConnect", r));
  log.push(`secure: authorized=${s.authorized} protocol=${s.getProtocol()} alpn=${s.alpnProtocol} local=${s.localAddress} raw.destroyed=${raw.destroyed}`);
  s.write("ping\n");
  let got = "";
  s.on("data", (d) => (got += d));
  await new Promise((r) => s.once("close", r));
  await new Promise((r) => setTimeout(r, 20));
  log.push(`got=${JSON.stringify(got)} raw.destroyed=${raw.destroyed}`);
  console.log(`over a connected net.Socket: ${log.join(" | ")}`);
}

{
  const log = [];
  const raw = net.connect(TPORT, "127.0.0.1");
  const s = tls.connect({ socket: raw, servername: "localhost", ca: CA });
  log.push(`at once: connecting=${s.connecting}`);
  for (const ev of ["connect", "secureConnect"]) s.on(ev, () => log.push(ev));
  s.on("error", (e) => log.push(`error ${e.code}`));
  await new Promise((r) => s.once("secureConnect", r));
  console.log(`over a connecting net.Socket: ${log.join(" | ")} got=${JSON.stringify(await exchange(s, "ping2\n"))}`);
}

{
  const log = [];
  const raw = net.connect(TPORT, "127.0.0.1");
  const duplex = new Duplex({
    write(chunk, enc, cb) { raw.write(chunk, cb); },
    final(cb) { raw.end(); cb(); },
    read() {},
  });
  raw.on("data", (d) => duplex.push(d));
  raw.on("end", () => duplex.push(null));
  const s = tls.connect({ socket: duplex, servername: "localhost", ca: CA });
  log.push(`wrap=${s._handle._parentWrap.constructor.name}`);
  s.on("secureConnect", () => log.push(`secureConnect authorized=${s.authorized} remote=${s.remoteAddress}`));
  s.on("error", (e) => log.push(`error ${e.code}`));
  await new Promise((r) => s.once("secureConnect", r));
  console.log(`over a JS Duplex: ${log.join(" | ")} got=${JSON.stringify(await exchange(s, "ping3\n"))}`);
}

{
  const log = [];
  const raw = net.connect(TPORT, "127.0.0.1");
  await new Promise((r) => raw.once("connect", r));
  raw.on("close", () => log.push("raw close"));
  const s = tls.connect({ socket: raw, servername: "localhost" });
  s.on("secureConnect", () => log.push("secureConnect"));
  s.on("close", () => log.push("close"));
  await new Promise((r) => s.once("error", (e) => { log.push(`error ${e.code}`); r(); }));
  await new Promise((r) => setTimeout(r, 30));
  console.log(`a refused certificate: ${log.join(" | ")} raw.destroyed=${raw.destroyed}`);
}

{
  const t = new tls.TLSSocket(new PassThrough());
  const JSStreamSocket = t._handle._parentWrap.constructor;
  console.log(`JSStreamSocket: ${JSStreamSocket.name} ${new JSStreamSocket(new PassThrough()) instanceof net.Socket}`);
}

// TLS in TLS: an https proxy, then TLS to the target inside that tunnel.
{
  const { server: proxy, connects, port: PPORT } = await connectProxy(true);
  const outer = tls.connect({ host: "127.0.0.1", port: PPORT, servername: "localhost", ca: CA });
  await new Promise((r) => outer.once("secureConnect", r));
  const answer = await new Promise((r) => tunnel(outer, "localhost", TPORT, r));
  const inner = tls.connect({ socket: outer, servername: "localhost", ca: CA });
  await new Promise((r) => inner.once("secureConnect", r));
  inner.write("ping4\n");
  const got = await new Promise((resolve) => {
    let data = "";
    inner.on("data", (d) => {
      data += d;
      if (data.endsWith("\n")) resolve(data);
    });
  });
  inner.destroy();
  console.log(`TLS in TLS: proxy answered ${JSON.stringify(answer)} saw ${connects.length} CONNECT, inner authorized=${inner.authorized} got=${JSON.stringify(got)}`);
  proxy.close();
}

// https.request through a CONNECT tunnel an agent opens by hand.
{
  const { server: proxy, connects, port: PPORT } = await connectProxy(false);
  class TunnelAgent extends https.Agent {
    createConnection(options, cb) {
      const raw = net.connect(PPORT, "127.0.0.1");
      raw.once("connect", () =>
        tunnel(raw, options.host, options.port, () => cb(null, tls.connect({ socket: raw, servername: options.servername, ca: CA }))),
      );
    }
  }
  const res = await new Promise((resolve) => {
    const req = https.get({ host: "localhost", port: TPORT, agent: new TunnelAgent() }, (res) => {
      let body = "";
      res.on("data", (d) => (body += d));
      res.on("end", () => resolve(`${res.statusCode} ${body} authorized=${req.socket.authorized}`));
    });
    req.on("error", (e) => resolve(`error ${e.code} ${e.message}`));
  });
  console.log(`https through a CONNECT tunnel: ${res}, proxy saw ${connects.map((l) => l.replace(`:${TPORT} `, ":TPORT "))}`);
  proxy.close();
}

// net.Socket in paused mode, as https-proxy-agent reads a proxy's answer.
{
  const server = net.createServer((c) => {
    c.write("first ");
    setTimeout(() => c.end("second"), 20);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const log = [];
  const s = net.connect(server.address().port, "127.0.0.1");
  s.on("end", () => log.push("end"));
  await new Promise((resolve) => {
    const onReadable = () => {
      let chunk;
      while ((chunk = s.read()) !== null) log.push(`read ${JSON.stringify(String(chunk))}`);
    };
    s.on("readable", onReadable);
    s.on("close", resolve);
  });
  console.log(`paused mode: ${log.join(" | ")}`);
  server.close();
}

// Held once the 'readable' listener goes, until a 'data' listener comes.
{
  const server = net.createServer((c) => c.end("held bytes"));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const s = net.connect(server.address().port, "127.0.0.1");
  const onReadable = () => {};
  s.on("readable", onReadable);
  await new Promise((r) => s.once("connect", r));
  s.removeListener("readable", onReadable);
  await new Promise((r) => setTimeout(r, 50));
  const got = await new Promise((resolve) => {
    let data = "";
    s.on("data", (d) => (data += d));
    s.on("end", () => resolve(data));
  });
  console.log(`held until a 'data' listener: ${JSON.stringify(got)}`);
  server.close();
}

// push() into a socket with no handle.
{
  const log = [];
  const fake = new net.Socket({ writable: false });
  log.push(`writable=${fake.writable} readable=${fake.readable}`);
  fake.readable = true;
  fake.on("data", (d) => log.push(`data ${JSON.stringify(String(d))}`));
  fake.on("end", () => log.push("end"));
  fake.push(Buffer.from("replayed"));
  log.push("pushed");
  fake.push(null);
  await new Promise((r) => setTimeout(r, 20));
  console.log(`push: ${log.join(" | ")}`);
}
target.close();
console.log("done");
