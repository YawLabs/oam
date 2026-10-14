// fs.cp, fs.cpSync and fs.promises.cp honour their options as node's do
// (#390): `force` (with `force: false` a file already there is kept),
// `errorOnExist` (ERR_FS_CP_EEXIST), `filter` (sync and async, a promise
// refused by cpSync), `dereference`, `verbatimSymlinks`,
// `preserveTimestamps` and `mode`, and node's checks of each.
//
// Regression guard: oam's copy read `recursive` alone, so `force: false`
// overwrote the file it was asked to keep and `errorOnExist` never failed.
//
// Each row is one scenario in one form, in a directory of its own: the
// outcome (the error's class, code and message, paths relative) and the
// tree left behind. Left out, as docs/node-divergences.md records: the
// cpSync rows where node's C++ fails by its platform's std::filesystem --
// errorOnExist over a directory, a file copied onto a directory, and a
// directory onto a file (node aborts).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-410-")));
const home = process.cwd();
const norm = (s) => {
  let out = String(s);
  for (const r of [root, path.toNamespacedPath(root)]) out = out.split(r).join("<R>");
  return out.replace(/\\/g, "/").replace(/<R>\/\d+/g, "<R>/N");
};
const tree = (p) => {
  const out = [];
  const walk = (q, rel) => {
    const st = fs.lstatSync(q);
    if (st.isSymbolicLink()) out.push(`${rel}->${norm(fs.readlinkSync(q))}`);
    else if (st.isDirectory()) {
      out.push(rel + "/");
      for (const e of fs.readdirSync(q).sort()) walk(path.join(q, e), rel + "/" + e);
    } else out.push(`${rel}=${fs.readFileSync(q, "utf8")}`);
  };
  walk(p, ".");
  return out.join(" ");
};
const errStr = (e) => `${e.constructor.name}/${e.name} ${e.code} ${e.errno ?? ""} ${e.syscall ?? ""} | ${norm(e.message)}` +
  (e.path !== undefined ? ` | path=${norm(e.path)}` : "");
const forms = {
  sync: (s, d, o) => {
    try {
      o === undefined ? fs.cpSync(s, d) : fs.cpSync(s, d, o);
      return Promise.resolve();
    } catch (e) {
      return Promise.reject(e);
    }
  },
  promise: (s, d, o) => (o === undefined ? fs.promises.cp(s, d) : fs.promises.cp(s, d, o)),
  callback: (s, d, o) => new Promise((resolve, reject) => {
    const cb = (e, ...rest) => (e ? reject(e) : resolve(rest.length));
    try {
      o === undefined ? fs.cp(s, d, cb) : fs.cp(s, d, o, cb);
    } catch (e) {
      e.thrownFromCall = true;
      reject(e);
    }
  }),
};
const links = (() => {
  try {
    fs.writeFileSync(path.join(root, "t"), "");
    fs.symlinkSync("t", path.join(root, "l"));
    return true;
  } catch {
    return false;
  } finally {
    fs.rmSync(path.join(root, "l"), { force: true });
    fs.rmSync(path.join(root, "t"), { force: true });
  }
})();
const W = (p, c) => {
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, c);
};
const dir = (b) => {
  W(b + "/s/a", "A");
  W(b + "/s/sub/b", "B");
};
const scenarios = [];
const S = (name, setup, src, dest, opts, extra = {}) => scenarios.push({ name, setup, src, dest, opts, ...extra });

S("file new", (b) => W(b + "/s", "S"), "s", "d");
S("file over file", (b) => { W(b + "/s", "S"); W(b + "/d", "D"); }, "s", "d");
S("file over file force:false", (b) => { W(b + "/s", "S"); W(b + "/d", "D"); }, "s", "d", { force: false });
S("file over file errorOnExist", (b) => { W(b + "/s", "S"); W(b + "/d", "D"); }, "s", "d", { force: false, errorOnExist: true });
S("file over file force wins", (b) => { W(b + "/s", "S"); W(b + "/d", "D"); }, "s", "d", { force: true, errorOnExist: true });
S("file into missing parents", (b) => W(b + "/s", "S"), "s", "x/y/d");
S("file mode EXCL over file", (b) => { W(b + "/s", "S"); W(b + "/d", "D"); }, "s", "d", { mode: fs.constants.COPYFILE_EXCL });
S("dir new", dir, "s", "d", { recursive: true });
S("dir over dir", (b) => { dir(b); W(b + "/d/a", "OLD"); W(b + "/d/keep", "K"); }, "s", "d", { recursive: true });
S("dir over dir force:false", (b) => { dir(b); W(b + "/d/a", "OLD"); }, "s", "d", { recursive: true, force: false });
S("dir over dir errorOnExist", (b) => { dir(b); W(b + "/d/a", "OLD"); }, "s", "d",
  { recursive: true, force: false, errorOnExist: true }, { skip: ["sync"] });
S("dir over empty dir errorOnExist", (b) => { dir(b); fs.mkdirSync(b + "/d"); }, "s", "d", { recursive: true, force: false, errorOnExist: true });
S("dir into missing parents", dir, "s", "x/y/d", { recursive: true });
S("dir without recursive", dir, "s", "d", undefined, { skip: ["sync"] });
S("dir into itself", dir, "s", "s/sub/x", { recursive: true, filter: () => true }, { skip: ["sync"] });
S("onto itself", (b) => W(b + "/s", "S"), "s", "s", undefined, { skip: ["sync"] });
S("missing src", () => {}, "nope", "d", undefined, { skip: ["sync"] });
S("filter skips top", dir, "s", "d", { recursive: true, filter: () => false });
S("filter skips a file", dir, "s", "d", { recursive: true, filter: (s) => !s.endsWith("b") });
S("filter skips a dir", dir, "s", "d", { recursive: true, filter: (s) => !s.endsWith("sub") });
S("filter async", dir, "s", "d", { recursive: true, filter: async (s) => !s.endsWith("a") });
S("filter throws", dir, "s", "d", { recursive: true, filter: () => { throw new Error("boom"); } });
S("filter truthy", dir, "s", "d", { recursive: true, filter: () => 1 });
S("filter path force:false", (b) => { dir(b); W(b + "/d/a", "OLD"); }, "s", "d", { recursive: true, force: false, filter: () => true });
S("filter path errorOnExist", (b) => { dir(b); W(b + "/d/a", "OLD"); }, "s", "d",
  { recursive: true, force: false, errorOnExist: true, filter: () => true });
S("options a number", dir, "s", "d", 5);
S("options null", dir, "s", "d", null);
S("options an array", dir, "s", "d", []);
S("force a string", dir, "s", "d", { force: "yes" });
S("filter a string", dir, "s", "d", { filter: "x" });
S("dereference with verbatimSymlinks", dir, "s", "d", { dereference: true, verbatimSymlinks: true });
S("mode out of range", dir, "s", "d", { mode: 99 });
S("mode a string", dir, "s", "d", { mode: "1" });
S("recursive a number", dir, "s", "d", { recursive: 1 });
S("preserveTimestamps", (b) => { W(b + "/s", "S"); fs.utimesSync(b + "/s", 1000, 2000); }, "s", "d", { preserveTimestamps: true }, { times: "d" });
S("preserveTimestamps nested", (b) => { dir(b); fs.utimesSync(b + "/s/sub/b", 1000, 3000); }, "s", "d",
  { recursive: true, preserveTimestamps: true }, { times: "d/sub/b" });
S("preserveTimestamps read-only", (b) => { W(b + "/s", "S"); fs.chmodSync(b + "/s", 0o444); fs.utimesSync(b + "/s", 1000, 2000); }, "s", "d",
  { preserveTimestamps: true }, { times: "d" });
S("link to a file", (b) => { W(b + "/t", "T"); fs.symlinkSync("t", b + "/s"); }, "s", "d", undefined, { links: true });
S("link verbatim", (b) => { W(b + "/t", "T"); fs.symlinkSync("t", b + "/s"); }, "s", "d", { verbatimSymlinks: true }, { links: true });
S("link dereferenced", (b) => { W(b + "/t", "T"); fs.symlinkSync("t", b + "/s"); }, "s", "d", { dereference: true }, { links: true });
S("dir with a link", (b) => { dir(b); fs.symlinkSync("a", b + "/s/l"); }, "s", "d", { recursive: true }, { links: true });
S("dir with a link verbatim", (b) => { dir(b); fs.symlinkSync("a", b + "/s/l"); }, "s", "d", { recursive: true, verbatimSymlinks: true }, { links: true });
S("dir with a link dereferenced", (b) => { dir(b); fs.symlinkSync("a", b + "/s/l"); }, "s", "d", { recursive: true, dereference: true }, { links: true });
S("link over a link", (b) => { W(b + "/t", "T"); W(b + "/u", "U"); fs.symlinkSync("t", b + "/s"); fs.symlinkSync("u", b + "/d"); }, "s", "d",
  { force: false }, { links: true });
S("dir link", (b) => { dir(b); fs.symlinkSync("s", b + "/l", "dir"); }, "l", "d", { recursive: true }, { links: true });
S("preserveTimestamps on a file kept", (b) => { W(b + "/s/a", "A"); fs.utimesSync(b + "/s/a", 1000, 2000); W(b + "/d/a", "OLD"); fs.utimesSync(b + "/d/a", 5000, 6000); },
  "s", "d", { recursive: true, force: false, preserveTimestamps: true }, { times: "d/a" });
S("link over a dir link", (b) => { W(b + "/t", "T"); fs.mkdirSync(b + "/u"); fs.symlinkSync("t", b + "/s"); fs.symlinkSync("u", b + "/d", "dir"); },
  "s", "d", undefined, { links: true });
S("dir with a dir link, twice", (b) => { W(b + "/s/real/x", "X"); fs.symlinkSync("real", b + "/s/dl", "dir"); fs.cpSync(b + "/s", b + "/d", { recursive: true }); },
  "s", "d", { recursive: true }, { links: true, skip: ["promise", "callback"] });
S("dir link dereferenced", (b) => { dir(b); fs.symlinkSync("s", b + "/l", "dir"); }, "l", "d", { recursive: true, dereference: true }, { links: true });

let n = 0;
try {
  // A junction (Windows): cpSync copies it as the directory it leads to.
  // Made with mklink, which both runtimes see the same way.
  if (process.platform === "win32") {
    const b = path.join(root, "junction");
    W(b + "/s/real/x", "X");
    const made = (await import("node:child_process")).spawnSync("cmd", ["/c", "mklink", "/J", path.join(b, "s", "jl"), path.join(b, "s", "real")]);
    if (made.status === 0) {
      process.chdir(b);
      for (const dest of ["o", "o"]) {
        try {
          fs.cpSync("s", dest, { recursive: true });
          console.log("junction [sync]", "ok ||", tree("o"));
        } catch (e) {
          console.log("junction [sync]", errStr(e));
        }
      }
      process.chdir(home);
    } else {
      console.log("junction: mklink failed");
    }
  } else {
    console.log("junction: Windows only");
  }
  // unlink of a directory link removes the link, as libuv's does.
  if (links) {
    const b = path.join(root, "unlink");
    fs.mkdirSync(path.join(b, "u"), { recursive: true });
    fs.symlinkSync(path.join(b, "u"), path.join(b, "l"), "dir");
    try {
      fs.unlinkSync(path.join(b, "l"));
      console.log("unlinkSync of a dir link: ok", fs.existsSync(path.join(b, "u")), fs.existsSync(path.join(b, "l")));
    } catch (e) {
      console.log("unlinkSync of a dir link:", errStr(e));
    }
  }
  // A falsy rejection reaches fs.cp's callback as ERR_FALSY_VALUE_REJECTION.
  {
    const b = path.join(root, "falsy");
    W(b + "/s/a", "A");
    for (const [label, filter] of [["throws undefined", () => { throw undefined; }], ["rejects null", () => Promise.reject(null)]]) {
      const args = await new Promise((r) => fs.cp(path.join(b, "s"), path.join(b, "d"), { recursive: true, filter }, (...a) => r(a)));
      console.log("filter", label, "[callback]", args.length, args[0] && args[0].code, args[0] && args[0].reason);
    }
  }
  for (const sc of scenarios) {
    for (const [form, run] of Object.entries(forms)) {
      if (sc.skip?.includes(form)) continue;
      if (sc.links && !links) {
        console.log(`${sc.name} [${form}] no symlinks here`);
        continue;
      }
      const b = path.join(root, String(++n));
      fs.mkdirSync(b);
      sc.setup(b);
      process.chdir(b);
      let res;
      try {
        const cbArgs = await run(sc.src, sc.dest, sc.opts);
        res = form === "callback" ? `ok (callback given ${cbArgs + 1} arguments)` : "ok";
      } catch (e) {
        res = (e.thrownFromCall ? "throws " : "") + errStr(e);
      }
      process.chdir(home);
      let times = "";
      if (sc.times && res.startsWith("ok")) {
        const from = fs.statSync(path.join(b, sc.times.replace(/^d/, "s")));
        const to = fs.statSync(path.join(b, sc.times));
        times = ` | mtime kept ${to.mtimeMs === from.mtimeMs}, writable ${(to.mode & 0o200) !== 0}`;
      }
      console.log(`${sc.name} [${form}] ${res} || ${tree(b)}${times}`);
    }
  }
} finally {
  process.chdir(home);
  fs.rmSync(root, { recursive: true, force: true });
}
