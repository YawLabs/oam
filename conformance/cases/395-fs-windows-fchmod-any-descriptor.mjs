// fchmod on Windows works through any descriptor of a file (#322). libuv's
// fs__fchmod does not set the attribute on the handle it is given: it REOPENS
// that handle for FILE_WRITE_ATTRIBUTES and sets it on the new one, so what the
// descriptor was opened for does not matter -- `fchmodSync(fd, 0o444)` through
// a descriptor opened `r` makes the file read-only, and `0o666` through one
// makes it writable again -- also where read-only is the only attribute the
// file has -- and the file's change time does not move. What cannot be
// reopened fails there: a directory is `EPERM`, an anonymous pipe (a piped
// stdin or stdout) `EBUSY`. The NUL device reopens and has no attributes to
// read: `EINVAL`. A named pipe opened by name is reopened, which its server
// sees as a second connection, and the call succeeds.
//
// Regression guard: oam set the attribute on the descriptor's own handle, and
// a handle opened `r` has no FILE_WRITE_ATTRIBUTES: `EPERM: operation not
// permitted, fchmod`. NUL's descriptor was `EISDIR`, and a piped stdin's
// succeeded.
//
// Windows-only by design: on POSIX fchmod(2) is one call on the descriptor in
// both runtimes. Elsewhere the case prints one line so node == oam still holds.
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

if (process.platform !== "win32") {
  console.log("windows-only");
  process.exit(0);
}

const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-395-"));
const attrib = (...args) => execFileSync("attrib", args).toString();
// Whichever row throws, the directory goes -- read-only and hidden files too.
process.on("exit", () => {
  try {
    for (const name of fs.readdirSync(made)) {
      try {
        attrib("-r", "-h", "-s", path.join(made, name));
      } catch {}
    }
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});

const pipe = "\\\\.\\pipe\\oam-case-395-" + process.pid;
const scrub = (s) => (typeof s === "string" ? s.split(made).join("<D>").split(pipe).join("<PIPE>") : s);
const shape = (e) =>
  e && e.code
    ? `${e.code} ${e.errno} ${e.syscall} path=${JSON.stringify(scrub(e.path))} ${scrub(e.message)}`
    : "threw " + String(e);
const called = (f) => new Promise((res, rej) => f((e, v) => (e ? rej(e) : res(v))));
// The attribute letters attrib prints before the path, which it spells its
// own way: found by its shape, a drive or a share, not by the text of `p`.
const attributes = (p) => {
  const found = /^(.*?)\s*(?:[A-Za-z]:|\\\\)/.exec(attrib(p));
  return found ? found[1].replace(/\s+/g, "") : "?";
};
const octal = (mode) => (mode & 0o777).toString(8);
let made_n = 0;
const fresh = (...attrs) => {
  const p = path.join(made, "f" + made_n++);
  fs.writeFileSync(p, "0123456789");
  if (attrs.length) attrib(...attrs, p);
  return p;
};
const writable = (p) => {
  try {
    attrib("-r", p);
  } catch {}
};

const forms = [
  ["fchmodSync", (fd, mode) => fs.fchmodSync(fd, mode)],
  ["fchmod", (fd, mode) => called((cb) => fs.fchmod(fd, mode, cb))],
];
const flagsList = ["r", "r+", "w", "w+", "a", "a+", "wx", "ax+"];

// ---- A regular file, by every flag ----
for (const flags of flagsList) {
  for (const [form, chmod] of forms) {
    for (const mode of [0o444, 0o666, 0o000, 0o200, 0o400, 0o777, 0o555]) {
      const p = flags.includes("x") ? path.join(made, "x" + made_n++) : fresh();
      let line;
      const fd = fs.openSync(p, flags);
      try {
        const result = await chmod(fd, mode);
        line = `OK ${JSON.stringify(result)} fstat ${octal(fs.fstatSync(fd).mode)}`;
      } catch (e) {
        line = shape(e);
      }
      fs.closeSync(fd);
      console.log(`${flags} | ${form} ${octal(mode)}: ${line} | stat ${octal(fs.statSync(p).mode)} ${attributes(p)}`);
      writable(p);
    }
  }
  // FileHandle.chmod.
  for (const mode of [0o444, 0o666]) {
    const p = flags.includes("x") ? path.join(made, "x" + made_n++) : fresh();
    const handle = await fs.promises.open(p, flags);
    let line;
    try {
      line = `OK ${JSON.stringify(await handle.chmod(mode))} ${octal((await handle.stat()).mode)}`;
    } catch (e) {
      line = shape(e);
    }
    await handle.close();
    console.log(`${flags} | FileHandle.chmod ${octal(mode)}: ${line} | stat ${octal(fs.statSync(p).mode)} ${attributes(p)}`);
    writable(p);
  }
}

// ---- A file that is already read-only, hidden or system ----
const kinds = [
  ["read-only", ["+r"]],
  ["hidden", ["+h"]],
  ["system", ["+s"]],
  ["read-only hidden", ["+r", "+h"]],
  ["no archive bit", ["-a"]],
  // Read-only and nothing else: clearing it leaves no attribute at all.
  ["read-only, no archive bit", ["+r", "-a"]],
  ["hidden, no archive bit", ["+h", "-a"]],
];
for (const [kind, attrs] of kinds) {
  for (const flags of ["r", "r+", "a"]) {
    for (const mode of [0o666, 0o444, 0o200]) {
      const p = fresh(...attrs);
      let line;
      try {
        const fd = fs.openSync(p, flags);
        try {
          const before = fs.fstatSync(fd, { bigint: true });
          fs.fchmodSync(fd, mode);
          const after = fs.fstatSync(fd, { bigint: true });
          // The change time stays where it was, and so do the others.
          const moved = ["ctimeNs", "mtimeNs", "atimeNs", "birthtimeNs"].filter((t) => before[t] !== after[t]);
          line = "OK fstat " + octal(Number(after.mode)) + " moved " + JSON.stringify(moved);
        } finally {
          fs.closeSync(fd);
        }
      } catch (e) {
        line = shape(e);
      }
      console.log(`${kind} | ${flags} fchmodSync ${octal(mode)}: ${line} | stat ${octal(fs.statSync(p).mode)} ${attributes(p)}`);
    }
  }
}

// ---- The same file through two descriptors, and after it is gone ----
{
  const p = fresh();
  const first = fs.openSync(p, "r");
  const second = fs.openSync(p, "r+");
  const rows = [];
  for (const [label, fd, mode] of [["first", first, 0o444], ["second", second, 0o666], ["first again", first, 0o444]]) {
    try {
      fs.fchmodSync(fd, mode);
      rows.push(`${label} ${octal(fs.fstatSync(second).mode)}`);
    } catch (e) {
      rows.push(`${label} ${shape(e)}`);
    }
  }
  // A write through a descriptor opened before the file became read-only.
  try {
    rows.push("write " + fs.writeSync(second, "x"));
  } catch (e) {
    rows.push("write " + e.code);
  }
  fs.closeSync(first);
  fs.closeSync(second);
  console.log("two descriptors: " + rows.join(", ") + " | " + attributes(p));
  writable(p);

  const gone = fresh();
  const fd = fs.openSync(gone, "r");
  fs.unlinkSync(gone);
  let line;
  try {
    fs.fchmodSync(fd, 0o444);
    line = "OK";
  } catch (e) {
    line = shape(e);
  }
  fs.closeSync(fd);
  console.log("unlinked while open: " + line + " | exists " + fs.existsSync(gone));
  try {
    fs.fchmodSync(fd, 0o444);
    console.log("closed descriptor: OK");
  } catch (e) {
    console.log("closed descriptor: " + shape(e));
  }
}

// ---- What cannot be reopened ----
const refused = async (label, open, after = () => {}) => {
  for (const [form, chmod] of forms) {
    for (const mode of [0o444, 0o666]) {
      let line;
      let fd;
      try {
        fd = open();
        line = "OK " + JSON.stringify(await chmod(fd, mode));
      } catch (e) {
        line = shape(e);
      }
      if (fd !== undefined) fs.closeSync(fd);
      const note = (await after()) ?? "";
      console.log(`${label} | ${form} ${octal(mode)}: ${line}${note}`);
    }
  }
};
fs.mkdirSync(path.join(made, "dir"));
for (const flags of ["r", "r+", "a"]) {
  await refused("directory " + flags, () => fs.openSync(path.join(made, "dir"), flags));
  await refused("NUL device " + flags, () => fs.openSync("\\\\.\\NUL", flags));
}
console.log("directory | after: " + octal(fs.statSync(path.join(made, "dir")).mode) + " " + attributes(path.join(made, "dir")));

// A named pipe. The reopen opens the pipe by name again, so each row is two
// connections to the server, the second made while the first is still open;
// the next row waits until the server has taken both and seen them close.
let connections = 0;
let live = 0;
const server = net.createServer((socket) => {
  connections++;
  live++;
  socket.on("error", () => {});
  socket.on("close", () => live--);
  socket.resume();
});
await new Promise((res) => server.listen(pipe, res));
let seen = 0;
const settled = async () => {
  for (let i = 0; i < 2000 && (connections - seen < 2 || live > 0); i++) {
    await new Promise((res) => setTimeout(res, 5));
  }
  const made = connections - seen;
  seen = connections;
  return " | connections " + made;
};
for (const flags of ["r", "r+", "w"]) {
  await refused("pipe " + flags, () => fs.openSync(pipe, flags), settled);
}
await new Promise((res) => server.close(res));

// ---- Descriptors 0-2 of a child: anonymous pipes, and NUL ----
const child = `
const fs = require("node:fs");
const out = [];
for (const fd of [0, 1]) for (const mode of [0o444, 0o666]) {
  try { fs.fchmodSync(fd, mode); out.push(fd + " " + mode.toString(8) + " OK"); }
  catch (e) { out.push(fd + " " + mode.toString(8) + " " + e.code + " " + e.errno + " " + e.syscall + " " + e.message); }
}
fs.writeSync(2, out.join("\\n"));
`;
for (const [label, stdio] of [["piped", ["pipe", "pipe", "pipe"]], ["NUL", ["ignore", "ignore", "pipe"]]]) {
  const ran = spawnSync(process.execPath, ["-e", child], { stdio, input: stdio[0] === "pipe" ? "" : undefined });
  for (const line of String(ran.stderr).split("\n")) console.log(`child stdio ${label} | fchmodSync ${line}`);
}
