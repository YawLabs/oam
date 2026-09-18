// An http server refuses the request heads node's parser refuses, before a
// handler runs: Content-Length together with Transfer-Encoding (either
// order), a second Content-Length, a Transfer-Encoding coding after
// `chunked`, a bare LF line ending, obs-fold -- on a plain request and on an
// upgrade request alike -- each answered `400` with the connection closed.
// oam's server used to accept the first four and hand the handler both
// length headers, so a request could be framed one way by a proxy in front
// and another way here; its upgrade path accepted all of them.
//
// Whether a request is an upgrade is decided by its head alone, as in node:
// an `Upgrade` header plus `upgrade` listed in `Connection`. oam used to
// send any connection whose first bytes held a `connection: ...upgrade`
// line -- a body's included -- down the upgrade path.
//
// A second server with `insecureHTTPParser: true` shows the rules node
// relaxes there (and the duplicate Content-Length it still refuses).
//
// Every request is written raw, one per connection, and only the status
// lines and what a handler saw are printed.
import http from "node:http";
import net from "node:net";

const H = "Host: x\r\n";
const NUL = String.fromCharCode(0);
const CHUNKED_ABC = "3\r\nabc\r\n0\r\n\r\n";
const cases = {
  "cl": `POST / HTTP/1.1\r\n${H}Content-Length: 3\r\n\r\nabc`,
  "chunked": `POST / HTTP/1.1\r\n${H}Transfer-Encoding: chunked\r\n\r\n${CHUNKED_ABC}`,
  "cl then te": `POST / HTTP/1.1\r\n${H}Content-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n${CHUNKED_ABC}`,
  "te then cl": `POST / HTTP/1.1\r\n${H}Transfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n${CHUNKED_ABC}`,
  "cl 0 and te": `POST / HTTP/1.1\r\n${H}Content-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n${CHUNKED_ABC}`,
  "cl and te, then a second request": `POST / HTTP/1.1\r\n${H}Content-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /second HTTP/1.1\r\n${H}\r\n`,
  "cl and te on GET": `GET / HTTP/1.1\r\n${H}Content-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n`,
  "empty te coding then chunked": `POST / HTTP/1.1\r\n${H}Transfer-Encoding: , chunked\r\n\r\n${CHUNKED_ABC}`,
  "duplicate cl, same value": `POST / HTTP/1.1\r\n${H}Content-Length: 3\r\nContent-Length: 3\r\n\r\nabc`,
  "duplicate cl, other case": `POST / HTTP/1.1\r\n${H}Content-Length: 3\r\ncontent-length: 3\r\n\r\nabc`,
  "duplicate cl, other value": `POST / HTTP/1.1\r\n${H}Content-Length: 3\r\nContent-Length: 4\r\n\r\nabcd`,
  "chunked twice, two lines": `POST / HTTP/1.1\r\n${H}Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n${CHUNKED_ABC}`,
  "chunked twice, one line": `POST / HTTP/1.1\r\n${H}Transfer-Encoding: chunked, chunked\r\n\r\n${CHUNKED_ABC}`,
  "gzip line then chunked line": `POST / HTTP/1.1\r\n${H}Transfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n${CHUNKED_ABC}`,
  "chunked then gzip": `POST / HTTP/1.1\r\n${H}Transfer-Encoding: chunked, gzip\r\n\r\n${CHUNKED_ABC}`,
  "bare LF everywhere": `GET / HTTP/1.1\nHost: x\n\n`,
  "bare LF in a header line": `GET / HTTP/1.1\r\nHost: x\nX-A: 1\r\n\r\n`,
  "bare LF in the request line": `GET / HTTP/1.1\n${H}\r\n`,
  "bare LF blank line": `GET / HTTP/1.1\r\n${H}\n`,
  "LF CRLF blank line": `GET / HTTP/1.1\r\nHost: x\n\r\n`,
  "obs-fold": `GET / HTTP/1.1\r\n${H}X-A: a\r\n b\r\n\r\n`,
  "leading CRLF": `\r\nGET / HTTP/1.1\r\n${H}\r\n`,
  "upgrade": `GET /up HTTP/1.1\r\n${H}Connection: Upgrade\r\nUpgrade: x\r\n\r\n`,
  "upgrade after a leading CRLF": `\r\nGET /up HTTP/1.1\r\n${H}Connection: Upgrade\r\nUpgrade: x\r\n\r\n`,
  "connection upgrade without an Upgrade header": `GET /plain HTTP/1.1\r\n${H}Connection: Upgrade\r\n\r\n`,
  "a body that mentions connection: upgrade": `POST /plain HTTP/1.1\r\n${H}Content-Length: 33\r\n\r\nconnection: upgrade\r\nupgrade: x\r\n`,
  "upgrade, cl and te": `POST /up HTTP/1.1\r\n${H}Connection: Upgrade\r\nUpgrade: x\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n${CHUNKED_ABC}`,
  "upgrade, duplicate cl": `GET /up HTTP/1.1\r\n${H}Connection: Upgrade\r\nUpgrade: x\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n`,
  "upgrade, obs-fold": `GET /up HTTP/1.1\r\n${H}Connection: Upgrade\r\nUpgrade: x\r\nX-A: a\r\n b\r\n\r\n`,
  "upgrade, bare LF": `GET /up HTTP/1.1\r\n${H}Connection: Upgrade\r\nUpgrade: x\nX-A: 1\r\n\r\n`,
  "upgrade, NUL in a value": `GET /up HTTP/1.1\r\n${H}Connection: Upgrade\r\nUpgrade: x\r\nX-A: a${NUL}b\r\n\r\n`,
};

// What the insecure server is sent: the rules it relaxes, and the one it
// keeps.
const insecureCases = [
  "cl then te",
  "chunked twice, two lines",
  "chunked twice, one line",
  "bare LF in a header line",
  "duplicate cl, same value",
];

function serve(options) {
  const seen = [];
  const server = http.createServer(options, (req, res) => {
    const chunks = [];
    req.on("data", (d) => chunks.push(d));
    req.on("end", () => {
      seen.push(
        `${req.method} ${req.url} cl=${req.headers["content-length"]} te=${req.headers["transfer-encoding"]} body=${JSON.stringify(Buffer.concat(chunks).toString("latin1"))}`,
      );
      res.end("ok");
    });
  });
  server.on("upgrade", (req, socket) => {
    seen.push(`upgrade ${req.method} ${req.url}`);
    socket.end("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n");
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, seen })));
}

// One raw request per connection. Resolves with the status lines once the
// server has closed, or 300 ms after the last bytes arrived.
function send(port, bytes) {
  return new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1");
    let raw = "";
    let timer = null;
    let settled = false;
    let backstop = null;
    const done = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(backstop);
      socket.destroy();
      const statuses = [...raw.matchAll(/HTTP\/1\.[01] \d{3}[^\r\n]*/g)].map((m) => m[0]);
      resolve(statuses.join(" + ") || "(nothing)");
    };
    socket.on("data", (d) => {
      raw += d.toString("latin1");
      clearTimeout(timer);
      timer = setTimeout(done, 300);
    });
    socket.on("close", done);
    socket.on("error", () => {});
    socket.write(Buffer.from(bytes, "latin1"));
    // A backstop, so a runtime that never answers fails the case instead
    // of hanging it.
    backstop = setTimeout(done, 3000);
  });
}

async function run(label, options, names) {
  const { server, seen } = await serve(options);
  for (const name of names) {
    const before = seen.length;
    const status = await send(server.address().port, cases[name]);
    await new Promise((r) => setTimeout(r, 20));
    const handled = seen.slice(before);
    console.log(`${label} | ${name} | ${status} | ${handled.length ? handled.join(" ; ") : "no handler"}`);
  }
  await new Promise((r) => server.close(r));
}

await run("strict", {}, Object.keys(cases));
await run("insecure", { insecureHTTPParser: true }, insecureCases);
