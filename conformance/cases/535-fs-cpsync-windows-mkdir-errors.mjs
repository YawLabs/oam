// cpSync of a directory whose destination cannot be made, on Windows
// (#360): node reports the failure of its C++ std::filesystem
// create_directories with MSVC's error -- the Win32 code as errno, the
// name the CRT's errno table gives that number as code, and the system's
// message. Measured on node v22.22.2, Windows 11:
//   * a directory that may not be written in: 5, EIO, "Access is denied."
//   * a missing drive, or a name too long: 3, ESRCH, "The system cannot
//     find the path specified."
//   * a name with a character Windows does not allow: 123, ENOPROTOOPT,
//     "The filename, directory name, or volume label syntax is incorrect."
//     (a missing drive wins over a bad name below it)
// The async forms, which go through mkdir, are shown beside them.
//
// Regression guard: oam gave these libstdc++'s Linux shape (EPERM 1, or
// ENOENT 2 "No such file or directory").
//
// Elsewhere the case prints one line: these are Windows' errors.
import cp from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

if (process.platform !== "win32") {
  console.log("Windows-only rows not run");
  process.exit(0);
}
const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-535-")));
process.chdir(root);
const drive = "QRSTUVWXYZ".split("").find((l) => !fs.existsSync(`${l}:/`));
const norm = (s) => {
  let out = String(s);
  for (const r of [path.toNamespacedPath(root), root]) out = out.split(r).join("<R>");
  if (drive) out = out.split(`${drive}:`).join("<D>:");
  return out.replace(/\\/g, "/").replace(/[\x00-\x1f]/g, (c) => `\\x${c.charCodeAt(0).toString(16).padStart(2, "0")}`);
};
const errStr = (e) => `${JSON.stringify(e.code)} ${e.errno} ${e.syscall} keys=${Object.keys(e)} | ${norm(e.message)}`;
fs.mkdirSync("src/sub", { recursive: true });
fs.writeFileSync("src/sub/in.txt", "in");
fs.mkdirSync("ok");
fs.mkdirSync("ro");
// Everyone (by SID, so no account name) denied adding a file or directory.
cp.execFileSync("icacls", ["ro", "/deny", "*S-1-1-0:(AD,WD)"], { stdio: "ignore" });

const forms = {
  cpSync: (s, d, o) => {
    try {
      return Promise.resolve(`returned ${fs.cpSync(s, d, o)}`);
    } catch (e) {
      return Promise.reject(e);
    }
  },
  "promises.cp": (s, d, o) => fs.promises.cp(s, d, o).then((v) => `resolved ${v}`),
  cp: (s, d, o) =>
    new Promise((resolve, reject) => fs.cp(s, d, o, (e, v) => (e ? reject(e) : resolve(`called back ${v}`)))),
};
const rows = [
  ["under a dir that may not be written in", "ro/x/copy"],
  ["in a dir that may not be written in", "ro/copy"],
  ["a bad name", "bad<name/copy"],
  ["a bad name, deeper", "ok/sub/bad>leaf"],
  ["a control character", "bad\x01/copy"],
  ["a colon in a name", "ok/c:d/copy"],
  ["a name too long", `ok/${"x".repeat(300)}/copy`],
];
if (drive) {
  rows.push(["a missing drive", `${drive}:/nope/copy`], ["a bad name on a missing drive", `${drive}:/bad<x/copy`]);
}
for (const [name, form] of Object.entries(forms)) {
  for (const [label, dest] of rows) {
    let out;
    try {
      out = await form("src", dest, { recursive: true });
    } catch (e) {
      out = errStr(e);
    }
    console.log(`${name} ${label}: ${out}`);
  }
}

cp.execFileSync("icacls", ["ro", "/remove:d", "*S-1-1-0"], { stdio: "ignore" });
process.chdir(os.tmpdir());
fs.rmSync(root, { recursive: true, force: true });
