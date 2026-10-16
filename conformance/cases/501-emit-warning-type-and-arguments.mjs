// process.emitWarning's (warning, type, code, ctor) and (warning, options)
// forms, as node's: the options form reads `options.type || 'Warning'`, so an
// empty or falsy type falls back to Warning, and both forms validate the type
// and the code under the ARGUMENT's name ("type", "code"), never as an
// options property; a positional empty type gives the name Warning too
// (#367). The type and code are checked before the warning itself. A
// DeprecationWarning under process.noDeprecation is dropped even when
// process.throwDeprecation is set.
//
// What is printed goes to stderr, which the harness does not compare, so the
// printing scenario runs as a child and its stderr is printed here, with the
// pid and the program name in the one-time hint taken out.
import { spawnSync } from "node:child_process";

const scenario = process.argv[2];

if (scenario === "calls") {
  const heard = [];
  process.on("warning", (w) => heard.push(`${JSON.stringify(w.name)} ${JSON.stringify(w.message)} code=${JSON.stringify(w.code)} detail=${JSON.stringify(w.detail)}`));
  const calls = [
    ['"a2", ""', ["a2", ""]],
    ['"a3", null', ["a3", null]],
    ['"a4", undefined, "C4"', ["a4", undefined, "C4"]],
    ['"m", { type: 5 }', ["m", { type: 5 }]],
    ['"m", { type: 0 }', ["m", { type: 0 }]],
    ['"m", { type: "" }', ["m", { type: "" }]],
    ['"m", { type: null }', ["m", { type: null }]],
    ['"m", { type: false }', ["m", { type: false }]],
    ['"m", { type: NaN }', ["m", { type: NaN }]],
    ['"m", { type: true }', ["m", { type: true }]],
    ['"m", { type: "T", code: 5 }', ["m", { type: "T", code: 5 }]],
    ['"m", { code: 0 }', ["m", { code: 0 }]],
    ['"m", { code: "" }', ["m", { code: "" }]],
    ['"m", { code: null }', ["m", { code: null }]],
    ['"m", { detail: 5 }', ["m", { detail: 5 }]],
    ['"m", { detail: "d", code: "C" }', ["m", { detail: "d", code: "C" }]],
    ['"m", { ctor: 5 }', ["m", { ctor: 5 }]],
    ['"m", 5', ["m", 5]],
    ['"m", 0', ["m", 0]],
    ['"m", false', ["m", false]],
    ['"m", ["arr"]', ["m", ["arr"]]],
    ['"m", "T", 5', ["m", "T", 5]],
    ['"m", "T", null', ["m", "T", null]],
    ['"m", "T", ""', ["m", "T", ""]],
    ['"m", "T", "C", 5', ["m", "T", "C", 5]],
    ['5', [5]],
    ['5, 5', [5, 5]],
    ['5, { code: 5 }', [5, { code: 5 }]],
    ['null', [null]],
    ['undefined', [undefined]],
    ['an Error named ""', [Object.assign(new Error("e"), { name: "" })]],
    ['"m", { type: "DeprecationWarning", code: 7 }', ["m", { type: "DeprecationWarning", code: 7 }]],
  ];
  for (const [label, args] of calls) {
    try {
      process.emitWarning(...args);
      console.log(`${label}: ok`);
    } catch (e) {
      console.log(`${label}: ${e.name} [${e.code}] ${e.message}`);
    }
  }
  setImmediate(() => {
    for (const line of heard) console.log("heard", line);
  });
} else if (scenario === "no-and-throw-deprecation") {
  process.on("warning", (w) => console.log("heard", w.name, w.message));
  process.noDeprecation = true;
  process.throwDeprecation = true;
  process.emitWarning("dropped", "DeprecationWarning");
  process.emitWarning("not a deprecation");
  setImmediate(() => console.log("nothing thrown"));
} else {
  const run = (name) => {
    const child = spawnSync(process.execPath, [import.meta.filename, name], { encoding: "utf8" });
    console.log(`## ${name}`);
    console.log(`exit ${child.status}`);
    for (const line of child.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
    for (const line of child.stderr.split(/\r?\n/)) {
      if (line === "") continue;
      const shown = line.replace(/\((node|oam):\d+\)/g, "(node:PID)").replace(/^\(Use `(node|oam) /, "(Use `node ");
      console.log(`  err | ${shown}`);
    }
  };
  run("calls");
  run("no-and-throw-deprecation");
}
