// net.Socket write() / end() ordering: what is written reaches the peer in
// call order with the FIN behind it, whether the calls were made before the
// connect, after it, or all in one callback; write callbacks run in order,
// then end()'s, then the 'finish' listeners; and a write the socket took is
// delivered even when destroy() is the next line.
//
// Regression guard (#156): oam's end() waited for the write before it to
// complete before it even asked for the shutdown, so the FIN left one op
// round trip behind the data. The shutdown is now requested in the same
// turn and queued behind the writes natively; this pins that nothing about
// the order an application sees changed with it, and two things that did
// (end()'s callback ran after the 'finish' listeners, and a write followed
// at once by destroy() was dropped).
//
// Also pinned: the callbacks of a write the socket took and of an end()
// run before the 'close' of a destroy() on the next line (oam ran them a
// loop turn after it).
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const log = [];
function flush(label) {
  console.log(`${label}: ${log.join(" | ")}`);
  log.length = 0;
}
const settle = () => new Promise((resolve) => setTimeout(resolve, 100));
const closed = (socket) => new Promise((resolve) => socket.once("close", resolve));

// Answers its first 'data' with a write and an end() in the same callback.
const answering = net.createServer((socket) => {
  socket.on("error", () => {});
  socket.once("data", () => {
    socket.write("response");
    socket.end();
  });
});
await new Promise((resolve) => answering.listen(0, "127.0.0.1", resolve));

// Collects what it is sent, and answers the client's FIN with "bye".
let received = "";
const collecting = net.createServer({ allowHalfOpen: true }, (socket) => {
  socket.on("error", () => {});
  socket.on("data", (d) => (received += d));
  socket.on("end", () => {
    log.push(`peer got ${JSON.stringify(received)} then FIN`);
    received = "";
    socket.end("bye");
  });
});
await new Promise((resolve) => collecting.listen(0, "127.0.0.1", resolve));
const port = collecting.address().port;

function watch(socket) {
  socket.on("connect", () => log.push("connect"));
  socket.on("data", (d) => log.push(`data ${d}`));
  socket.on("end", () => log.push("end"));
  socket.on("finish", () => log.push("finish"));
  socket.on("close", (hadError) => log.push(`close ${hadError}`));
}

{
  const socket = net.connect(answering.address().port, "127.0.0.1");
  watch(socket);
  socket.write("a", () => log.push("write callback a"));
  socket.write("b", () => log.push("write callback b"));
  await closed(socket);
  flush("the peer writes and ends in one callback");
}

{
  // Everything before the connect: the data, then the FIN.
  const socket = net.connect(port, "127.0.0.1");
  watch(socket);
  socket.write("hello ", () => log.push("write callback"));
  socket.end("world", () => log.push("end callback"));
  await closed(socket);
  flush("write() and end(data) before the connect");
}

{
  const socket = net.connect(port, "127.0.0.1");
  await new Promise((resolve) => socket.once("connect", resolve));
  socket.on("finish", () => log.push("finish listener added before end()"));
  for (let i = 0; i < 5; i++) socket.write(`w${i} `, () => log.push(`callback ${i}`));
  socket.end(() => log.push("end callback"));
  log.push("end() returned");
  socket.on("finish", () => log.push("finish listener added after end()"));
  socket.on("data", (d) => log.push(`data ${d}`));
  await closed(socket);
  flush("five writes and end(callback) on a connected socket");
}

{
  // From a 'connect' listener, behind a write made before the connect.
  const socket = net.connect(port, "127.0.0.1");
  socket.write("early ");
  socket.on("connect", () => {
    socket.write("late");
    socket.end();
  });
  socket.on("data", (d) => log.push(`data ${d}`));
  await closed(socket);
  flush("a write before the connect, a write and end() in 'connect'");
}

{
  // A write the socket takes is on its way: destroy() on the next line does
  // not take it back, and its callback reports success.
  const socket = net.connect(port, "127.0.0.1");
  await new Promise((resolve) => socket.once("connect", resolve));
  socket.write("kept", (e) => log.push(`write callback ${e ? e.code : "ok"}`));
  socket.destroy();
  await settle();
  flush("write() then destroy()");
}

{
  // end() twice, and a write after it: one 'finish', both callbacks.
  const socket = net.connect(port, "127.0.0.1");
  await new Promise((resolve) => socket.once("connect", resolve));
  socket.on("error", (e) => log.push(`error ${e.code}`));
  socket.on("finish", () => log.push("finish"));
  socket.end("once", () => log.push("first end callback"));
  socket.end(() => log.push("second end callback"));
  socket.on("data", (d) => log.push(`data ${d}`));
  await closed(socket);
  flush("end() twice");
}

// Reads what it is sent and says nothing, half-open, so the client's own
// end() is all that finishes its side.
const quiet = net.createServer({ allowHalfOpen: true }, (socket) => {
  socket.on("error", () => {});
  socket.resume();
});
await new Promise((resolve) => quiet.listen(0, "127.0.0.1", resolve));
const quietPort = quiet.address().port;

{
  // write(), end() and destroy() in a row on a connected socket: the write
  // and end() callbacks run first, then 'close'.
  const socket = net.connect(quietPort, "127.0.0.1");
  await new Promise((resolve) => socket.once("connect", resolve));
  socket.write("kept", (e) => log.push(`write callback ${e === null ? "null" : e && e.code}`));
  socket.end((e) => log.push(`end callback ${e && e.code}`));
  socket.on("close", (hadError) => log.push(`close ${hadError}`));
  socket.destroy();
  log.push("destroy() returned");
  await settle();
  flush("write(), end(), destroy()");
}

quiet.close();
answering.close();
collecting.close();
await settle();
console.log("done");
