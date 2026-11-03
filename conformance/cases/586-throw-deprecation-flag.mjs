// --throw-deprecation, as node v22.22.2 runs it: process.throwDeprecation is
// a read-only, enumerable, configurable `true`; a deprecation warning is not
// printed but thrown on the next tick as an uncaught exception (exit 1, the
// synchronous code after the emit still runs, later ticks do not); an
// 'uncaughtException' handler receives it; --no-deprecation still wins, in
// either order; NODE_OPTIONS carries it (execArgv does not then list it); a
// Worker's own execArgv carries it. A plain Warning is still printed.
//
// oam refused the flag outright ("unexpected argument '--throw-deprecation'",
// exit 2) and ignored it in NODE_OPTIONS. Its noDeprecation was also writable
// under --no-deprecation, where node's is read-only.
//
// The uncaught report's first lines name the throw site, a node:internal or
// oam: builtin frame, so only the error's own lines are compared.
import { spawnSync } from "node:child_process";
import { Worker, isMainThread } from "node:worker_threads";

const scenario = process.argv[2];

function descriptor(name) {
  return JSON.stringify(Object.getOwnPropertyDescriptor(process, name) ?? null);
}

if (!isMainThread) {
  process.emitWarning("from the worker", "DeprecationWarning", "DEP0997");
} else if (scenario === "emit") {
  console.log("throwDeprecation", descriptor("throwDeprecation"));
  console.log("noDeprecation", descriptor("noDeprecation"));
  // Sorted: oam lists the flags it takes in a fixed order where node keeps
  // the order given (docs/node-divergences.md, the trace-flags entry).
  console.log("execArgv", JSON.stringify([...process.execArgv].sort()));
  process.on("exit", (code) => console.log("exit event", code));
  process.emitWarning("boom", "DeprecationWarning", "DEP0999");
  console.log("after emit (sync)");
  process.nextTick(() => console.log("next tick"));
  setTimeout(() => console.log("timeout"), 0);
} else if (scenario === "handled") {
  process.on("uncaughtException", (err, origin) =>
    console.log("uncaught", err.name, err.code, err.message, origin));
  process.emitWarning("boom", "DeprecationWarning", "DEP0999");
  process.emitWarning("plain", "Warning", "W1");
  setTimeout(() => console.log("still running"), 0);
} else if (scenario === "worker") {
  const worker = new Worker(new URL(import.meta.url), { execArgv: ["--throw-deprecation"] });
  worker.on("error", (err) => console.log("worker error", err.name, err.code, err.message));
  worker.on("exit", (code) => console.log("worker exit", code));
} else {
  const run = (name, flags = [], env = {}) => {
    const child = spawnSync(process.execPath, [...flags, import.meta.filename, name], {
      encoding: "utf8",
      env: { ...process.env, ...env },
    });
    console.log(`## ${[...flags, ...Object.entries(env).map(([k, v]) => `${k}=${v}`), name].join(" ")}`);
    console.log(`exit ${child.status}`);
    for (const line of child.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
    // The error's own lines: its name and message, and its code.
    for (const line of child.stderr.split(/\r?\n/)) {
      if (/^(DeprecationWarning|\(node:\d+\)|\(oam:\d+\))|code: 'DEP/.test(line.trim())) {
        console.log(`  err | ${line.trim().replace(/^\((node|oam):\d+\)/, "(<pid>)")}`);
      }
    }
  };
  run("emit");
  run("emit", ["--throw-deprecation"]);
  run("emit", ["--throw-deprecation", "--no-deprecation"]);
  run("emit", ["--no-deprecation", "--throw-deprecation"]);
  run("emit", [], { NODE_OPTIONS: "--throw-deprecation" });
  run("emit", ["--no-deprecation"]);
  run("handled", ["--throw-deprecation"]);
  run("worker");
}
