// When a connected socket's end() reports back: its callbacks and 'finish'
// come after every tick and microtask queued meanwhile, and before any
// immediate -- whether end() was called from a microtask (code after an
// await), a timer, an immediate or a tick -- and, called from a timer or an
// immediate, after the rest of that loop phase.
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

// end() inside a timer or an immediate: 'finish' waits out the rest of that
// phase. Node runs the timers due when its timers phase began, then the
// pending phase ('finish'), then the immediates queued before its check
// phase began; a phase's own additions wait for the next loop iteration.
// (oam once ran 'finish' at the very next turn, ahead of a sibling timer or
// immediate already due, so the sibling's resetAndDestroy() reset a socket
// node refuses to reset.)
const spin = (ms) => {
  const start = Date.now();
  while (Date.now() - start < ms);
};
const reset = (socket, log) => {
  try {
    socket.resetAndDestroy();
  } catch (e) {
    log.push(`reset threw ${e.code}`);
  }
};
for (const [name, act] of [
  ["timer end(), sibling timer", (s, log) => {
    setTimeout(() => { log.push("t1 end"); s.end(); }, 5);
    setTimeout(() => log.push("t2"), 5);
  }],
  ["timer end() in a tick, sibling timer", (s, log) => {
    setTimeout(() => { log.push("t1"); process.nextTick(() => s.end()); }, 5);
    setTimeout(() => log.push("t2"), 5);
  }],
  ["timer end(), immediate from it, sibling timer", (s, log) => {
    setTimeout(() => {
      log.push("t1 end");
      s.end();
      setImmediate(() => log.push("immediate"));
    }, 5);
    setTimeout(() => log.push("t2"), 5);
  }],
  ["timer end(), a timer it sets due before it returns", (s, log) => {
    setTimeout(() => {
      log.push("t1 end");
      s.end();
      setTimeout(() => log.push("t3"), 1);
      spin(5);
    }, 5);
  }],
  ["timer end(), sibling timer resetAndDestroy()", (s, log) => {
    setTimeout(() => { log.push("t1 end"); s.end(); }, 5);
    setTimeout(() => { log.push("t2 reset"); reset(s, log); }, 5);
  }],
  ["immediate end(), sibling immediates, one from it", (s, log) => {
    setImmediate(() => {
      log.push("i1 end");
      s.end();
      setImmediate(() => log.push("i3"));
    });
    setImmediate(() => log.push("i2"));
  }],
  ["immediate end(), timer due when the check phase ends", (s, log) => {
    setImmediate(() => {
      log.push("i1 end");
      s.end();
      setTimeout(() => log.push("t"), 1);
      spin(5);
    });
  }],
  ["immediate end(), sibling immediate resetAndDestroy()", (s, log) => {
    setImmediate(() => { log.push("i1 end"); s.end(); });
    setImmediate(() => { log.push("i2 reset"); reset(s, log); });
  }],
]) {
  const socket = await connected();
  const log = [];
  socket.on("error", (e) => log.push(`error ${e.code} ${e.syscall}`));
  socket.on("finish", () => log.push("finish"));
  act(socket, log);
  await new Promise((resolve) => setTimeout(resolve, 100));
  socket.destroy();
  console.log(`${name}: ${log.join(" | ")}`);
}

server.close();
process.exit(0);
