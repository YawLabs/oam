// A typeless .js file as a worker_threads Worker's entry, and as the script
// child_process.fork() runs, is decided as node decides a program's entry:
// CommonJS unless it has module syntax. A CommonJS worker entry is its
// thread's main module and has require; one with an import is an ES module
// without it, and the thread warns MODULE_TYPELESS_PACKAGE_JSON for its
// package.json as a process would (counted here from stderr, which is not
// compared).
//
// Regression guard: oam ran a typeless worker or fork entry as an ES module,
// so a CommonJS one died on its first require.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-465-"));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(dir, rel)), { recursive: true });
  fs.writeFileSync(path.join(dir, rel), text);
};
write("app/package.json", JSON.stringify({ name: "app" }));
write(
  "app/worker-cjs.js",
  "const { parentPort } = require('node:worker_threads');\nparentPort.postMessage(['cjs', typeof require, require.main === module].join(' '));\n",
);
write(
  "app/worker-esm.js",
  "import { parentPort } from 'node:worker_threads';\nparentPort.postMessage(['esm', typeof require, typeof import.meta.url].join(' '));\n",
);
write(
  "app/child-cjs.js",
  "process.send(['fork cjs', typeof require, require.main === module].join(' '), () => process.disconnect());\n",
);
write(
  "app/child-esm.js",
  "import process from 'node:process';\nprocess.send(['fork esm', typeof require].join(' '), () => process.disconnect());\n",
);
write(
  "app/workers.cjs",
  `const { Worker } = require('node:worker_threads');
const path = require('node:path');
(async () => {
  for (const f of ['worker-cjs.js', 'worker-esm.js']) {
    const w = new Worker(path.join(__dirname, f));
    const message = await new Promise((resolve) => {
      w.on('message', resolve);
      w.on('error', (e) => resolve('error ' + e.message.split('\\n')[0]));
    });
    await new Promise((r) => w.on('exit', r));
    console.log(f, '->', message);
  }
})();
`,
);
write(
  "app/forks.cjs",
  `const { fork } = require('node:child_process');
const path = require('node:path');
(async () => {
  for (const f of ['child-cjs.js', 'child-esm.js']) {
    const c = fork(path.join(__dirname, f), [], { stdio: ['ignore', 'inherit', 'ignore', 'ipc'] });
    let message = '(none)';
    c.on('message', (m) => { message = m; });
    const code = await new Promise((r) => c.on('exit', r));
    console.log(f, '->', message, 'exit', code);
  }
})();
`,
);
try {
  for (const file of ["workers.cjs", "forks.cjs"]) {
    const r = spawnSync(process.execPath, [file], { cwd: path.join(dir, "app"), encoding: "utf8" });
    const warnings = r.stderr.split("[MODULE_TYPELESS_PACKAGE_JSON]").length - 1;
    console.log(`${file}: ${r.stdout.trim().split(/\r?\n/).join(" | ")} (status ${r.status}, ${warnings} warning(s))`);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
