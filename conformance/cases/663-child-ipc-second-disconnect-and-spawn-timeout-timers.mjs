// Two child_process behaviours, pinned against node on the same host.
//
// A second process.disconnect() in a forked child: node emits
// ERR_IPC_DISCONNECTED as an 'error' on process, synchronously, so with no
// 'error' listener the call throws (and an uncaught one ends the child).
// oam returned silently.
//
// spawn's `timeout` kills the child with the runtime's own timers: a script
// that swaps globalThis.setTimeout/clearTimeout for stubs (as fake-timer
// libraries do) does not stop it. oam armed it through the global and the
// child was never killed.
import { fork, spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const self = fileURLToPath(import.meta.url);

if (process.argv[2] === "child") {
  const events = [];
  process.on("disconnect", () => {
    events.push("disconnect");
    console.log("child events", events.join(" | "));
  });
  process.disconnect();
  try {
    process.disconnect();
    events.push("second returned");
  } catch (err) {
    events.push(`second threw ${err.name} ${err.code} ${err.message} ${err instanceof Error}`);
  }
  process.on("error", (err) => events.push(`error event ${err.code}`));
  process.disconnect();
  console.log("connected", process.connected);
} else if (process.argv[2] === "uncaught") {
  process.disconnect();
  process.disconnect();
  console.log("not reached");
} else {
  const first = fork(self, ["child"]);
  first.on("exit", (code) => {
    console.log("child exit", code);
    const second = fork(self, ["uncaught"], { stdio: ["inherit", "pipe", "pipe", "ipc"] });
    let out = "";
    let err = "";
    second.stdout.on("data", (d) => (out += d));
    second.stderr.on("data", (d) => (err += d));
    second.on("close", (code2) => {
      console.log("uncaught exit", code2, JSON.stringify(out), /ERR_IPC_DISCONNECTED/.test(err), /IPC channel is already disconnected/.test(err));
      spawnTimeout();
    });
  });
}

function spawnTimeout() {
  const realSetTimeout = globalThis.setTimeout;
  const realClearTimeout = globalThis.clearTimeout;
  let stubCalls = 0;
  globalThis.setTimeout = () => {
    stubCalls++;
    return 0;
  };
  globalThis.clearTimeout = () => {
    stubCalls++;
  };
  const child = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], { timeout: 200, stdio: "ignore" });
  globalThis.setTimeout = realSetTimeout;
  globalThis.clearTimeout = realClearTimeout;
  // A backstop, so a failure is an answer and not a hang.
  const backstop = setTimeout(() => {
    console.log("timeout never fired");
    child.kill();
  }, 10000);
  child.on("exit", (code, signal) => {
    clearTimeout(backstop);
    console.log("timeout exit", code, signal, "stub calls", stubCalls);
  });
}
