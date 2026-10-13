// A truncating open on Windows is libuv's CREATE_ALWAYS (#320, #321). node's
// `w`, `w+` and `writeFile` REPLACE what the name holds in one CreateFileW,
// and everything that open answers follows:
//
// - a named pipe opens, the write reaches its server, and the server sees one
//   connection; so does a device (`\\.\NUL`, `\\.\CON`);
// - a hidden or system file is refused `EPERM`, though `r+` and `a` open it:
//   CREATE_ALWAYS must be given the attributes the file already has;
// - the file that was there is replaced: its alternate data streams are gone
//   and its attributes are the archive bit again;
// - a directory is `EISDIR`, and a read-only file `EPERM`.
//
// Regression guard: oam opened through std, whose truncating open is
// OPEN_ALWAYS and then a truncation of the handle. That truncation is refused
// on a pipe -- `EINVAL: invalid argument, open`, after the open had connected
// to the pipe's server, which the error path then connected to a second time
// -- and it emptied a hidden or system file node refuses to touch, leaving the
// streams and attributes CREATE_ALWAYS resets.
//
// Windows-only by design: on POSIX open(2)'s O_TRUNC is the one operation both
// runtimes make. Elsewhere the case prints one line so node == oam still holds.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

if (process.platform !== "win32") {
  console.log("windows-only");
  process.exit(0);
}

const start = process.cwd();
const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-394-"));
// A relative name is opened here: a bare `NUL` is a FILE of that name in the
// current directory, in both runtimes, and must not be left in the caller's.
process.chdir(made);
const attrib = (...args) => execFileSync("attrib", args).toString();
// Whichever row throws, the directory goes -- hidden and read-only files too.
process.on("exit", () => {
  try {
    process.chdir(start);
    for (const name of fs.readdirSync(made)) {
      try {
        attrib("-r", "-h", "-s", path.join(made, name));
      } catch {}
    }
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});

const pipe = "\\\\.\\pipe\\oam-case-394-" + process.pid;
// The drive or share the temp directory is on.
const root = path.parse(made).root;
const scrub = (s) =>
  typeof s === "string" ? s.split(made).join("<D>").split(pipe).join("<PIPE>").split(root).join("<ROOT>") : s;
const shape = (e) =>
  e && e.code
    ? `${e.code} ${e.errno} ${e.syscall} path=${JSON.stringify(scrub(e.path))} ${scrub(e.message)}`
    : "threw " + String(e);
const called = (f) => new Promise((res, rej) => f((e, v) => (e ? rej(e) : res(v))));

// ---- A named pipe ----

let connections = 0;
let live = 0;
let arrived = "";
const server = net.createServer((socket) => {
  connections++;
  live++;
  socket.on("data", (d) => (arrived += d));
  socket.on("error", () => {});
  socket.on("close", () => live--);
});
await new Promise((res) => server.listen(pipe, res));

// The server has seen every connection the row made, and each has closed.
const settled = async (from) => {
  for (let i = 0; i < 2000 && (connections === from || live > 0); i++) {
    await new Promise((res) => setTimeout(res, 5));
  }
};
const overPipe = async (label, f) => {
  const from = connections;
  let line;
  try {
    line = "OK " + JSON.stringify(await f());
  } catch (e) {
    line = shape(e);
  }
  await settled(from);
  console.log(`pipe | ${label}: ${line} | connections ${connections - from} arrived ${JSON.stringify(arrived)}`);
  arrived = "";
};
const written = (fd, text) => {
  try {
    return fs.writeSync(fd, text);
  } catch (e) {
    return e.code;
  }
};

for (const flags of ["r", "r+", "w", "w+", "wx", "wx+", "a", "a+", "ax", "ax+"]) {
  await overPipe("openSync " + flags, () => {
    const fd = fs.openSync(pipe, flags);
    try {
      const st = fs.fstatSync(fd);
      return [st.isFile(), st.isFIFO(), st.size, flags === "r" ? null : written(fd, "to-" + flags)];
    } finally {
      fs.closeSync(fd);
    }
  });
  await overPipe("open " + flags, async () => {
    const fd = await called((cb) => fs.open(pipe, flags, cb));
    try {
      return flags === "r" ? null : written(fd, "cb-" + flags);
    } finally {
      fs.closeSync(fd);
    }
  });
  await overPipe("promises.open " + flags, async () => {
    const handle = await fs.promises.open(pipe, flags);
    try {
      return flags === "r" ? null : (await handle.write("p-" + flags)).bytesWritten;
    } catch (e) {
      return e.code;
    } finally {
      await handle.close();
    }
  });
}
await overPipe("writeFileSync", () => fs.writeFileSync(pipe, "sync"));
await overPipe("writeFileSync empty", () => fs.writeFileSync(pipe, ""));
await overPipe("writeFile", () => called((cb) => fs.writeFile(pipe, "callback", cb)));
await overPipe("promises.writeFile", () => fs.promises.writeFile(pipe, "promise"));
await overPipe("writeFileSync flag w+", () => fs.writeFileSync(pipe, "plus", { flag: "w+" }));
await overPipe("appendFileSync", () => fs.appendFileSync(pipe, "append"));
await overPipe("createWriteStream", () =>
  new Promise((res, rej) => {
    const stream = fs.createWriteStream(pipe);
    stream.on("error", rej);
    stream.on("close", () => res(stream.bytesWritten));
    stream.end("stream");
  }),
);
// What std's open did after it opened, node asks for by name and is refused.
await overPipe("truncateSync", () => fs.truncateSync(pipe, 0));
await overPipe("ftruncateSync", () => {
  const fd = fs.openSync(pipe, "r+");
  try {
    return fs.ftruncateSync(fd, 0);
  } finally {
    fs.closeSync(fd);
  }
});
await new Promise((res) => server.close(res));
console.log("pipe | connections in all: " + connections);

// ---- What the open finds on a disk ----

const row = (label, f) => {
  let line;
  try {
    line = "OK " + JSON.stringify(scrub(f()));
  } catch (e) {
    line = shape(e);
  }
  console.log(label + ": " + line);
};
// The attribute letters attrib prints before the path.
const attributes = (p) => attrib(p).slice(0, attrib(p).indexOf(p)).replace(/\s+/g, "");
const state = (p) => [fs.statSync(p).size, attributes(p)];
let made_n = 0;
const fresh = (...attrs) => {
  const p = path.join(made, "f" + made_n++);
  fs.writeFileSync(p, "0123456789");
  if (attrs.length) attrib(...attrs, p);
  return p;
};
const kinds = [
  ["plain", []],
  ["hidden", ["+h"]],
  ["system", ["+s"]],
  ["hidden system", ["+h", "+s"]],
  ["read-only", ["+r"]],
  ["read-only hidden", ["+r", "+h"]],
  ["no archive bit", ["-a"]],
];
for (const [kind, attrs] of kinds) {
  for (const flags of ["w", "w+", "r+", "a", "a+", "wx"]) {
    const p = fresh(...attrs);
    row(`${kind} | openSync ${flags}`, () => {
      fs.closeSync(fs.openSync(p, flags));
      return state(p);
    });
    row(`${kind} | after ${flags}`, () => state(p));
  }
  const forms = [
    ["writeFileSync", (p) => fs.writeFileSync(p, "x")],
    ["writeFileSync empty", (p) => fs.writeFileSync(p, "")],
    ["appendFileSync", (p) => fs.appendFileSync(p, "x")],
    ["truncateSync", (p) => fs.truncateSync(p, 2)],
    ["copyFileSync onto", (p) => fs.copyFileSync(path.join(made, "f0"), p)],
  ];
  for (const [label, f] of forms) {
    const p = fresh(...attrs);
    row(`${kind} | ${label}`, () => (f(p), state(p)));
    row(`${kind} | after ${label}`, () => state(p));
  }
  for (const [label, f] of [
    ["writeFile", (p) => called((cb) => fs.writeFile(p, "x", cb))],
    ["promises.writeFile", (p) => fs.promises.writeFile(p, "x")],
    ["promises.open w", async (p) => (await fs.promises.open(p, "w")).close()],
    ["open w+", async (p) => fs.closeSync(await called((cb) => fs.open(p, "w+", cb)))],
  ]) {
    const p = fresh(...attrs);
    let line;
    try {
      await f(p);
      line = "OK";
    } catch (e) {
      line = shape(e);
    }
    console.log(`${kind} | ${label}: ${line} ${JSON.stringify(state(p))}`);
  }
}

// The file is replaced, not emptied: a stream beside its data goes with it.
for (const flags of ["w", "w+", "r+", "a"]) {
  const p = fresh();
  fs.writeFileSync(p + ":side", "stream");
  row(`alternate stream | after openSync ${flags}`, () => {
    fs.closeSync(fs.openSync(p, flags));
    return fs.readFileSync(p + ":side", "utf8");
  });
}
{
  const p = fresh();
  fs.writeFileSync(p + ":side", "stream");
  row("alternate stream | after writeFileSync", () => (fs.writeFileSync(p, "x"), fs.readFileSync(p + ":side", "utf8")));
  // A truncating open of the stream itself replaces only the stream.
  row("alternate stream | openSync w of the stream", () => {
    fs.closeSync(fs.openSync(p + ":side", "w"));
    return [fs.readFileSync(p + ":side", "utf8"), fs.readFileSync(p, "utf8")];
  });
}

// A file another descriptor holds open is replaced under it.
{
  const p = fresh();
  const held = fs.openSync(p, "r");
  row("held open | openSync w", () => (fs.closeSync(fs.openSync(p, "w")), fs.fstatSync(held).size));
  fs.closeSync(held);
}

// A directory, and names that are no file. A junction needs no privilege to
// make. Through one whose target is gone a truncating open creates nothing;
// the appending open after it makes the target, as a directory.
fs.mkdirSync(path.join(made, "dir"));
fs.symlinkSync(path.join(made, "dir"), path.join(made, "to-dir"), "junction");
fs.symlinkSync(path.join(made, "gone"), path.join(made, "to-gone"), "junction");
fs.symlinkSync(path.join(made, "f0"), path.join(made, "to-file"), "junction");
row("junction to nothing | a truncating open makes", () => {
  for (const flags of ["w", "w+"]) {
    try {
      fs.closeSync(fs.openSync(path.join(made, "to-gone"), flags));
    } catch {}
  }
  return fs.existsSync(path.join(made, "gone"));
});
const names = [
  ["directory", path.join(made, "dir")],
  ["junction to a directory", path.join(made, "to-dir")],
  ["junction to nothing", path.join(made, "to-gone")],
  ["junction to a file", path.join(made, "to-file")],
  ["directory with a separator", path.join(made, "dir") + "\\"],
  ["missing", path.join(made, "missing")],
  ["missing parent", path.join(made, "no", "file")],
  ["under a file", path.join(made, "f0", "x")],
  ["file with a separator", path.join(made, "f0") + "\\"],
  ["root", root],
  ["a file named NUL", "NUL"],
  ["\\\\.\\NUL", "\\\\.\\NUL"],
  ["\\\\?\\NUL", "\\\\?\\NUL"],
  ["\\\\.\\CON", "\\\\.\\CON"],
  ["\\\\.\\CONOUT$", "\\\\.\\CONOUT$"],
  ["no such pipe", pipe + "-gone"],
];
for (const [label, p] of names) {
  // Not the console's fstat: node refuses it for a descriptor opened to
  // write, which is fstat's own divergence and no part of the open.
  const console_ = label.startsWith("\\\\.\\CON");
  for (const flags of ["w", "w+", "r", "r+", "a"]) {
    row(`${label} | openSync ${flags}`, () => {
      const fd = fs.openSync(p, flags);
      try {
        return console_ ? "opened" : fs.fstatSync(fd).isFile();
      } finally {
        fs.closeSync(fd);
      }
    });
  }
  row(`${label} | writeFileSync`, () => fs.writeFileSync(p, ""));
  let line;
  try {
    await fs.promises.writeFile(p, "");
    line = "OK";
  } catch (e) {
    line = shape(e);
  }
  console.log(`${label} | promises.writeFile: ${line}`);
}
// What the rows above made is a file.
row("missing | made", () => [fs.statSync(path.join(made, "missing")).isFile(), fs.existsSync(path.join(made, "no"))]);
row("junction to nothing | the appending open made", () => fs.statSync(path.join(made, "gone")).isDirectory());
