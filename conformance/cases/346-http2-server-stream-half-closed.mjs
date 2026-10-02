// A server stream that has responded and ended stays open for the request
// still coming, as nghttp2's half-closed stream does: data and a trailer
// section the client sends afterwards reach the stream ('data', 'trailers',
// 'end'), and it closes once both sides are done. Unless JS never asked for
// the request body (no 'data' listener, no read(), not paused or resumed):
// then node closes the stream once the response is out, and what came of
// the body is dumped so the readable side still ends -- 'end', then 'close'.
// Measured on node v22.22.2.
//
// Regression guard: oam closed every server stream when its response ended,
// so a request body or trailer section that came later was lost; and an
// h2c server collected the whole request body before it emitted 'stream'.
import http2 from "node:http2";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

let serverLog = [];
let serverClosed = null;
const S = (e) => serverLog.push(e);
const server = http2.createServer();
server.on("stream", (stream, headers) => {
  const mode = headers[":path"].slice(1);
  S("stream");
  if (mode.startsWith("read")) stream.on("data", (d) => S("data " + JSON.stringify(String(d))));
  if (mode.startsWith("paused")) stream.pause();
  stream.on("trailers", (t, flags, raw) => S("trailers " + JSON.stringify(raw)));
  stream.on("end", () => S("end"));
  stream.on("close", () => {
    S("close " + stream.rstCode + " readableLength=" + stream.readableLength);
    serverClosed();
  });
  stream.respond({ ":status": 200 });
  stream.end("done");
  if (mode.startsWith("paused")) setTimeout(() => stream.resume(), 200);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const client = http2.connect("http://127.0.0.1:" + server.address().port);

for (const mode of ["read", "read-trailers", "paused-trailers", "unread", "unread-trailers"]) {
  const events = [];
  const closed = new Promise((r) => (serverClosed = r));
  const withTrailers = mode.endsWith("trailers");
  await new Promise((resolve) => {
    const req = client.request({ ":method": "POST", ":path": "/" + mode }, withTrailers ? { waitForTrailers: true } : {});
    if (withTrailers) req.on("wantTrailers", () => req.sendTrailers({ x: "1" }));
    let body = "";
    req.on("data", (d) => (body += d));
    req.on("end", () => events.push("end " + JSON.stringify(body)));
    req.on("error", (e) => events.push("error " + e.code));
    req.on("close", () => {
      events.push("close " + req.rstCode);
      resolve();
    });
    // The rest of the request comes once the response has.
    req.on("response", () => setTimeout(() => req.end("late"), 100));
  });
  await closed;
  console.log(mode);
  for (const e of events) console.log("  client " + e);
  for (const s of serverLog) console.log("  server " + s);
  serverLog = [];
}
client.close();
server.close();
