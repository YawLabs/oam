// What prints a process warning is a 'warning' listener on process like any
// other -- node's `onWarning` -- so it shows in `listeners`, a program that
// reports warnings its own way can take it off, and it prints whatever
// Error the event carries, emitted directly or through emitWarning (#342).
//
// The printing goes to stderr, which the harness does not compare, so each
// scenario runs as a child and its stderr is printed here. Two things are
// taken out of it: the pid, and node's one-time hint line (`(Use `node
// --trace-warnings ...` ...)`), which names flags oam does not have and so
// does not print (docs/node-divergences.md).
import { spawnSync } from "node:child_process";
import { Worker, isMainThread, parentPort } from "node:worker_threads";

const scenario = process.argv[2];

if (!isMainThread) {
  parentPort.postMessage(process.listeners("warning").map((f) => `${f.name}/${f.length}`));
} else if (scenario === "captured") {
  // The printing goes through process.stderr: a program that has taken that
  // over gets its warnings there.
  const written = [];
  const write = process.stderr.write;
  process.stderr.write = (chunk) => (written.push(String(chunk)), true);
  process.emitWarning("to whoever holds process.stderr");
  setImmediate(() => {
    process.stderr.write = write;
    const lines = written.join("").replace(/\((node|oam):\d+\)/g, "(node:PID)").split("\n");
    console.log("captured:", JSON.stringify(lines.filter((line) => line !== "" && !line.startsWith("(Use `"))));
  });
} else if (scenario === "emit") {
  const seen = [];
  console.log("events:", process.eventNames().map(String));
  console.log("listeners:", process.listeners("warning").map((f) => `${f.name}/${f.length}`));
  process.on("warning", (w) => seen.push(typeof w === "string" ? `string ${w}` : `${w.name}: ${w.message}`));
  process.emitWarning("dep", "DeprecationWarning", "DEP_X");
  process.emitWarning("dep again", "DeprecationWarning", "DEP_Y");
  process.emitWarning("plain");
  process.emitWarning("experimental", "ExperimentalWarning");
  // Emitted directly: printed when it is an Error, and only then.
  process.emit("warning", "a raw string");
  process.emit("warning", Object.assign(new Error("emitted directly"), { name: "CustomWarning" }));
  process.emit("warning", { name: "NotAnError", message: "an object" });
  const odd = Object.assign(new Error("its toString is not a function"), { name: "OddWarning", toString: 5 });
  process.emit("warning", odd);
  const own = Object.assign(new Error("ignored"), { name: "OwnWarning", toString: () => "what its own toString says" });
  process.emit("warning", own);
  const detailed = Object.assign(new Error("with a detail"), { name: "DetailWarning", code: "C1", detail: "the detail line" });
  process.emit("warning", detailed);
  setImmediate(() => console.log("heard:", JSON.stringify(seen)));
} else if (scenario === "removed") {
  // The documented way to take over: remove the default, add one's own.
  const [printer] = process.listeners("warning");
  process.removeAllListeners("warning");
  console.log("after removeAllListeners:", process.listenerCount("warning"));
  process.emitWarning("nobody prints this");
  process.emit("warning", new Error("nor this"));
  setImmediate(() => {
    // Put back, it prints again; taken off by reference, it stops.
    if (printer) process.on("warning", printer);
    process.emitWarning("printed once it is back");
    setImmediate(() => {
      if (printer) process.off("warning", printer);
      process.emitWarning("and not after off");
      setImmediate(() => console.log("listeners at the end:", process.listenerCount("warning")));
    });
  });
} else if (scenario === "worker") {
  const worker = new Worker(new URL(import.meta.url));
  worker.on("message", (names) => {
    console.log("in a worker:", names);
    // A worker given --no-warnings of its own has none.
    const quiet = new Worker(new URL(import.meta.url), { execArgv: ["--no-warnings"] });
    quiet.on("message", (none) => console.log("in a worker with execArgv --no-warnings:", none));
  });
} else {
  const run = (name, flags = [], env = {}) => {
    const child = spawnSync(process.execPath, [...flags, import.meta.filename, name], {
      encoding: "utf8",
      env: { ...process.env, ...env },
    });
    const stderr = child.stderr
      .replace(/\((node|oam):\d+\)/g, "(node:PID)")
      .split(/\r?\n/)
      .filter((line) => line !== "" && !line.startsWith("(Use `"));
    console.log(`## ${[...flags, ...Object.entries(env).map(([k, v]) => `${k}=${v}`), name].join(" ")}`);
    console.log(`exit ${child.status}`);
    for (const line of child.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
    for (const line of stderr) console.log(`  err | ${line}`);
  };
  run("emit");
  run("emit", ["--no-warnings"]);
  run("emit", [], { NODE_NO_WARNINGS: "1" });
  run("emit", [], { NODE_NO_WARNINGS: "0" });
  run("emit", ["--no-deprecation"]);
  run("emit", ["--disable-warning=DEP_X"]);
  run("emit", ["--disable-warning=ExperimentalWarning", "--disable-warning=C1"]);
  run("removed");
  run("removed", ["--no-warnings"]);
  run("captured");
  run("worker");
  run("worker", ["--no-warnings"]);
}
