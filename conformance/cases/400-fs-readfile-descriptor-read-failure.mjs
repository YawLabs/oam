// Callback fs.readFile of a descriptor drops a read that fails (#334). node
// closes nothing it was handed, and its close path for a user's descriptor
// forgets the error it was called with: the callback gets what was read so
// far -- an empty Buffer, or `""` with an encoding -- and no error, for a
// descriptor that cannot be read (opened to write only) and for a directory.
// A descriptor that is not open at all fails the stat before any read, and
// node's callback then reports the TypeError its own cleanup trips over
// (`The "list" argument must be an instance of Array. Received null`).
// readFileSync and FileHandle.readFile report the failure itself, except
// that readFileSync with a utf8 encoding never stats: a bad descriptor there
// is `EBADF ... read`.
//
// Regression guard: oam's callback form reported the read's error (`EBADF`
// or `EISDIR` `read`) and a bad descriptor's as `EBADF` `fstat` in every form.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-400-"));
// Whichever row throws, the directory goes.
process.on("exit", () => {
  try {
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});
const file = path.join(made, "f");
fs.writeFileSync(file, "0123456789");
fs.mkdirSync(path.join(made, "dir"));
const value = (v) => (typeof v === "string" ? "string " + JSON.stringify(v) : `${v.constructor.name} ${JSON.stringify(v.toString())}`);
const shape = (e) => [e.constructor.name, e.code, e.syscall ?? "-", e.message].join(" / ");
const viaCallback = (fd, options) =>
  new Promise((res) => {
    try {
      const done = (e, v) => res("callback " + (e ? shape(e) : value(v)));
      if (options === undefined) fs.readFile(fd, done);
      else fs.readFile(fd, options, done);
    } catch (e) {
      res("threw " + shape(e));
    }
  });
const viaSync = (fd, options) => {
  try {
    return "sync " + value(fs.readFileSync(fd, options));
  } catch (e) {
    return "sync " + shape(e);
  }
};

// What each row opens, by all three forms: the path and the flag.
const opens = [
  ["file r", file, "r"],
  ["file r+", file, "r+"],
  ["file a+", file, "a+"],
  ["file a, write-only", file, "a"],
  ["file w, write-only", path.join(made, "w"), "w"],
  ["file O_WRONLY", file, fs.constants.O_WRONLY],
  // A directory opens for reading everywhere; its read is what fails.
  ["directory r", path.join(made, "dir"), "r"],
];
for (const [label, target, flag] of opens) {
  const open = () => fs.openSync(target, flag);
  for (const options of [undefined, "utf8", { encoding: "hex" }, { encoding: null }, "buffer"]) {
    const name = options === undefined ? "no options" : JSON.stringify(options);
    let fd;
    try {
      fd = open();
    } catch (e) {
      console.log(`${label} | ${name} | open ${e.code}`);
      continue;
    }
    console.log(`${label} | ${name} | ${await viaCallback(fd, options)}`);
    fs.closeSync(fd);
    fd = open();
    console.log(`${label} | ${name} | ${viaSync(fd, options)}`);
    fs.closeSync(fd);
    // FileHandle.readFile reads the "buffer" encoding its own way; not this case's.
    if (options === "buffer") continue;
    if (flag === "w") fs.writeFileSync(file, "0123456789");
    const handle = await fs.promises.open(target, flag);
    console.log(`${label} | ${name} | FileHandle ` + (await handle.readFile(options).then(value, shape)));
    await handle.close();
  }
}

// The descriptor is left where the reads left it, and open.
{
  const fd = fs.openSync(file, "r");
  const first = await viaCallback(fd, "utf8");
  const second = await viaCallback(fd, "utf8");
  let open;
  try {
    fs.fstatSync(fd);
    open = "still open";
  } catch (e) {
    open = e.code;
  }
  console.log(`twice | ${first} | ${second} | ${open}`);
  fs.closeSync(fd);
}

// A descriptor that is not open.
{
  const closed = fs.openSync(file, "r");
  fs.closeSync(closed);
  for (const [label, fd] of [["closed", closed], ["never opened", 99999]]) {
    for (const options of [undefined, "utf8", "utf-8", "UTF8", { encoding: "hex" }]) {
      const name = options === undefined ? "no options" : JSON.stringify(options);
      console.log(`${label} | ${name} | ${await viaCallback(fd, options)} | ${viaSync(fd, options)}`);
    }
  }
  // A number that is no descriptor, by the sync form (the callback form
  // throws node's range error from a later tick, where nothing can catch it).
  for (const fd of [-1, 2 ** 31, 1.5]) {
    console.log(`descriptor ${fd} | ${viaSync(fd, "utf8")} | ${viaSync(fd)}`);
  }
}

// The callback form looks at the descriptor a tick after the call: one the
// caller closes at once is closed by then. One descriptor per process state,
// so the number is not one a later open has been handed.
{
  const fd = fs.openSync(file, "r");
  const answer = new Promise((res) => fs.readFile(fd, "utf8", (e, v) => res(e ? shape(e) : value(v))));
  fs.closeSync(fd);
  console.log("closed in the same tick | " + (await answer));
}
// A signal that is aborted ends the read like a failed read does: what was
// read before it, no error, and the descriptor where it was.
{
  const position = (fd) => {
    const b = Buffer.alloc(4);
    return JSON.stringify(b.toString("latin1", 0, fs.readSync(fd, b, 0, 4, null)));
  };
  for (const options of [{ signal: AbortSignal.abort() }, { signal: AbortSignal.abort(), encoding: "utf8" }]) {
    const fd = fs.openSync(file, "r");
    const answer = await viaCallback(fd, options);
    console.log(`aborted signal | ${options.encoding ?? "no encoding"} | ${answer} | next read ${position(fd)}`);
    fs.closeSync(fd);
  }
  const controller = new AbortController();
  const fd = fs.openSync(file, "r");
  const answer = viaCallback(fd, { signal: controller.signal });
  controller.abort();
  console.log(`aborted in the same tick | ${await answer} | next read ${position(fd)}`);
  fs.closeSync(fd);
  const sync = fs.openSync(file, "r");
  console.log("readFileSync ignores a signal | " + viaSync(sync, { signal: AbortSignal.abort(), encoding: "utf8" }));
  fs.closeSync(sync);
}
// The utf8 fast path checks a flag it has no use for; no other encoding does.
for (const options of [{ encoding: "utf8", flag: "zz" }, { encoding: "utf-8", flag: 7.5 }, { encoding: "hex", flag: "zz" }, { flag: "zz" }, { encoding: "utf8", flag: "w" }]) {
  const fd = fs.openSync(file, "r");
  const b = Buffer.alloc(4);
  const answer = viaSync(fd, options);
  console.log(`descriptor with a flag | ${JSON.stringify(options)} | ${answer} | next read ${JSON.stringify(b.toString("latin1", 0, fs.readSync(fd, b, 0, 4, null)))}`);
  fs.closeSync(fd);
}

// The "buffer" encoding with nothing read: "" from the sync and callback
// forms, whose toString never looks the name up for an empty Buffer, and
// refused by fs.promises.readFile, which settles the encoding first.
{
  const empty = path.join(made, "empty");
  fs.writeFileSync(empty, "");
  for (const [name, p] of [["empty", empty], ["ten bytes", file]]) {
    const viaPath = await new Promise((res) => fs.readFile(p, "buffer", (e, v) => res(e ? e.code : value(v))));
    const viaPromise = await fs.promises.readFile(p, "buffer").then(value, (e) => e.code);
    console.log(`"buffer", ${name} | ${viaSync(p, "buffer")} | callback ${viaPath} | promise ${viaPromise}`);
  }
}
