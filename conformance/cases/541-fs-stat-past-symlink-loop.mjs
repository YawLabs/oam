// stat of a path that goes on past a looping symlink, and what reads a
// path's attributes without following it (#362).
//
// On Windows libuv stats a path first with GetFileInformationByName (Windows
// 11 24H2 and later). That answers ERROR_PATH_NOT_FOUND for a path that
// continues past a link to itself, and libuv reports it as it is: ENOENT.
// For the looping link itself the call cannot answer, libuv opens the path
// instead, and the open fails ERROR_CANT_RESOLVE_FILENAME: ELOOP. A recursive
// mkdir reports what its stat says, so `mkdir("l/a", { recursive: true })`
// is ENOENT in all three forms, and `mkdir("l", ...)` ELOOP.
//
// access and chmod ask GetFileAttributesW / SetFileAttributesW, which read
// and set a link's own attributes: a looping or dangling link is accessible
// (existsSync, which stats as well, still says false), and chmod of a
// dangling link changes the link. A read-only directory passes access W_OK
// (libuv: directories cannot be read-only on Windows).
//
// Regression guard: oam stat through a handle whose open failed ELOOP for
// both, and access and chmod followed the link.
//
// Windows only. Creating a symlink needs Developer Mode or elevation; a
// refused one prints its refusal, the same under both runtimes.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

if (process.platform !== "win32") {
  console.log("Windows link loops: nothing to stat here");
} else {
  const start = process.cwd();
  const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-541-"));
  process.chdir(made);
  const D = process.cwd();
  process.on("exit", () => {
    try {
      process.chdir(start);
      for (const f of ["ro", "rof", "dang"]) {
        try {
          fs.chmodSync(f, 0o666);
        } catch {}
      }
      fs.rmSync(made, { recursive: true, force: true });
    } catch {}
  });
  const scrub = (s) => (typeof s === "string" ? s.split(D).join("<D>").split(made).join("<D>") : s);
  const full = (e) => `${e.code}/${e.errno}/${e.syscall} ${scrub(e.message)} path ${JSON.stringify(scrub(e.path))}`;
  const brief = (e) => `${e.code}/${e.errno}`;
  const sync = (f, ok = (v) => (v === undefined ? "ok" : JSON.stringify(scrub(v)))) => {
    try {
      return ok(f());
    } catch (e) {
      return full(e);
    }
  };
  const mode = (s) => `mode ${s.mode.toString(8)}`;
  const cb = (f, ok = (v) => (v === undefined ? "ok" : scrub(String(v)))) =>
    new Promise((resolve) => f((e, v) => resolve(e ? full(e) : ok(v))));
  const settle = async (p, ok = (v) => (v === undefined ? "ok" : scrub(String(v)))) => {
    try {
      return ok(await p);
    } catch (e) {
      return full(e);
    }
  };

  let linked = true;
  try {
    fs.symlinkSync("l", "l");
    fs.symlinkSync("l2", "l1");
    fs.symlinkSync("l1", "l2");
    fs.symlinkSync("missing", "dang");
  } catch (e) {
    linked = false;
    console.log("symlink refused:", e.code);
  }
  if (linked) {
    for (const p of ["l", "l/a", "l\\a", "l/a/b", "l1", "l1/a", "dang", "dang/a"]) {
      console.log(JSON.stringify(p));
      console.log("  statSync", sync(() => fs.statSync(p), mode));
      console.log("  lstatSync", sync(() => fs.lstatSync(p), mode));
      console.log("  statSync throwIfNoEntry false", sync(() => fs.statSync(p, { throwIfNoEntry: false }), (s) => (s ? mode(s) : String(s))));
      console.log("  stat", await cb((done) => fs.stat(p, done), mode));
      console.log("  promises.lstat", await settle(fs.promises.lstat(p), mode));
      console.log("  accessSync", sync(() => fs.accessSync(p)));
      console.log("  access", await cb((done) => fs.access(p, done)));
      console.log("  existsSync", fs.existsSync(p));
      console.log("  realpathSync", sync(() => fs.realpathSync(p)));
    }
    for (const p of ["l/a", "l/a/b", "l1/a", "l"]) {
      console.log(`mkdirSync ${JSON.stringify(p)} recursive`, sync(() => fs.mkdirSync(p, { recursive: true })));
      console.log(`mkdir ${JSON.stringify(p)} recursive`, await cb((done) => fs.mkdir(p, { recursive: true }, done)));
      console.log(`promises.mkdir ${JSON.stringify(p)} recursive`, await settle(fs.promises.mkdir(p, { recursive: true })));
    }
    console.log("mkdirSync \"l/a\"", sync(() => fs.mkdirSync("l/a")));
    console.log("chmodSync dang 0o444", sync(() => fs.chmodSync("dang", 0o444)));
    console.log("  lstat after", sync(() => fs.lstatSync("dang"), mode));
    console.log("chmod dang 0o666", await cb((done) => fs.chmod("dang", 0o666, done)));
    console.log("  lstat after", sync(() => fs.lstatSync("dang"), mode));
    console.log("promises.chmod dang 0o444", await settle(fs.promises.chmod("dang", 0o444)));
    console.log("  lstat after", sync(() => fs.lstatSync("dang"), mode));
  }

  fs.mkdirSync("ro");
  fs.chmodSync("ro", 0o444);
  console.log("read-only directory", sync(() => fs.statSync("ro"), mode));
  console.log("  accessSync W_OK", sync(() => fs.accessSync("ro", fs.constants.W_OK)));
  console.log("  promises.access W_OK", await settle(fs.promises.access("ro", fs.constants.W_OK)));
  fs.writeFileSync("rof", "");
  fs.chmodSync("rof", 0o444);
  console.log("read-only file", sync(() => fs.statSync("rof"), mode));
  console.log("  accessSync W_OK", sync(() => fs.accessSync("rof", fs.constants.W_OK)));
  console.log("  access W_OK", await cb((done) => fs.access("rof", fs.constants.W_OK, done)));
  console.log("  accessSync R_OK", sync(() => fs.accessSync("rof", fs.constants.R_OK)));
  console.log("chmodSync missing", sync(() => fs.chmodSync("missing", 0o444)));
  console.log("accessSync missing", sync(() => fs.accessSync("missing")));
  console.log("chmodSync rof 0o666", sync(() => fs.chmodSync("rof", 0o666)), sync(() => fs.statSync("rof"), mode));
}
