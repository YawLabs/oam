// child_process stdio dispositions. 'inherit' must hand the child THIS
// process's own fds, which is what lets a launcher script -- the shape every
// npm `bin` shim uses -- pass its stdio straight through to a grandchild.
//
// Regression guard: spawn() used to ignore the `stdio` option entirely and
// always pipe, so a grandchild behind such a launcher wrote into pipes the
// launcher never forwarded and read from a stdin nobody fed. An MCP sidecar
// launched that way booted and then sat mute forever, which is invisible from
// the launcher's own exit status.
//
// Every observable here is printed by THIS process, never written by an
// inherited child, so the compared stdout is ordered deterministically: the
// grandchildren that produce output are read back through a captured pipe, and
// the children used for shape checks are silent.
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

// pid-scoped: the runner executes the node and oam legs back to back, and a
// shared fixed path would let one leg's cleanup delete the other's fixtures.
const dir = path.join(os.tmpdir(), `oam-conf-stdio-${process.pid}`);
mkdirSync(dir, { recursive: true });

const grand = path.join(dir, "grand.cjs");
writeFileSync(
  grand,
  "process.stdout.write('GRAND_OUT\\n');process.stderr.write('GRAND_ERR\\n');",
);

const silent = path.join(dir, "silent.cjs");
writeFileSync(silent, "process.exitCode = 0;");

// Re-spawns the grandchild on the SAME runtime with the disposition named in
// argv[2], and contributes no output of its own -- so whatever the parent
// captures came from the grandchild, through the launcher's fds or not at all.
const launcher = path.join(dir, "launcher.cjs");
writeFileSync(
  launcher,
  "const {spawn}=require('child_process');" +
    `const c=spawn(process.execPath,[${JSON.stringify(grand)}],{stdio:process.argv[2]});` +
    "c.on('close',(code)=>process.exit(code));",
);

const throughLauncher = (mode) =>
  new Promise((done) => {
    const cp = spawn(process.execPath, [launcher, mode], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    let out = "";
    let err = "";
    cp.stdout.on("data", (d) => { out += d; });
    cp.stderr.on("data", (d) => { err += d; });
    cp.on("close", (code) => done({ out: out.trim(), err: err.trim(), code }));
  });

// 1. inherit: the grandchild's writes reach the top through two hops.
const inherited = await throughLauncher("inherit");
console.log("inherit out", JSON.stringify(inherited.out));
console.log("inherit err", JSON.stringify(inherited.err));
console.log("inherit exit", inherited.code);

// 2. ignore: the grandchild's writes go to the null device, not to the parent.
const ignored = await throughLauncher("ignore");
console.log("ignore out", JSON.stringify(ignored.out));
console.log("ignore err", JSON.stringify(ignored.err));
console.log("ignore exit", ignored.code);

// 3. pipe: still piped into the LAUNCHER, which never forwards -- so nothing
//    reaches the top. The contrast with (1) is the whole point.
const piped = await throughLauncher("pipe");
console.log("pipe out", JSON.stringify(piped.out));
console.log("pipe err", JSON.stringify(piped.err));

// 4. Stream shape. node reports null for every non-piped slot, on the child
//    and in child.stdio alike.
const shape = spawn(process.execPath, [silent], { stdio: "inherit" });
console.log(
  "inherit shape",
  shape.stdin === null,
  shape.stdout === null,
  shape.stderr === null,
);
console.log(
  "inherit stdio",
  shape.stdio.length,
  shape.stdio[0] === null,
  shape.stdio[1] === null,
  shape.stdio[2] === null,
);
await new Promise((r) => shape.on("close", r));

const mixed = spawn(process.execPath, [silent], { stdio: ["ignore", "pipe", "inherit"] });
console.log(
  "mixed shape",
  mixed.stdin === null,
  mixed.stdout !== null,
  mixed.stderr === null,
);
await new Promise((r) => mixed.on("close", r));

const dflt = spawn(process.execPath, [silent]);
console.log(
  "default shape",
  dflt.stdin !== null,
  dflt.stdout !== null,
  dflt.stderr !== null,
);
await new Promise((r) => dflt.on("close", r));

// 5. spawnSync mirrors it: a slot that was never piped reads back as null,
//    not as an empty buffer.
const syncInherit = spawnSync(process.execPath, [silent], { stdio: "inherit" });
console.log(
  "spawnSync inherit",
  syncInherit.status,
  syncInherit.stdout === null,
  syncInherit.stderr === null,
  syncInherit.output.length,
);

const syncPipe = spawnSync(process.execPath, [silent]);
console.log(
  "spawnSync pipe",
  syncPipe.status,
  syncPipe.stdout !== null,
  syncPipe.stderr !== null,
);

// 6. fork() inherits by default -- a forked child's console output belongs to
//    the PARENT's stdout -- and only silent:true pipes it. Read back through a
//    launcher for the same reason as above.
const forked = path.join(dir, "forked.cjs");
writeFileSync(forked, "console.log('FORK_OUT');process.exit(0);");

const forkLauncher = path.join(dir, "fork-launcher.cjs");
writeFileSync(
  forkLauncher,
  "const {fork}=require('child_process');" +
    `const c=fork(${JSON.stringify(forked)},[],{silent:process.argv[2]==='silent'});` +
    "c.on('close',(code)=>process.exit(code));",
);

const throughFork = (mode) =>
  new Promise((done) => {
    const cp = spawn(process.execPath, [forkLauncher, mode], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    let out = "";
    cp.stdout.on("data", (d) => { out += d; });
    cp.on("close", (code) => done({ out: out.trim(), code }));
  });

const forkDefault = await throughFork("default");
console.log("fork default", JSON.stringify(forkDefault.out), forkDefault.code);
const forkSilent = await throughFork("silent");
console.log("fork silent", JSON.stringify(forkSilent.out), forkSilent.code);

rmSync(dir, { recursive: true, force: true });
console.log("exit 0");
