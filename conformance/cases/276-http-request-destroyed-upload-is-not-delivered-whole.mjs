// A streamed (chunked) upload that the client destroys part-way must reach
// the server as an aborted request, never as a complete one -- measured on
// node v22.22.2, whose server sees 'aborted'. On oam's own transport the
// upload rides an outbound body channel, and req.destroy() cancels it by
// sending the transport an error. When the server had stopped reading, the
// channel was full, the error could not be queued and was dropped with the
// channel: the transport saw the body END, wrote the closing 0-chunk, and the
// server read a complete request holding a truncated body.
//
// Only what the server saw is printed.
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

const outcome = new Promise((resolve) => {
  const server = http.createServer((req, res) => {
    // Not reading for a while: the client's upload backs up behind it.
    req.pause();
    setTimeout(() => {
      req.on("data", () => {});
      req.on("end", () => {
        resolve("a complete request");
        res.end("ok");
      });
      req.on("aborted", () => resolve("an aborted request"));
      req.on("error", () => resolve("an aborted request"));
      req.resume();
    }, 1000);
  });
  server.listen(0, "127.0.0.1", () => {
    const req = http.request({
      port: server.address().port,
      host: "127.0.0.1",
      method: "POST",
      headers: { "transfer-encoding": "chunked" },
    });
    req.on("error", () => {});
    const chunk = Buffer.alloc(256 * 1024, 0x61);
    for (let i = 0; i < 40; i++) req.write(chunk);
    setTimeout(() => req.destroy(), 300);
  });
});
console.log("server saw", await outcome);
process.exit(0);
