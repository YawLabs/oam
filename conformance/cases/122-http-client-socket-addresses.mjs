// A ClientRequest's socket reports the connection it went over, as node's
// does (measured on node v22.22.2): req.socket is null until 'socket', its
// addresses are unset while it connects, and by 'response' they are the
// DIALLED peer -- the IP address, never the host as written -- and the local
// end the server saw, with res.socket === req.socket. An application that
// checks req.socket.remoteAddress against a deny-list after connecting relies
// on exactly this. oam's req.socket used to be a fixed object naming the host
// as written, with localAddress '127.0.0.1' and localPort 0.
//
// Every target runs twice: once as is, and once with a 'connect' listener on
// the socket (which in oam sends the request over a socket it connects
// itself); the facts are the same. https runs with rejectUnauthorized:false
// and verifying against the fixture CA. The servers are raw net / tls
// listeners bound to 127.0.0.1 and ::1 separately, which answer every request
// and record the port each connection came from.
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


const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// The port the last request's connection came from, as its server saw it.
let seenPort = null;
function answer(c) {
  let head = "";
  c.on("error", () => {});
  c.on("data", (d) => {
    head += d.toString("latin1");
    if (!head.includes("\r\n\r\n")) return;
    seenPort = c.remotePort;
    c.end("HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
  });
}
const listen = (server, host) => new Promise((r) => server.listen(0, host, r));
const srv4 = net.createServer(answer);
await listen(srv4, "127.0.0.1");
const srv6 = net.createServer(answer);
await listen(srv6, "::1");
const tsrv4 = tls.createServer({ key: LEAF_KEY, cert: LEAF }, answer);
tsrv4.on("tlsClientError", () => {});
await listen(tsrv4, "127.0.0.1");
const P4 = srv4.address().port;
const P6 = srv6.address().port;
const T4 = tsrv4.address().port;

async function probe(label, mod, options, watch, tlsFacts) {
  const events = [];
  seenPort = null;
  const req = mod.get({ agent: new mod.Agent(), ...options });
  events.push(`after get: socket=${req.socket}`);
  req.on("socket", (s) => {
    // (node has already bound a literal's local end here; oam fills both
    // ends in when the connection is made.)
    events.push(`socket: ${s.constructor.name} remote=${s.remoteAddress} connecting=${s.connecting}`);
    if (watch) {
      s.on("connect", () => events.push(`connect: remote=${s.remoteAddress} family=${s.remoteFamily}`));
    }
  });
  const outcome = await new Promise((resolve) => {
    req.on("response", (res) => {
      const s = res.socket;
      let line =
        `remote=${s.remoteAddress} remotePort=${s.remotePort === options.port} family=${s.remoteFamily} ` +
        `local=${s.localAddress} localPort=${s.localPort === seenPort} same=${s === req.socket} ` +
        `address=${s.address().address === s.localAddress && s.address().port === s.localPort}`;
      if (tlsFacts) {
        line += ` encrypted=${s.encrypted} authorized=${s.authorized} cn=${s.getPeerCertificate().subject.CN}`;
      }
      res.resume();
      res.on("end", () => resolve(line));
    });
    req.on("error", (e) => resolve(`ERROR ${e.code} ${e.message}`));
  });
  console.log(`${label}${watch ? " (watched)" : ""}: ${outcome} | ${JSON.stringify(events)}`);
  await sleep(10);
}

for (const watch of [false, true]) {
  await probe("http 127.0.0.1", http, { host: "127.0.0.1", port: P4 }, watch);
  await probe("http [::1]", http, { host: "::1", port: P6 }, watch);
  await probe("http localhost", http, { host: "localhost", port: P4 }, watch);
  await probe("https 127.0.0.1, rejectUnauthorized:false", https, { host: "127.0.0.1", port: T4, rejectUnauthorized: false }, watch, true);
}
await probe("https localhost, verified", https, { host: "localhost", port: T4, ca: CA }, true, true);
// Host spellings the URL parser would rewrite go over net.connect with the
// string as given: the platform's resolver decides (and the socket reports
// what it dialled), or it fails, as in node.
for (const host of ["LocalHost", "0177.0.0.1", "127.000.000.001", "127.0.0.1.", "%31%32%37.0.0.1"]) {
  await probe(`http host ${JSON.stringify(host)}`, http, { host, port: P4 }, false);
}

// The facts are there at 'response' even when the server closes the
// connection straight after its answer: node's parser emits 'response' as it
// reads the head, before the socket gets to the EOF behind it, and closes
// the socket after. oam's head comes back from its parser a few ticks later,
// and a socket that ended itself on that EOF in between reported no local
// address and no peer certificate there (about one request in eight).
let alive = 0;
let closedAfter = 0;
const ROUNDS = 25;
for (let i = 0; i < ROUNDS; i++) {
  for (const [mod, options] of [
    [https, { host: "127.0.0.1", port: T4, rejectUnauthorized: false }],
    [http, { host: "127.0.0.1", port: P4 }],
  ]) {
    await new Promise((resolve) => {
      const req = mod.get({ agent: new mod.Agent(), ...options });
      req.on("socket", (s) => {
        s.on("connect", () => {});
        s.on("close", () => {
          closedAfter++;
          resolve();
        });
      });
      req.on("response", (res) => {
        const s = res.socket;
        const up = !s.destroyed && s.address().port === s.localPort &&
          (mod === http || s.getPeerCertificate().subject.CN === "localhost");
        if (up) alive++;
        res.resume();
      });
      req.on("error", (e) => {
        console.log("ERROR", e.code);
        resolve();
      });
    });
  }
}
console.log(`closing server: socket up at 'response' ${alive}/${2 * ROUNDS}, closed after ${closedAfter}/${2 * ROUNDS}`);

srv4.close();
srv6.close();
tsrv4.close();
