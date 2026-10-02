// The bytes an http.request header value goes out as, measured on node
// v22.22.2. node keeps the request head as a string and writes it joined to
// the first thing sent after it, as a server response does (see
// 251-http-response-header-bytes): joined to a string body in utf8 (or no
// encoding) with no chunk-size line ahead of it -- req.end('text'), a GET's
// write('text'), a write with a content-length, flushHeaders() -- the head
// goes out as UTF-8 (`café` is caf\xc3\xa9); before anything else -- end(),
// end(''), a Buffer, a string in another encoding, a POST's chunked write --
// one byte per code point (caf\xe9). oam sent every value one byte per code
// point. end('text') knows its length unless a Trailer header is set or the
// content-length header was removed: then a POST's body is chunked and its
// head one byte per code point (oam sent UTF-8 there); a GET's is not.
//
// Each request goes out three ways: with no agent (oam's transport), through
// a keep-alive agent, and as an upgrade request (a head oam writes on the
// socket itself). Each line is the x-v header line the server received.
import http from "node:http";
import net from "node:net";

const V = "caf\u00e9\u00ff";
let onHead = null;
const server = net.createServer((socket) => {
  let buf = Buffer.alloc(0);
  socket.on("data", (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    const end = buf.indexOf("\r\n\r\n");
    if (end < 0 || onHead === null) return;
    const line = buf.subarray(0, end).toString("latin1").split("\r\n").find((l) => /^x-v:/i.test(l)) || "(none)";
    const report = onHead;
    onHead = null;
    report(
      [...Buffer.from(line, "latin1")]
        .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : `\\x${b.toString(16).padStart(2, "0")}`))
        .join(""),
    );
    socket.end("HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
  });
  socket.on("error", () => {});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();

const sends = {
  "end()": (req) => req.end(),
  "end('')": (req) => req.end(""),
  "end('x')": (req) => req.end("x"),
  "end('x', 'utf8')": (req) => req.end("x", "utf8"),
  "end('x', 'latin1')": (req) => req.end("x", "latin1"),
  "end(buffer)": (req) => req.end(Buffer.from("x")),
  "write('x'), end()": (req) => { req.write("x"); req.end(); },
  "write(buffer), end()": (req) => { req.write(Buffer.from("x")); req.end(); },
  "content-length, write('x')": (req) => { req.setHeader("content-length", "1"); req.write("x"); req.end(); },
  "transfer-encoding, end('x')": (req) => { req.setHeader("transfer-encoding", "chunked"); req.end("x"); },
  "flushHeaders(), end()": (req) => { req.flushHeaders(); req.end(); },
  "content-length removed, end('x')": (req) => { req.removeHeader("content-length"); req.end("x"); },
  "both framing headers removed, write('x')": (req) => {
    req.removeHeader("content-length");
    req.removeHeader("transfer-encoding");
    req.write("x");
    req.end();
  },
};
// node refuses a GET's Trailer header (its body is not chunked) when the
// head is built: ERR_HTTP_TRAILER_INVALID, which oam's client does not throw.
const postSends = {
  "Trailer header, end('x')": (req) => { req.setHeader("trailer", "x-t"); req.end("x"); },
};
const kinds = {
  "no agent": () => ({ agent: false }),
  "keep-alive agent": () => ({ agent: new http.Agent({ keepAlive: true }) }),
  upgrade: () => ({ agent: false, headers: { connection: "upgrade", upgrade: "x-test" } }),
};

for (const [kind, options] of Object.entries(kinds)) {
  for (const method of ["GET", "POST"]) {
    for (const [label, send] of Object.entries(method === "POST" ? { ...sends, ...postSends } : sends)) {
      const opts = options();
      const head = new Promise((resolve) => (onHead = resolve));
      const done = new Promise((resolve) => {
        const req = http.request(
          { port, host: "127.0.0.1", method, ...opts, headers: { ...opts.headers, "x-v": V } },
          (res) => {
            res.resume();
            res.on("end", resolve);
          },
        );
        req.on("error", (e) => {
          console.log(`  error ${e.code}`);
          resolve();
        });
        send(req);
      });
      console.log(`${kind}, ${method} ${label}`.padEnd(50), await head);
      await done;
      if (opts.agent) opts.agent.destroy();
    }
  }
}
server.close();
