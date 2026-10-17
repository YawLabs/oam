// fs's stat forms honour their options as node's do (#392):
// `throwIfNoEntry` (statSync / lstatSync only: `false` itself answers
// undefined for a path that is not there -- ENOENT, or for statSync alone
// ENOTDIR, so lstatSync under a file still throws ENOTDIR on POSIX, where
// that path is ENOTDIR rather than Windows' ENOENT), `bigint` in
// every form (statSync, lstatSync, fstatSync, stat, lstat, fstat,
// fs.promises.stat / lstat, FileHandle.stat) giving a BigIntStats with the
// `*Ns` fields, node's unvalidated option reads (null throws reading them,
// at node's point in each form), and the Stats / BigIntStats shape:
// prototypes, enumerable members, lazy Date fields, the deprecated
// fs.Stats constructor.
//
// Regression guard: oam's stat forms ignored their options -- a
// `throwIfNoEntry: false` existence check threw ENOENT, `{ bigint: true }`
// gave plain numbers -- and the times were whole milliseconds where node's
// keep the fraction.
//
// Only what is the same from run to run is printed: shapes, types,
// relations between the two forms, and times set with utimes.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import util from "node:util";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-415-")));
const norm = (s) => String(s).split(root).join("<R>").replace(/\\/g, "/");
const errStr = (e) => `${e.name} ${e.code} ${e.syscall ?? ""} | ${norm(e.message)}`;
const show = (label, fn) => {
  try {
    const v = fn();
    console.log(label, "->", describe(v));
  } catch (e) {
    console.log(label, "THROWS", errStr(e));
  }
};
const showAsync = async (label, fn) => {
  try {
    console.log(label, "->", describe(await fn()));
  } catch (e) {
    console.log(label, "REJECTS", errStr(e));
  }
};
const callback = (label, start) => new Promise((resolve) => {
  try {
    start((err, v) => {
      console.log(label, err ? `ERR ${errStr(err)}` : `-> ${describe(v)}`);
      resolve();
    });
  } catch (e) {
    console.log(label, "THROWS", errStr(e));
    resolve();
  }
});
// A stat as its class and the type of every own field, in order.
const describe = (v) => {
  if (v === undefined || v === null || typeof v !== "object") return String(v);
  return `${v.constructor.name} {${Object.keys(v).map((k) => `${k}:${typeof v[k]}`).join(",")}}`;
};

const file = path.join(root, "f.txt");
fs.writeFileSync(file, "hello");
const sub = path.join(root, "d");
fs.mkdirSync(sub);
const missing = path.join(root, "nope");
const underFile = path.join(file, "child");

try {
  // ---- throwIfNoEntry
  for (const [name, fn] of [["statSync", fs.statSync], ["lstatSync", fs.lstatSync]]) {
    show(`${name} missing, false`, () => fn(missing, { throwIfNoEntry: false }));
    show(`${name} under a file, false`, () => fn(underFile, { throwIfNoEntry: false }));
    show(`${name} missing, false + bigint`, () => fn(missing, { throwIfNoEntry: false, bigint: true }));
    show(`${name} file, false`, () => fn(file, { throwIfNoEntry: false }));
    for (const value of [true, undefined, 0, "", null, "no"]) {
      show(`${name} missing, ${JSON.stringify(value) ?? "undefined"}`, () => fn(missing, { throwIfNoEntry: value }));
    }
  }
  await showAsync("promises.stat missing, false", () => fs.promises.stat(missing, { throwIfNoEntry: false }));
  await callback("stat missing, false", (cb) => fs.stat(missing, { throwIfNoEntry: false }, cb));

  // ---- the options: read raw, in node's order
  show("statSync null", () => fs.statSync(file, null));
  show("lstatSync null", () => fs.lstatSync(file, null));
  show("statSync bad path, null", () => fs.statSync(42, null));
  show("statSync 5", () => fs.statSync(file, 5));
  show("statSync 'x'", () => fs.statSync(file, "x"));
  show("statSync undefined", () => fs.statSync(file, undefined));
  show("statSync bigint 1", () => fs.statSync(file, { bigint: 1 }));
  show("statSync bigint 'yes'", () => fs.statSync(file, { bigint: "yes" }));
  show("fstatSync bad fd, null", () => fs.fstatSync("x", null));
  show("fstatSync bad fd", () => fs.fstatSync("x", { bigint: true }));
  show("stat bad path, null", () => fs.stat(42, null, () => {}));
  show("lstat bad path, null", () => fs.lstat(42, null, () => {}));
  show("fstat bad fd, null", () => fs.fstat("x", null, () => {}));
  show("fstat bad fd", () => fs.fstat("x", {}, () => {}));
  show("stat no callback", () => fs.stat(file, { bigint: true }));
  await showAsync("promises.stat null", () => fs.promises.stat(file, null));
  await showAsync("promises.lstat null", () => fs.promises.lstat(file, null));
  {
    const handle = await fs.promises.open(file);
    await showAsync("FileHandle.stat null", () => handle.stat(null));
    await handle.close();
  }
  for (const [name, call] of [
    ["statSync", (o) => fs.statSync(missing, o)],
    ["lstatSync", (o) => fs.lstatSync(missing, o)],
  ]) {
    const order = [];
    call({
      get bigint() { order.push("bigint"); return false; },
      get throwIfNoEntry() { order.push("throwIfNoEntry"); return false; },
    });
    console.log(name, "reads", order.join(", "));
  }
  for (const [name, f] of Object.entries({
    stat: fs.stat, lstat: fs.lstat, fstat: fs.fstat, statSync: fs.statSync, lstatSync: fs.lstatSync,
    fstatSync: fs.fstatSync, "promises.stat": fs.promises.stat, "promises.lstat": fs.promises.lstat,
  })) {
    console.log(`${name}: name ${JSON.stringify(f.name)}, length ${f.length}`);
  }
  // How often each form reads `options.bigint`. Node's callback forms read
  // it twice -- FSReqCallback's read picks the Stats type, the binding's
  // argument is the second -- so a getter that throws on its second read
  // throws synchronously there; every other form reads it once.
  {
    const fd = fs.openSync(file);
    for (const [name, call] of [
      ["stat", (o, cb) => fs.stat(file, o, cb)],
      ["lstat", (o, cb) => fs.lstat(file, o, cb)],
      ["fstat", (o, cb) => fs.fstat(fd, o, cb)],
      ["statSync", (o, cb) => cb(null, fs.statSync(file, o))],
      ["fstatSync", (o, cb) => cb(null, fs.fstatSync(fd, o))],
      ["promises.stat", (o, cb) => fs.promises.stat(file, o).then((v) => cb(null, v), cb)],
    ]) {
      let reads = 0;
      const s = await new Promise((r) => call({ get bigint() { return ++reads === 1; } }, (e, v) => r(e ?? v)));
      let second = "no throw";
      try {
        await new Promise((r, j) => {
          let n = 0;
          call({ get bigint() { if (n++) throw new Error("second read"); return false; } }, (e) => (e ? j(e) : r()));
        });
      } catch (e) {
        second = `throws ${e.message}`;
      }
      console.log(`${name}: bigint read ${reads}x, ${s.constructor.name}, a throwing second read: ${second}`);
    }
    fs.closeSync(fd);
    let reads = 0;
    show("fstat bad fd, reads", () => fs.fstat(-5, { get bigint() { reads++; return false; } }, () => {}));
    console.log("fstat bad fd read bigint", reads, "times");
  }

  // ---- bigint in every form
  const fd = fs.openSync(file);
  const handle = await fs.promises.open(file);
  const big = {
    statSync: fs.statSync(file, { bigint: true }),
    lstatSync: fs.lstatSync(file, { bigint: true }),
    fstatSync: fs.fstatSync(fd, { bigint: true }),
    stat: await new Promise((r) => fs.stat(file, { bigint: true }, (e, s) => r(e ?? s))),
    lstat: await new Promise((r) => fs.lstat(file, { bigint: true }, (e, s) => r(e ?? s))),
    fstat: await new Promise((r) => fs.fstat(fd, { bigint: true }, (e, s) => r(e ?? s))),
    "promises.stat": await fs.promises.stat(file, { bigint: true }),
    "promises.lstat": await fs.promises.lstat(file, { bigint: true }),
    "FileHandle.stat": await handle.stat({ bigint: true }),
    "dir statSync": fs.statSync(sub, { bigint: true }),
  };
  await handle.close();
  fs.closeSync(fd);
  const plain = fs.statSync(file);
  for (const [name, s] of Object.entries(big)) {
    console.log(name, describe(s));
    console.log(
      "  ",
      s.isFile(), s.isDirectory(), s.isSymbolicLink(), s.isFIFO(), s.isSocket(), s.isCharacterDevice(), s.isBlockDevice(),
      "| Ms = Ns / 1e6:", ["atime", "mtime", "ctime", "birthtime"].every((t) => s[t + "Ms"] === s[t + "Ns"] / 1000000n),
      "| Dates:", ["atime", "mtime", "ctime", "birthtime"].every((t) => s[t].getTime() === Math.round(Number(s[t + "Ms"])))
    );
  }
  // The two forms of one stat agree: every integer field, exactly where the
  // double can hold it, and each `*Ms` is node's sec * 1e3 + nsec / 1e6.
  {
    const b = big.statSync;
    console.log("same integers:", ["dev", "mode", "nlink", "uid", "gid", "rdev", "blksize", "size", "blocks"]
      .map((k) => `${k}=${BigInt(plain[k]) === b[k]}`).join(" "));
    console.log("ino:", Number(b.ino) === plain.ino);
    console.log("Ms from Ns:", ["mtime", "birthtime"].map((t) => {
      const ns = b[t + "Ns"];
      return plain[t + "Ms"] === Number(ns / 1000000000n) * 1000 + Number(ns % 1000000000n) / 1e6;
    }).join(" "));
  }
  // Times set by utimes, as both forms read them back: the fraction of a
  // millisecond stays. The fractional ones on Windows alone: how utimes
  // turns a double into the file's time is the platform's (libuv truncates
  // `time * 1e7` to 100 ns ticks there), and that is not what is tested.
  for (const t of [1500000000.25, 1500000000.0625, 2.0000625, 1234567890.001]) {
    if (t !== 1500000000.25 && process.platform !== "win32") {
      console.log(`utimes ${t}: Windows only`);
      continue;
    }
    fs.utimesSync(file, t, t);
    const p = fs.statSync(file);
    const b = fs.statSync(file, { bigint: true });
    console.log(`utimes ${t}:`, p.mtimeMs, p.atimeMs, b.mtimeNs, b.mtimeMs, b.atimeNs, p.mtime.toISOString(), b.mtime.toISOString());
  }

  // ---- the classes
  const b = big.statSync;
  const protoOf = (o) => Object.getPrototypeOf(o);
  console.log("chain:", protoOf(b).constructor.name, protoOf(protoOf(b)).constructor.name,
    protoOf(plain).constructor.name, protoOf(protoOf(plain)).constructor.name,
    protoOf(protoOf(b)) === protoOf(protoOf(plain)));
  console.log("Stats.prototype:", Object.getOwnPropertyNames(protoOf(plain)).join(","));
  console.log("BigIntStats.prototype:", Object.getOwnPropertyNames(protoOf(b)).join(","));
  console.log("StatsBase.prototype:", Object.getOwnPropertyNames(protoOf(protoOf(b))).join(","));
  const forIn = (o) => { const out = []; for (const k in o) out.push(k); return out.join(","); };
  console.log("for-in Stats:", forIn(plain));
  console.log("for-in BigIntStats:", forIn(b));
  console.log("instanceof fs.Stats:", plain instanceof fs.Stats, b instanceof fs.Stats);
  console.log(util.inspect(b).replace(/-?\d+n/g, "Nn"));
  console.log(util.inspect(plain).replace(/-?[\d.]+(?=,?\n)/g, "N"));
  // The Date fields: computed on first read, then an own, writable,
  // non-enumerable data property; assignable.
  {
    const s = fs.statSync(file);
    const before = Object.getOwnPropertyNames(s).includes("mtime");
    const d = s.mtime;
    const desc = Object.getOwnPropertyDescriptor(s, "mtime");
    console.log("lazy mtime:", before, d === s.mtime, desc.writable, desc.enumerable, desc.configurable, Object.keys(s).includes("mtime"));
    s.atime = "set";
    console.log("assigned atime:", s.atime, Object.getOwnPropertyDescriptor(s, "atime").enumerable);
  }
  // The deprecated constructor.
  {
    const warnings = [];
    process.on("warning", (w) => warnings.push(`${w.name} ${w.code} ${w.message}`));
    const S = fs.Stats;
    console.log("fs.Stats:", S.name, S.length, protoOf(S).name, S.prototype === protoOf(plain));
    const made = new S(1, 0o100644, 3, 4, 5, 6, 7, 8, 9, 10, 1500, 2500.5, 3000, 4000);
    console.log(util.inspect(made, { breakLength: Infinity }), made instanceof S, made.isFile(), made.mtime.toISOString());
    console.log(util.inspect(new S(), { breakLength: Infinity }));
    // One warning per CODE across every deprecate wrapper, as node's
    // shared set gives: fs.Stats has warned DEP0180, so a util.deprecate
    // wrapper under that code stays silent.
    util.deprecate(() => {}, "another DEP0180", "DEP0180")();
    await new Promise((r) => setImmediate(r));
    console.log("warnings:", warnings.join(" / "));
  }
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
