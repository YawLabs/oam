// A `shell` option given as a STRING names the shell to run: the command goes
// to that program (as `-c <command>`, or `/d /s /c "<command>"` for cmd.exe),
// not to the platform default. Pinned through a shell that does not exist --
// the spawn must fail for THAT name, where running the default shell instead
// would succeed -- and through the platform's own shell named explicitly.
import { exec, execFile, spawn, spawnSync } from "node:child_process";

const missing = "oam-conf-no-such-shell";
const shape = (err) =>
  err ? `${err.code} | ${err.syscall} | ${err.path} | ${JSON.stringify(err.spawnargs)}` : "no error";

// Sync.
const sync = spawnSync("echo hi", { shell: missing });
console.log("spawnSync:", shape(sync.error), "| status", sync.status);

// Async: 'error', then 'close'.
await new Promise((done) => {
  const cp = spawn("echo hi", { shell: missing });
  cp.on("error", (err) => console.log("spawn:", shape(err)));
  cp.on("close", done);
});

// exec and execFile hand the name through as well.
await new Promise((done) => {
  exec("echo hi", { shell: missing }, (err) => {
    console.log("exec:", err ? `${err.code} | ${err.path} | ${err.cmd}` : "no error");
    done();
  });
});
await new Promise((done) => {
  execFile("echo", ["hi"], { shell: missing }, (err) => {
    console.log("execFile:", err ? `${err.code} | ${err.path} | ${err.cmd}` : "no error");
    done();
  });
});

// The platform's shell, named: the command runs under it.
const named = process.platform === "win32" ? "cmd.exe" : "/bin/sh";
const ok = spawnSync("echo hi", { shell: named, encoding: "utf8" });
console.log("named shell:", ok.status, JSON.stringify(ok.stdout.trim()));
await new Promise((done) => {
  exec("echo hi", { shell: named }, (err, stdout) => {
    console.log("exec named shell:", err ? err.code : null, JSON.stringify(stdout.trim()));
    done();
  });
});
