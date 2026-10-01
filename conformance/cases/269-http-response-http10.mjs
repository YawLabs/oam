// A node:http server's response to an HTTP/1.0 request, measured on node
// v22.22.2:
//
// - the status line says HTTP/1.1, the version node speaks (oam's said
//   HTTP/1.0: hyper answers in the peer's version);
// - `Connection: close` is sent (node keeps no HTTP/1.0 connection) where
//   the handler did not set a connection header; oam sent none;
// - a body whose length end() did not set in a header is ended by closing
//   the connection: no content-length for end('text') (oam sent one), no
//   chunks for write();
// - unless the request said `TE: chunked`: then the body is chunked as for
//   HTTP/1.1 -- write(), writeHead() then end(), trailers -- and end('text')
//   sends its length (oam ended every such body by closing);
// - a transfer-encoding header the handler set is honoured (oam's was
//   dropped);
// - a Trailer header where the body cannot be chunked throws
//   ERR_HTTP_TRAILER_INVALID from whatever builds the head (end(), write(),
//   writeHead()), leaving headersSent false; over HTTP/1.1 too, for a 204
//   or a response with a content-length. oam sent the head as it stood.
//
// Each response is its status line, its header lines other than date
// (names lowercased and sorted: the order and the case node writes them in
// are other concerns), and its body as sent. Trailer names are written in
// title case, which oam's trailers keep (270 has the case rules).
import http from "node:http";
import net from "node:net";

const refused = (build, recover) => (res) => {
  try {
    build(res);
    console.log("  no throw");
  } catch (e) {
    console.log(`  ${e.name} ${e.code}: ${e.message} | headersSent=${res.headersSent}`);
    recover(res);
  }
};
const routes = [
  ["end()", (res) => res.end()],
  ["end('text')", (res) => res.end("text")],
  ["end(buffer)", (res) => res.end(Buffer.from("ab"))],
  ["write('a'), end('b')", (res) => { res.write("a"); res.end("b"); }],
  ["content-length, end('text')", (res) => { res.setHeader("content-length", "4"); res.end("text"); }],
  ["writeHead(200), end('text')", (res) => { res.writeHead(200); res.end("text"); }],
  ["write, addTrailers, end", (res) => { res.write("a"); res.addTrailers({ "X-T": "1" }); res.end(); }],
  ["transfer-encoding header, write", (res) => { res.setHeader("transfer-encoding", "chunked"); res.write("a"); res.end(); }],
  // The handler's own connection header is the only one sent.
  ["connection header, end('x')", (res) => { res.setHeader("connection", "close"); res.end("x"); }],
  ["statusMessage, end()", (res) => { res.statusMessage = "Fine"; res.end(); }],
  ["Trailer header, end('x')", refused(
    (res) => { res.setHeader("trailer", "x-t"); res.end("x"); },
    (res) => { res.removeHeader("trailer"); res.end("recovered"); },
  )],
  ["Trailer header, write('x')", refused(
    (res) => { res.setHeader("trailer", "x-t"); res.write("x"); res.end(); },
    (res) => { res.removeHeader("trailer"); res.end("recovered"); },
  )],
  ["writeHead(Trailer)", refused(
    (res) => { res.writeHead(200, { trailer: "x-t" }); res.end("x"); },
    (res) => { res.writeHead(201); res.end("recovered"); },
  )],
  // Chunked (and its trailers sent) only for a request saying `TE: chunked`.
  ["Trailer header, addTrailers, end('x')", refused(
    (res) => { res.setHeader("trailer", "x-t"); res.addTrailers({ "X-T": "1" }); res.end("x"); },
    (res) => { res.removeHeader("trailer"); res.end("recovered"); },
  )],
];
const http11 = [
  ["1.1: writeHead(content-length, Trailer)", refused(
    (res) => res.writeHead(200, { "content-length": "1", trailer: "x" }),
    (res) => { res.writeHead(500); res.end("x"); },
  )],
  ["1.1: 204 with a Trailer header", refused(
    (res) => { res.statusCode = 204; res.setHeader("trailer", "x"); res.end(); },
    (res) => { res.removeHeader("trailer"); res.end(); },
  )],
];
const all = [...routes, ...http11];

const server = http.createServer((req, res) => all[Number(req.url.slice(1))][1](res));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

const esc = (buf) =>
  [...buf]
    .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : b === 13 ? "\\r" : b === 10 ? "\\n" : `\\x${b.toString(16).padStart(2, "0")}`))
    .join("");

const fetchRaw = (path, version, extra) =>
  new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1", () =>
      socket.write(`GET ${path} HTTP/${version}\r\nhost: x\r\n${extra}\r\n`),
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
        .filter((line) => !/^date:/.test(line))
        .sort();
      resolve(`${status} | ${fields.join(" | ")} | body ${esc(buf.subarray(end + 4))}`);
    });
  });

for (let i = 0; i < routes.length; i++) {
  console.log(`1.0: ${routes[i][0]}`);
  console.log(`    ${await fetchRaw(`/${i}`, "1.0", "")}`);
}
for (let i = 0; i < routes.length; i++) {
  console.log(`1.0 TE chunked: ${routes[i][0]}`);
  console.log(`    ${await fetchRaw(`/${i}`, "1.0", "te: chunked\r\n")}`);
}
for (let i = routes.length; i < all.length; i++) {
  console.log(all[i][0]);
  console.log(`    ${await fetchRaw(`/${i}`, "1.1", "connection: close\r\n")}`);
}
server.close();
