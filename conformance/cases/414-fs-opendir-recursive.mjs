// fs.opendir, opendirSync and fs.promises.opendir with `recursive` (#286).
//
// node's Dir reads `bufferSize` entries at a time (32 by default). With
// `recursive`, handing out a directory's Dirent opens that directory and
// queues it, and each later read first pulls one chunk from the queue's head
// -- rotating it to the back while it has more -- before the top directory
// is read again. So a tree whose directories fit in a chunk comes out breadth
// first, and with a smaller bufferSize a subdirectory's entries interleave
// with the rest of its parent's. Every Dirent carries the directory it was
// read from as `parentPath` (and `path`). A link to a directory is listed,
// not followed. A directory gone by the time its Dirent is handed out fails
// that read with its open's error (`opendir` and the path), and the walk
// goes on.
//
// Regression guard: oam's Dir took one list and one parent for all of it,
// ignored every option, and so listed the top level only.
//
// Entries come in the file system's own order, which both runtimes read the
// same way. The link row is a junction on Windows (no privilege needed) and
// a directory symlink elsewhere.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { inspect } from "node:util";

const root = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-414-"));
process.on("exit", () => {
  try {
    fs.rmSync(root, { recursive: true, force: true });
  } catch {}
});

const at = (p) => path.join(root, p);
const mk = (p) => fs.mkdirSync(at(p), { recursive: true });
const wf = (p) => fs.writeFileSync(at(p), p);
mk("a/x/deep");
mk("b");
mk("c/y");
for (const f of ["1.txt", "z.txt", "a/1.txt", "a/2.txt", "a/x/2.txt", "a/x/deep/z.txt", "b/3.txt", "b/4.txt", "b/5.txt", "c/y/6.txt"]) wf(f);
let linkRow = "link";
try {
  fs.symlinkSync(at("a/x"), at("c/link"), process.platform === "win32" ? "junction" : "dir");
} catch (e) {
  linkRow = "link refused " + e.code;
}
console.log(linkRow);

const rel = (p) => path.relative(root, p).split(path.sep).join("/") || ".";
const kind = (d) => (d.isDirectory() ? "d" : d.isSymbolicLink() ? "l" : "f");
const show = (d) => (d === null ? "null" : `${rel(d.parentPath)}/${d.name}:${kind(d)}${d.path === d.parentPath ? "" : " path!=parentPath"}`);
const scrub = (s) => String(s).split(root).join("<root>").split(path.sep).join("/");
const shape = (e) => `${e.code} ${e.syscall} path=${e.path === undefined ? "none" : scrub(e.path)} ${scrub(e.message)}`;
const called = (f) => new Promise((res, rej) => f((e, v) => (e ? rej(e) : res(v))));

const drainSync = (dir) => {
  const out = [];
  for (let d; (d = dir.readSync()) !== null; ) out.push(show(d));
  dir.closeSync();
  return out.join(" ");
};

// readSync, over the whole tree and with each bufferSize that reorders it.
for (const opts of [{ recursive: true }, { recursive: true, bufferSize: 1 }, { recursive: true, bufferSize: 2 }, { recursive: true, bufferSize: 3 }, { recursive: 1 }, { recursive: false }, {}]) {
  console.log(`opendirSync ${JSON.stringify(opts)}: ${drainSync(fs.opendirSync(root, opts))}`);
}
console.log(`opendirSync subdir: ${drainSync(fs.opendirSync(at("a"), { recursive: true }))}`);

// The callback forms: fs.opendir, and read(cb) to the end.
{
  const dir = await called((cb) => fs.opendir(root, { recursive: true, bufferSize: 2 }, cb));
  const out = [];
  for (let d; (d = await called((cb) => dir.read(cb))) !== null; ) out.push(show(d));
  await called((cb) => dir.close(cb));
  console.log(`opendir read(cb): ${out.join(" ")}`);
}

// The promise forms: fs.promises.opendir, and read() to the end.
{
  const dir = await fs.promises.opendir(root, { recursive: true });
  const out = [];
  for (let d; (d = await dir.read()) !== null; ) out.push(show(d));
  await dir.close();
  console.log(`promises.opendir read(): ${out.join(" ")}`);
}

// for await, and entries(): both close the handle when they are done.
{
  const dir = await fs.promises.opendir(root, { recursive: true, bufferSize: 1 });
  const out = [];
  for await (const d of dir) out.push(show(d));
  console.log(`for await: ${out.join(" ")}`);
  await dir.close().then(
    () => console.log("close after for await: resolved"),
    (e) => console.log(`close after for await: ${e.code}`),
  );
}
{
  const dir = fs.opendirSync(root, { recursive: true, bufferSize: 3 });
  const out = [];
  for await (const d of dir.entries()) out.push(show(d));
  console.log(`entries(): ${out.join(" ")}`);
  try {
    dir.readSync();
  } catch (e) {
    console.log(`readSync after entries(): ${e.code}`);
  }
}

// Breaking out of a for await closes the handle too, mid-walk.
{
  const dir = fs.opendirSync(root, { recursive: true });
  const out = [];
  for await (const d of dir) {
    out.push(show(d));
    if (out.length === 7) break;
  }
  console.log(`break: ${out.join(" ")}`);
  try {
    dir.readSync();
  } catch (e) {
    console.log(`readSync after break: ${e.code}`);
  }
}

// Closed mid-walk, with opened subdirectories queued: every later call fails.
{
  const dir = fs.opendirSync(root, { recursive: true, bufferSize: 1 });
  const out = [];
  for (let i = 0; i < 4; i++) out.push(show(dir.readSync()));
  dir.closeSync();
  console.log(`closed mid-walk after: ${out.join(" ")}`);
  for (const [label, f] of [
    ["readSync", () => dir.readSync()],
    ["closeSync", () => dir.closeSync()],
    ["read(cb)", () => dir.read(() => {})],
  ]) {
    try {
      f();
      console.log(`${label}: no throw`);
    } catch (e) {
      console.log(`${label}: ${e.code}`);
    }
  }
  await dir.read().catch((e) => console.log(`read(): ${e.code}`));
  await dir.close().catch((e) => console.log(`close(): ${e.code}`));
}

// A read of the top directory is in flight: the sync forms refuse, and the
// reads queued meanwhile come back in order, each a tick after the last.
{
  const dir = fs.opendirSync(root, { recursive: true });
  const log = [];
  const done = new Promise((res) => {
    dir.read((e, d) => log.push(`first ${e ? e.code : show(d)}`));
    dir.read((e, d) => {
      log.push(`second ${e ? e.code : show(d)}`);
      res();
    });
  });
  log.push("read(cb) returned");
  for (const [label, f] of [
    ["readSync", () => dir.readSync()],
    ["closeSync", () => dir.closeSync()],
  ]) {
    try {
      f();
      log.push(`${label} during read: no throw`);
    } catch (e) {
      log.push(`${label} during read: ${e.code} ${e.message}`);
    }
  }
  await done;
  dir.read((e, d) => log.push(`buffered ${show(d)}`));
  process.nextTick(() => log.push("tick after buffered read(cb)"));
  log.push("buffered read(cb) returned");
  await new Promise((res) => setTimeout(res, 10));
  dir.closeSync();
  console.log(log.join("\n"));
}

// A directory removed after its parent was read (the first read takes in
// the whole of a directory this small): its Dirent's read fails with the
// open's error, which names the path, and the walk goes on. Which of the
// two is removed is whichever the first read did not hand out, so that one
// is opened only after it is gone.
for (const [label, read] of [
  ["readSync", (dir) => dir.readSync()],
  ["read()", (dir) => dir.read()],
  ["read(cb)", (dir) => called((cb) => dir.read(cb))],
]) {
  mk("r/d1");
  mk("r/d2");
  wf("r/f.txt");
  wf("r/d1/inner.txt");
  wf("r/d2/inner.txt");
  const dir = fs.opendirSync(at("r"), { recursive: true });
  const out = [];
  for (;;) {
    let d;
    try {
      d = await read(dir);
    } catch (e) {
      out.push(`[${shape(e)}]`);
      continue;
    }
    if (d === null) break;
    out.push(show(d));
    if (out.length === 1) fs.rmSync(at(d.name === "d1" ? "r/d2" : "r/d1"), { recursive: true });
  }
  dir.closeSync();
  console.log(`removed subdirectory, ${label}: ${out.join(" ")}`);
  fs.rmSync(at("r"), { recursive: true });
}

// What the options are checked for, and when.
for (const options of [{ bufferSize: 0 }, { bufferSize: 1.5 }, { bufferSize: "2" }, { bufferSize: undefined }, { bufferSize: 4294967296 }, 42, "not-an-encoding"]) {
  try {
    drainSync(fs.opendirSync(root, options));
    console.log(`opendirSync ${inspect(options)}: ok`);
  } catch (e) {
    console.log(`opendirSync ${inspect(options)}: ${e.code} ${e.message}`);
  }
}
try {
  fs.opendirSync(at("missing"), { bufferSize: 0 });
} catch (e) {
  console.log(`opendirSync missing with a bad bufferSize: ${e.code}`);
}
await fs.promises.opendir(root, 42).catch((e) => console.log(`promises.opendir 42: ${e.code}`));
try {
  fs.opendir(root, 42, () => {});
} catch (e) {
  console.log(`opendir 42: ${e.code}`);
}
console.log(`lengths: ${fs.opendir.length} ${fs.opendirSync.length} ${fs.promises.opendir.length}`);
console.log(`path is an own property: ${Object.keys(fs.opendirSync(root)).includes("path")}`);
console.log(`[Symbol.asyncIterator] is entries: ${fs.Dir.prototype[Symbol.asyncIterator] === fs.Dir.prototype.entries}`);
