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

const opens = [
  ["file r", () => fs.openSync(file, "r")],
  ["file r+", () => fs.openSync(file, "r+")],
  ["file a+", () => fs.openSync(file, "a+")],
  ["file a, write-only", () => fs.openSync(file, "a")],
  ["file w, write-only", () => fs.openSync(path.join(made, "w"), "w")],
  ["file O_WRONLY", () => fs.openSync(file, fs.constants.O_WRONLY)],
];
// A directory opens for reading everywhere; its read is what fails.
opens.push(["directory r", () => fs.openSync(path.join(made, "dir"), "r")]);
for (const [label, open] of opens) {
  for (const options of [undefined, "utf8", { encoding: "hex" }, { encoding: null }]) {
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
    const handle = await fs.promises.open(typeof label === "string" && label.startsWith("directory") ? path.join(made, "dir") : file, label.includes("write-only") || label.includes("WRONLY") ? "a" : "r");
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
