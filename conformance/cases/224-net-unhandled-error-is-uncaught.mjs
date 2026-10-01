// A net.Socket 'error' nobody listens for is an uncaught exception, raised
// outside any promise: process.on('uncaughtException') sees it once, with
// origin 'uncaughtException', 'close' still follows, and
// process.on('unhandledRejection') sees nothing.
//
// Regression guard (#164): oam emitted a failed connect's 'error' from
// inside the promise reaction that settled the native connect, so with no
// listener the throw rejected the socket's internal write chain and was
// reported as an unhandled rejection -- the wrong process-level event for
// any program that handles the two differently. A write after end() emitted
// (and threw) inside the write() call itself, and a failed write rejected
// the same chain.
import net from "node:net";
import tls from "node:tls";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const log = [];
let waiting = null;
function wake() {
  if (waiting) {
    const resolve = waiting;
    waiting = null;
    resolve();
  }
}
process.on("uncaughtException", (e, origin) => {
  log.push(`uncaughtException(${origin}) ${e.constructor.name} ${e.code} syscall=${e.syscall}`);
  wake();
});
process.on("unhandledRejection", (e) => {
  log.push(`unhandledRejection ${e && e.code}`);
  wake();
});
const processEvent = () => new Promise((resolve) => { waiting = resolve; });
const settle = () => new Promise((resolve) => setTimeout(resolve, 100));
function flush(label) {
  console.log(`${label}: ${log.join(" | ") || "(nothing)"}`);
  log.length = 0;
}

// A loopback port nothing listens on.
async function closedPort() {
  const server = net.createServer();
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  await new Promise((resolve) => server.close(resolve));
  return port;
}

{
  const socket = net.connect(await closedPort(), "127.0.0.1");
  socket.on("close", (hadError) => log.push(`close(${hadError})`));
  await processEvent();
  await settle();
  flush("net.connect refused, no 'error' listener");
}

{
  // The write queued behind the connect is told the socket closed; the
  // connect error is still the uncaught one.
  const socket = net.connect(await closedPort(), "127.0.0.1");
  socket.write("queued", (e) => log.push(`write callback ${e && e.code}`));
  socket.on("close", (hadError) => log.push(`close(${hadError})`));
  await processEvent();
  await settle();
  flush("refused, a write with a callback queued");
}

{
  const socket = net.connect(await closedPort(), "127.0.0.1");
  socket.write("queued");
  socket.end();
  socket.on("close", (hadError) => log.push(`close(${hadError})`));
  await processEvent();
  await settle();
  flush("refused, a write and end() queued without callbacks");
}

{
  const socket = tls.connect(await closedPort(), "127.0.0.1");
  socket.on("close", (hadError) => log.push(`close(${hadError})`));
  await processEvent();
  await settle();
  flush("tls.connect refused, no 'error' listener");
}

const server = net.createServer((socket) => {
  socket.on("error", () => {});
  socket.resume();
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();
async function connected() {
  const socket = net.connect(port, "127.0.0.1");
  await new Promise((resolve) => socket.once("connect", resolve));
  return socket;
}

{
  // write() after end() returns false and throws nothing; its error
  // destroys the socket and, unheard, is uncaught.
  const socket = await connected();
  socket.on("close", (hadError) => log.push(`close(${hadError})`));
  socket.end();
  try {
    log.push(`write returned ${socket.write("late")}`);
  } catch (e) {
    log.push(`write threw ${e.code}`);
  }
  await processEvent();
  await settle();
  flush("write after end(), no 'error' listener");
}

{
  // With a callback and a listener: the callback first, then 'error', then
  // 'close' -- none of them inside write().
  const socket = await connected();
  socket.on("error", (e) => log.push(`error ${e.code}`));
  socket.on("close", (hadError) => log.push(`close(${hadError})`));
  socket.end();
  log.push(`write returned ${socket.write("late", (e) => log.push(`callback ${e && e.code}`))}`);
  log.push("write() returned");
  await settle();
  flush("write after end(), a callback and a listener");
}

{
  // A destroyed socket: the callback alone hears of it.
  const socket = await connected();
  socket.on("error", (e) => log.push(`error ${e.code}`));
  socket.on("close", (hadError) => log.push(`close(${hadError})`));
  socket.destroy();
  log.push(`write returned ${socket.write("late", (e) => log.push(`callback ${e && e.code}`))}`);
  log.push("write() returned");
  await settle();
  flush("write after destroy(), a callback and a listener");
  log.push(`write returned ${socket.write("later")}`);
  await settle();
  flush("write after destroy(), no callback");
}

server.close();
await settle();
flush("after everything");
console.log("done");
