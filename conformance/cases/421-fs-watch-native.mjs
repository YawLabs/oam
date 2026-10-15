// fs.watch and fs.promises.watch report OS change notifications, as node's
// do (libuv's uv_fs_event_t: ReadDirectoryChangesW / inotify / FSEvents).
//
// Only what node guarantees on every platform is printed: the OS coalesces
// and orders events differently (FSEvents keeps a created flag sticky, a
// write may arrive as one event or two), so each check waits for an event
// that NAMES the entry it touched -- retrying the touch until one does, since
// an FSEvents stream starts asynchronously -- and prints the name, never a
// count or a full sequence. Covered: a directory watch names a new entry; a
// file watch names the file; `recursive: true` names a nested entry with its
// relative path; `encoding: 'buffer'` hands back Buffers; close() emits
// 'close' once on the next tick and stops events; a signal closes the
// watcher; ref()/unref() return the watcher; a missing path throws node's
// ENOENT shape; fs.promises.watch is an async generator whose values have
// a null prototype, that return() and an abort end. An unref'd watcher and a
// `persistent: false` one are left open at the end: the process must still
// exit (the harness times out otherwise).
//
// Regression guard: oam's fs.watch polled the watched path's mtime every
// 100 ms -- no 'rename' events, nothing from inside a directory, unref() a
// no-op so a watcher pinned the process forever -- and fs.promises.watch did
// not exist.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const root = fs.mkdtempSync(path.join(os.tmpdir(), "oam-421-"));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const slash = (name) => (name === null ? null : String(name).replace(/\\/g, "/"));

// Resolve once `watcher` emits a change whose filename satisfies `want`,
// touching (`act`) every 100 ms until it does; give up after 10 s.
function until(watcher, want, act) {
  return new Promise((resolve, reject) => {
    let n = 0;
    const timer = setInterval(() => {
      if (++n > 100) {
        cleanup();
        reject(new Error("no matching event"));
      } else act(n);
    }, 100);
    const onChange = (eventType, filename) => {
      if (want(filename)) {
        cleanup();
        resolve({ eventType, filename });
      }
    };
    const cleanup = () => {
      clearInterval(timer);
      watcher.removeListener("change", onChange);
    };
    watcher.on("change", onChange);
    act(0);
  });
}

// A directory watch names an entry created inside it.
{
  const dir = path.join(root, "d");
  fs.mkdirSync(dir);
  const w = fs.watch(dir);
  console.log("watcher", Object.getPrototypeOf(w).constructor.name, w instanceof (await import("node:events")).EventEmitter);
  console.log("proto keys", Object.getOwnPropertyNames(Object.getPrototypeOf(w)).join(","));
  console.log("ref/unref return the watcher", w.ref() === w, w.unref() === w, w.ref() === w);
  const seen = await until(w, (f) => f === "a.txt", (n) => fs.writeFileSync(path.join(dir, "a.txt"), String(n)));
  console.log("dir watch:", typeof seen.eventType, seen.filename);
  let after = 0;
  w.on("change", () => after++);
  let closes = 0;
  w.on("close", (...args) => console.log("close event", ++closes, args.length));
  console.log("close() returns", w.close());
  w.close();
  console.log("after close(), before the tick");
  console.log("ref after close", w.ref() === w);
  fs.writeFileSync(path.join(dir, "late.txt"), "x");
  await sleep(300);
  console.log("events after close:", after);
}

// A file watch names the file.
{
  const file = path.join(root, "f.txt");
  fs.writeFileSync(file, "0");
  const w = fs.watch(file, { encoding: "utf8" });
  const seen = await until(w, (f) => f === "f.txt", (n) => fs.appendFileSync(file, String(n)));
  console.log("file watch:", seen.filename);
  w.close();
}

// encoding: 'buffer' hands back Buffers.
{
  const dir = path.join(root, "b");
  fs.mkdirSync(dir);
  const w = fs.watch(dir, "buffer");
  const seen = await until(w, (f) => Buffer.isBuffer(f) && f.toString() === "buf.txt", (n) => fs.writeFileSync(path.join(dir, "buf.txt"), String(n)));
  console.log("buffer filename:", Buffer.isBuffer(seen.filename), seen.filename.toString());
  w.close();
}

// recursive: true names a nested entry by its relative path.
{
  const dir = path.join(root, "r");
  fs.mkdirSync(path.join(dir, "sub"), { recursive: true });
  const w = fs.watch(dir, { recursive: true });
  const seen = await until(w, (f) => slash(f) === "sub/deep.txt", (n) => fs.writeFileSync(path.join(dir, "sub", "deep.txt"), String(n)));
  console.log("recursive:", slash(seen.filename));
  w.close();
}

// A signal closes the watcher; an aborted one closes it on the next tick.
{
  const ac = new AbortController();
  const w = fs.watch(root, { signal: ac.signal });
  const closed = new Promise((r) => w.on("close", r));
  ac.abort();
  await closed;
  console.log("closed by abort");
  const w2 = fs.watch(root, { signal: AbortSignal.abort() });
  let early = false;
  w2.on("close", () => { early = true; });
  console.log("pre-aborted, still open in the same tick:", early);
  await sleep(0);
  console.log("pre-aborted closed:", early);
}

// A path that is not there throws node's watch error.
try {
  fs.watch("oam-421-missing");
} catch (e) {
  console.log(e.constructor.name, e.code, e.syscall, e.path, e.filename, Object.keys(e).join(","), e.message);
}
try {
  fs.watch(root, 5);
} catch (e) {
  console.log(e.name, e.code, e.message);
}

// fs.promises.watch.
{
  const dir = path.join(root, "p");
  fs.mkdirSync(dir);
  const it = fsp.watch(dir);
  console.log("promises.watch", Object.prototype.toString.call(it));
  let n = 0;
  const timer = setInterval(() => fs.writeFileSync(path.join(dir, "p.txt"), String(++n)), 100);
  for await (const event of it) {
    if (event.filename === "p.txt") {
      console.log("promises event:", Object.getPrototypeOf(event), typeof event.eventType, event.filename);
      break;
    }
  }
  clearInterval(timer);
  console.log("after break:", JSON.stringify(await it.next()));
  // A fresh, quiet directory: a late event from the writes above would
  // settle the pending next() before the abort does.
  const quiet = path.join(root, "q");
  fs.mkdirSync(quiet);
  const ac = new AbortController();
  const it2 = fsp.watch(quiet, { signal: ac.signal });
  const pending = it2.next();
  setTimeout(() => ac.abort("stop"), 50);
  try {
    await pending;
  } catch (e) {
    console.log("aborted:", e.name, e.code, e.cause, Object.keys(e).join(","));
  }
  try {
    await fsp.watch(dir, { encoding: "buffer" }).next();
  } catch (e) {
    console.log(e.code, e.message);
  }
  try {
    await fsp.watch("oam-421-missing").next();
  } catch (e) {
    console.log(e.code, e.syscall, e.message);
  }
}

fs.rmSync(root, { recursive: true, force: true });

// Left open on purpose, on a directory nothing deletes: neither may keep the
// process alive.
fs.watch(import.meta.dirname).unref();
fs.watch(import.meta.dirname, { persistent: false });
console.log("done");
