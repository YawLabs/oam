// An http2 client stream receives the response's trailer section: node's
// ClientHttp2Stream emits 'trailers' (headers, flags, rawHeaders) -- a
// null-prototype object with node's toHeaderObject joins, flags 5
// (END_STREAM | END_HEADERS), the flat raw list -- before 'end'. The server
// sends it from a waitForTrailers stream's 'wantTrailers' with
// sendTrailers(), which holds node's checks (NOT_READY before 'wantTrailers'
// or without waitForTrailers, ALREADY_SENT, no pseudo-headers, an object)
// and records sentTrailers; an empty section sends no trailing HEADERS at
// all, and nothing listening for 'wantTrailers' sends that empty one. The
// compatibility API's res.setTrailer / addTrailers go out the same way.
// Measured on node v22.22.2.
//
// Regression guard: oam's http2 client never surfaced trailers, the server
// stream's sendTrailers() always threw NOT_READY and 'wantTrailers' never
// came, and the cleartext server's stream (its own, older class) had no
// sendTrailers at all.
import http2 from "node:http2";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const show = (x) => JSON.stringify(x && typeof x === "object" && !Array.isArray(x) ? Object.fromEntries(Object.entries(x)) : x);
const serverLog = [];
const S = (...a) => serverLog.push(a.join(" "));
const tryIt = (label, fn) => {
  try {
    fn();
    S(label, "ok");
  } catch (e) {
    S(label, e.code, e.name, e.message);
  }
};

// The raw stream API and the compatibility API each on a server of its own:
// a 'request' listener puts the compatibility layer -- and its own
// 'wantTrailers' listener -- on every stream of its server.
const server = http2.createServer();
server.on("stream", (stream, headers) => {
  const path = headers[":path"];
  stream.resume();
  S("sentTrailers at first", stream.sentTrailers);
  if (path === "/wait") {
    stream.respond({ ":status": 200 }, { waitForTrailers: true });
    tryIt("before wantTrailers", () => stream.sendTrailers({ a: "1" }));
    stream.on("wantTrailers", () => {
      tryIt("pseudo", () => stream.sendTrailers({ ":status": 200 }));
      tryIt("string", () => stream.sendTrailers("x"));
      tryIt("connection", () => stream.sendTrailers({ connection: "close" }));
      tryIt("single value", () => stream.sendTrailers({ "content-type": ["a", "b"] }));
      // Cookies of 20 bytes and more: nghttp2 sends a shorter one
      // never-indexed, which node lists in [http2.sensitiveHeaders] (and oam
      // does not -- docs/node-divergences.md entry 42).
      tryIt("send", () => stream.sendTrailers({ "x-t": "1", "X-Arr": ["a", "b"], "set-cookie": ["c1", "c2"], cookie: ["session=0123456789abcdef", "theme=0123456789abcdefgh"], num: 5 }));
      S("sentTrailers", show(stream.sentTrailers), Object.getPrototypeOf(stream.sentTrailers));
      tryIt("again", () => stream.sendTrailers({ b: "2" }));
    });
    stream.end("body");
  } else if (path === "/nowait") {
    stream.respond({ ":status": 200 });
    tryIt("without waitForTrailers", () => stream.sendTrailers({ a: "1" }));
    stream.end("x");
  } else if (path === "/empty") {
    stream.respond({ ":status": 200 }, { waitForTrailers: true });
    stream.on("wantTrailers", () => stream.sendTrailers({}));
    stream.end("x");
  } else if (path === "/unheard") {
    // Nothing listens for 'wantTrailers': node sends the empty section.
    stream.respond({ ":status": 200 }, { waitForTrailers: true });
    stream.end("x");
  } else if (path === "/later") {
    stream.respond({ ":status": 200 }, { waitForTrailers: true });
    stream.on("wantTrailers", () => setTimeout(() => stream.sendTrailers({ late: "yes" }), 20));
    stream.write("a");
    setTimeout(() => stream.end("b"), 10);
  } else if (path === "/end-stream") {
    // endStream: there is no data and no trailers, whatever waitForTrailers says.
    stream.on("wantTrailers", () => S("unexpected wantTrailers"));
    stream.respond({ ":status": 200 }, { waitForTrailers: true, endStream: true });
  }
});
const compatServer = http2.createServer((req, res) => {
  req.resume();
  if (req.url === "/compat") {
    res.setTrailer("x-res", "v");
    res.addTrailers({ "X-Two": ["p", "q"] });
    res.end("done");
  } else if (req.url === "/compat-head-first") {
    res.writeHead(200);
    res.setTrailer("x-res", "v");
    res.end();
  } else if (req.url === "/compat-no-body") {
    // Headers and end at once: END_STREAM on the HEADERS, no trailers.
    res.setTrailer("x-res", "v");
    res.end();
  } else {
    res.end("plain");
  }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
await new Promise((r) => compatServer.listen(0, "127.0.0.1", r));
const client = http2.connect("http://127.0.0.1:" + server.address().port);
const compatClient = http2.connect("http://127.0.0.1:" + compatServer.address().port);
const paths = ["/wait", "/nowait", "/empty", "/unheard", "/later", "/end-stream", "/compat", "/compat-head-first", "/compat-no-body", "/compat-plain"];
for (const path of paths) {
  const events = [];
  const c = path.startsWith("/compat") ? compatClient : client;
  await new Promise((resolve) => {
    const req = c.request({ ":path": path });
    req.on("response", (h, flags) => events.push("response " + flags));
    req.on("trailers", (...args) => {
      const [t, flags, raw] = args;
      events.push(`trailers nargs=${args.length} ${show(t)} proto=${Object.getPrototypeOf(t)} flags=${flags} raw=${JSON.stringify(raw)} sensitive=${JSON.stringify(t[http2.sensitiveHeaders])}`);
    });
    let body = "";
    req.on("data", (d) => (body += d));
    req.on("end", () => events.push("end " + JSON.stringify(body)));
    req.on("close", () => {
      events.push("close " + req.rstCode);
      resolve();
    });
  });
  // The server's side of it is over by now (its stream closes first).
  await new Promise((r) => setTimeout(r, 30));
  console.log(path);
  for (const e of events) console.log("  client " + e);
  for (const s of serverLog.splice(0)) console.log("  server " + s);
}
client.close();
compatClient.close();
server.close();
compatServer.close();
