// A whole-file fs call is two operations, and node reports them differently:
// readFile / writeFile / appendFile OPEN the path, then READ or WRITE the
// descriptor. A failed open names syscall `open` and the path; a failed read
// or write names syscall `read` / `write` and NO path ("EBUSY: resource busy
// or locked, read"). oam read the whole file in one std call and labelled
// every failure `open` with the path -- so a read failure on a file that
// exists also turned `process.loadEnvFile` into a false ENOENT where node
// throws "Contents of '<path>' should be a valid string.".
//
// Read and write failures produced deterministically, per platform:
// - Windows: another process holds a byte-range lock (LockFileEx) over the
//   file; open succeeds and the read or write fails EBUSY. The lock is held
//   by a PowerShell child for the duration of the checks.
// - Linux: /proc/self/mem opens and fails the read at offset 0 with EIO;
//   /dev/full opens and fails every write with ENOSPC.
// macOS has no such file, so it runs only the platform-neutral part (a
// missing file still fails the open, with the path).
// Runs in a fresh temp directory; absolute paths print relative to it.
import { spawn } from "node:child_process";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const home = process.cwd();
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-355-"));
process.chdir(dir);
const cwd = process.cwd();

const scrub = (v) => {
  if (typeof v !== "string") return v;
  return v.split("\\\\?\\" + cwd).join("<ns-cwd>").split(cwd).join("<cwd>");
};
const show = (name, e) => {
  if (!e) return console.log(name, "no error");
  console.log(name, JSON.stringify({
    name: e.name,
    code: e.code,
    errno: typeof e.errno,
    syscall: e.syscall,
    path: scrub(e.path),
    message: scrub(e.message),
  }));
};
const sync = (name, f) => {
  try {
    f();
    show(name);
  } catch (e) {
    show(name, e);
  }
};
const cb = (name, f) => new Promise((res) => f((e) => (show(name, e), res())));
const prom = async (name, f) => {
  try {
    await f();
    show(name);
  } catch (e) {
    show(name, e);
  }
};

// Every readFile surface, on a file whose read fails.
const reads = async (label, file) => {
  sync(`${label} readFileSync`, () => fs.readFileSync(file));
  sync(`${label} readFileSync utf8`, () => fs.readFileSync(file, "utf8"));
  sync(`${label} readFileSync latin1`, () => fs.readFileSync(file, "latin1"));
  await cb(`${label} readFile cb`, (k) => fs.readFile(file, k));
  await cb(`${label} readFile cb utf8`, (k) => fs.readFile(file, "utf8", k));
  await prom(`${label} promises.readFile`, () => fsp.readFile(file));
  await prom(`${label} promises.readFile utf8`, () => fsp.readFile(file, "utf8"));
  await prom(`${label} FileHandle.readFile`, async () => {
    const h = await fsp.open(file);
    try {
      await h.readFile();
    } finally {
      await h.close();
    }
  });
  sync(`${label} loadEnvFile`, () => process.loadEnvFile(file));
};
// Every writeFile / appendFile surface, on a file whose write fails.
const writes = async (label, file) => {
  sync(`${label} writeFileSync`, () => fs.writeFileSync(file, "x"));
  sync(`${label} appendFileSync`, () => fs.appendFileSync(file, "x"));
  await cb(`${label} writeFile cb`, (k) => fs.writeFile(file, "x", k));
  await cb(`${label} appendFile cb`, (k) => fs.appendFile(file, "x", k));
  await prom(`${label} promises.writeFile`, () => fsp.writeFile(file, "x"));
  await prom(`${label} promises.appendFile`, () => fsp.appendFile(file, "x"));
};

// Holds an exclusive byte-range lock over `file` in another process until
// `release()` is called; resolves once the lock is held.
const lockInChild = (file) =>
  new Promise((resolve, reject) => {
    const script = [
      "$ErrorActionPreference = 'Stop'",
      `$f = [System.IO.File]::Open('${file.replace(/'/g, "''")}', 'Open', 'ReadWrite', 'ReadWrite')`,
      "$f.Lock(0, 1048576)",
      "[Console]::Out.WriteLine('locked'); [Console]::Out.Flush()",
      "[void][Console]::In.ReadLine()",
      "$f.Unlock(0, 1048576); $f.Close()",
    ].join("; ");
    const child = spawn("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], {
      stdio: ["pipe", "pipe", "inherit"],
    });
    let out = "";
    child.on("error", reject);
    child.stdout.on("data", (d) => {
      out += d;
      if (out.includes("locked")) {
        resolve(() => new Promise((done) => {
          child.on("exit", done);
          child.stdin.end("\n");
        }));
      }
    });
    child.on("exit", (code) => {
      if (!out.includes("locked")) reject(new Error(`lock helper exited ${code}`));
    });
  });

try {
  // The open half still fails the open, with the path, on every platform.
  await reads("missing", "missing.env");

  if (process.platform === "win32") {
    fs.writeFileSync("locked.env", "A=1\n");
    const release = await lockInChild(path.join(cwd, "locked.env"));
    try {
      await reads("locked", "locked.env");
      await writes("locked", "locked.env");
    } finally {
      await release();
    }
    // The lock is gone: the same file reads and loads.
    sync("unlocked readFileSync", () => fs.readFileSync("locked.env", "utf8"));
    sync("unlocked loadEnvFile", () => process.loadEnvFile("locked.env"));
  } else if (fs.existsSync("/proc/self/mem") && fs.existsSync("/dev/full")) {
    await reads("proc-mem", "/proc/self/mem");
    await writes("dev-full", "/dev/full");
  }
} finally {
  process.chdir(home);
  fs.rmSync(dir, { recursive: true, force: true });
}
