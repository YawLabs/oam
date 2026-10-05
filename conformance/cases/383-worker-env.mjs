// What process.env a worker_threads Worker starts with, by its `env` option,
// as node's Worker constructor reads it:
//
//   * undefined / null / process.env: a COPY of the creator's process.env as
//     it is at `new Worker` -- assignments and deletions made by the script
//     included -- that neither side's later writes reach;
//   * an object: exactly its own enumerable string-keyed entries, each value
//     stringified (undefined becomes 'undefined'), and kept as UTF-8 (a
//     lone surrogate becomes U+FFFD);
//   * SHARE_ENV: the creator's own environment, shared -- each side sees the
//     other's writes.
//
// A worker's own copy is case-sensitive even on Windows; a shared one folds
// case as the main thread's does. A worker has no '=C:' style names. Its
// children get its process.env, plus on Windows libuv's additions read from
// the main thread's environment (so one the main thread deleted is not
// added). Anything else as `env` throws ERR_INVALID_ARG_TYPE.
//
// oam ignored the option: every worker got the environment oam started with
// (a variable the script had deleted came back, one it had assigned was
// missing, and an object env was not applied).
//
// The top level re-runs this file as a "stage" child with W_BOOT in its
// START-UP environment, which the stage deletes before making any worker.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import {
  SHARE_ENV,
  Worker,
  isMainThread,
  parentPort,
  workerData,
} from "node:worker_threads";

const self = fileURLToPath(import.meta.url);
const LIBUV = [
  "HOMEDRIVE", "HOMEPATH", "LOGONSERVER", "PATH", "SYSTEMDRIVE", "SYSTEMROOT",
  "TEMP", "USERDOMAIN", "USERNAME", "USERPROFILE", "WINDIR",
];
const mine = (env) =>
  Object.keys(env)
    .filter((k) => /^W_/i.test(k))
    .sort()
    .map((k) => `${k}=${escape(env[k])}`)
    .join(" ");

if (!isMainThread) {
  const mode = workerData;
  const report = {
    mode,
    vars: mine(process.env),
    hidden: process.env["=C:"] === undefined,
  };
  if (mode === "object") {
    // Sorted: node keeps a worker's object env in a std::unordered_map,
    // whose iteration order is the C++ library's (insertion order under
    // MSVC, reversed under libc++ and libstdc++ for this few keys), so the
    // set of keys is node's and their order is not.
    report.keys = Object.keys(process.env).sort();
    report.caseFold = process.env.w_obj !== undefined;
  }
  if (mode === "child") {
    const r = spawnSync(
      process.execPath,
      ["-e", `
        const libuv = ${JSON.stringify(LIBUV)};
        const keys = Object.keys(process.env);
        console.log(JSON.stringify({
          vars: keys.filter((k) => /^W_/i.test(k)).sort(),
          libuv: libuv.filter((n) => keys.some((k) => k.toUpperCase() === n)),
        }));`],
      { encoding: "utf8" },
    );
    report.child = r.stdout.trim();
  }
  if (mode === "share") {
    report.caseFold = process.env.w_set !== undefined;
    // Wait for the main thread's write to show up here, with no message
    // in between to carry it.
    let tries = 0;
    const poll = setInterval(() => {
      if (process.env.W_LATE === undefined && ++tries < 500) return;
      clearInterval(poll);
      report.late = process.env.W_LATE;
      process.env.W_FROM_WORKER = "w";
      parentPort.postMessage(report);
    }, 10);
    parentPort.postMessage("ready");
  } else {
    process.env.W_FROM_WORKER = "w";
    parentPort.postMessage(report);
  }
} else if (process.argv[2] === "stage") {
  delete process.env.W_BOOT;
  process.env.W_SET = "set";
  // Gone from the main thread's environment, so libuv does not add it to a
  // worker's child either.
  delete process.env.USERDOMAIN;

  const run = (mode, opts) =>
    new Promise((resolve) => {
      const w = new Worker(self, { workerData: mode, ...opts });
      w.on("message", (m) => {
        if (m === "ready") {
          process.env.W_LATE = "late";
          return;
        }
        console.log(JSON.stringify(m));
      });
      w.on("error", (e) => console.log(mode, "error", e.message));
      w.on("exit", resolve);
    });

  await run("default", {});
  // Taken at construction: this assignment comes too late for the worker.
  const late = run("default-late", {});
  process.env.W_AFTER = "after";
  await late;
  delete process.env.W_AFTER;
  await run("null", { env: null });
  await run("procenv", { env: process.env });
  console.log("after own copies, W_FROM_WORKER:", process.env.W_FROM_WORKER);
  await run("object", {
    env: Object.assign(Object.create({ W_INHERITED: "i" }), {
      W_OBJ: "o",
      W_NUM: 2,
      W_UNDEF: undefined,
      W_SURROGATE: "s\ud800",
      [Symbol("s")]: "x",
    }),
  });
  await run("child", { env: { W_CHILD: "c" } });
  await run("share", { env: SHARE_ENV });
  console.log("after share, W_FROM_WORKER:", process.env.W_FROM_WORKER);

  for (const bad of ["str", 1, true, () => {}]) {
    try {
      new Worker(self, { env: bad });
      console.log("no throw");
    } catch (e) {
      console.log(e.name, e.code, JSON.stringify(e.message));
    }
  }
} else {
  const r = spawnSync(process.execPath, [self, "stage"], {
    env: { ...process.env, W_BOOT: "boot" },
    encoding: "utf8",
  });
  process.stdout.write(r.stdout);
  process.stdout.write(r.stderr);
  console.log("stage exit", r.status);
}
