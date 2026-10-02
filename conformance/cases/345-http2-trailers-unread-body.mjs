// A trailer section reaches its stream's 'trailers' whether or not the body
// ahead of it is read: node's nghttp2 keeps taking a stream's frames in while
// JS does not read -- into the readable buffer, past its highWaterMark --
// until the stream's flow-control window (65535 bytes at the default
// settings) is used up. So a body under a window, never read, still has its
// trailers emitted (readableLength holding the body); one of a window or more
// holds them back until it is read. The same on the client's response and the
// server's request. Measured on node v22.22.2.
//
// Regression guard: oam read a body only on demand, so neither stream
// emitted 'trailers' for a body nobody read.
import http2 from "node:http2";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const sizes = [0, 5, 16384, 40000, 65534, 65535, 100000];
let current = null;
const server = http2.createServer();
server.on("stream", (stream, headers) => {
  const [, side, n] = headers[":path"].split("/");
  const body = Buffer.alloc(Number(n), 97);
  if (side === "server") {
    // The request body is not read until its trailers come, or a moment;
    // the response waits for it.
    current.watch(stream, "server", () => {
      stream.respond({ ":status": 200 });
      stream.end();
    });
    return;
  }
  stream.resume();
  stream.on("end", () => {
    stream.respond({ ":status": 200 }, { waitForTrailers: true });
    stream.on("wantTrailers", () => stream.sendTrailers({ "x-t": "v" }));
    stream.end(body);
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const client = http2.connect("http://127.0.0.1:" + server.address().port);

for (const side of ["client", "server"]) {
  for (const n of sizes) {
    const events = [];
    await new Promise((resolve) => {
      let pending = 2;
      const done = () => --pending === 0 && resolve();
      // Wait for 'trailers' or 300 ms, whichever comes first; then read the
      // rest so the stream can finish.
      current = {
        watch(stream, who, then) {
          let settled = false;
          const settle = (what) => {
            if (settled) return;
            settled = true;
            clearTimeout(timer);
            events.push(`${who} ${what} readableLength=${stream.readableLength}`);
            stream.on("end", () => {
              events.push(who + " end");
              if (then) then();
            });
            stream.resume();
          };
          const timer = setTimeout(() => settle("no trailers yet"), 300);
          stream.on("trailers", (t, flags, raw) => settle("trailers " + JSON.stringify(raw)));
          stream.on("close", () => {
            events.push(who + " close " + stream.rstCode);
            done();
          });
        },
      };
      const req = client.request({ ":method": "POST", ":path": `/${side}/${n}` }, side === "server" ? { waitForTrailers: true } : {});
      if (side === "client") {
        current.watch(req, "client");
        req.end();
        done();
      } else {
        req.on("wantTrailers", () => req.sendTrailers({ "x-t": "v" }));
        req.resume();
        req.on("close", done);
        req.end(Buffer.alloc(n, 97));
      }
    });
    console.log(side, n);
    for (const e of events) console.log("  " + e);
  }
}
client.close();
server.close();
