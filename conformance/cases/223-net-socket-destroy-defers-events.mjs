// net.Socket.destroy() emits nothing inside the call. Node tears the socket
// down synchronously (`destroyed`, the handle closed) and defers the
// events: 'error' to the next tick, then 'close' -- from the handle's close
// callback for a socket that has a handle (connected, or still connecting),
// which runs after the immediates already queued and before any timer; on
// the next tick, with no argument, for a socket that never had one.
//
// Regression guard (#189): oam emitted 'error' and 'close' from inside
// destroy(), so a 'close' listener ran in the middle of whatever loop was
// destroying sockets. Node's own Agent.prototype.destroy indexes the list
// its 'close' listener splices; ported code of that shape skipped every
// second socket. A destroyed socket also went on to emit 'end' when its
// parked read came back.
//
// Each scenario logs what happens around one destroy(): the call's own
// brackets, a process.nextTick queued right after it, a setImmediate queued
// right BEFORE it and a setTimeout(0) queued after it. (An immediate queued
// AFTER destroy() in the same turn is not logged: node runs it before
// 'close', oam after -- docs/node-divergences.md.)
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const log = [];
function flush(label) {
  console.log(`${label}: ${log.join(" > ")}`);
  log.length = 0;
}
const settle = () => new Promise((resolve) => setTimeout(resolve, 80));

const accepted = [];
const server = net.createServer((socket) => {
  socket.on("error", () => {});
  accepted.push(socket);
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();

async function connected() {
  const socket = net.connect({ port, host: "127.0.0.1" });
  await new Promise((resolve) => socket.once("connect", resolve));
  return socket;
}

function watch(socket) {
  socket.on("error", (e) => log.push(`error(${e.message})`));
  socket.on("close", (hadError) => log.push(`close(${hadError})`));
  socket.on("end", () => log.push("end"));
  socket.on("finish", () => log.push("finish"));
}

// destroy() from a timer callback, with the probes around it.
function destroyFromTimer(socket, err) {
  return new Promise((resolve) => {
    setTimeout(() => {
      setImmediate(() => log.push("immediate"));
      log.push("destroy(");
      const returned = socket.destroy(err);
      log.push(returned === socket ? ")" : ") returned something else");
      log.push(`destroyed=${socket.destroyed}`);
      process.nextTick(() => log.push("tick"));
      setTimeout(() => log.push("timeout"), 0);
      resolve();
    }, 5);
  });
}

for (const withError of [false, true]) {
  const socket = await connected();
  watch(socket);
  await destroyFromTimer(socket, withError ? new Error("boom") : undefined);
  await settle();
  flush(`connected socket, destroy(${withError ? "err" : ""})`);
}

for (const withError of [false, true]) {
  // Still connecting: the dial of an IP literal starts on the next tick,
  // and a socket destroyed first opens nothing.
  const socket = net.connect({ port, host: "127.0.0.1" });
  watch(socket);
  socket.on("connect", () => log.push("connect"));
  await destroyFromTimerNow(socket, withError ? new Error("boom") : undefined);
  await settle();
  flush(`connecting socket, destroy(${withError ? "err" : ""})`);
}

for (const withError of [false, true]) {
  // Never connected: no handle, so 'close' comes on the next tick too, and
  // without an argument.
  const socket = new net.Socket();
  socket.on("error", (e) => log.push(`error(${e.message})`));
  socket.on("close", (...args) => log.push(`close(${args.length} args)`));
  await destroyFromTimerNow(socket, withError ? new Error("boom") : undefined);
  await settle();
  flush(`fresh socket, destroy(${withError ? "err" : ""})`);
}

// As destroyFromTimer, without the timer hop: the socket must be destroyed
// in the turn it was made in.
function destroyFromTimerNow(socket, err) {
  setImmediate(() => log.push("immediate"));
  log.push("destroy(");
  socket.destroy(err);
  log.push(")");
  process.nextTick(() => log.push("tick"));
  setTimeout(() => log.push("timeout"), 0);
  return Promise.resolve();
}

{
  // The accepted end of a connection is a socket like any other.
  const next = new Promise((resolve) => server.once("connection", resolve));
  const client = await connected();
  client.on("error", () => {});
  const socket = await next;
  watch(socket);
  await destroyFromTimer(socket, new Error("boom"));
  await settle();
  flush("accepted socket, destroy(err)");
  client.destroy();
}

{
  // A second destroy() is ignored, error and all; the state a listener
  // reads is already final.
  const socket = await connected();
  socket.on("error", (e) => log.push(`error(${e.message}) destroyed=${socket.destroyed}`));
  socket.on("close", (hadError) => log.push(`close(${hadError}) readyState=${socket.readyState}`));
  await new Promise((resolve) => {
    setTimeout(() => {
      socket.destroy(new Error("first"));
      socket.destroy(new Error("second"));
      socket.destroy();
      log.push(`after three calls: destroyed=${socket.destroyed} readyState=${socket.readyState}`);
      resolve();
    }, 5);
  });
  await settle();
  flush("destroy() three times");
}

{
  // A graceful close is not a destroy()'s business until both sides are
  // done: 'finish', 'end', then 'close' -- and a destroy() after that is
  // silent.
  const socket = await connected();
  watch(socket);
  // The peer answers the FIN with its own (allowHalfOpen is off).
  socket.end();
  await settle();
  flush("end(), then the peer's");
  await destroyFromTimer(socket);
  await settle();
  flush("destroy() after the close");
}

{
  // A write that has completed, then destroy(err): the callback has run,
  // and nothing reports the write again.
  const socket = await connected();
  watch(socket);
  await new Promise((resolve) => socket.write("abc", (e) => {
    log.push(`write callback(${e ? e.code : "ok"})`);
    resolve();
  }));
  await destroyFromTimer(socket, new Error("boom"));
  await settle();
  flush("write, then destroy(err)");
}

{
  // The shape that broke: destroying sockets while iterating the list their
  // own 'close' listeners splice (node's Agent.prototype.destroy).
  const sockets = [await connected(), await connected(), await connected()];
  const live = sockets.slice();
  for (const socket of live) {
    socket.on("close", () => {
      const i = live.indexOf(socket);
      if (i !== -1) live.splice(i, 1);
    });
  }
  for (let n = 0; n < live.length; n++) live[n].destroy();
  console.log("destroyed flags after the live-list loop:", sockets.map((s) => s.destroyed).join(","));
  console.log("still listed when the loop returns:", live.length);
  await settle();
  console.log("listed once 'close' has run:", live.length);
}

{
  // destroy(err) with no 'error' listener: the error is an uncaught
  // exception -- raised after destroy() returned -- and 'close' still
  // follows.
  const socket = await connected();
  process.once("uncaughtException", (e, origin) => log.push(`uncaughtException(${e.message}, ${origin})`));
  socket.on("close", (hadError) => log.push(`close(${hadError})`));
  await new Promise((resolve) => {
    setTimeout(() => {
      log.push("destroy(");
      socket.destroy(new Error("boom"));
      log.push(")");
      resolve();
    }, 5);
  });
  await settle();
  flush("destroy(err), no 'error' listener");
}

for (const socket of accepted) socket.destroy();
server.close();
await settle();
console.log("done");
