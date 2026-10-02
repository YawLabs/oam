// A node:http response writes each header name in the case the handler
// spelled it -- the spelling it was last set with (setHeader), the first
// one (appendHeader to a name already there), each one given to writeHead()
// -- and the names it adds itself as `Content-Length`, `Transfer-Encoding`,
// `Connection`, `Date`. getRawHeaderNames() answers the stored spellings.
// Measured on node v22.22.2. oam wrote every name lowercase (hyper's
// HeaderName), and had no getRawHeaderNames().
//
// Each response is its head's lines in order, the Date line left out (node
// writes it after the handler's fields, hyper last; its value is the time),
// and its body.
import http from "node:http";
import net from "node:net";

const show = (res) =>
  console.log(`  names=${JSON.stringify(res.getHeaderNames())} raw=${JSON.stringify(res.getRawHeaderNames())}`);

const routes = [
  ["setHeader mixed, lower, upper", (res) => {
    res.setHeader("X-Mixed-Case", "1");
    res.setHeader("lower", "2");
    res.setHeader("UPPER", "3");
    show(res);
    res.end("x");
  }],
  ["setHeader X-A then x-a", (res) => { res.setHeader("X-A", "1"); res.setHeader("x-a", "2"); show(res); res.end(); }],
  ["setHeader x-b then X-B", (res) => { res.setHeader("x-b", "1"); res.setHeader("X-B", "2"); show(res); res.end(); }],
  ["appendHeader X-C then x-c", (res) => { res.appendHeader("X-C", "1"); res.appendHeader("x-c", "2"); show(res); res.end(); }],
  ["setHeader x-d, appendHeader X-D", (res) => { res.setHeader("x-d", "1"); res.appendHeader("X-D", "2"); show(res); res.end(); }],
  ["removeHeader then set again", (res) => { res.setHeader("X-E", "1"); res.removeHeader("x-e"); res.setHeader("x-e", "2"); show(res); res.end(); }],
  ["writeHead object", (res) => { res.writeHead(200, { "X-Fast": "1", "Content-Type": "text/plain" }); res.end("x"); }],
  ["writeHead flat list", (res) => { res.writeHead(200, ["X-Flat", "1", "x-flat", "2"]); res.end("x"); }],
  ["writeHead pairs", (res) => { res.writeHead(200, [["X-R", "1"], ["x-r", "2"], ["X-r", "3"]]); res.end(); }],
  ["setHeader, writeHead flat list", (res) => { res.setHeader("X-Prog", "0"); res.writeHead(200, ["X-PROG", "1"]); show(res); res.end("x"); }],
  ["setHeader, writeHead object", (res) => { res.setHeader("x-prog", "0"); res.writeHead(200, { "X-Prog": "1" }); show(res); res.end("x"); }],
  ["content-length lower", (res) => { res.setHeader("content-length", "1"); res.end("x"); }],
  ["CONTENT-LENGTH upper", (res) => { res.setHeader("CONTENT-LENGTH", "1"); res.end("x"); }],
  ["connection lower", (res) => { res.setHeader("connection", "close"); res.end("x"); }],
  ["transfer-encoding lower", (res) => { res.setHeader("transfer-encoding", "chunked"); res.end("x"); }],
  ["setHeaders(Map)", (res) => { res.setHeaders(new Map([["X-Map", "1"]])); show(res); res.end("x"); }],
  ["setHeaders(Headers)", (res) => { res.setHeaders(new Headers([["X-Hdrs", "1"]])); show(res); res.end("x"); }],
  ["Set-Cookie list", (res) => { res.setHeader("Set-Cookie", ["a=1", "b=2"]); res.end("x"); }],
  ["write, Title-Case trailer", (res) => { res.setHeader("X-Stream", "1"); res.write("a"); res.addTrailers({ "X-Trail": "1" }); res.end(); }],
  ["no fields", (res) => res.end("x")],
];

const server = http.createServer((req, res) => routes[Number(req.url.slice(1))][1](res));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

const fetchRaw = (path, version) =>
  new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1", () =>
      socket.write(`GET ${path} HTTP/${version}\r\nhost: x\r\nconnection: close\r\n\r\n`),
    );
    let buf = Buffer.alloc(0);
    socket.on("data", (chunk) => (buf = Buffer.concat([buf, chunk])));
    socket.on("close", () => {
      const end = buf.indexOf("\r\n\r\n");
      const lines = buf.subarray(0, end).toString("latin1").split("\r\n").filter((line) => !/^date:/i.test(line));
      resolve(`${lines.join(" | ")} || ${JSON.stringify(buf.subarray(end + 4).toString("latin1"))}`);
    });
  });

for (let i = 0; i < routes.length; i++) {
  console.log(routes[i][0]);
  console.log(`    ${await fetchRaw(`/${i}`, "1.1")}`);
}
// The fields node adds for an HTTP/1.0 client.
console.log("1.0: no fields");
console.log(`    ${await fetchRaw(`/${routes.length - 1}`, "1.0")}`);
server.close();
