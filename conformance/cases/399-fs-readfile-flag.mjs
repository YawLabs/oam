// readFile's `flag` option reaches the open (#336). node opens the path with
// the flag it is given and then reads the descriptor, so the flag's side
// effects and its access mode are the call's: `"w"` and `"w+"` empty the file
// (and `"w"` then fails the read, `EBADF`), `"a"` and `"wx"` create a file
// that is missing, `"wx"` refuses one that is there (`EEXIST`), and `"a+"`
// reads what the file holds.
//
// Regression guard: oam's readFile always opened read-only. A flag changed
// nothing: `readFileSync(f, { flag: "w" })` returned the content of a file
// node has just emptied, and with a missing file every flag was `ENOENT`
// where node creates it.
//
// Each row reports what the call answered and what the file holds afterwards.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-399-"));
// Whichever row throws, the directory goes.
process.on("exit", () => {
  try {
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});
const called = (f) => new Promise((res, rej) => f((e, v) => (e ? rej(e) : res(v))));
const shape = (e) => (e && e.code ? `${e.code} ${e.syscall ?? ""}`.trim() : "threw " + String(e));
const holds = (p) => {
  try {
    const st = fs.statSync(p);
    return st.isDirectory() ? "a directory" : JSON.stringify(fs.readFileSync(p, "utf8"));
  } catch (e) {
    return e.code;
  }
};
const C = fs.constants;
const flags = [
  undefined, null, "r", "r+", "rs", "rs+", "w", "w+", "wx", "wx+", "a", "a+", "ax", "ax+", "as", "as+",
  C.O_RDONLY, C.O_RDWR, C.O_WRONLY, C.O_RDWR | C.O_CREAT, C.O_RDONLY | C.O_CREAT, C.O_RDONLY | C.O_TRUNC | C.O_CREAT,
  "zz", "", 1.5, {},
];
const forms = [
  ["readFileSync", (p, o) => fs.readFileSync(p, o)],
  ["readFile", (p, o) => called((cb) => fs.readFile(p, o, cb))],
  ["promises.readFile", (p, o) => fs.promises.readFile(p, o)],
];
let made_n = 0;
fs.mkdirSync(path.join(made, "dir"));
for (const flag of flags) {
  for (const target of ["existing", "missing", "directory"]) {
    for (const [form, read] of forms) {
      const p = target === "directory" ? path.join(made, "dir") : path.join(made, "f" + made_n++);
      if (target === "existing") fs.writeFileSync(p, "0123456789");
      let line;
      try {
        const got = await read(p, { flag, encoding: "utf8" });
        line = "read " + JSON.stringify(got);
      } catch (e) {
        line = shape(e);
      }
      const label = typeof flag === "number" ? "bits " + flag : JSON.stringify(flag) ?? "undefined";
      console.log(`${label} | ${target} | ${form}: ${line} | holds ${holds(p)}`);
    }
  }
}
// Without an encoding the answer is a Buffer, whatever the flag.
{
  const p = path.join(made, "buffer");
  fs.writeFileSync(p, "0123456789");
  const got = fs.readFileSync(p, { flag: "r+" });
  console.log("flag r+, no encoding: " + [Buffer.isBuffer(got), got.length].join(" "));
  const viaPromise = await fs.promises.readFile(p, { flag: "a+" });
  console.log("flag a+, promise, no encoding: " + [Buffer.isBuffer(viaPromise), viaPromise.length].join(" "));
}
// The descriptor the call opened is closed again, also when the read fails.
{
  const p = path.join(made, "closed");
  fs.writeFileSync(p, "0123456789");
  for (let i = 0; i < 300; i++) {
    try {
      fs.readFileSync(p, { flag: "a" });
    } catch {}
    try {
      fs.readFileSync(p, { flag: "r+" });
    } catch {}
  }
  fs.unlinkSync(p);
  console.log("600 reads later the file is gone: " + !fs.existsSync(p));
}

// What node checks before it opens anything: the options and, in the forms
// that take one, a signal already aborted. With a flag the open is what
// empties or creates the file, so a call node refuses must leave it alone.
{
  const abortShape = (e) => [e.constructor.name, e.name, e.code, String(e.cause)].join(" / ");
  const bad = [
    ["encoding", { flag: "w+", encoding: "nope" }],
    ["encoding, a+", { flag: "a+", encoding: "nope" }],
    ["options 5", 5],
    ["signal 1", { flag: "w+", signal: 1 }],
    ["aborted", { flag: "w+", signal: AbortSignal.abort() }],
    ["aborted with a reason", { flag: "a+", signal: AbortSignal.abort("why") }],
    ["aborted, plain read", { signal: AbortSignal.abort() }],
  ];
  for (const [label, options] of bad) {
    for (const target of ["existing", "missing"]) {
      for (const [form, read] of forms) {
        const p = path.join(made, "v" + made_n++);
        if (target === "existing") fs.writeFileSync(p, "0123456789");
        let line;
        try {
          const got = await read(p, options);
          line = "read " + (typeof got === "string" ? JSON.stringify(got) : "a Buffer of " + got.length);
        } catch (e) {
          line = e && e.name === "AbortError" ? abortShape(e) : e.code;
        }
        console.log(`${label} | ${target} | ${form}: ${line} | holds ${holds(p)}`);
      }
    }
  }
  // The callback form throws a bad flag from the call itself, names a
  // numeric one "options.flag", and looks at the flag before the path.
  const thrown = (f) => {
    let late = "no callback";
    try {
      f((e) => (late = "callback " + (e ? e.code : "ok")));
      return "returned, " + late;
    } catch (e) {
      return "threw " + e.code + " " + e.message;
    }
  };
  const p = path.join(made, "order");
  fs.writeFileSync(p, "0123456789");
  console.log("callback, flag zz: " + thrown((cb) => fs.readFile(p, { flag: "zz" }, cb)));
  console.log("callback, flag 1.5: " + thrown((cb) => fs.readFile(p, { flag: 1.5 }, cb)));
  console.log("callback, bad flag and bad path: " + thrown((cb) => fs.readFile(5.5, { flag: "zz" }, cb)));
  console.log("callback, bad encoding and bad flag: " + thrown((cb) => fs.readFile(p, { encoding: "nope", flag: "zz" }, cb)));
  console.log("callback, aborted: " + thrown((cb) => fs.readFile(p, { signal: AbortSignal.abort() }, cb)));
  console.log("sync, bad flag and bad path: " + thrown(() => fs.readFileSync(5.5, { flag: "zz" })));
  console.log("promise, flag 1.5: " + (await fs.promises.readFile(p, { flag: 1.5 }).then(() => "read", (e) => e.code + " " + e.message)));
  console.log("promise, bad flag and bad path: " + (await fs.promises.readFile(5.5, { flag: "zz" }).then(() => "read", (e) => e.code)));
}
