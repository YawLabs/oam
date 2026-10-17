// What prints a process warning is a 'warning' listener on process like any
// other -- node's `onWarning` -- so it shows in `listeners`, a program that
// reports warnings its own way can take it off, and it prints whatever
// Error the event carries, emitted directly or through emitWarning (#342).
//
// The printing goes to stderr, which the harness does not compare, so each
// scenario runs as a child and its stderr is printed here, with the pid taken
// out. The one-time hint after the first printed warning (`(Use `node
// --trace-warnings ...` ...)`, `--trace-deprecation` when it is a
// deprecation) is kept, apart from the program name it starts with: node
// names basename(process.argv0), which is `oam` under oam (#364).
//
// Under --trace-warnings / --trace-deprecation (on the command line or in
// NODE_OPTIONS) the printer writes the warning's stack in place of its
// toString(), for the warnings the flag covers, and prints no hint for those.
// Each stack is cut to its first frame, given as file:line:column: the frames
// below it are each runtime's own module loader. A Worker given either flag
// in its own execArgv traces as if its process had been (node applies a
// worker's execArgv to that worker).
import { spawnSync } from "node:child_process";
import { Worker, isMainThread, parentPort, workerData } from "node:worker_threads";

const scenario = process.argv[2];

// stderr text -> the lines compared: the pid and the hint's program name taken
// out, each stack cut to its first frame's location.
function normalize(text) {
  const out = [];
  let inStack = false;
  for (const line of text.split(/\r?\n/)) {
    if (line === "") continue;
    if (/^\s+at /.test(line)) {
      if (!inStack) {
        const at = /([^\\/(]+\.mjs):(\d+):(\d+)\)?$/.exec(line);
        out.push(`    at ${at ? `${at[1]}:${at[2]}:${at[3]}` : "<elsewhere>"}`);
      }
      inStack = true;
      continue;
    }
    inStack = false;
    out.push(line.replace(/\((node|oam):\d+\)/g, "(node:PID)").replace(/^\(Use `(node|oam) /, "(Use `node "));
  }
  return out;
}

if (!isMainThread && workerData === "traced") {
  parentPort.postMessage(
    ["traceProcessWarnings", "traceDeprecation"].map(
      (name) => `${name} ${JSON.stringify(Object.getOwnPropertyDescriptor(process, name))}`,
    ),
  );
  process.emitWarning("from a worker");
  process.emitWarning("deprecated in a worker", "DeprecationWarning", "DEP_W");
} else if (!isMainThread) {
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
    console.log("captured:", JSON.stringify(normalize(written.join(""))));
  });
} else if (scenario === "emit") {
  const seen = [];
  console.log("events:", process.eventNames().map(String));
  console.log("listeners:", process.listeners("warning").map((f) => `${f.name}/${f.length}`));
  for (const name of ["traceProcessWarnings", "traceDeprecation"]) {
    console.log(`${name}:`, JSON.stringify(Object.getOwnPropertyDescriptor(process, name)));
  }
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
} else if (scenario === "deprecation-first") {
  // The hint names the flag for the kind of the first warning printed.
  process.emitWarning("printed first", "DeprecationWarning", "DEP_Z");
  process.emitWarning("printed second");
  process.emitWarning("printed third", "DeprecationWarning");
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
} else if (scenario === "traced-worker") {
  // One worker after the other, so their stderr does not interleave.
  const next = (list) => {
    if (list.length === 0) return;
    const [execArgv, ...rest] = list;
    const worker = new Worker(new URL(import.meta.url), { execArgv, workerData: "traced" });
    worker.on("message", (props) => console.log(`worker with execArgv ${execArgv.join(" ")}:`, props));
    worker.on("exit", () => next(rest));
  };
  next([["--trace-warnings"], ["--trace-deprecation"], ["--trace-warnings", "--trace-deprecation"]]);
} else {
  const run = (name, flags = [], env = {}) => {
    const child = spawnSync(process.execPath, [...flags, import.meta.filename, name], {
      encoding: "utf8",
      env: { ...process.env, ...env },
    });
    const stderr = normalize(child.stderr);
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
  // The trace flags: each covers its own kind, and the hint names the other.
  run("emit", ["--trace-warnings"]);
  run("emit", ["--trace-deprecation"]);
  run("emit", ["--trace-warnings", "--trace-deprecation"]);
  run("emit", [], { NODE_OPTIONS: "--trace-warnings" });
  run("emit", [], { NODE_OPTIONS: "--trace-deprecation" });
  run("emit", ["--trace-deprecation", "--no-deprecation"]);
  run("deprecation-first");
  run("deprecation-first", ["--trace-warnings"]);
  run("deprecation-first", ["--trace-deprecation"]);
  run("deprecation-first", ["--disable-warning=DEP_Z"]);
  run("removed", ["--trace-warnings"]);
  run("captured", ["--trace-warnings"]);
  run("worker");
  run("worker", ["--no-warnings"]);
  run("traced-worker");
}
