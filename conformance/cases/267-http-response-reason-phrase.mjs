// A node:http response's status line carries the response's status message
// -- res.statusMessage, or writeHead()'s reason -- not always the status
// code's standard reason phrase. Measured on node v22.22.2. oam sent the
// standard one (hyper's) whatever the message said, and the message's
// spelling where node's table and hyper's differ (418 `I'm a Teapot`, a
// code with no standard phrase: `unknown`).
//
// The phrase is part of the head, so it goes out in the head's bytes: joined
// to a UTF-8 string body as UTF-8, otherwise one byte per code point (see
// 251-http-response-header-bytes).
//
// Each line is the response's status line, bytes outside printable ASCII
// as \xNN.
import http from "node:http";
import net from "node:net";

const routes = [
  ["statusMessage, end('x')", (res) => { res.statusMessage = "Fine Thanks"; res.end("x"); }],
  ["statusMessage, end()", (res) => { res.statusMessage = "Fine"; res.end(); }],
  ["statusMessage, write('x')", (res) => { res.statusMessage = "Streaming"; res.write("x"); res.end(); }],
  ["writeHead(201, reason)", (res) => { res.writeHead(201, "Made It"); res.end("x"); }],
  ["writeHead(202, reason, headers)", (res) => { res.writeHead(202, "Later", { "x-a": "1" }); res.end(); }],
  ["writeHead(200, '')", (res) => { res.writeHead(200, ""); res.end("x"); }],
  ["statusMessage '', statusCode 404", (res) => { res.statusMessage = ""; res.statusCode = 404; res.end(); }],
  ["statusMessage 'OK', statusCode 404", (res) => { res.statusCode = 404; res.statusMessage = "OK"; res.end(); }],
  ["statusMessage, then statusCode 500", (res) => { res.statusMessage = "X"; res.statusCode = 500; res.end(); }],
  ["statusMessage with a tab", (res) => { res.statusMessage = "a\tb"; res.end(); }],
  ["statusMessage caf\\xe9, end('x')", (res) => { res.statusMessage = "café"; res.end("x"); }],
  ["statusMessage caf\\xe9, end(buffer)", (res) => { res.statusMessage = "café"; res.end(Buffer.from("x")); }],
  ["statusMessage caf\\xe9, write('x')", (res) => { res.statusMessage = "café"; res.write("x"); res.end(); }],
  ["statusMessage caf\\xe9, flushHeaders()", (res) => { res.statusMessage = "café"; res.flushHeaders(); res.end(); }],
  ["writeHead(200, 'caf\\xe9'), end('x')", (res) => { res.writeHead(200, "café"); res.end("x"); }],
  ["statusCode 418", (res) => { res.statusCode = 418; res.end(); }],
  ["statusCode 299", (res) => { res.statusCode = 299; res.end(); }],
  ["writeHead(599)", (res) => { res.writeHead(599); res.end(); }],
  ["statusCode 404", (res) => { res.statusCode = 404; res.end(); }],
  ["statusCode 200", (res) => res.end()],
];

const server = http.createServer((req, res) => routes[Number(req.url.slice(1))][1](res));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

const statusLine = (method, path) =>
  new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1", () =>
      socket.write(`${method} ${path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n`),
    );
    let buf = Buffer.alloc(0);
    socket.on("data", (chunk) => (buf = Buffer.concat([buf, chunk])));
    socket.on("close", () =>
      resolve(
        [...buf.subarray(0, buf.indexOf("\r\n"))]
          .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : `\\x${b.toString(16).padStart(2, "0")}`))
          .join(""),
      ),
    );
  });

for (let i = 0; i < routes.length; i++) {
  console.log(routes[i][0].padEnd(40), await statusLine("GET", `/${i}`));
}
// A HEAD response's head is never joined to a body.
console.log("HEAD: statusMessage caf\\xe9, end('x')".padEnd(40), await statusLine("HEAD", "/10"));
server.close();
