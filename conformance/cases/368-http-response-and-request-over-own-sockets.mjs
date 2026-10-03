// What a test double needs from http to stand in for the network, as nock
// 14 (@mswjs/interceptors) uses it:
//  - a ServerResponse built over `new IncomingMessage(socket)` and given a
//    socket with assignSocket() writes the whole HTTP/1.1 message to that
//    socket, framed as node's _storeHeader frames it -- oam wrote only the
//    body bytes;
//  - a ClientRequest whose agent hands back a socket with a write() of its
//    own writes the request into it while it is still connecting -- oam
//    waited for 'connect', which such a socket only emits once it has the
//    request -- and reads the response the socket pushes, from a socket
//    with no handle (oam's resume() failed it with "read handle 0 is gone").
// (#207)
import http from "node:http";
import net from "node:net";
import { Writable } from "node:stream";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const { HTTPParser } = require("_http_common");

function respond(label, setup, act) {
  const req = new http.IncomingMessage(new net.Socket());
  setup(req);
  const res = new http.ServerResponse(req);
  res.sendDate = false;
  const out = [];
  res.assignSocket(new Writable({
    write(chunk, encoding, callback) {
      out.push(Buffer.from(chunk, encoding));
      callback();
    },
  }));
  act(res);
  return new Promise((resolve) => setImmediate(() => {
    console.log(label + ": " + JSON.stringify(Buffer.concat(out).toString("latin1")));
    resolve();
  }));
}
const v11 = (req) => {
  req.httpVersionMajor = 1;
  req.httpVersionMinor = 1;
  req.httpVersion = "1.1";
};
const v10 = (req) => {
  req.httpVersionMajor = 1;
  req.httpVersionMinor = 0;
  req.httpVersion = "1.0";
};
const none = () => {};

await respond("no version, end", none, (res) => res.end("hi"));
await respond("no version, write then end", none, (res) => {
  res.writeHead(201, { "X-A": "1" });
  res.write("a");
  res.end("b");
});
await respond("no version, connection and date removed", none, (res) => {
  res.removeHeader("connection");
  res.removeHeader("date");
  res.writeHead(200, "OK", [["X-Raw", "v"], ["x-raw", "w"]]);
  res.end("body");
});
await respond("1.1, end", v11, (res) => res.end("hi"));
await respond("1.1, write then end", v11, (res) => {
  res.write("a");
  res.end("bc");
});
await respond("1.1, writeHead then end", v11, (res) => {
  res.writeHead(200);
  res.end("hi");
});
await respond("1.1, HEAD", (req) => {
  v11(req);
  req.method = "HEAD";
}, (res) => res.end("ignored"));
await respond("1.1, 204", v11, (res) => {
  res.statusCode = 204;
  res.end();
});
await respond("1.1, 304 after a write", v11, (res) => {
  res.writeHead(304);
  res.write("x");
  res.end();
});
await respond("1.1, content-length set", v11, (res) => {
  res.setHeader("Content-Length", 2);
  res.write("h");
  res.end("i");
});
await respond("1.1, connection removed", v11, (res) => {
  res.removeHeader("connection");
  res.write("a");
  res.end();
});
await respond("1.1, trailers", v11, (res) => {
  res.setHeader("Trailer", "X-T");
  res.addTrailers({ "X-T": "done" });
  res.write("a");
  res.end();
});
await respond("1.1, flushHeaders", v11, (res) => {
  res.setHeader("X-F", "1");
  res.flushHeaders();
  res.end("z");
});
await respond("1.1, empty end", v11, (res) => res.end());
await respond("1.0, write then end", v10, (res) => {
  res.write("a");
  res.end("b");
});
await respond("1.0 asking for chunks", (req) => {
  v10(req);
  req.headers.te = "chunked";
}, (res) => {
  res.write("a");
  res.end("b");
});
await respond("utf8 body joined to the head", v11, (res) => {
  res.setHeader("X-C", "caf\u00e9");
  res.end("\u00e9");
});
{
  const req = new http.IncomingMessage(new net.Socket());
  const res = new http.ServerResponse(req);
  const out = [];
  res.assignSocket(new Writable({ write(c, e, cb) { out.push(Buffer.from(c)); cb(); } }));
  res.end();
  await new Promise((r) => setImmediate(r));
  const head = Buffer.concat(out).toString("latin1");
  console.log("Date sent by default:", /\r\nDate: [A-Z][a-z]{2}, \d\d [A-Z][a-z]{2} \d{4} \d\d:\d\d:\d\d GMT\r\n/.test(head));
}
{
  const socket = new net.Socket();
  const req = new http.IncomingMessage(socket);
  console.log("IncomingMessage(socket):", req.socket === socket, req.connection === socket,
    req.httpVersion, req.method, JSON.stringify(req.url), req.statusCode, req.complete,
    JSON.stringify(req.headers), JSON.stringify(req.rawHeaders));
  req._addHeaderLines(["Set-Cookie", "a", "set-cookie", "b", "Cookie", "c", "cookie", "d", "Host", "h1",
    "host", "h2", "X-Y", "1", "x-y", "2"], 16);
  console.log("_addHeaderLines:", JSON.stringify(req.headers));
}

// A ClientRequest over a socket that answers what it is written, as nock's
// MockHttpSocket does: 'connect' comes only after the request is in.
class AnsweringSocket extends net.Socket {
  constructor() {
    super();
    this.connecting = true;
    this.requestParser = new HTTPParser();
    this.requestParser.initialize(HTTPParser.REQUEST, {});
    this.requestParser[HTTPParser.kOnHeadersComplete] = (major, minor, headers, method, url) => {
      this.head = { method, url, headers };
    };
    this.body = "";
    this.requestParser[HTTPParser.kOnBody] = (chunk) => {
      this.body += chunk.toString("latin1");
    };
    this.requestParser[HTTPParser.kOnMessageComplete] = () => this.answer();
  }
  write(chunk, encoding, callback) {
    if (typeof encoding === "function") callback = encoding;
    this.wroteWhileConnecting = this.wroteWhileConnecting || this.connecting;
    this.requestParser.execute(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk, encoding));
    if (typeof callback === "function") callback();
    return true;
  }
  answer() {
    const names = [];
    for (let i = 0; i < this.head.headers.length; i += 2) names.push(this.head.headers[i].toLowerCase());
    console.log("socket got:", this.head.method, this.head.url, names.sort().join(","),
      JSON.stringify(this.body), "while connecting:", this.wroteWhileConnecting === true);
    this.connecting = false;
    this.emit("connect");
    this.push("HTTP/1.1 200 OK\r\nContent-Length: 7\r\nX-From: socket\r\n\r\nanswer!");
    this.push(null);
  }
}
const agent = new http.Agent();
agent.createConnection = () => new AnsweringSocket();
await new Promise((resolve, reject) => {
  const req = http.request({ host: "example.invalid", path: "/ask?q=1", method: "POST", agent,
    headers: { "content-type": "text/plain" } }, (res) => {
    let body = "";
    res.setEncoding("utf8");
    res.on("data", (c) => (body += c));
    res.on("end", () => {
      console.log("response:", res.statusCode, res.headers["x-from"], body);
      resolve();
    });
  });
  req.on("error", reject);
  req.end("question");
});
