// The `mode` of an open that creates its file is applied (#324): node hands
// it to open(2), and on Windows a mode with no owner-write bit creates the
// file read-only. Every form that takes one, creating a file with a
// read-only mode and with the default; then the same forms over a file that
// is already there.
//
// Only the owner-write bit is printed: it is the one bit Windows keeps, and
// the rest of a unix mode goes through the umask of whoever runs this.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-403-"));
process.chdir(dir);
const writable = (f) => ((fs.statSync(f).mode & 0o200) !== 0 ? "writable" : "read-only");
const closeStream = (s) => new Promise((resolve, reject) => {
  s.on("error", reject);
  s.end("x", () => s.close(resolve));
});

const forms = {
  openSync: (f, mode) => fs.closeSync(fs.openSync(f, "w", mode)),
  "openSync a": (f, mode) => fs.closeSync(fs.openSync(f, "a", mode)),
  "openSync wx": (f, mode) => fs.closeSync(fs.openSync(f, "wx", mode)),
  "openSync a+": (f, mode) => fs.closeSync(fs.openSync(f, "a+", mode)),
  open: (f, mode) => new Promise((resolve, reject) => {
    fs.open(f, "w", mode, (err, fd) => (err ? reject(err) : fs.close(fd, resolve)));
  }),
  "promises.open": async (f, mode) => (await fs.promises.open(f, "w", mode)).close(),
  writeFileSync: (f, mode) => fs.writeFileSync(f, "x", { mode }),
  "writeFileSync wx": (f, mode) => fs.writeFileSync(f, "x", { mode, flag: "wx" }),
  appendFileSync: (f, mode) => fs.appendFileSync(f, "x", { mode }),
  writeFile: (f, mode) => new Promise((resolve, reject) => {
    fs.writeFile(f, "x", { mode }, (err) => (err ? reject(err) : resolve()));
  }),
  appendFile: (f, mode) => new Promise((resolve, reject) => {
    fs.appendFile(f, "x", { mode }, (err) => (err ? reject(err) : resolve()));
  }),
  "promises.writeFile": (f, mode) => fs.promises.writeFile(f, "x", { mode }),
  "promises.writeFile wx": (f, mode) => fs.promises.writeFile(f, "x", { mode, flag: "wx" }),
  "promises.appendFile": (f, mode) => fs.promises.appendFile(f, "x", { mode }),
  createWriteStream: (f, mode) => closeStream(fs.createWriteStream(f, { mode })),
};

let n = 0;
for (const [name, form] of Object.entries(forms)) {
  const row = [];
  for (const mode of [0o444, 0o666, "444", undefined]) {
    const f = `created-${n++}.txt`;
    try {
      await form(f, mode);
      row.push(writable(f));
    } catch (e) {
      row.push(e.code);
    }
  }
  console.log(`${name}: creating with 0o444, 0o666, "444", none ->`, row.join(", "));
}

// A file that is already there. An open that only opens it leaves it as it
// is, whatever the mode says. One that truncates ("w") does too on unix,
// where the mode is for a file open(2) creates; on Windows it is
// CREATE_ALWAYS, which gives the file the attributes asked for, so the
// mode's read-only lands on a file that was writable.
for (const [name, form] of Object.entries(forms)) {
  if (name.endsWith("wx")) continue;
  const f = `existing-${n++}.txt`;
  fs.writeFileSync(f, "old");
  let outcome;
  try {
    await form(f, 0o444);
    outcome = writable(f);
  } catch (e) {
    outcome = e.code;
  }
  console.log(`${name}: over a writable file with 0o444 ->`, outcome);
}

// The rest of the mode, where there is one: a file created 0o600 is nobody
// else's whatever the umask (on Windows the answer is no, from both).
for (const [name, form] of Object.entries(forms)) {
  const f = `private-${n++}.txt`;
  await form(f, 0o600);
  console.log(`${name}: created 0o600 is the owner's alone ->`, (fs.statSync(f).mode & 0o077) === 0);
}

// And a read-only file refuses a writing open, as one made by chmod does.
fs.writeFileSync("ro.txt", "x", { mode: 0o444 });
const root = typeof process.getuid === "function" && process.getuid() === 0;
if (root) {
  console.log("writing to the read-only file: EACCES or EPERM");
} else {
  try {
    fs.writeFileSync("ro.txt", "y");
    console.log("writing to the read-only file: written");
  } catch (e) {
    console.log("writing to the read-only file:", e.code === "EACCES" || e.code === "EPERM" ? "EACCES or EPERM" : e.code);
  }
}
console.log("it still reads:", fs.readFileSync("ro.txt", "utf8"));

process.chdir(os.tmpdir());
for (const f of fs.readdirSync(dir)) {
  fs.chmodSync(path.join(dir, f), 0o666);
}
fs.rmSync(dir, { recursive: true, force: true });
