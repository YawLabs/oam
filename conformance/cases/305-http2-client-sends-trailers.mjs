// An http2 client stream sends request trailers, and the server receives
// them: request(headers, { waitForTrailers: true }) holds the request body's
// end back once its data is out and emits 'wantTrailers'; sendTrailers()
// then ends it (sentTrailers records the section; NOT_READY before
// 'wantTrailers', ALREADY_SENT after), an empty section and an unheard
// 'wantTrailers' end it with no trailing HEADERS. The server stream emits
// 'trailers' (headers, flags, rawHeaders) before its 'end', and the
// compatibility API's req.trailers / req.rawTrailers -- {} and [] at the
// start -- are filled in by then. Measured on node v22.22.2.
//
// Regression guard: oam's http2 client had no waitForTrailers, 'wantTrailers'
// or sendTrailers(), and its servers dropped a request's trailer section
// (no 'trailers', req.trailers left empty).
import http2 from "node:http2";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const show = (x) => JSON.stringify(x && typeof x === "object" && !Array.isArray(x) ? Object.fromEntries(Object.entries(x)) : x);
const serverLog = [];
const S = (...a) => serverLog.push(a.join(" "));

// The raw stream API and the compatibility API each on a server of its own.
const server = http2.createServer();
server.on("stream", (stream) => {
  let body = "";
  stream.on("data", (d) => (body += d));
  stream.on("trailers", (...args) => {
    const [t, flags, raw] = args;
    S(`trailers nargs=${args.length} ${show(t)} proto=${Object.getPrototypeOf(t)} flags=${flags} raw=${JSON.stringify(raw)}`);
  });
  stream.on("end", () => {
    S("end " + JSON.stringify(body));
    stream.respond({ ":status": 200 });
    stream.end("ok");
  });
});
const compatServer = http2.createServer((req, res) => {
  S("at the request", show(req.trailers), JSON.stringify(req.rawTrailers), Object.getPrototypeOf(req.trailers) === Object.prototype);
  req.resume();
  req.on("end", () => {
    S("at the end", show(req.trailers), JSON.stringify(req.rawTrailers));
    res.end("ok");
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
await new Promise((r) => compatServer.listen(0, "127.0.0.1", r));
const client = http2.connect("http://127.0.0.1:" + server.address().port);
const compatClient = http2.connect("http://127.0.0.1:" + compatServer.address().port);

const fields = { "x-c": "1", "X-D": ["a", "b"], cookie: ["session=0123456789abcdef", "theme=0123456789abcdefgh"] };
const runs = [
  ["raw: trailers", client, { waitForTrailers: true }, { want: (r) => r.sendTrailers(fields) }],
  ["raw: empty trailers", client, { waitForTrailers: true }, { want: (r) => r.sendTrailers({}) }],
  ["raw: nobody listens", client, { waitForTrailers: true }, {}],
  ["raw: no waitForTrailers", client, {}, {}],
  ["raw: checks", client, { waitForTrailers: true }, {
    body: (r, log) => {
      try { r.sendTrailers({ a: "1" }); } catch (e) { log("before wantTrailers " + e.code); }
      r.write("p");
      r.end("q");
    },
    want: (r, log) => {
      try { r.sendTrailers({ ":path": "/" }); } catch (e) { log("pseudo " + e.code); }
      // Sent later than 'wantTrailers', the trailers still go out.
      setTimeout(() => {
        r.sendTrailers({ late: "1" });
        try { r.sendTrailers({ again: "1" }); } catch (e) { log("again " + e.code); }
      }, 20);
    },
  }],
  ["compat: trailers", compatClient, { waitForTrailers: true }, { want: (r) => r.sendTrailers(fields) }],
  ["compat: none", compatClient, {}, {}],
];
for (const [label, c, options, hooks] of runs) {
  const events = [];
  const log = (e) => events.push(e);
  await new Promise((resolve) => {
    const req = c.request({ ":path": "/", ":method": "POST" }, options);
    log("sentTrailers at first " + req.sentTrailers);
    if (hooks.want) req.on("wantTrailers", () => { log("wantTrailers"); hooks.want(req, log); });
    req.on("response", (h, flags) => log("response " + flags));
    req.resume();
    req.on("end", () => log("end"));
    req.on("close", () => {
      log("close " + req.rstCode + " sentTrailers " + show(req.sentTrailers));
      resolve();
    });
    if (hooks.body) hooks.body(req, log);
    else req.end("payload");
  });
  await new Promise((r) => setTimeout(r, 30));
  console.log(label);
  for (const e of events) console.log("  client " + e);
  for (const s of serverLog.splice(0)) console.log("  server " + s);
}
client.close();
compatClient.close();
server.close();
compatServer.close();
