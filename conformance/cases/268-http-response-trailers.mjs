// A node:http response's trailers (res.addTrailers) go out after the last
// chunk of a chunked body, measured on node v22.22.2:
//
// - whether or not a Trailer header names them, and whether or not the
//   request said `TE: trailers` -- node sends every field it was given, a
//   repeated one (an array value, a [name, value] list) as often as it
//   repeats, each value one byte per code point;
// - not at all when the body is not chunked: a content-length (set, or the
//   one end('text') works out), a HEAD request, a 204.
//
// oam kept them and sent none (hyper sends only the trailers a Trailer
// header declares, and only to a request that said `TE: trailers`).
//
// Each response is its status line, its header lines other than date and
// connection (names lowercased and sorted), and its body as sent, trailer
// names lowercased: the case node writes names in is another concern.
import http from "node:http";
import net from "node:net";

const T = { "x-trail": "v1", "x-latin": "café" };
const routes = [
  ["write, addTrailers, end", (res) => { res.write("a"); res.addTrailers(T); res.end("b"); }],
  ["addTrailers, write, end", (res) => { res.addTrailers(T); res.write("a"); res.end(); }],
  ["writeHead, addTrailers, end('text')", (res) => { res.writeHead(200); res.addTrailers(T); res.end("text"); }],
  ["writeHead, addTrailers, end()", (res) => { res.writeHead(200); res.addTrailers(T); res.end(); }],
  ["Trailer header naming one, end('text')", (res) => { res.setHeader("trailer", "x-trail"); res.addTrailers(T); res.end("text"); }],
  ["array values", (res) => { res.write("a"); res.addTrailers({ "x-m": ["1", "2"], "x-one": ["solo"] }); res.end(); }],
  ["[name, value] list", (res) => { res.write("a"); res.addTrailers([["x-p", "1"], ["x-p", "2"]]); res.end(); }],
  ["addTrailers twice", (res) => { res.write("a"); res.addTrailers({ "x-first": "1" }); res.addTrailers({ "x-second": "2" }); res.end(); }],
  ["framing field names", (res) => { res.write("a"); res.addTrailers({ "content-length": "5", host: "h" }); res.end(); }],
  ["no fields", (res) => { res.write("a"); res.addTrailers({}); res.end(); }],
  ["addTrailers after end", (res) => { res.write("a"); res.end(); res.addTrailers(T); }],
  ["addTrailers, end('text') (a length)", (res) => { res.addTrailers(T); res.end("text"); }],
  ["content-length, write, addTrailers", (res) => { res.setHeader("content-length", "1"); res.write("a"); res.addTrailers(T); res.end(); }],
  ["204, addTrailers", (res) => { res.statusCode = 204; res.addTrailers(T); res.end(); }],
];
const requests = [
  ...routes.map(([label], i) => [label, "GET", i, ""]),
  ["TE: trailers request, write, addTrailers", "GET", 0, "te: trailers\r\n"],
  ["HEAD, write, addTrailers", "HEAD", 0, ""],
];

const server = http.createServer((req, res) => routes[Number(req.url.slice(1))][1](res));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

const esc = (text) =>
  [...Buffer.from(text, "latin1")]
    .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : b === 13 ? "\\r" : b === 10 ? "\\n" : `\\x${b.toString(16).padStart(2, "0")}`))
    .join("");
const lowerName = (line) => {
  const colon = line.indexOf(":");
  return colon < 0 ? line : line.slice(0, colon).toLowerCase() + line.slice(colon);
};

const fetchRaw = (method, path, extra) =>
  new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1", () =>
      socket.write(`${method} ${path} HTTP/1.1\r\nhost: x\r\n${extra}connection: close\r\n\r\n`),
    );
    let buf = Buffer.alloc(0);
    socket.on("data", (chunk) => (buf = Buffer.concat([buf, chunk])));
    socket.on("close", () => {
      const end = buf.indexOf("\r\n\r\n");
      const lines = buf.subarray(0, end).toString("latin1").split("\r\n");
      const status = lines.shift();
      const fields = lines.map(lowerName).filter((line) => !/^(date|connection):/.test(line)).sort();
      const body = buf.subarray(end + 4).toString("latin1").split("\r\n").map(lowerName).join("\r\n");
      resolve(`${status} | ${fields.join(" | ")} | body ${esc(body)}`);
    });
  });

for (const [label, method, route, extra] of requests) {
  console.log(`${label}:\n    ${await fetchRaw(method, `/${route}`, extra)}`);
}
server.close();
