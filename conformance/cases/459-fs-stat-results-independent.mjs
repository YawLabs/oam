// Every stat form's result is its own: a Stats taken from a sync stat keeps
// its values through the stats that follow it, and stats in flight together
// -- promises, callbacks, FileHandle.stat, with sync stats run while they
// wait -- each answer for their own file. The sync and async forms of one
// stat agree field for field, in both the number and the bigint form.
//
// Regression guard: oam's sync stats write into one reused record, as
// node's binding fills its one `statValues` array, and its async stats each
// resolve with a record of their own; a Stats that kept a view of the
// shared record, or an async result built over it, would read whichever
// stat came last.
//
// Only what is the same from run to run is printed: sizes, kinds, and
// whether two results agree.
import assert from "node:assert";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-459-")));
const files = [];
for (let i = 0; i < 24; i++) {
  const file = path.join(root, `f${i}.txt`);
  fs.writeFileSync(file, "x".repeat(i));
  files.push(file);
}
const sub = path.join(root, "d");
fs.mkdirSync(sub);
const missing = path.join(root, "nope");
const equal = (a, b) => {
  try {
    assert.deepStrictEqual(a, b);
    return true;
  } catch {
    return false;
  }
};
const sizes = (list) => list.map((s) => String(s.size)).join(",");

try {
  // ---- a sync Stats keeps its values through later stats
  {
    const first = fs.statSync(files[5]);
    const big = fs.statSync(files[6], { bigint: true });
    const fd = fs.openSync(files[7]);
    const others = [
      fs.statSync(files[3]),
      fs.lstatSync(sub),
      fs.fstatSync(fd),
      fs.fstatSync(fd, { bigint: true }),
      fs.statSync(missing, { throwIfNoEntry: false }),
      fs.lstatSync(files[9], { bigint: true }),
    ];
    fs.closeSync(fd);
    console.log("held:", first.size, first.isFile(), typeof big.size, big.size, big.isFile());
    console.log("later:", others.map((s) => (s === undefined ? "undefined" : `${s.size}:${s.isDirectory()}`)).join(" "));
    console.log("held again:", first.size, big.size, first.isDirectory(), big.isDirectory());
  }

  // ---- stats in flight together, sync stats run meanwhile
  {
    const promised = files.map((f, i) => fs.promises.stat(f, { bigint: i % 2 === 1 }));
    const syncMeanwhile = [fs.statSync(files[23]), fs.statSync(sub), fs.lstatSync(files[0])];
    const called = await Promise.all(
      files.map((f, i) => new Promise((resolve, reject) => {
        const form = i % 3 === 0 ? fs.lstat : fs.stat;
        form(f, { bigint: i % 4 === 0 }, (err, s) => (err ? reject(err) : resolve(s)));
      })),
    );
    const handles = await Promise.all(files.slice(0, 8).map((f) => fs.promises.open(f)));
    const viaHandles = await Promise.all(handles.map((h, i) => h.stat({ bigint: i % 2 === 0 })));
    await Promise.all(handles.map((h) => h.close()));
    const fds = files.slice(8, 16).map((f) => fs.openSync(f));
    const viaFstat = await Promise.all(
      fds.map((fd) => new Promise((resolve, reject) => fs.fstat(fd, (e, s) => (e ? reject(e) : resolve(s))))),
    );
    for (const fd of fds) fs.closeSync(fd);
    console.log("promises:", sizes(await Promise.all(promised)));
    console.log("callbacks:", sizes(called));
    console.log("FileHandle.stat:", sizes(viaHandles));
    console.log("fstat:", sizes(viaFstat));
    console.log("sync meanwhile:", sizes(syncMeanwhile), syncMeanwhile.map((s) => s.isDirectory()).join(","));
  }

  // ---- the forms of one stat agree, field for field
  {
    const file = files[11];
    const fd = fs.openSync(file);
    const handle = await fs.promises.open(file);
    for (const bigint of [false, true]) {
      const o = { bigint };
      const sync = fs.statSync(file, o);
      const forms = {
        lstatSync: fs.lstatSync(file, o),
        "promises.stat": await fs.promises.stat(file, o),
        "promises.lstat": await fs.promises.lstat(file, o),
        stat: await new Promise((r) => fs.stat(file, o, (e, s) => r(e ?? s))),
        lstat: await new Promise((r) => fs.lstat(file, o, (e, s) => r(e ?? s))),
      };
      console.log(
        bigint ? "bigint" : "number",
        Object.entries(forms).map(([name, s]) => `${name}=${equal(s, sync)}`).join(" "),
      );
      // A descriptor's stat against itself, through each form.
      const fsync = fs.fstatSync(fd, o);
      const fasync = await new Promise((r) => fs.fstat(fd, o, (e, s) => r(e ?? s)));
      const fhandle = await handle.stat(o);
      console.log("  fstat forms:", equal(fasync, fsync), equal(fhandle, fsync), fsync.size, fsync.isFile());
      const dirSync = fs.statSync(sub, o);
      const dirAsync = await fs.promises.stat(sub, o);
      console.log("  directory:", equal(dirAsync, dirSync), dirSync.isDirectory(), dirSync.isFile());
    }
    await handle.close();
    fs.closeSync(fd);
  }
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
