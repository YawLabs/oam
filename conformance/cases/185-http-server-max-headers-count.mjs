// An http/https server refuses a request head only on its byte size
// (`maxHeaderSize` -> 431), never on how many header fields it carries. Past
// `server.maxHeadersCount` fields the extra are dropped and the request is
// still served; `maxHeadersCount === 0` is no limit. Printed for each request:
// the status code, the number of header fields the handler was given
// (`req.headers`), the `rawHeaders` count, and -- for a truncation -- whether
// the FIRST custom field survived and the LAST was dropped (via the lowercased
// `req.headers`, so header-name case never enters it). Measured on node
// v22.22.2 (#202).
import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";

const watchdog = setTimeout(() => { console.log("WATCHDOG"); process.exit(9); }, 20000);

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

// A raw request head with exactly `fields` header fields (Host and the
// Connection: close that lets the socket close both counted), each custom
// value `valLen` bytes.
function head(fields, valLen) {
  const v = "v".repeat(valLen);
  let s = "GET / HTTP/1.1\r\nHost: h\r\nConnection: close\r\n";
  for (let i = 1; i <= fields - 2; i++) s += "h" + i + ": " + v + "\r\n";
  return s + "\r\n";
}

function send(port, secure, fields, valLen) {
  return new Promise((resolve) => {
    let done = false;
    const onConn = (sock) => {
      let buf = "";
      const fin = (v) => { if (done) return; done = true; try { sock.destroy(); } catch {} resolve(v); };
      const statusOf = () => (buf.split("\r\n")[0] || "").replace("HTTP/1.1 ", "").split(" ")[0];
      // Resolve as soon as the status line is in -- the handler has already
      // recorded the request by the time the server writes its response.
      sock.on("data", (d) => { buf += d; if (buf.includes("\r\n")) fin(statusOf()); });
      sock.on("close", () => fin(statusOf()));
      sock.on("error", () => fin("ERR"));
      sock.write(head(fields, valLen));
    };
    const s = secure
      ? tls.connect({ host: "127.0.0.1", port, rejectUnauthorized: false, servername: "localhost" }, () => onConn(s))
      : net.connect(port, "127.0.0.1", () => onConn(s));
    s.on("error", () => { if (!done) { done = true; resolve("ERR"); } });
    setTimeout(() => { try { s.destroy(); } catch {} if (!done) { done = true; resolve("TIMEOUT"); } }, 6000);
  });
}

async function run(label, makeServer, requests) {
  const seen = [];
  const server = makeServer((req, res) => {
    let raw = 0;
    for (let i = 0; i < req.rawHeaders.length; i += 2) raw++;
    seen.push({ raw, headers: req.headers, hdr: Object.keys(req.headers).length });
    res.end("ok");
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  for (const rq of requests) {
    const before = seen.length;
    const status = await send(port, rq.secure, rq.fields, rq.valLen || 1);
    let line = label + " " + rq.name + ": status=" + status;
    if (seen.length > before) {
      const rec = seen[before];
      line += " hdr=" + rec.hdr + " raw=" + rec.raw;
      if (rq.order) {
        const keptFirst = rec.headers.h1 !== undefined;
        const keptLast = rec.headers["h" + (rq.fields - 2)] !== undefined;
        line += " keptFirst=" + keptFirst + " keptLast=" + keptLast;
      }
    } else {
      line += " (no handler)";
    }
    console.log(line);
  }
  await new Promise((r) => server.close(r));
}

// 1. Default http server: fields far inside maxHeaderSize are served whatever
// their count, and past the 1000-field default the extra are dropped.
await run("http-default", (h) => http.createServer(h), [
  { name: "100", fields: 100 },
  { name: "101", fields: 101 },
  { name: "1000", fields: 1000 },
  { name: "1500", fields: 1500 },
]);

// 2. A raised maxHeaderSize does not change the field behavior.
await run("http-bigsize", (h) => http.createServer({ maxHeaderSize: 65536 }, h), [
  { name: "151", fields: 151 },
]);

// 3. maxHeadersCount caps the handler's headers; the first N are kept.
await run("http-mhc5", (h) => { const s = http.createServer(h); s.maxHeadersCount = 5; return s; }, [
  { name: "40", fields: 40, order: true },
]);

// 4. Under the limit nothing is dropped; over it the first N are kept.
await run("http-mhc50", (h) => { const s = http.createServer(h); s.maxHeadersCount = 50; return s; }, [
  { name: "20", fields: 20 },
  { name: "100", fields: 100, order: true },
]);

// 5. maxHeadersCount === 0 is no limit.
await run("http-mhc0", (h) => { const s = http.createServer(h); s.maxHeadersCount = 0; return s; }, [
  { name: "200", fields: 200 },
]);

// 6. 431 is driven by bytes (maxHeaderSize), not the field count: few large
// fields over the 16 KiB default are refused before the handler runs.
await run("http-bytes", (h) => http.createServer(h), [
  { name: "60x400B", fields: 60, valLen: 400 },
]);

// 7. https behaves the same over TLS.
await run("https", (h) => https.createServer({ cert: CERT, key: KEY, maxHeaderSize: 65536 }, h), [
  { name: "151", fields: 151, secure: true },
  { name: "default-1500", fields: 1500, secure: true },
]);

clearTimeout(watchdog);
process.exit(0);
