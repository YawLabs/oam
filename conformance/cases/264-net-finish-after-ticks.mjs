// When a connected socket's end() reports back: its callbacks and 'finish'
// come after every tick and microtask queued meanwhile, and before any
// immediate -- whether end() was called from a microtask (code after an
// await), a timer, an immediate or a tick.
//
// Regression guard: with nothing queued the natives shut the socket down
// inside the end() call, and oam emitted 'finish' from a microtask: ahead of
// the ticks the caller queued after end() (and, called from a microtask,
// ahead of them all). Node v22.22.2 reports the shutdown from the loop. The
// window shows in resetAndDestroy(): node refuses it with EINVAL while the
// shutdown is under way, so `end(); process.nextTick(() =>
// resetAndDestroy())` is EINVAL there, where oam reset the socket.
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const server = net.createServer((peer) => {
  peer.on("error", () => {});
  peer.resume();
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

async function connected() {
  const socket = net.connect(port, "127.0.0.1");
  await new Promise((resolve) => socket.once("connect", resolve));
  return socket;
}

for (const [from, schedule] of [
  ["a microtask", (fn) => queueMicrotask(fn)],
  ["a timer", (fn) => setTimeout(fn, 5)],
  ["an immediate", (fn) => setImmediate(fn)],
  ["a tick", (fn) => process.nextTick(fn)],
]) {
  for (const [name, act] of [
    ["end(cb)", (s, log) => s.end(() => log.push("end cb"))],
    ["end(data, cb)", (s, log) => s.end("x", () => log.push("end cb"))],
    ["write(); end(cb)", (s, log) => { s.write("x"); s.end(() => log.push("end cb")); }],
  ]) {
    const socket = await connected();
    const log = [];
    socket.on("finish", () => log.push(`finish (writableFinished ${socket.writableFinished})`));
    await new Promise((done) => {
      schedule(() => {
        setImmediate(() => log.push("immediate before"));
        act(socket, log);
        log.push(`returned (writableFinished ${socket.writableFinished})`);
        setImmediate(() => log.push("immediate after"));
        process.nextTick(() => {
          log.push("tick");
          process.nextTick(() => log.push("tick in tick"));
        });
        Promise.resolve().then(() => log.push("microtask"));
      });
      setTimeout(done, 100);
    });
    socket.destroy();
    console.log(`from ${from}, ${name}: ${log.join(" | ")}`);
  }
}

// The window resetAndDestroy() is refused in. (After the refusal node
// leaves the socket open, and the process with it, where oam closes it --
// docs/node-divergences.md entry 46 -- so 'close' is not logged and the
// case exits explicitly.)
{
  const socket = await connected();
  const log = [];
  socket.on("error", (e) => log.push(`error ${e.code} ${e.syscall}`));
  socket.on("finish", () => log.push("finish"));
  socket.end();
  process.nextTick(() => {
    try {
      socket.resetAndDestroy();
      log.push("reset returned");
    } catch (e) {
      log.push(`reset threw ${e.code}`);
    }
  });
  await new Promise((resolve) => setTimeout(resolve, 100));
  console.log(`end(); nextTick resetAndDestroy(): ${log.join(" | ")}`);
}

server.close();
process.exit(0);
