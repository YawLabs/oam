// A response whose connection's socket the handler destroys -- directly or
// through req.destroy() -- in the same tick as its writes: node's socket
// errors the writes it holds with ERR_STREAM_DESTROYED (ahead of its 'close',
// and of a destroyed request's 'close'), sends nothing, and the response
// closes with the socket, after the request's 'aborted': never 'finish', and
// end()'s callback, which waits for 'finish', is never called. Whether the
// writes come before the destroy or after it, and whether end() comes in the
// same tick or the next, the story is the same.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const POST = "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabc";

async function exchange(label, handle) {
  const events = [];
  const callback = (tag) => (err) => events.push(`${tag} callback ${err ? err.code : "ok"}`);
  let settled;
  const done = new Promise((resolve) => { settled = resolve; });
  const server = http.createServer((req, res) => {
    let closes = 0;
    const closed = () => { if (++closes === 3) settled(); };
    req.socket.on("close", () => { events.push("socket close"); closed(); });
    req.on("aborted", () => events.push("req aborted"));
    req.on("error", (e) => events.push(`req error ${e.code} ${e.message}`));
    req.on("close", () => { events.push("req close"); closed(); });
    res.on("finish", () => events.push("res finish"));
    res.on("error", (e) => events.push(`res error ${e.code}`));
    res.on("close", () => {
      events.push(`res close writableEnded=${res.writableEnded} writableFinished=${res.writableFinished}`);
      closed();
    });
    handle(req, res, callback);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const client = net.connect(server.address().port, "127.0.0.1");
  client.on("error", () => {});
  client.write(POST);
  await done;
  // Anything still to come (an end() callback that should never come) has
  // its chance.
  await new Promise((resolve) => setTimeout(resolve, 50));
  console.log(`--- ${label}`);
  for (const line of events) console.log(`  ${line}`);
  client.destroy();
  server.close();
}

await exchange("write, socket.destroy()", (req, res, cb) => {
  res.write("x", cb("write"));
  req.socket.destroy();
});
await exchange("write, write, socket.destroy()", (req, res, cb) => {
  res.write("x", cb("write 1"));
  res.write("y", cb("write 2"));
  req.socket.destroy();
});
await exchange("write, socket.destroy(), write", (req, res, cb) => {
  res.write("x", cb("write 1"));
  req.socket.destroy();
  res.write("y", cb("write 2"));
});
await exchange("write, socket.destroy(), end", (req, res, cb) => {
  res.write("x", cb("write"));
  req.socket.destroy();
  res.end("y", cb("end"));
});
await exchange("write, req.destroy(), end", (req, res, cb) => {
  res.write("x", cb("write"));
  req.destroy();
  res.end("y", cb("end"));
});
await exchange("write, req.destroy()", (req, res, cb) => {
  res.write("x", cb("write"));
  req.destroy();
});
await exchange("socket.destroy(), write, end", (req, res, cb) => {
  req.socket.destroy();
  res.write("x", cb("write"));
  res.end("y", cb("end"));
});
await exchange("socket.destroy(), end", (req, res, cb) => {
  req.socket.destroy();
  res.end("y", cb("end"));
});
await exchange("write, socket.destroy(), end on the next tick", (req, res, cb) => {
  res.write("x", cb("write"));
  req.socket.destroy();
  process.nextTick(() => res.end("y", cb("end")));
});
await exchange("flushHeaders(), socket.destroy()", (req, res) => {
  res.flushHeaders();
  req.socket.destroy();
});
