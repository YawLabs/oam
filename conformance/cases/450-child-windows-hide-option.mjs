// node's `windowsHide` spawn option, end to end.
//
// oam used to drop the option on the floor and start EVERY Windows child with
// CREATE_NO_WINDOW, which makes Windows start a fresh hidden conhost.exe per
// console child -- about 35 ms a spawn. libuv sets that flag only for
// `windowsHide: true` (and only when no stdio slot is inherited); without it
// a console child shares the parent's console. The flag is not observable
// from a script, so this case pins what is:
//
//   1. node's type check: anything but a boolean, null or undefined throws
//      ERR_INVALID_ARG_TYPE from spawn, spawnSync, fork and the Sync exec
//      forms, before any child starts. exec and execFile do NOT throw: they
//      hand spawn `!!options.windowsHide`.
//   2. a child's output, exit code and stdio shapes are the same with
//      `windowsHide` true, false or absent, over every spawn path oam has --
//      spawnSync, spawn, the extra-fd spawn (more than three stdio slots),
//      execFileSync, exec and an inherited slot.
//
// Children are this runtime's own binary (process.execPath), so the same
// lines hold on every platform; POSIX ignores the option, as node does.
import {
  exec,
  execFile,
  execFileSync,
  execSync,
  fork,
  spawn,
  spawnSync,
} from "node:child_process";
import { rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = os.tmpdir();
const child = path.join(dir, `oam-conf-450-${process.pid}.cjs`);
writeFileSync(
  child,
  "const fs=require('fs');" +
    "const extra=process.argv[2]==='extra';" +
    "process.stdout.write('out:'+process.argv.slice(2).join(',')+'\\n');" +
    "process.stderr.write('err\\n');" +
    "if(extra)fs.writeSync(4,'fd4\\n');" +
    "process.exitCode=Number(process.argv[3]||0);",
);

// 1. The type check, form by form.
const bad = [1, "yes", {}];
const forms = {
  spawn: (o) => spawn(process.execPath, [child], o),
  spawnSync: (o) => spawnSync(process.execPath, [child], o),
  execFile: (o) => execFile(process.execPath, [child], o),
  execFileSync: (o) => execFileSync(process.execPath, [child], o),
  exec: (o) => exec(`"${process.execPath}" "${child}"`, o),
  execSync: (o) => execSync(`"${process.execPath}" "${child}"`, o),
  fork: (o) => fork(child, [], o),
};
for (const [name, run] of Object.entries(forms)) {
  for (const value of bad) {
    try {
      run({ windowsHide: value });
      console.log(name, JSON.stringify(value), "no throw");
    } catch (e) {
      console.log(name, JSON.stringify(value), e.name, e.code, e.message);
    }
  }
}

// 2. Same results whatever the option says.
const settings = [true, false, undefined, null];
const show = (v) => (v === undefined ? "absent" : String(v));
const opt = (v) => (v === undefined ? {} : { windowsHide: v });

for (const v of settings) {
  const r = spawnSync(process.execPath, [child, "sync", "3"], opt(v));
  console.log(
    "spawnSync",
    show(v),
    r.status,
    JSON.stringify(String(r.stdout)),
    JSON.stringify(String(r.stderr)),
  );
}

const collect = (stream) =>
  new Promise((resolve) => {
    if (!stream) return resolve(null);
    let s = "";
    stream.on("data", (c) => (s += c));
    stream.on("end", () => resolve(s));
  });

for (const v of settings) {
  const cp = spawn(process.execPath, [child, "async", "4"], opt(v));
  const [out, err, code] = await Promise.all([
    collect(cp.stdout),
    collect(cp.stderr),
    new Promise((r) => cp.on("close", r)),
  ]);
  console.log("spawn", show(v), code, JSON.stringify(out), JSON.stringify(err));
}

for (const v of settings) {
  const cp = spawn(process.execPath, [child, "extra", "5"], {
    ...opt(v),
    // fd 4: oam's extra fds are one-way, and a 'pipe' at fd 4 is the child's
    // write end (the CDP convention); node's is two-way, so it reads alike.
    stdio: ["ignore", "pipe", "pipe", "ignore", "pipe"],
  });
  const [out, err, fd4, code] = await Promise.all([
    collect(cp.stdio[1]),
    collect(cp.stdio[2]),
    collect(cp.stdio[4]),
    new Promise((r) => cp.on("close", r)),
  ]);
  console.log("extra-fd", show(v), code, JSON.stringify(out), JSON.stringify(err), JSON.stringify(fd4));
}

for (const v of settings) {
  // An inherited slot: libuv leaves CREATE_NO_WINDOW off whatever the option
  // says. The child's stdout is this process's own, so its line lands here,
  // ahead of the status line printed once spawnSync has returned.
  const r = spawnSync(process.execPath, [child, "inherit"], {
    ...opt(v),
    stdio: ["ignore", "inherit", "pipe"],
  });
  console.log("inherit", show(v), r.status, r.stdout, JSON.stringify(String(r.stderr)));
}

for (const v of settings) {
  const out = execFileSync(process.execPath, [child, "efs"], { ...opt(v), encoding: "utf8", stdio: "pipe" });
  console.log("execFileSync", show(v), JSON.stringify(out));
}

for (const v of settings) {
  const out = await new Promise((resolve) =>
    exec(`"${process.execPath}" "${child}" exec`, opt(v), (err, stdout, stderr) =>
      resolve([err && err.code, stdout, stderr]),
    ),
  );
  console.log("exec", show(v), JSON.stringify(out));
}

rmSync(child, { force: true });
