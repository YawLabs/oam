// An https server hands an upgrade or a CONNECT to its 'upgrade' / 'connect'
// listener with a socket, the same as node and as oam's http server -- so a
// WebSocket handshake reaches the listener (not the ordinary 'request' handler)
// and a CONNECT can be answered. Before #205 an https server served every
// upgrade as an ordinary request and closed a CONNECT. Each scenario prints the
// server event it fired and the status line the client got back (and, where the
// listener answers over the raw socket, the payload it wrote after the head).
// Measured on node v22.22.2.
import https from "node:https";
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

function raw(port, reqBytes) {
  return new Promise((resolve) => {
    const s = tls.connect({ host: "127.0.0.1", port, rejectUnauthorized: false, servername: "localhost" }, () => s.write(reqBytes));
    let buf = "";
    let done = false;
    const fin = () => {
      if (done) return;
      done = true;
      try { s.destroy(); } catch {}
      const line = (buf.split("\r\n")[0] || "(closed, no answer)").trim();
      // the payload the listener wrote after the head, if any
      const i = buf.indexOf("\r\n\r\n");
      const tail = i >= 0 ? buf.slice(i + 4) : "";
      resolve({ line, tail });
    };
    s.on("data", (d) => { buf += d.toString("latin1"); });
    s.on("close", fin);
    s.on("error", fin);
    setTimeout(fin, 1500);
  });
}

async function scenario(label, install, reqBytes, showTail) {
  const events = [];
  const server = https.createServer({ cert: CERT, key: KEY }, (req, res) => {
    events.push("request " + req.method + " " + req.url);
    res.writeHead(200);
    res.end("ordinary");
  });
  install(server, events);
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const { line, tail } = await raw(server.address().port, reqBytes);
  await new Promise((r) => setTimeout(r, 40));
  await new Promise((r) => server.close(r));
  let out = `${label} | server: ${events.join(" ; ") || "(no event)"} | client: ${line}`;
  if (showTail) out += " | tail: " + JSON.stringify(tail);
  console.log(out);
}

const UPGRADE = "GET /socket HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
const CONNECT = "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";

// The listener answers over the raw socket and writes a payload after the head,
// to show the connection was handed over and is writable both ways.
const withUpgrade = (s, ev) => s.on("upgrade", (req, socket) => {
  ev.push("upgrade " + req.method + " " + req.url);
  socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\nPING");
});
const withConnect = (s, ev) => s.on("connect", (req, socket) => {
  ev.push("connect " + req.url);
  socket.write("HTTP/1.1 200 Connection Established\r\n\r\nTUNNEL");
});
const none = () => {};

await scenario("upgrade + listener", withUpgrade, UPGRADE, true);
await scenario("upgrade + no listener", none, UPGRADE, false);
await scenario("connect + listener", withConnect, CONNECT, true);
await scenario("connect + no listener", none, CONNECT, false);

clearTimeout(watchdog);
process.exit(0);
