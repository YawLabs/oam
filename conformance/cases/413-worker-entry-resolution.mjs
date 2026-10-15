// How `new Worker(filename)` finds and runs its entry, as node's does (#287).
//
//   * A string entry must be absolute or start with './' or '../'; anything
//     else -- a bare name, a `file://` URL string -- throws ERR_WORKER_PATH
//     from the constructor. A URL object is the file it names.
//   * The constructor never opens the entry. A missing one is reported by
//     the started thread: the Worker is made, emits 'error' with an Error
//     whose code is MODULE_NOT_FOUND (message "Cannot find module '<path>'",
//     requireStack []), and exits 1.
//   * The thread finds it as node finds a program's entry: the file, then
//     with .js added, then a directory's index.js.
//   * An extension neither loader knows (x.txt, and x.js. -- a trailing dot,
//     which node reads as the extension ".") runs as CommonJS, unless the
//     package is "type": "module" or the source has module syntax: then the
//     ES module loader refuses it with a TypeError, ERR_UNKNOWN_FILE_EXTENSION.
//
// oam threw a TypeError ("worker script not found") from the constructor for
// a missing entry, resolved bare names against the cwd, and refused x.js.
// and x.txt as an "unsupported module type"; its 'error' event carried only
// a message, never a code.
import { Worker } from "node:worker_threads";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oam-413-"));
const scrub = (s) => String(s).split(tmp).join("<TMP>").split(path.sep).join("/");

function row(label, make) {
  return new Promise((resolve) => {
    let worker;
    try {
      worker = make();
    } catch (e) {
      console.log(`${label}: throws ${e.name} ${e.code} ${scrub(e.message)}`);
      resolve();
      return;
    }
    const seen = [`made threadId:${typeof worker.threadId}`];
    worker.on("message", (m) => seen.push(`message ${scrub(m)}`));
    worker.on("error", (e) => {
      seen.push(
        `error ${e instanceof Error} ${e.constructor.name} ${e.code} ${scrub(e.message)}` +
          ` keys:${Object.keys(e).join(",")}` +
          (e.requireStack ? ` requireStack:${JSON.stringify(e.requireStack)}` : ""),
      );
    });
    worker.on("exit", (code) => {
      seen.push(`exit ${code}`);
      console.log(`${label}: ${seen.join(" | ")}`);
      resolve();
    });
  });
}

const cjs =
  "require('node:worker_threads').parentPort.postMessage(" +
  "'cjs ' + require('node:path').basename(__filename));";
const esm =
  "import { parentPort } from 'node:worker_threads';" +
  "parentPort.postMessage('esm ' + import.meta.url.split('/').pop());";

function dir(name, type, files) {
  const d = path.join(tmp, name);
  fs.mkdirSync(d);
  if (type) fs.writeFileSync(path.join(d, "package.json"), JSON.stringify({ type }));
  for (const [file, source] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(d, file)), { recursive: true });
    fs.writeFileSync(path.join(d, file), source);
  }
  return d;
}

// A relative entry is resolved against the cwd when the Worker is made.
async function inDir(d, run) {
  const cwd = process.cwd();
  process.chdir(d);
  try {
    await run();
  } finally {
    process.chdir(cwd);
  }
}

try {
  // The constructor's own checks.
  await row("bare name", () => new Worker("worker-413.js"));
  await row("file: URL string", () => new Worker("file:///worker-413.js"));

  // A missing entry: made, then MODULE_NOT_FOUND.
  await row("missing absolute", () => new Worker(path.join(tmp, "nope.js")));
  await row("missing .mjs", () => new Worker(path.join(tmp, "nope.mjs")));
  await row("missing URL", () => new Worker(pathToFileURL(path.join(tmp, "nope.js"))));
  await row("missing directory", () => new Worker(path.join(tmp, "no-dir", "w.js")));
  await row("missing trailing dot", () => new Worker(path.join(tmp, "nope.js.")));
  await row("directory, no index", () => new Worker(tmp));
  await inDir(tmp, () => row("missing ./relative", () => new Worker("./nope.js")));

  // "type": "commonjs": every entry here is CommonJS unless it has module syntax.
  const c = dir("cjs", "commonjs", {
    "a.js": cjs,
    "sub/index.js": cjs,
    "x.txt": cjs,
    "w.js.": cjs,
    "m.mjs.": esm,
  });
  await row("commonjs: .js added", () => new Worker(path.join(c, "a")));
  await row("commonjs: directory index", () => new Worker(path.join(c, "sub")));
  await row("commonjs: x.txt", () => new Worker(path.join(c, "x.txt")));
  await row("commonjs: w.js.", () => new Worker(path.join(c, "w.js.")));
  await row("commonjs: m.mjs. (module syntax)", () => new Worker(path.join(c, "m.mjs.")));

  // No "type": the same, decided by the source.
  const n = dir("untyped", null, { "x.txt": cjs, "w.js.": cjs, "e.js.": esm, "e.txt": esm });
  await row("untyped: x.txt", () => new Worker(path.join(n, "x.txt")));
  await row("untyped: w.js.", () => new Worker(path.join(n, "w.js.")));
  await row("untyped: e.js. (module syntax)", () => new Worker(path.join(n, "e.js.")));
  await row("untyped: e.txt (module syntax)", () => new Worker(path.join(n, "e.txt")));
  await inDir(n, () => row("untyped: ./w.js.", () => new Worker("./w.js.")));
  await inDir(c, () => row("untyped: ../ relative", () => new Worker("../untyped/x.txt")));

  // "type": "module": an unknown extension is the ES module loader's, which refuses it.
  const m = dir("module", "module", { "a.js": esm, "x.txt": esm, "w.js.": esm, "c.cjs.": cjs });
  await row("module: .js added", () => new Worker(path.join(m, "a")));
  await row("module: x.txt", () => new Worker(path.join(m, "x.txt")));
  await row("module: w.js.", () => new Worker(path.join(m, "w.js.")));
  await row("module: c.cjs.", () => new Worker(path.join(m, "c.cjs.")));
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
