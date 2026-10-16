// realpathSync and realpath of a path in a Windows device namespace -- a
// `\\?\` or `\\.\` path, which a recursive mkdirSync hands back (#361).
//
// node's realpathSync and callback realpath are JS (lib/fs.js): they split
// the resolved path at its root -- for a device path that is the first two
// components, `\\?\C:\` or `\\.\pipe\` -- lstat the root, then lstat each
// component down from it. On v22.22.2 the root of `\\?\C:\x` lstats as the
// volume, which fails EISDIR, `lstat 'C:'`, so realpathSync of any `\\?\C:\`
// path fails though the path is there; `\\.\Global\NUL` fails at its root
// `\\.\Global` (EBADF), and NUL, whose root is the whole path, comes back as
// it was given. The `.native` forms and fs.promises.realpath ask the OS for
// the whole path and resolve it.
//
// Regression guard: oam's realpathSync asked the OS first and ran node's
// walk only when that failed, so it resolved every one of these.
//
// Windows only.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

if (process.platform !== "win32") {
  console.log("Windows device namespaces: nothing to resolve here");
} else {
  const start = process.cwd();
  const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-542-"));
  process.chdir(made);
  const D = fs.realpathSync.native(process.cwd());
  process.on("exit", () => {
    try {
      process.chdir(start);
      fs.rmSync(made, { recursive: true, force: true });
    } catch {}
  });
  const drive = D[0];
  const scrub = (s) =>
    typeof s === "string"
      ? s.split(D).join("<D>").split(made).join("<D>").split(`${drive}:`).join("<V>:")
      : s;
  const full = (e) => `${e.code}/${e.errno}/${e.syscall} ${scrub(e.message)} path ${JSON.stringify(scrub(e.path))}`;
  const sync = (f) => {
    try {
      return JSON.stringify(scrub(f()));
    } catch (e) {
      return full(e);
    }
  };
  const cb = (f) => new Promise((resolve) => f((e, v) => resolve(e ? full(e) : JSON.stringify(scrub(v)))));
  const settle = async (p) => {
    try {
      return JSON.stringify(scrub(await p));
    } catch (e) {
      return full(e);
    }
  };

  const first = fs.mkdirSync("o/p", { recursive: true });
  console.log("mkdirSync o/p recursive", JSON.stringify(scrub(first)));
  fs.writeFileSync("o/p/f.txt", "f");
  const names = [
    first,
    `${first}\\p`,
    `${first}\\p\\f.txt`,
    `${first}\\missing`,
    `\\\\?\\${D}`,
    `\\\\.\\${D}\\o`,
    `\\\\?\\${drive}:\\`,
    `\\\\.\\${drive}:\\`,
    `\\\\?\\${drive}:`,
    "\\\\.\\NUL",
    "\\\\?\\NUL",
    "\\\\.\\Global\\NUL",
    "\\\\.\\GLOBALROOT\\Device\\Null",
    "\\\\.\\pipe\\",
    "\\\\.\\mailslot",
    "\\\\?\\GLOBALROOT\\Device\\KsecDD",
  ];
  for (const p of names) {
    console.log(JSON.stringify(scrub(p)));
    console.log("  realpathSync", sync(() => fs.realpathSync(p)));
    console.log("  realpathSync buffer", sync(() => fs.realpathSync(p, "buffer").toString()));
    console.log("  realpath", await cb((done) => fs.realpath(p, done)));
    console.log("  realpathSync.native", sync(() => fs.realpathSync.native(p)));
    console.log("  realpath.native", await cb((done) => fs.realpath.native(p, done)));
    console.log("  promises.realpath", await settle(fs.promises.realpath(p)));
  }
}
