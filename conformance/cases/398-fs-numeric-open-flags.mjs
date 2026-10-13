// Numeric open flags mean what their bits say (#328). node hands the number to
// open(2) / libuv as it is: `O_WRONLY` opens an existing file for writing and
// leaves its content alone, `O_WRONLY | O_CREAT` creates one that is missing
// and does not truncate one that is there, and only `O_TRUNC` truncates.
//
// Regression guard: oam turned the number into the nearest fopen-style string,
// and no such string spells "write, do not truncate, do not append": `O_WRONLY`,
// `O_WRONLY | O_CREAT` and `O_RDWR | O_CREAT` opened as `"w"` / `"w+"` and
// emptied the file, and `O_WRONLY` created one that was missing.
//
// The same went for a flag string's own bits: the synchronous spellings
// (`"rs+"`, `"as"`) opened without O_SYNC, and `fs.constants` listed values
// that were not the platform's (O_SYNC on Windows, where node has none; one
// O_DIRECTORY for every platform).
//
// Each row opens an existing ten-byte file and a missing one, writes two bytes
// through the descriptor where it can, and reports what the file holds. On
// Windows the hidden-file, alternate-stream and dangling-junction rows show
// which opens replace the file (#333 is the junction one).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-398-"));
// Whichever row throws, the directory goes.
process.on("exit", () => {
  try {
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});
const scrub = (s) => (typeof s === "string" ? s.split(made).join("<D>").split(path.sep).join("/") : s);
const shape = (e) => (e && e.code ? `${e.code} ${e.syscall}` : "threw " + scrub(String(e)));
const called = (f) => new Promise((res, rej) => f((e, v) => (e ? rej(e) : res(v))));
const C = fs.constants;
const bits = { O_CREAT: C.O_CREAT, O_TRUNC: C.O_TRUNC, O_EXCL: C.O_EXCL, O_APPEND: C.O_APPEND };
const access = { O_RDONLY: C.O_RDONLY, O_WRONLY: C.O_WRONLY, O_RDWR: C.O_RDWR };
let made_n = 0;
const read = (p) => {
  try {
    return JSON.stringify(fs.readFileSync(p, "utf8"));
  } catch (e) {
    return e.code;
  }
};
const tryIt = (f) => {
  try {
    return f();
  } catch (e) {
    return e.code;
  }
};
// What the descriptor allows, and where its writes land.
const use = (fd) => {
  const wrote = tryIt(() => fs.writeSync(fd, "AB"));
  const got = tryIt(() => {
    const b = Buffer.alloc(4);
    return JSON.stringify(b.toString("latin1", 0, fs.readSync(fd, b, 0, 4, 0)));
  });
  return `write=${wrote} read=${got}`;
};

const combos = [];
for (const [aName, a] of Object.entries(access)) {
  for (let mask = 0; mask < 16; mask++) {
    const names = [aName];
    let n = a;
    Object.entries(bits).forEach(([name, bit], i) => {
      if (mask & (1 << i)) {
        names.push(name);
        n |= bit;
      }
    });
    combos.push([names.join("|"), n]);
  }
}
const forms = [
  ["openSync", (p, n) => fs.openSync(p, n)],
  ["open", (p, n) => called((cb) => fs.open(p, n, cb))],
];
for (const [label, n] of combos) {
  for (const existing of [true, false]) {
    for (const [form, open] of forms) {
      const p = path.join(made, "f" + made_n++);
      if (existing) fs.writeFileSync(p, "0123456789");
      let line;
      try {
        const fd = await open(p, n);
        try {
          line = "opened " + use(fd);
        } finally {
          fs.closeSync(fd);
        }
      } catch (e) {
        line = shape(e);
      }
      console.log(`${label} | ${existing ? "existing" : "missing"} | ${form}: ${line} | holds ${read(p)}`);
    }
    // The promise form, and writeFileSync, which takes the number as `flag`.
    {
      const p = path.join(made, "f" + made_n++);
      if (existing) fs.writeFileSync(p, "0123456789");
      let line;
      try {
        const handle = await fs.promises.open(p, n);
        try {
          line = "opened " + (await handle.write("AB").then((r) => r.bytesWritten, (e) => e.code));
        } finally {
          await handle.close();
        }
      } catch (e) {
        line = shape(e);
      }
      console.log(`${label} | ${existing ? "existing" : "missing"} | promises.open: ${line} | holds ${read(p)}`);
    }
    {
      const p = path.join(made, "f" + made_n++);
      if (existing) fs.writeFileSync(p, "0123456789");
      const line = tryIt(() => (fs.writeFileSync(p, "AB", { flag: n }), "written"));
      console.log(`${label} | ${existing ? "existing" : "missing"} | writeFileSync flag: ${line} | holds ${read(p)}`);
    }
  }
}

// Numbers that are no flags.
for (const n of [-1, 3, 4, 1.5, 2 ** 31, NaN, "1", 1n, {}]) {
  const p = path.join(made, "f" + made_n++);
  fs.writeFileSync(p, "0123456789");
  let line;
  try {
    const fd = fs.openSync(p, n);
    line = "opened " + use(fd);
    fs.closeSync(fd);
  } catch (e) {
    line = e.code + " " + (e.syscall ?? scrub(e.message));
  }
  console.log(`flags ${typeof n} ${String(n)}: ${line} | holds ${read(p)}`);
}

// What `fs.constants` calls the flags here, and node's flag strings as bits.
console.log("constants | " + Object.keys(C).filter((k) => k.startsWith("O_") || k.startsWith("UV_FS_O")).map((k) => k + "=" + C[k]).join(" "));
for (const flags of ["r", "rs", "sr", "r+", "rs+", "w", "wx", "xw", "w+", "wx+", "a", "ax", "as", "sa", "a+", "ax+", "as+", "sa+", "zz", "R", ""]) {
  for (const existing of [true, false]) {
    const p = path.join(made, "f" + made_n++);
    if (existing) fs.writeFileSync(p, "0123456789");
    let line;
    try {
      const fd = fs.openSync(p, flags);
      line = "opened " + use(fd);
      fs.closeSync(fd);
    } catch (e) {
      line = e.code + " " + (e.syscall ?? "");
    }
    console.log(`flags ${JSON.stringify(flags)} | ${existing ? "existing" : "missing"}: ${line} | holds ${read(p)}`);
  }
}

if (process.platform === "win32") {
  const { execFileSync } = await import("node:child_process");
  const attrib = (...args) => execFileSync("attrib", args).toString();
  process.on("exit", () => {
    try {
      for (const name of fs.readdirSync(made)) {
        try {
          attrib("-r", "-h", "-s", path.join(made, name));
        } catch {}
      }
    } catch {}
  });
  // Only CREATE_ALWAYS replaces the file: a hidden file is refused by it and
  // by nothing else, and a stream beside the data survives every other open.
  for (const [label, n] of combos.filter(([, bits]) => (bits & 3) !== 0)) {
    const hidden = path.join(made, "h" + made_n++);
    fs.writeFileSync(hidden, "0123456789");
    attrib("+h", hidden);
    const withStream = path.join(made, "s" + made_n++);
    fs.writeFileSync(withStream, "0123456789");
    fs.writeFileSync(withStream + ":side", "stream");
    const open = (p) => {
      try {
        fs.closeSync(fs.openSync(p, n));
        return "opened";
      } catch (e) {
        return e.code;
      }
    };
    console.log(`${label} | hidden: ${open(hidden)} holds ${read(hidden)} | with a stream: ${open(withStream)} stream ${read(withStream + ":side")}`);
  }
  // Through a junction whose target is gone (#333): an exclusive open makes
  // the target, as a directory, and hands back its descriptor.
  for (const flags of ["wx", "wx+", "ax", "ax+", "w", "a", "r", "r+", C.O_WRONLY | C.O_CREAT, C.O_RDWR | C.O_CREAT | C.O_EXCL]) {
    const link = path.join(made, "j" + made_n++);
    const target = path.join(made, "t" + made_n++);
    fs.symlinkSync(target, link, "junction");
    let line;
    try {
      const fd = fs.openSync(link, flags);
      line = "opened directory=" + fs.fstatSync(fd).isDirectory();
      fs.closeSync(fd);
    } catch (e) {
      line = shape(e);
    }
    console.log(`junction to nothing | ${JSON.stringify(flags)}: ${line} | target ${fs.existsSync(target) ? (fs.statSync(target).isDirectory() ? "a directory" : "a file") : "missing"}`);
  }
}
