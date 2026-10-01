// node's writeHead() builds the response head there and then: the status,
// the fields and how the body is framed are fixed, whatever the response
// does next. Measured on node v22.22.2.
//
// - Headers handed to writeHead() on a response no header method has
//   touched go into the head only: getHeader(), hasHeader(),
//   getHeaderNames() and getHeaders() never see them. (Through setHeader()
//   first, they do.) oam kept them in the header store.
// - The body is framed when the head is built. writeHead() comes before
//   end() knows the body's length, so `writeHead(200); end('text')` is
//   chunked -- as is `writeHead(200); end()`, which sends the last chunk
//   alone. A content-length given to writeHead() frames it by length.
//   oam framed every end() by the length it was given.
// - A statusCode or statusMessage set after writeHead() does not change
//   the status line. oam sent the later one.
// - A HEAD request's, a 204's and a 304's response carries no body and no
//   content-length for what end() was given.
//
// Each response is shown as its status line, its header lines other than
// date and connection (names lowercased and sorted: the order and the case
// node writes them in are other concerns), and its body bytes as sent.
import http from "node:http";
import net from "node:net";

const show = (res, tag) =>
  console.log(
    `  ${tag}: headersSent=${res.headersSent} getHeader=${JSON.stringify(res.getHeader("x-a"))}` +
      ` hasHeader=${res.hasHeader("x-a")} names=${JSON.stringify(res.getHeaderNames())}` +
      ` headers=${JSON.stringify(res.getHeaders())} statusCode=${res.statusCode}`,
  );

const routes = [
  ["writeHead(object), end('text')", (res) => { res.writeHead(200, { "x-a": "1" }); show(res, "object"); res.end("text"); }],
  ["writeHead(flat list), end('text')", (res) => { res.writeHead(200, ["x-a", "1"]); show(res, "flat"); res.end("text"); }],
  ["writeHead(pairs), end('x')", (res) => { res.writeHead(200, [["x-a", "1"], ["x-a", "2"]]); show(res, "pairs"); res.end("x"); }],
  ["setHeader, writeHead(object), end('text')", (res) => { res.setHeader("x-b", "2"); res.writeHead(200, { "x-a": "1" }); show(res, "progressive"); res.end("text"); }],
  ["setHeader, writeHead(201), end('x')", (res) => { res.setHeader("x-a", "0"); res.writeHead(201); show(res, "progressive bare"); res.end("x"); }],
  ["writeHead(200), end()", (res) => { res.writeHead(200); res.end(); }],
  ["writeHead(200), end('')", (res) => { res.writeHead(200); res.end(""); }],
  ["writeHead(200), end(buffer)", (res) => { res.writeHead(200); res.end(Buffer.from("ab")); }],
  ["writeHead(content-length), end('text')", (res) => { res.writeHead(200, { "content-length": "4" }); res.end("text"); }],
  ["writeHead(transfer-encoding), end('x')", (res) => { res.writeHead(200, { "transfer-encoding": "chunked" }); res.end("x"); }],
  ["writeHead(200), write('a'), end('b')", (res) => { res.writeHead(200); res.write("a"); res.end("b"); }],
  ["writeHead(200), flushHeaders(), end('x')", (res) => { res.writeHead(200); res.flushHeaders(); res.end("x"); }],
  ["writeHead(200), statusCode = 404, end('x')", (res) => {
    res.writeHead(200);
    res.statusCode = 404;
    res.statusMessage = "Nope";
    show(res, "after");
    res.end("x");
  }],
  ["content-length removed, writeHead(200), end('x')", (res) => { res.removeHeader("content-length"); res.writeHead(200); res.end("x"); }],
  ["writeHead(204), end()", (res) => { res.writeHead(204); res.end(); }],
  ["writeHead(304), end('x')", (res) => { res.writeHead(304); res.end("x"); }],
  ["statusCode 204, end('x')", (res) => { res.statusCode = 204; res.end("x"); }],
  ["end('text')", (res) => res.end("text")],
  ["end()", (res) => res.end()],
  ["write('a'), end('b')", (res) => { res.write("a"); res.end("b"); }],
];
const head = [
  ["HEAD: writeHead(200), end('x')", (res) => { res.writeHead(200); res.end("x"); }],
  ["HEAD: end('x')", (res) => res.end("x")],
];
const all = [...routes, ...head];

const server = http.createServer((req, res) => all[Number(req.url.slice(1))][1](res));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

const esc = (buf) =>
  [...buf]
    .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : b === 13 ? "\\r" : b === 10 ? "\\n" : `\\x${b.toString(16).padStart(2, "0")}`))
    .join("");

const fetchRaw = (method, path) =>
  new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1", () =>
      socket.write(`${method} ${path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n`),
    );
    let buf = Buffer.alloc(0);
    socket.on("data", (chunk) => (buf = Buffer.concat([buf, chunk])));
    socket.on("close", () => {
      const end = buf.indexOf("\r\n\r\n");
      const lines = buf.subarray(0, end).toString("latin1").split("\r\n");
      const status = lines.shift();
      const fields = lines
        .map((line) => {
          const colon = line.indexOf(":");
          return line.slice(0, colon).toLowerCase() + line.slice(colon);
        })
        .filter((line) => !/^(date|connection):/.test(line))
        .sort();
      resolve(`${status} | ${fields.join(" | ")} | body ${esc(buf.subarray(end + 4))}`);
    });
  });

for (let i = 0; i < all.length; i++) {
  const method = i < routes.length ? "GET" : "HEAD";
  console.log(`${all[i][0]}:\n    ${await fetchRaw(method, `/${i}`)}`);
}
server.close();
