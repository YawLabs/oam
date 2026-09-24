// An https server dispatches the handler on the request head and streams the
// body, the same as node and as oam's http server -- so the handler can answer
// (and cut) an upload before it has all arrived. Before #203, https buffered the
// whole body and the handler ran only once it ended, so it could neither answer
// early nor refuse an upload. Two halves, each run against http and https:
//   dispatch-on-head: the handler answers on entry without reading the body,
//     while the client withholds the body terminator -- the status it gets back
//     (or "(none)" if the handler never ran) is printed.
//   early-cut: a handler that answers `413` and destroys the request on entry;
//     the client streams until it is cut, and whether it was cut before it
//     finished a multi-megabyte upload is printed.
// Measured on node v22.22.2.
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

function connect(port, secure) {
  return secure
    ? tls.connect({ host: "127.0.0.1", port, rejectUnauthorized: false, servername: "localhost" })
    : net.connect(port, "127.0.0.1");
}
const HEAD = "POST /u HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n";
const CHUNK = "10\r\n0123456789abcdef\r\n"; // one 16-byte chunk, no 0-terminator

// A handler answers on entry without reading the body; withhold the terminator
// and see whether the status comes back before it is sent.
function dispatchOnHead(label, makeServer, secure) {
  return new Promise((resolve) => {
    const server = makeServer((req, res) => {
      res.writeHead(200);
      res.end("dispatched");
    });
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      const sock = connect(port, secure);
      let raw = "";
      let done = false;
      const finish = () => {
        if (done) return;
        done = true;
        try { sock.destroy(); } catch {}
        const m = raw.match(/HTTP\/1\.1 (\d{3})/);
        console.log(`${label} dispatch-on-head: ${m ? m[1] : "(none)"}`);
        server.close(() => resolve());
      };
      const backstop = setTimeout(finish, 4000);
      sock.on("data", (d) => { raw += d.toString("latin1"); if (raw.includes("\r\n\r\n")) { clearTimeout(backstop); finish(); } });
      sock.on("error", () => {});
      sock.on(secure ? "secureConnect" : "connect", () => sock.write(HEAD + CHUNK));
    });
  });
}

// A handler refuses on entry (413 + destroy); the client streams up to LIMIT
// bytes and reports whether it was cut before finishing.
const LIMIT = 8 * 1024 * 1024;
function earlyCut(label, makeServer, secure) {
  return new Promise((resolve) => {
    const server = makeServer((req, res) => {
      res.writeHead(413);
      res.end();
      req.destroy();
    });
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      const sock = connect(port, secure);
      let sent = 0;
      let cut = false;
      let done = false;
      const block = "f000\r\n" + "x".repeat(0xf000) + "\r\n"; // a ~60 KiB chunk
      const finish = () => {
        if (done) return;
        done = true;
        try { sock.destroy(); } catch {}
        console.log(`${label} early-cut: ${cut && sent < LIMIT}`);
        server.close(() => resolve());
      };
      const backstop = setTimeout(finish, 8000);
      const pump = () => {
        while (!done && sent < LIMIT) {
          sent += block.length;
          if (!sock.write(block)) return; // wait for drain
        }
        if (sent >= LIMIT && !done) { clearTimeout(backstop); finish(); }
      };
      sock.on("drain", pump);
      sock.on("error", () => { cut = true; clearTimeout(backstop); finish(); });
      sock.on("close", () => { cut = true; clearTimeout(backstop); finish(); });
      sock.on("data", () => {}); // drain the 413 response
      sock.on(secure ? "secureConnect" : "connect", () => { sock.write(HEAD); pump(); });
    });
  });
}

await dispatchOnHead("http", (h) => http.createServer(h), false);
await dispatchOnHead("https", (h) => https.createServer({ cert: CERT, key: KEY }, h), true);
await earlyCut("http", (h) => http.createServer(h), false);
await earlyCut("https", (h) => https.createServer({ cert: CERT, key: KEY }, h), true);

clearTimeout(watchdog);
process.exit(0);
