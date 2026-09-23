// oam's TLS clients against oam's TLS servers, in one process: under node
// both ends are node's, under oam both are oam's. The clients --
// tls.connect with a local port and an ALPN offer, https.request with a
// client certificate (which sends it over tls.connect), and http2.connect
// (a session over tls.connect with the session's TLS options) -- and the
// servers -- tls.createServer, https.createServer and
// http2.createSecureServer with requestCert, ca and ALPN -- were fixed on
// separate branches, each tested against node's other half.
//
// A client whose certificate the server's CA signed is served, and both
// ends report the handshake (ALPN, authorized, the peer's certificate, the
// client's local port as the server's remote port); a client with none is
// refused with node's alert on the client and node's code on the server;
// an http2 secure server serves h2 and, under allowHTTP1, HTTP/1.1. A server
// whose version range offers nothing refuses each client with node's alert
// and reports it per connection, an https server as a tls one does.
//
// Regression guard: before the merge that brought these together, oam's
// http2.createSecureServer served cleartext and ignored requestCert, its
// https.createServer never asked for a client certificate, and its
// http2.connect never ran over tls.connect -- each half passed against
// node's, and nothing ran the two against each other.
//
// Fixtures: the case 141 throwaway P-256 CA (valid 2025-2125), the
// localhost leaf it signed and a client leaf it signed ("oam client").
import http2 from "node:http2";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 45000).unref();

const CA = `-----BEGIN CERTIFICATE-----
MIIBmjCCAUGgAwIBAgIUHjF3aO/Nr2SNMEQNV9GNuumIljswCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAaMRgwFgYDVQQDDA9vYW0gaDJzIHRlc3QgQ0EwWTATBgcq
hkjOPQIBBggqhkjOPQMBBwNCAAR6EfahtynuI8VLuixWn6GiZ3BYWFdJEqP1FfLE
lCBVF/69Rm6fDrzSVP/GWO7qsNhAZmyIVWyRQJcQiBv55omto2MwYTAdBgNVHQ4E
FgQUOlIo6O4tIFNjD7vXJV51FU2DLQcwHwYDVR0jBBgwFoAUOlIo6O4tIFNjD7vX
JV51FU2DLQcwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYwCgYIKoZI
zj0EAwIDRwAwRAIgFFCfCAiuzT1cHBF7zAQEVxSrWsoco8cOD49S6whO4vsCIC/T
xtSxdoSsByDfaJz7qxOrhJzSD5lDwUdNMe3EoP9l
-----END CERTIFICATE-----
`;
const CERT = `-----BEGIN CERTIFICATE-----
MIIBvjCCAWWgAwIBAgIUOy7BLDqzc+0IZz2NWG95hnXgrd4wCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIB
BggqhkjOPQMBBwNCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0bj0ngz5dpnyIRlajs
UptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1Vo4GMMIGJMBoGA1UdEQQTMBGC
CWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDATBgNVHSUE
DDAKBggrBgEFBQcDATAdBgNVHQ4EFgQUmxnUU2rP4FwgoXrkCkeRxNgCVycwHwYD
VR0jBBgwFoAUOlIo6O4tIFNjD7vXJV51FU2DLQcwCgYIKoZIzj0EAwIDRwAwRAIg
ItB5f9aIsf9D8cXBvJvvr5ahB57RK7DgAsIVf5uJ0zcCIBPOR2Z+ycbeeByMKH2v
shKfeR1QdaoQHwJKJln0q1fo
-----END CERTIFICATE-----
`;
const KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgQLidYpqFITu5wno8
Fw5b5Ahrg5eTwH0UqA7RU57egNKhRANCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0b
j0ngz5dpnyIRlajsUptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1V
-----END PRIVATE KEY-----
`;
const CLIENT_CERT = `-----BEGIN CERTIFICATE-----
MIIBoTCCAUigAwIBAgIUOy7BLDqzc+0IZz2NWG95hnXgrd8wCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAVMRMwEQYDVQQDDApvYW0gY2xpZW50MFkwEwYHKoZIzj0C
AQYIKoZIzj0DAQcDQgAEulhTChDco8oZzXpPqo3iqtybv/nUXKwS67GiGZ23ra4b
5Ta8McX1MVv2p0WA1/JYyncszN9kbKwE1oeV0Q0lTKNvMG0wCQYDVR0TBAIwADAL
BgNVHQ8EBAMCB4AwEwYDVR0lBAwwCgYIKwYBBQUHAwIwHQYDVR0OBBYEFCk7s+uR
ZEuahksP0Vn6QPqJ+TmIMB8GA1UdIwQYMBaAFDpSKOjuLSBTYw+71yVedRVNgy0H
MAoGCCqGSM49BAMCA0cAMEQCIFOOnRBbxbAbIOcU15I7xnKlD5QXj7P2ZHQbxax0
goFxAiBEgrUhNh9pzkHEQGCdqJNAtqjNUURu8GVWs9re4QYI7A==
-----END CERTIFICATE-----
`;
const CLIENT_KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgT9C7qt8YZkF23ize
WR5qGRqTlTsUdSjwsf/UmBhdckihRANCAAS6WFMKENyjyhnNek+qjeKq3Ju/+dRc
rBLrsaIZnbetrhvlNrwxxfUxW/anRYDX8ljKdyzM32RsrATWh5XRDSVM
-----END PRIVATE KEY-----
`;

const cn = (cert) => (cert && cert.subject ? cert.subject.CN : "none");
const settle = (ms) => new Promise((r) => setTimeout(r, ms));
async function freePort() {
  const s = net.createServer();
  await new Promise((r) => s.listen(0, "127.0.0.1", r));
  const p = s.address().port;
  await new Promise((r) => s.close(r));
  return p;
}
const listen = (server) => new Promise((r) => server.listen(0, "127.0.0.1", () => r(server.address().port)));
const clientCert = { cert: CLIENT_CERT, key: CLIENT_KEY };
// A client the server refuses records its code, node's name for the alert
// the server sent (ERR_SSL_TLSV13_ALERT_CERTIFICATE_REQUIRED after the
// handshake, on every client; #196), and the server's code, which names
// why. Never printed: the messages, OpenSSL's diagnostics under node
// (docs/node-divergences.md entry 34).

// ---- tls.connect against tls.createServer
{
  const events = [];
  const server = tls.createServer(
    { key: KEY, cert: CERT, ca: [CA], requestCert: true, rejectUnauthorized: true, ALPNProtocols: ["h2", "http/1.1"] },
    (socket) => {
      events.push(
        "secureConnection alpn=" + socket.alpnProtocol + " authorized=" + socket.authorized +
          " peer=" + cn(socket.getPeerCertificate()) + " from=" + socket.remoteAddress +
          (socket.remotePort === server.expectPort ? ":the client's local port" : ":another port"),
      );
      socket.end("hello from the server");
    },
  );
  server.on("tlsClientError", (err) => events.push("tlsClientError " + err.code));
  const port = await listen(server);
  for (const [label, extra] of [["certificate", clientCert], ["no certificate", {}]]) {
    const localPort = await freePort();
    server.expectPort = localPort;
    const out = [];
    await new Promise((resolve) => {
      const s = tls.connect({
        host: "127.0.0.1", port, servername: "localhost", ca: [CA],
        localAddress: "127.0.0.1", localPort, ALPNProtocols: ["x-unknown", "http/1.1"], ...extra,
      });
      s.setEncoding("utf8");
      s.on("secureConnect", () => out.push("secureConnect alpn=" + s.alpnProtocol + " authorized=" + s.authorized +
        " server=" + cn(s.getPeerCertificate()) + " localPort=" + (s.localPort === localPort ? "as asked" : s.localPort)));
      s.on("data", (d) => out.push("data " + JSON.stringify(d)));
      s.on("error", (e) => out.push("error " + e.code + " " + Object.keys(e).join(",")));
      s.on("close", resolve);
    });
    await settle(50);
    console.log("tls " + label + ": " + out.join(" | "));
  }
  server.close();
  for (const e of events) console.log("  tls server " + e);
}

// ---- https.request against https.createServer
{
  const events = [];
  const server = https.createServer({ key: KEY, cert: CERT, ca: [CA], requestCert: true }, (req, res) => {
    events.push("request authorized=" + req.client.authorized + " peer=" + cn(req.socket.getPeerCertificate()) +
      " alpn=" + req.socket.alpnProtocol + " protocol=" + req.socket.getProtocol());
    res.end("secret");
  });
  server.on("clientError", (err) => events.push("clientError " + err.code));
  const port = await listen(server);
  for (const [label, extra] of [["certificate", clientCert], ["no certificate", {}], ["certificate, agent: false", { ...clientCert, agent: false }]]) {
    const line = await new Promise((resolve) => {
      const req = https.request({ host: "127.0.0.1", port, servername: "localhost", ca: [CA], path: "/", ...extra }, (res) => {
        const server = cn(res.socket.getPeerCertificate());
        const authorized = res.socket.authorized;
        let body = "";
        res.setEncoding("utf8");
        res.on("data", (d) => (body += d));
        res.on("end", () => resolve(res.statusCode + " " + body + " server=" + server + " authorized=" + authorized));
      });
      req.on("error", (e) => resolve("error " + e.code + " " + Object.keys(e).join(",")));
      req.end();
    });
    await settle(50);
    console.log("https " + label + ": " + line);
  }
  server.close();
  for (const e of events) console.log("  https server " + e);
}

// ---- http2.connect (and https.request, allowHTTP1) against http2.createSecureServer
{
  const events = [];
  const server = http2.createSecureServer(
    { key: KEY, cert: CERT, ca: [CA], requestCert: true, rejectUnauthorized: true, allowHTTP1: true },
    (req, res) => {
      events.push("request httpVersion=" + req.httpVersion + " " + req.url + " authorized=" + req.socket.authorized +
        " peer=" + cn(req.socket.getPeerCertificate()) + " alpn=" + req.socket.alpnProtocol);
      res.end("h" + req.httpVersion + " " + req.url);
    },
  );
  server.on("session", (session) => events.push("session alpn=" + session.alpnProtocol + " authorized=" + session.socket.authorized));
  server.on("tlsClientError", (err) => events.push("tlsClientError " + err.code));
  const port = await listen(server);
  for (const [label, extra] of [["certificate", clientCert], ["no certificate", {}]]) {
    const out = [];
    await new Promise((resolve) => {
      const session = http2.connect(`https://127.0.0.1:${port}`, { servername: "localhost", ca: [CA], ...extra });
      session.on("connect", () => out.push("connect alpn=" + session.alpnProtocol + " encrypted=" + session.encrypted +
        " server=" + cn(session.socket.getPeerCertificate())));
      // Not recorded: after the server's alert oam's session emits 'error'
      // and 'close', node's neither (docs/node-divergences.md entry 44).
      session.on("error", () => {});
      const stream = session.request({ ":path": "/over-h2" });
      stream.setEncoding("utf8");
      let body = "";
      stream.on("response", (h) => out.push("response " + h[":status"]));
      stream.on("data", (d) => (body += d));
      stream.on("end", () => out.push("body " + JSON.stringify(body)));
      stream.on("error", (e) => out.push("stream error " + e.code + " " + Object.keys(e).join(",")));
      // The stream's end, not the session's: node emits no session 'close'
      // after a peer's fatal alert.
      stream.on("close", () => {
        out.push("stream close");
        session.close();
        session.destroy();
        resolve();
      });
    });
    await settle(50);
    console.log("http2 " + label + ": " + out.join(" | "));
  }
  const h1 = await new Promise((resolve) => {
    const req = https.request({ host: "127.0.0.1", port, servername: "localhost", ca: [CA], path: "/over-h1", ...clientCert }, (res) => {
      let body = "";
      res.setEncoding("utf8");
      res.on("data", (d) => (body += d));
      res.on("end", () => resolve(res.statusCode + " " + JSON.stringify(body)));
    });
    req.on("error", (e) => resolve("error " + e.code));
    req.end();
  });
  console.log("http2 allowHTTP1 via https.request: " + h1);
  await settle(50);
  server.close();
  for (const e of events) console.log("  http2 server " + e);
}

// ---- a server whose version range offers nothing
for (const [name, make] of [
  ["tls", (o) => tls.createServer(o, (s) => s.end())],
  ["https", (o) => https.createServer(o, (req, res) => res.end())],
]) {
  const events = [];
  const server = make({ key: KEY, cert: CERT, maxVersion: "TLSv1.1" });
  server.on("tlsClientError", (e) => events.push("tlsClientError " + e.code));
  server.on("clientError", (e) => events.push("clientError " + e.code));
  const port = await listen(server);
  const seen = await new Promise((resolve) => {
    const s = tls.connect({ host: "127.0.0.1", port, servername: "localhost", ca: [CA] });
    s.on("secureConnect", () => resolve("secureConnect"));
    s.on("error", (e) => resolve("error " + e.code));
  });
  await settle(100);
  server.close();
  console.log(name + " range offering nothing: client " + seen + " | server " + (events.join(", ") || "nothing"));
}
