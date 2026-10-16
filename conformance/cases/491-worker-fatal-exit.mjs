// A worker whose run fails fatally ends as node's does (#399, items 3 and 4):
//
//   * inside the worker, process.exitCode becomes 1 and 'exit' is emitted
//     with 1, before the parent hears of the error: what the listener writes
//     is there when the parent's 'error' event runs. Nothing the listener
//     queues runs. The thread's exit code is process.exitCode as the
//     listener leaves it (a listener that sets 9 makes the worker exit 9);
//   * the parent's 'error' event carries the worker's error, not a rendered
//     report: an Error comes back as an object of the nearest built-in error
//     class in its prototype chain, with its own message, name, stack and
//     other own properties (node's error serializer; a `cause` rebuilt the
//     same way), a Symbol as Symbol.for its description, a function as its
//     inspection string, any other thrown value as itself, and an unhandled
//     rejection of a non-Error as node's ERR_UNHANDLED_REJECTION error;
//   * nothing is printed by the runtime itself (the parent handles 'error').
//
// The error the parent gets is the one that ended the run: what an
// uncaughtException listener threw in place of the original (with nothing of
// oam's own bookkeeping on it), not a failed dynamic import() the worker
// caught; and a cause chain 40 deep arrives whole.
//
// (The listener writes to a file, not through parentPort: node delivers the
// error on the worker's internal port, so its order against a user message
// is not fixed.)
//
// oam returned before emitting 'exit' in the worker, handed the parent the
// whole rendered report as err.message (a plain Error whatever was thrown),
// and printed that report a second time on stderr under "worker N:". The
// top level re-runs this file as a "stage" child to see its stderr.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { Worker, isMainThread, workerData } from "node:worker_threads";

const self = fileURLToPath(import.meta.url);

const fixture = `
const fs = require("node:fs");
const { workerData: { kind, log } } = require("node:worker_threads");
process.on("exit", (code) => {
  fs.appendFileSync(log, "exit listener " + code + " exitCode " + process.exitCode + "; ");
  if (kind === "exitcode") process.exitCode = 9;
  Promise.resolve().then(() => fs.appendFileSync(log, "exit listener promise job; "));
});
class MyError extends RangeError {
  constructor(message) { super(message); this.extra = 1; }
}
if (kind === "sync") throw new TypeError("boom sync");
if (kind === "timer") setTimeout(() => { throw new Error("boom timer"); }, 1);
if (kind === "reject") Promise.reject(new RangeError("boom reject"));
if (kind === "reject-string") Promise.reject("rs");
if (kind === "throw-42") throw 42;
if (kind === "throw-object") throw { a: 1, b: [2] };
if (kind === "custom") { const e = new MyError("mine"); e.code = "E_MINE"; throw e; }
if (kind === "exitcode") throw new Error("boom exitcode");
if (kind === "throw-symbol") throw Symbol("s");
if (kind === "throw-undefined") throw undefined;
if (kind === "throw-function") throw function named() {};
if (kind === "cause") throw new Error("outer", { cause: new TypeError("inner") });
if (kind === "deep-cause") {
  let e = new Error("level 0");
  for (let i = 1; i < 40; i++) e = new Error("level " + i, { cause: e });
  throw e;
}
if (kind === "handler-rethrow") {
  process.on("uncaughtException", () => { throw new Error("again"); });
  throw new Error("first");
}
if (kind === "dyn-import") {
  const bad = require("node:url").pathToFileURL(require("node:path").join(__dirname, "bad.mjs")).href;
  import(bad).catch(() => setTimeout(() => { throw new Error("after a caught import"); }, 1));
}
`;

function describe(e) {
  if (typeof e === "symbol") return `symbol ${String(e)} registered ${Symbol.keyFor(e)}`;
  if (e === null || typeof e !== "object") return `${typeof e} ${String(e)}`;
  const proto = Object.getPrototypeOf(e);
  const parts = [
    `class ${proto && proto.constructor ? proto.constructor.name : null}`,
    `instanceof ${e instanceof Error}`,
    `message ${JSON.stringify(e.message)}`,
    `name ${e.name}`,
    `code ${e.code}`,
    `keys ${Reflect.ownKeys(e).map(String).join(",")}`,
  ];
  if (typeof e.stack === "string") {
    const first = e.stack.split("\n")[0];
    const where = e.stack.includes("w.cjs") || e.stack.includes("491-");
    parts.push(`stack ${JSON.stringify(first)} ${where}`);
  }
  if ("cause" in e) {
    const cause = e.cause;
    let depth = 0;
    for (let c = e; c !== null && typeof c === "object" && "cause" in c; c = c.cause) depth++;
    parts.push(`cause ${Object.getPrototypeOf(cause).constructor.name} ${cause.message} depth ${depth}`);
  }
  if (e instanceof Error) {
    const tag = Object.getOwnPropertyDescriptor(e, Symbol.toStringTag);
    parts.push(`tag ${tag && tag.value}`);
  } else {
    parts.push(`json ${JSON.stringify(e)}`);
  }
  return parts.join(" ");
}

if (!isMainThread) {
  // The ES module entry's row.
  process.on("exit", (code) => {
    fs.appendFileSync(workerData.log, `exit listener ${code} exitCode ${process.exitCode}; `);
    Promise.resolve().then(() => fs.appendFileSync(workerData.log, "exit listener promise job; "));
  });
  if (workerData.kind === "esm-dyn-import") {
    // A failed import() the worker catches -- one that does not compile, one
    // that throws as it runs -- is not the error that ends the run.
    const dir = path.dirname(workerData.log);
    for (const name of ["bad.mjs", "throws.mjs"]) {
      try {
        await import(pathToFileURL(path.join(dir, name)).href);
      } catch {}
    }
    setTimeout(() => { throw new Error("after a caught import"); }, 1);
  } else {
    throw new SyntaxError(`boom ${workerData.kind}`);
  }
} else if (process.argv[2] === "stage") {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-491-"));
  const file = path.join(dir, "w.cjs");
  fs.writeFileSync(file, fixture);
  fs.writeFileSync(path.join(dir, "bad.mjs"), "let x = ;\n");
  fs.writeFileSync(path.join(dir, "throws.mjs"), "throw new TypeError(\"as it runs\");\n");
  const kinds = [
    "sync", "timer", "reject", "reject-string", "throw-42", "throw-object",
    "custom", "exitcode", "throw-symbol", "throw-undefined", "throw-function", "cause", "deep-cause",
    "handler-rethrow", "dyn-import", "esm", "esm-dyn-import",
  ];
  for (const kind of kinds) {
    await new Promise((resolve) => {
      const log = path.join(dir, `${kind}.log`);
      fs.writeFileSync(log, "");
      const logged = () => `[log: ${fs.readFileSync(log, "utf8")}]`;
      const worker = new Worker(kind.startsWith("esm") ? self : file, { workerData: { kind, log } });
      const seen = [];
      worker.on("message", (m) => seen.push(`message ${m}`));
      worker.on("error", (e) => seen.push(`error ${logged()} ${describe(e)}`));
      worker.on("exit", (code) => {
        seen.push(`exit ${code} ${logged()}`);
        console.log(`${kind}: ${seen.join(" | ")}`);
        resolve();
      });
    });
  }
  fs.rmSync(dir, { recursive: true, force: true });
} else {
  const child = spawnSync(process.execPath, [self, "stage"], { encoding: "utf8" });
  process.stdout.write(child.stdout);
  console.log(`status ${child.status} stderr ${JSON.stringify(child.stderr)}`);
}
