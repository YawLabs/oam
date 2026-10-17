// fs.openAsBlob checks its arguments and opens the file synchronously
// (#374): node's openAsBlob validates the options, then the type, then the
// path, then opens and fstats the file at the call, so every one of those
// failures throws at the call rather than rejecting the promise it returns.
// A path it cannot open is a TypeError ERR_INVALID_ARG_VALUE "Unable to open
// file as blob" with `code` its only own property -- except a --permission
// denial, which keeps its own ERR_ACCESS_DENIED. On Windows, the NUL device
// opens as a Blob of no bytes (#383).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
// Real path, as cases 510/511: macOS's tmpdir sits under the /var ->
// /private/var symlink, and node --permission refuses a main-module path
// that crosses a symlink (resolveMainPath's realpathSync stats it), so the
// child would die before its first row.
const D = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-522-")));
const scrub = (s) => String(s).replaceAll(D, "<D>");
const desc = (e) => `${e.constructor.name} ${e.code} ${JSON.stringify(scrub(e.message))} own=${Object.keys(e).join("|")}`;
async function call(label, fn) {
  let p;
  try {
    p = fn();
  } catch (e) {
    console.log(label, "-> throws", desc(e));
    return;
  }
  console.log(
    label,
    "-> returns a promise that",
    await p.then(
      (b) => `resolves Blob ${b.size} ${JSON.stringify(b.type)}`,
      (e) => `rejects ${desc(e)}`,
    ),
  );
}
const f = path.join(D, "f");
fs.writeFileSync(f, "abc");
await call("a missing file", () => fs.openAsBlob(path.join(D, "nope")));
await call("a missing directory", () => fs.openAsBlob(path.join(D, "nope", "x")));
await call("under a file", () => fs.openAsBlob(path.join(f, "x")));
await call("options null", () => fs.openAsBlob(f, null));
await call("options a string", () => fs.openAsBlob(f, "text/plain"));
await call("options an array", () => fs.openAsBlob(f, []));
await call("options a function", () => fs.openAsBlob(f, () => {}));
await call("type 1", () => fs.openAsBlob(f, { type: 1 }));
await call("type {}", () => fs.openAsBlob(f, { type: {} }));
await call("type 0", () => fs.openAsBlob(f, { type: 0 }));
await call("type mixed case, non-ASCII", () => fs.openAsBlob(f, { type: "A/Bé" }));
await call("path 123", () => fs.openAsBlob(123));
await call("no path", () => fs.openAsBlob());
await call("a NUL byte", () => fs.openAsBlob("a\0b"));
await call("a file URL", () => fs.openAsBlob(new URL("file://" + (process.platform === "win32" ? "/" : "") + f.replaceAll("\\", "/"))));
await call("a Buffer", () => fs.openAsBlob(Buffer.from(f)));
await call("a bad type and a missing file", () => fs.openAsBlob(path.join(D, "nope"), { type: 5 }));
await call("a bad path and bad options", () => fs.openAsBlob(5, 5));
await call("a bad path and a bad type", () => fs.openAsBlob(5, { type: 5 }));
if (process.platform === "win32") {
  for (const p of ["\\\\.\\NUL", "\\\\?\\GLOBALROOT\\Device\\Null"]) {
    await call(`windows ${p}`, () => fs.openAsBlob(p));
  }
  const nul = await fs.openAsBlob("\\\\.\\NUL");
  console.log("NUL reads:", JSON.stringify(await nul.text()), (await nul.arrayBuffer()).byteLength);
}

// --permission: a denied path throws ERR_ACCESS_DENIED at the call, a
// missing allowed one ERR_INVALID_ARG_VALUE.
fs.mkdirSync(path.join(D, "other"));
fs.mkdirSync(path.join(D, "sub"));
fs.writeFileSync(path.join(D, "sub", "s.txt"), "s");
fs.writeFileSync(
  path.join(D, "other", "child.cjs"),
  `const fs = require("fs");
const path = require("path");
const P = path.dirname(__dirname);
const scrub = (s) => String(s).split(P).join("<P>").replaceAll("\\\\", "/");
const rows = [
  ["a denied directory", path.join(P, "sub")],
  ["a denied file", path.join(P, "sub", "s.txt")],
  ["a denied missing path", path.join(P, "nope")],
  ["an allowed file", __filename],
  ["an allowed missing path", path.join(__dirname, "nope")],
];
(async () => {
  for (const [label, p] of rows) {
    let r;
    try {
      const pr = fs.openAsBlob(p);
      r = await pr.then((b) => "resolves " + b.size, (e) => "rejects " + e.code);
    } catch (e) {
      r = "throws " + e.constructor.name + " " + e.code + " " + (e.permission || "-") + " " + scrub(e.resource) + " " + Object.keys(e).join("|") + " " + JSON.stringify(e.message);
    }
    console.log(label + ": " + r);
  }
  // The denial's message names the flag that grants it, per permission.
  for (const [label, f] of [
    ["readFileSync denied", () => fs.readFileSync(path.join(P, "sub", "s.txt"))],
    ["writeFileSync denied", () => fs.writeFileSync(path.join(__dirname, "w.txt"), "w")],
    ["spawnSync denied", () => require("child_process").spawnSync(process.execPath, ["-v"])],
    ["Worker denied", () => new (require("worker_threads").Worker)(__filename)],
  ]) {
    try {
      f();
      console.log(label + ": no throw");
    } catch (e) {
      console.log(label + ": " + e.code + " " + e.permission + " " + JSON.stringify(e.message));
    }
  }
})();
`,
);
const r = spawnSync(
  process.execPath,
  ["--permission", "--allow-fs-read=" + path.join(D, "other"), path.join(D, "other", "child.cjs")],
  { encoding: "utf8" },
);
console.log(r.stdout.trimEnd());
console.log("child exit", r.status);
fs.rmSync(D, { recursive: true, force: true });
