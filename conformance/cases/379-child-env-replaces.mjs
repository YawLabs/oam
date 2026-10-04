// What environment a child starts with, through every child_process entry
// point. Two rules, both node's (normalizeSpawnArguments + libuv):
//
//   * An explicit `env` REPLACES the child's environment. On POSIX the child
//     gets exactly those pairs. On Windows libuv's make_program_env also adds
//     HOMEDRIVE, HOMEPATH, LOGONSERVER, PATH, SYSTEMDRIVE, SYSTEMROOT, TEMP,
//     USERDOMAIN, USERNAME, USERPROFILE and WINDIR -- each only when the given
//     env has no variable of that name in any case, and only when the parent
//     still has it (so one deleted from process.env is not added back).
//   * Without `env` the child gets process.env AS IT IS AT THE CALL: a
//     variable assigned at runtime is passed, and one deleted from
//     process.env is not -- even though it was in the environment this
//     process was started with.
//
// oam used to lay the pairs over its own startup environment instead, so an
// explicit env still carried every variable of the parent, and a deletion
// from process.env did not reach the child.
//
// Shape. The top level re-runs this file as a "stage" child with CE_BOOT set
// in its START-UP environment (the only way to have a variable the stage did
// not assign itself); the stage then deletes CE_BOOT from process.env, assigns
// CE_SET, and launches a reporter through each entry point. The reporter
// prints only CE_* names with values, plus the names (never the values) of
// libuv's set that it has, sorted -- so the output is the same on any machine
// for the same platform, and the node and oam legs are compared on the same
// one. The IPC channel's variable a fork() child is started with differs by
// runtime and is outside that filter (each child also removes its own from
// process.env).
import {
  exec,
  execFile,
  execFileSync,
  execSync,
  fork,
  spawn,
  spawnSync,
} from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const LIBUV = [
  "HOMEDRIVE", "HOMEPATH", "LOGONSERVER", "PATH", "SYSTEMDRIVE", "SYSTEMROOT",
  "TEMP", "USERDOMAIN", "USERNAME", "USERPROFILE", "WINDIR",
];
const REPORTER = `
const LIBUV = ${JSON.stringify(LIBUV)};
const out = [];
for (const k of Object.keys(process.env).sort()) {
  if (/^CE_/i.test(k)) out.push(k + "=" + process.env[k]);
  else if (LIBUV.includes(k.toUpperCase())) out.push(k);
}
process.stdout.write(out.join(" ") + "\\n");
`;

const self = fileURLToPath(import.meta.url);
const quote = (p) => JSON.stringify(p);

if (process.argv[2] !== "stage") {
  // pid-scoped: the runner executes the node and oam legs back to back.
  const dir = path.join(os.tmpdir(), `oam-conf-childenv-${process.pid}`);
  mkdirSync(dir, { recursive: true });
  const reporter = path.join(dir, "reporter.cjs");
  writeFileSync(reporter, REPORTER);
  const r = spawnSync(process.execPath, [self, "stage", reporter], {
    encoding: "utf8",
    env: { ...process.env, CE_BOOT: "boot", CE_KEEP: "keep" },
  });
  process.stdout.write(r.stdout);
  if (r.status !== 0) console.log("stage failed:", r.status, r.stderr);
  rmSync(dir, { recursive: true, force: true });
} else {
  const reporter = process.argv[3];
  const exe = process.execPath;
  const shellCmd = `${quote(exe)} ${quote(reporter)}`;
  const line = (s) => String(s).trim();

  // The stage's own view first: CE_BOOT and CE_KEEP came in at start-up.
  console.log("stage sees:", process.env.CE_BOOT, process.env.CE_KEEP);
  delete process.env.CE_BOOT;
  process.env.CE_SET = "set";

  const explicit = { CE_ONLY: "only" };

  const viaSpawn = (opts) =>
    new Promise((resolve) => {
      const cp = spawn(exe, [reporter], opts);
      let out = "";
      cp.stdout.on("data", (d) => (out += d));
      cp.on("close", () => resolve(line(out)));
    });
  const viaExec = (opts) =>
    new Promise((resolve) => exec(shellCmd, opts, (_e, out) => resolve(line(out))));
  const viaExecFile = (opts) =>
    new Promise((resolve) =>
      execFile(exe, [reporter], opts, (_e, out) => resolve(line(out))),
    );
  const viaFork = (opts) =>
    new Promise((resolve) => {
      const cp = fork(reporter, [], { silent: true, ...opts });
      let out = "";
      cp.stdout.on("data", (d) => (out += d));
      cp.on("exit", () => resolve(line(out)));
    });
  // A plain spawn with an 'ipc' slot, and one with extra fds: each takes its
  // own route to the native spawn.
  const viaSpawnIpc = (opts) =>
    new Promise((resolve) => {
      const cp = spawn(exe, [reporter], { stdio: ["pipe", "pipe", "pipe", "ipc"], ...opts });
      let out = "";
      cp.stdout.on("data", (d) => (out += d));
      cp.on("close", () => resolve(line(out)));
    });
  const viaSpawnExtraFds = (opts) =>
    new Promise((resolve) => {
      const cp = spawn(exe, [reporter], {
        stdio: ["pipe", "pipe", "pipe", "ignore", "ignore"],
        ...opts,
      });
      let out = "";
      cp.stdout.on("data", (d) => (out += d));
      cp.on("close", () => resolve(line(out)));
    });

  const entries = [
    ["spawnSync", async (o) => line(spawnSync(exe, [reporter], { encoding: "utf8", ...o }).stdout)],
    ["execSync", async (o) => line(execSync(shellCmd, { encoding: "utf8", ...o }))],
    ["execFileSync", async (o) => line(execFileSync(exe, [reporter], { encoding: "utf8", ...o }))],
    ["spawn", viaSpawn],
    ["exec", viaExec],
    ["execFile", viaExecFile],
    ["fork", viaFork],
    ["spawn+ipc", viaSpawnIpc],
    ["spawn+fds", viaSpawnExtraFds],
  ];

  for (const [name, run] of entries) {
    console.log(`${name} default:`, await run({}));
    console.log(`${name} explicit:`, await run({ env: explicit }));
  }

  // The finer points of building the pairs, on the synchronous path.
  const sync = (env) => line(spawnSync(exe, [reporter], { encoding: "utf8", env }).stdout);
  // An inherited key counts, an undefined value is left out, the rest are
  // stringified.
  const proto = Object.assign(Object.create({ CE_PROTO: "p" }), {
    CE_UNDEF: undefined,
    CE_NULL: null,
    CE_NUM: 5,
  });
  console.log("prototype/undefined/null:", sync(proto));
  // A libuv name given in another case is not added a second time. (Real
  // values, only the names are printed: an oam reporter puts its code cache
  // under the temp directory, so a made-up TEMP would land one in the cwd.)
  console.log(
    "lower-case path given:",
    sync({ CE_X: "x", Path: process.env.PATH, temp: process.env.TEMP }),
  );
  // Values from process.env, not the start-up environment: assigned...
  process.env.CE_LATE = "late";
  console.log("after assigning CE_LATE:", sync(undefined));
  // ...and deleted. libuv adds a missing name only from the parent's current
  // environment, so a deleted TEMP is not put back into an explicit env. TMP
  // (outside the printed set) keeps the reporter's temp directory where it
  // was; without TMP or TEMP Windows would fall back to USERPROFILE.
  const tmp = os.tmpdir();
  const savedTemp = process.env.TEMP;
  delete process.env.TEMP;
  console.log("explicit, TEMP deleted:", sync({ CE_Y: "y", TMP: tmp }));
  if (savedTemp !== undefined) process.env.TEMP = savedTemp;
}
