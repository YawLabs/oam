// process.nextTick against promise jobs queued in the same code (#345). An
// ES module is evaluated inside a microtask drain, so at its top level the
// promise jobs -- and the code after a top-level await they settle -- run
// before any nextTick callback, in the main module, in a worker's, and in a
// module an ES module import()s. In CommonJS and in a timer callback ticks
// run first, as at every other tick point. (A module CommonJS import()s,
// and an importer that goes on after the call, are case 480's -- #397.)
//
// And on the way out, as node: an ES module entry that throws -- at its
// top level, or after a top-level await -- runs no tick before the
// uncaughtException ladder, whose origin for it is 'unhandledRejection'; a
// fatal microtask throw runs no tick after it; a nextTick from an 'exit'
// listener never runs, the promise jobs those listeners queue do. A
// top-level await that never settles exits naturally, code 13.
//
// A .js file with no package.json "type" is one node decides by its syntax
// (containsModuleSyntax): with an import or export (even `export {}`), a
// top-level await or import.meta, or a top-level declaration of a name
// CommonJS's wrapper owns (`const require`), it is an ES module (micro
// first); without, CommonJS (tick first), and its throw is an
// uncaughtException. oam runs both as ES modules but orders them as node
// does.
//
// Regression guard: oam ran an ES module's top-level ticks ahead of its
// promise jobs.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-411-"));
const body = `
const log = [];
process.nextTick(() => log.push("tick"));
Promise.resolve().then(() => log.push("micro"));
queueMicrotask(() => log.push("micro2"));
`;
const esm = `${body}
await null;
log.push("after await");
await new Promise((r) => setTimeout(r, 5));
console.log(log.join(" | "));
`;
const cjs = `${body}
setTimeout(() => console.log(log.join(" | ")), 5);
`;
const timer = `
setTimeout(() => {
  const log = [];
  process.nextTick(() => log.push("tick"));
  Promise.resolve().then(() => log.push("micro"));
  queueMicrotask(() => log.push("micro2"));
  setTimeout(() => console.log(log.join(" | ")), 5);
}, 0);
`;
// A module import() loads: its own top level, and what the importer sees.
const imported = `
export const log = [];
process.nextTick(() => log.push("tick"));
Promise.resolve().then(() => log.push("micro"));
queueMicrotask(() => log.push("micro2"));
`;
const importer = `
const { log } = await import("./imported.mjs");
log.push("after import");
await new Promise((r) => setTimeout(r, 5));
console.log(log.join(" | "));
`;
const throws = `
process.nextTick(() => console.log("tick"));
Promise.resolve().then(() => console.log("micro"));
throw new Error("boom");
`;
const throwsLate = `
process.nextTick(() => console.log("tick"));
await null;
console.log("after await");
throw new Error("boom");
`;
const origin = `
process.on("uncaughtException", (e, origin) => console.log("uncaughtException", origin, e.message));
process.nextTick(() => console.log("tick"));
throw new Error("boom");
`;
const microThrow = `
process.on("exit", (code) => console.log("exit", code));
process.nextTick(() => console.log("tick"));
queueMicrotask(() => { throw new Error("boom"); });
`;
const throwsAfterTimer = `
process.on("exit", (code) => { console.log("exit", code); Promise.resolve().then(() => console.log("micro in exit")); });
await new Promise((r) => setTimeout(r, 1));
Promise.resolve().then(() => console.log("micro"));
process.nextTick(() => console.log("tick"));
throw new Error("boom");
`;
const cjsFatalExit = `
process.on("exit", (code) => {
  console.log("exit", code);
  Promise.resolve().then(() => console.log("micro in exit"));
  process.nextTick(() => console.log("tick in exit"));
});
Promise.resolve().then(() => console.log("micro"));
throw new Error("boom");
`;
// An op completes in the same turn as the timer that resumes the await.
const sameTurn = `
import fs from "node:fs";
process.on("exit", (code) => console.log("exit", code));
await new Promise((r) => setTimeout(() => {
  fs.readFile(new URL(import.meta.url), () => console.log("read callback"));
  const end = Date.now() + 200;
  while (Date.now() < end);
  r();
}, 1));
process.nextTick(() => console.log("tick"));
throw new Error("boom");
`;
const unsettled = `
process.on("exit", (code) => {
  console.log("exit", code);
  process.nextTick(() => console.log("tick in exit"));
  Promise.resolve().then(() => console.log("micro in exit"));
});
await new Promise(() => {});
`;
const exitListener = `
process.on("exit", () => {
  process.nextTick(() => console.log("tick in exit"));
  Promise.resolve().then(() => console.log("micro in exit"));
  console.log("exit");
});
`;
// Typeless .js files: node decides by syntax.
const plain = `
const log = [];
process.nextTick(() => log.push("tick"));
Promise.resolve().then(() => log.push("micro"));
setTimeout(() => console.log(log.join(" | ")), 5);
`;
const withExport = `${plain}
export const x = 1;
`;
const withMeta = `${plain}
void import.meta.url;
`;
const exportEmpty = `${plain}
export {};
`;
const redeclared = `${plain}
const require = 1;
`;
const metaInComment = `${plain}
// import.meta
`;
const awaitInCall = `${plain}
console.log(await 1);
`;
const globalExport = `${plain}
const global = 1;
export {};
`;
const plainThrows = `
process.on("uncaughtException", (e, origin) => console.log("uncaughtException", origin));
process.nextTick(() => console.log("tick"));
Promise.resolve().then(() => console.log("micro"));
throw new Error("boom");
`;
// A worker whose entry is an ES module.
const worker = `
import { Worker } from "node:worker_threads";
new Worker(new URL("./order.mjs", import.meta.url));
`;
const files = {
  "order.mjs": esm,
  "order.cjs": cjs,
  "timer.cjs": timer,
  "imported.mjs": imported,
  "importer.mjs": importer,
  "worker.mjs": worker,
  "throws.mjs": throws,
  "throws-late.mjs": throwsLate,
  "origin.mjs": origin,
  "micro-throw.mjs": microThrow,
  "exit-listener.cjs": exitListener,
  "throws-after-timer.mjs": throwsAfterTimer,
  "cjs-fatal-exit.cjs": cjsFatalExit,
  "unsettled.mjs": unsettled,
  "same-turn.mjs": sameTurn,
};
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);
// The typeless files sit in a directory of their own whose package.json
// declares no "type", so nothing above decides for them.
const typeless = path.join(dir, "typeless");
fs.mkdirSync(typeless);
fs.writeFileSync(path.join(typeless, "package.json"), "{}\n");
for (const [name, text] of Object.entries({
  "plain.js": plain,
  "with-export.js": withExport,
  "with-meta.js": withMeta,
  "plain-throws.js": plainThrows,
  "export-empty.js": exportEmpty,
  "redeclared.js": redeclared,
  "meta-in-comment.js": metaInComment,
  "await-in-call.js": awaitInCall,
  "global-export.js": globalExport,
})) {
  fs.writeFileSync(path.join(typeless, name), text);
}
try {
  for (const [label, file] of [
    ["ES module top level", "order.mjs"],
    ["CommonJS top level", "order.cjs"],
    ["timer callback", "timer.cjs"],
    ["import() from an ES module", "importer.mjs"],
    ["worker with an ES module entry", "worker.mjs"],
  ]) {
    const run = spawnSync(process.execPath, [file], { cwd: dir, encoding: "utf8" });
    console.log(`${label}: ${run.stdout.trim()}${run.status === 0 ? "" : ` (status ${run.status}) ${run.stderr.trim().split("\n")[0]}`}`);
  }
  // Exits that fail: what ran (stdout) and the status; the report itself
  // names the file each runtime's way.
  for (const [label, file] of [
    ["ES module entry that throws", "throws.mjs"],
    ["ES module entry that throws after await", "throws-late.mjs"],
    ["origin of an ES module entry's throw", "origin.mjs"],
    ["fatal microtask throw", "micro-throw.mjs"],
    ["'exit' listener's tick and promise job", "exit-listener.cjs"],
    ["ES module entry that throws after a timer", "throws-after-timer.mjs"],
    ["CommonJS throw: what runs after", "cjs-fatal-exit.cjs"],
    ["top-level await that never settles", "unsettled.mjs"],
    ["throw after a timer with an op done in its turn", "same-turn.mjs"],
  ]) {
    const run = spawnSync(process.execPath, [file], { cwd: dir, encoding: "utf8" });
    console.log(`${label}: ${run.stdout.trim().split(/\r?\n/).join(" | ")} (status ${run.status})`);
  }
  for (const [label, file] of [
    ["typeless .js, no module syntax", "plain.js"],
    ["typeless .js with an export", "with-export.js"],
    ["typeless .js with import.meta", "with-meta.js"],
    ["typeless .js, no module syntax, throws", "plain-throws.js"],
    ["typeless .js with only export {}", "export-empty.js"],
    ["typeless .js redeclaring require", "redeclared.js"],
    ["typeless .js with import.meta in a comment", "meta-in-comment.js"],
    ["typeless .js awaiting inside a call", "await-in-call.js"],
    ["typeless .js declaring global, with export {}", "global-export.js"],
  ]) {
    const run = spawnSync(process.execPath, [path.join("typeless", file)], { cwd: dir, encoding: "utf8" });
    console.log(`${label}: ${run.stdout.trim().split(/\r?\n/).join(" | ")} (status ${run.status})`);
  }
  // A "type" other than "module" or "commonjs" is none to node, which
  // runs CommonJS under it as CommonJS; so does oam.
  const oddType = path.join(dir, "odd-type");
  fs.mkdirSync(oddType);
  fs.writeFileSync(path.join(oddType, "package.json"), JSON.stringify({ type: "cjs" }));
  fs.writeFileSync(path.join(oddType, "helper.js"), "module.exports = { v: 1 };\n");
  fs.writeFileSync(path.join(oddType, "main.js"), `${plain}\nconsole.log(typeof require, require("./helper.js").v);\n`);
  const odd = spawnSync(process.execPath, [path.join("odd-type", "main.js")], { cwd: dir, encoding: "utf8" });
  console.log(`"type": "cjs" with CommonJS: ${odd.stdout.trim().split(/\r?\n/).join(" | ")} (status ${odd.status})`);
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
