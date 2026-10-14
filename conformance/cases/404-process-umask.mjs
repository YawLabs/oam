// process.umask is the process's file mode creation mask: what is set is
// what a file or a directory created afterwards has taken off its mode, a
// child process starts with it, and a worker may read it but not set it.
//
// The mask this starts with is the runner's, so it is not printed: it is
// read, compared with what a set returns, and put back at the end. On
// Windows a mask keeps only the owner's read and write bits and the one
// thing it changes is whether a created file is read-only; both runtimes
// print what their platform does.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { Worker, isMainThread, parentPort } from "node:worker_threads";

const oct = (n) => n.toString(8);

if (!isMainThread) {
  const out = [];
  const attempt = (label, f) => {
    try {
      out.push(`${label}: ${oct(f())}`);
    } catch (e) {
      out.push(`${label}: ${e.name} ${e.code} | ${e.message}`);
    }
  };
  attempt("read", () => process.umask());
  attempt("read with undefined", () => process.umask(undefined));
  attempt("set", () => process.umask(0));
  attempt("set a bad mask", () => process.umask("9"));
  attempt("read again", () => process.umask());
  parentPort.postMessage(out);
} else {
  const start = process.umask();
  console.log("a read is a number:", typeof start, "and reads the same twice:", process.umask() === start);
  console.log("name and length:", process.umask.name, process.umask.length);

  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-404-"));
  let n = 0;
  // A mask can take away what a form needs (copying a file nobody may
  // read): then the row is the error's code.
  const created = (make) => {
    const f = path.join(dir, `f${n++}`);
    try {
      make(f);
      return oct(fs.statSync(f).mode & 0o777);
    } catch (e) {
      return e.code;
    }
  };
  const forms = {
    "openSync 0o666": (f) => fs.closeSync(fs.openSync(f, "w", 0o666)),
    "openSync 0o777": (f) => fs.closeSync(fs.openSync(f, "w", 0o777)),
    "openSync 0o600": (f) => fs.closeSync(fs.openSync(f, "w", 0o600)),
    writeFileSync: (f) => fs.writeFileSync(f, "x"),
    appendFileSync: (f) => fs.appendFileSync(f, "x"),
    mkdirSync: (f) => fs.mkdirSync(f),
    copyFileSync: (f) => {
      fs.writeFileSync(f + ".src", "x");
      fs.copyFileSync(f + ".src", f);
    },
  };

  console.log("the first set returns what was read:", process.umask(0) === start);
  for (const mask of [0, 0o22, 0o77, 0o222, 0o277, 0o400, 0o777, 0o1777]) {
    const was = process.umask(mask);
    const row = Object.entries(forms).map(([name, make]) => `${name}=${created(make)}`);
    console.log(`umask ${oct(mask)} (was ${oct(was)}) reads ${oct(process.umask())}:`, row.join(" "));
    process.umask(0);
  }

  // Through the promise and callback forms and a stream, under one mask.
  process.umask(0o222);
  const viaPromise = path.join(dir, "p");
  await fs.promises.writeFile(viaPromise, "x");
  const viaCallback = path.join(dir, "c");
  await new Promise((resolve, reject) => fs.writeFile(viaCallback, "x", (e) => (e ? reject(e) : resolve())));
  const viaStream = path.join(dir, "s");
  await new Promise((resolve, reject) => {
    const s = fs.createWriteStream(viaStream);
    s.on("error", reject);
    s.end("x", () => s.close(resolve));
  });
  console.log(
    "under 222: promises.writeFile, writeFile, createWriteStream ->",
    [viaPromise, viaCallback, viaStream].map((f) => oct(fs.statSync(f).mode & 0o777)).join(" "),
  );

  // A child starts with the mask its parent has.
  process.umask(0o27);
  const child = spawnSync(process.execPath, ["-e", "console.log(process.umask().toString(8))"], { encoding: "utf8" });
  console.log("a child of a 27 parent reads:", child.stdout.trim());
  process.umask(0);

  // What a mask may be.
  for (const mask of ["077", "0o77", "8", "", -1, 1.5, 2 ** 32, null, {}, true, NaN, 0n]) {
    const shown = typeof mask === "bigint" ? "0n" : Number.isNaN(mask) ? "NaN" : JSON.stringify(mask);
    try {
      const was = process.umask(mask);
      console.log(`umask(${shown}) -> ${oct(was)}, reads ${oct(process.umask())}`);
    } catch (e) {
      console.log(`umask(${shown}) throws ${e.name} ${e.code} | ${e.message}`);
    }
    process.umask(0);
  }

  // A worker reads the process's mask and may not set it.
  process.umask(0o27);
  const lines = await new Promise((resolve, reject) => {
    const worker = new Worker(new URL(import.meta.url));
    worker.on("message", resolve);
    worker.on("error", reject);
  });
  for (const line of lines) console.log("worker:", line);
  console.log("after the worker:", oct(process.umask()));

  process.umask(0);
  for (const f of fs.readdirSync(dir)) fs.chmodSync(path.join(dir, f), 0o777);
  process.umask(0o22);
  fs.rmSync(dir, { recursive: true, force: true });
  // On Windows 22 is kept as 0, the owner's bits being all there is.
  const kept = process.umask();
  console.log("put back:", process.umask(start) === kept && process.umask() === start);
}
