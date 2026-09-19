// An http server's req.url, and the request targets it answers 400
// (measured on node v22.22.2, whose verdicts are the same with
// insecureHTTPParser).
//
// node's req.url is the target byte for byte as the client sent it: an
// absolute form keeps its spelling (`HTTP://H/P`, `http://h` with no `/`), a
// fragment stays (`/p#f`), and `*` may be followed by more. oam read it from
// hyper's URI type, which lowercases the scheme, adds the `/`, drops the
// fragment and reduced `*x` to `""`.
//
// For any method but CONNECT, node takes only a target that starts with `/`
// or `*`, or a scheme of letters, `://` and an authority without `#`.
// hyper's URI type also takes an authority alone (`abc`, `example.test:443`,
// `.`) and odd schemes (`1http://`, `h-t.t+p://`), which oam handed to the
// handler with a req.url of `""` (or the scheme as rewritten) and answered
// 200. (hyper refuses a few absolute forms node takes -- `http://` with no
// host -- which is recorded in docs/node-divergences.md and not printed.)
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

const seen = [];
const server = http.createServer({ insecureHTTPParser: false }, (req, res) => {
  seen.push(`${req.method} ${JSON.stringify(req.url)}`);
  res.end("ok");
});
server.on("connect", (req, socket) => {
  seen.push(`connect ${JSON.stringify(req.url)}`);
  socket.end("HTTP/1.1 200 OK\r\n\r\n");
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

function raw(line) {
  return new Promise((resolve) => {
    let got = "";
    const s = net.connect(port, "127.0.0.1", () => {
      s.write(`${line}\r\nHost: h.test\r\nConnection: close\r\n\r\n`);
    });
    s.on("data", (d) => (got += d));
    s.on("error", (e) => resolve(`error ${e.code}`));
    s.on("close", () => resolve(`${got.split("\r\n")[0]}${seen.length ? " -> " + seen.splice(0).join(", ") : ""}`));
  });
}

for (const line of [
  // Taken, req.url as sent.
  "GET /p?q=1 HTTP/1.1",
  "GET //double/slash HTTP/1.1",
  "GET /#frag HTTP/1.1",
  "GET /p#f?q HTTP/1.1",
  "GET /a?b#c HTTP/1.1",
  "GET * HTTP/1.1",
  "OPTIONS * HTTP/1.1",
  "PUT * HTTP/1.1",
  "GET *x HTTP/1.1",
  "GET ** HTTP/1.1",
  "GET http://h.test HTTP/1.1",
  "GET http://h.test/p?q HTTP/1.1",
  "GET HTTP://H.TEST/P HTTP/1.1",
  "GET http://h.test?q HTTP/1.1",
  "GET http://h.test/p?q#f HTTP/1.1",
  "GET https://h.test:8443/p HTTP/1.1",
  "GET http://u:p@h.test/p HTTP/1.1",
  "GET http://[::1]:8/p HTTP/1.1",
  "GET http://h:x/ HTTP/1.1",
  "GET ws://h.test HTTP/1.1",
  "GET abc://h HTTP/1.1",
  // Refused.
  "GET abc HTTP/1.1",
  "GET a HTTP/1.1",
  "GET 1abc HTTP/1.1",
  "GET abc: HTTP/1.1",
  "GET abc:80 HTTP/1.1",
  "GET example.test:443 HTTP/1.1",
  "GET http:x HTTP/1.1",
  "GET . HTTP/1.1",
  "GET h-t.t+p://x/ HTTP/1.1",
  "GET 1http://x/ HTTP/1.1",
  "GET http://h.test#f HTTP/1.1",
  "OPTIONS abc HTTP/1.1",
  "POST abc HTTP/1.1",
  "GET ?q=1 HTTP/1.1",
  "GET # HTTP/1.1",
  // CONNECT takes a target in any of these forms.
  "CONNECT h.test:443 HTTP/1.1",
  "CONNECT /p HTTP/1.1",
  "CONNECT http://h.test/ HTTP/1.1",
]) {
  console.log(`${line}: ${await raw(line)}`);
}
server.close();
