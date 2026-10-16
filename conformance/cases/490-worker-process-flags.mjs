// A worker_threads Worker's process flags are decided as node's Worker
// constructor decides them (#370):
//
//   * with neither `env` nor `execArgv` given (or `env: SHARE_ENV`), the
//     worker inherits its creator's options whole -- the command line's and
//     NODE_OPTIONS's alike;
//   * given either, it starts from node's defaults, applies NODE_OPTIONS from
//     the environment it is given (its creator's when it is given none),
//     then its `execArgv` (its creator's process.execArgv when it is given
//     none). So `execArgv: []` under a parent run with `--no-warnings` has
//     the default 'warning' listener back, and `env: {}` drops a parent's
//     NODE_OPTIONS;
//   * a worker that a worker creates with neither inherits the options of
//     the worker that created it, not the main thread's;
//   * NODE_NO_WARNINGS is read from the worker's own process.env: an `env`
//     that sets it takes the listener off, one that leaves it out keeps it
//     even when the parent's environment had it.
//
// The flags covered are the per-environment ones oam knows: --no-warnings,
// --no-deprecation and the --tls-min/max-v1.x defaults.
//
// `stderr: true` sends what the worker writes to process.stderr -- its
// warnings included -- to `worker.stderr`, not to the process's stderr.
//
// oam ran every worker with the flags of its parent's command line, whatever
// the worker was given, and wrote a `stderr: true` worker's output to the
// process's stderr. The top level re-runs this file as a "stage" child under
// each parent configuration and prints the child's stdout and stderr.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { SHARE_ENV, Worker, isMainThread, parentPort, workerData } from "node:worker_threads";
import tls from "node:tls";

const self = fileURLToPath(import.meta.url);
// node's one-time `--trace-warnings` hint is not printed by oam (a documented
// divergence: oam has no such flag); the PID differs run to run.
const clean = (text) =>
  text
    .split("\n")
    .filter((line) => !line.startsWith("(Use `"))
    .join("\n")
    .replace(/\((node|oam):\d+\)/g, "(PID)");

if (!isMainThread && workerData.nest) {
  // A worker that creates one with no options: the inner worker inherits
  // this worker's options, not the main thread's.
  const inner = new Worker(self, { workerData: `${workerData.label} inner` });
  inner.on("message", (m) => parentPort.postMessage(m));
} else if (!isMainThread) {
  parentPort.postMessage({
    lc: process.listenerCount("warning"),
    nd: process.noDeprecation === true,
    tls: tls.DEFAULT_MIN_VERSION,
  });
  process.emitWarning(`from ${workerData}`);
} else if (process.argv[2] === "stage") {
  const rows = [
    ["default", {}],
    ["execArgv []", { execArgv: [] }],
    ["execArgv --no-warnings", { execArgv: ["--no-warnings"] }],
    ["execArgv --tls-min-v1.3", { execArgv: ["--tls-min-v1.3"] }],
    ["env {}", { env: {} }],
    ["env NODE_NO_WARNINGS", { env: { NODE_NO_WARNINGS: "1" } }],
    ["env NODE_OPTIONS", { env: { NODE_OPTIONS: "--no-deprecation" } }],
    ["SHARE_ENV", { env: SHARE_ENV }],
    ["stderr", { stderr: true }],
    ["stderr env NODE_NO_WARNINGS", { stderr: true, env: { NODE_NO_WARNINGS: "1" } }],
    ["nested execArgv --no-warnings", { execArgv: ["--no-warnings"] }, true],
    ["nested execArgv --tls-min-v1.3", { execArgv: ["--tls-min-v1.3"] }, true],
    ["nested env NODE_OPTIONS", { env: { NODE_OPTIONS: "--no-deprecation" } }, true],
    ["nested env {}", { env: {} }, true],
  ];
  for (const [label, options, nest] of rows) {
    await new Promise((resolve) => {
      const data = nest ? { label, nest } : label;
      const worker = new Worker(self, { ...options, workerData: data });
      let captured = "";
      if (options.stderr) worker.stderr.on("data", (chunk) => (captured += chunk));
      let report;
      worker.on("message", (m) => (report = m));
      worker.on("exit", (code) => {
        let line = `${label}: listeners ${report.lc} noDeprecation ${report.nd} tls ${report.tls} exit ${code}`;
        if (options.stderr) line += ` captured ${JSON.stringify(clean(captured))}`;
        console.log(line);
        resolve();
      });
    });
  }
} else {
  const base = { ...process.env };
  delete base.NODE_OPTIONS;
  delete base.NODE_NO_WARNINGS;
  const parents = [
    ["parent plain", [], {}],
    ["parent --no-warnings --tls-min-v1.0", ["--no-warnings", "--tls-min-v1.0"], {}],
    ["parent NODE_NO_WARNINGS=1", [], { NODE_NO_WARNINGS: "1" }],
    ["parent NODE_OPTIONS=--no-warnings", [], { NODE_OPTIONS: "--no-warnings" }],
  ];
  for (const [label, args, env] of parents) {
    const child = spawnSync(process.execPath, [...args, self, "stage"], {
      env: { ...base, ...env },
      encoding: "utf8",
    });
    console.log(`== ${label} (status ${child.status})`);
    process.stdout.write(child.stdout);
    console.log(`-- stderr\n${clean(child.stderr).trimEnd()}`);
  }
}
