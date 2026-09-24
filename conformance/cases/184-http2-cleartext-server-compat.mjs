// The cleartext http2.createServer (h2c) has the compatibility API, the same
// as http2.createSecureServer (#200). createServer(handler) puts the handler
// on 'request' and installs the 'stream' bridge, so it is called with an
// Http2ServerRequest and Http2ServerResponse (url, method, writeHead, end);
// a server whose only listener is 'request' answers the request; and the raw
// 'stream' API keeps working alongside a 'request' listener. Up to 0.16.3 the
// h2c server put the handler on 'stream' and had none of this, so a
// `(req, res)` server never answered. Measured on Node v22.22.2.
import http2 from "node:http2";

const watchdog = setTimeout(() => { console.log("WATCHDOG"); process.exit(9); }, 20000);
const listeners = (server) => "request=" + server.listenerCount("request") + " stream=" + server.listenerCount("stream");

// createServer(handler): the handler gets the compatibility (req, res).
{
  const seen = {};
  const server = http2.createServer((a, b) => {
    seen.argA = a && a.constructor && a.constructor.name;
    seen.argB = b && b.constructor && b.constructor.name;
    seen.url = a.url; seen.method = a.method;
    seen.bEnd = typeof b.end; seen.bWriteHead = typeof b.writeHead; seen.aRespond = typeof a.respond;
    b.writeHead(200, { "content-type": "text/plain" });
    b.end("hi " + a.url);
  });
  console.log("createServer(handler): " + listeners(server));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  await new Promise((resolve) => {
    const c = http2.connect("http://127.0.0.1:" + port);
    const req = c.request({ ":path": "/hello", ":method": "GET" });
    let body = "";
    req.setEncoding("utf8");
    req.on("response", (h) => (seen.status = h[":status"]));
    req.on("data", (d) => (body += d));
    req.on("end", () => { seen.body = body; c.close(); });
    req.on("close", resolve);
  });
  server.close();
  console.log("  argA=" + seen.argA + " argB=" + seen.argB + " a.url=" + JSON.stringify(seen.url) +
    " a.method=" + JSON.stringify(seen.method));
  console.log("  b.end=" + seen.bEnd + " b.writeHead=" + seen.bWriteHead + " a.respond=" + seen.aRespond +
    " status=" + seen.status + " body=" + JSON.stringify(seen.body));
}

// createServer() then on('request'): the common (req, res) shape answers.
{
  const server = http2.createServer();
  server.on("request", (req, res) => { res.statusCode = 201; res.end("only-request " + req.url); });
  console.log("on('request'): " + listeners(server));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  await new Promise((resolve) => {
    const c = http2.connect("http://127.0.0.1:" + port);
    const req = c.request({ ":path": "/only", ":method": "GET" });
    let body = "", status;
    req.setEncoding("utf8");
    req.on("response", (h) => (status = h[":status"]));
    req.on("data", (d) => (body += d));
    req.on("end", () => { console.log("  request-only status=" + status + " body=" + JSON.stringify(body)); c.close(); });
    req.on("close", resolve);
  });
  server.close();
}

// The raw 'stream' API works alongside a 'request' listener.
{
  const server = http2.createServer();
  server.on("request", () => {});
  server.on("stream", (stream, headers) => { console.log("  stream fired path=" + headers[":path"]); stream.respond({ ":status": 200 }); stream.end("via-stream"); });
  console.log("both: " + listeners(server));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;
  await new Promise((resolve) => {
    const c = http2.connect("http://127.0.0.1:" + port);
    const req = c.request({ ":path": "/both" });
    req.on("response", () => {});
    req.on("close", () => { c.close(); resolve(); });
    req.resume();
  });
  server.close();
}

clearTimeout(watchdog);
process.exit(0);
