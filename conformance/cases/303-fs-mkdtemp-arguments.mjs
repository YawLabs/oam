// fs.mkdtemp's arguments, as node v22.22.2 checks them in mkdtempSync,
// fs.mkdtemp and fs/promises.mkdtemp alike: the options first (getOptions: a
// string or object, a known encoding), then the prefix (getValidatedPath: a
// string, Buffer or file: URL without NUL bytes) -- the callback form checks
// its callback before either -- then, once per process, the warning for a
// template ending in "X". And the result in the encoding asked for.
//
// Regression guard: oam ignored the options outright -- `mkdtempSync(p,
// "buffer")` returned a string, a bogus encoding or a numeric options
// argument was accepted -- never warned, and its functions' lengths were 1, 0
// and 1 where node's are 2, 3 and 2. Names are random: shapes only.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const SUFFIX = /^[A-Za-z0-9]{6}$/;
const isName = (prefix, got) => got.length === prefix.length + 6 && got.startsWith(prefix) && SUFFIX.test(got.slice(prefix.length));
const err = (label, e) => console.log(`${label}: ${e.name} ${e.code} ${e.message}`);
function sync(label, fn) {
  try {
    const got = fn();
    console.log(`${label}: returned ${typeof got}`);
  } catch (e) {
    err(label, e);
  }
}
async function promised(label, fn) {
  let p;
  try {
    p = fn();
  } catch (e) {
    return err(`${label} (thrown)`, e);
  }
  await p.then(() => console.log(`${label}: resolved`), (e) => err(`${label} (rejected)`, e));
}

const warnings = [];
process.on("warning", (w) => warnings.push(`${w.name}: ${w.message}`));

console.log(`length: sync=${fs.mkdtempSync.length} callback=${fs.mkdtemp.length} promises=${fsp.mkdtemp.length}`);
console.log(`name: ${fs.mkdtempSync.name} ${fs.mkdtemp.name} ${fsp.mkdtemp.name}`);

const home = process.cwd();
const base = fs.mkdtempSync(path.join(os.tmpdir(), "oam-conf-303-"));
process.chdir(base);
try {
  for (const [label, prefix] of [
    ["number", 42], ["undefined", undefined], ["null", null], ["object", {}],
    ["nul byte", "a\0b"], ["buffer nul byte", Buffer.from("a\0b")], ["http url", new URL("http://x/y")],
  ]) {
    sync(`sync ${label}`, () => fs.mkdtempSync(prefix));
    await promised(`promises ${label}`, () => fsp.mkdtemp(prefix));
    sync(`callback ${label}`, () => fs.mkdtemp(prefix, () => console.log("callback called")));
  }
  sync("sync bogus encoding", () => fs.mkdtempSync("ok-", "bogus"));
  sync("sync bogus encoding in object", () => fs.mkdtempSync("ok-", { encoding: "bogus" }));
  sync("sync numeric options", () => fs.mkdtempSync("ok-", 5));
  sync("sync options before prefix", () => fs.mkdtempSync(42, "bogus"));
  await promised("promises bogus encoding", () => fsp.mkdtemp("ok-", "bogus"));
  await promised("promises numeric options", () => fsp.mkdtemp("ok-", 5));
  sync("callback missing", () => fs.mkdtemp("ok-"));
  sync("callback missing, bad prefix", () => fs.mkdtemp(42, "bogus"));
  sync("callback options before prefix", () => fs.mkdtemp(42, "bogus", () => {}));
  console.log(`made by refused calls: ${fs.readdirSync(".").length}`);

  // The result in the encoding asked for.
  const buf = fs.mkdtempSync("eb-", "buffer");
  console.log(`buffer: isBuffer=${Buffer.isBuffer(buf)} name=${isName("eb-", buf.toString())}`);
  const obj = fs.mkdtempSync("eo-", { encoding: "buffer" });
  console.log(`buffer in object: isBuffer=${Buffer.isBuffer(obj)} name=${isName("eo-", obj.toString())}`);
  const hex = fs.mkdtempSync("eh-", "hex");
  console.log(`hex: ${typeof hex} name=${isName("eh-", Buffer.from(hex, "hex").toString())}`);
  const b64 = fs.mkdtempSync("e6-", "base64");
  console.log(`base64: ${typeof b64} name=${isName("e6-", Buffer.from(b64, "base64").toString())}`);
  const latin1 = fs.mkdtempSync("é-", "latin1");
  console.log(`latin1: ${typeof latin1} length=${latin1.length} name=${isName("é-", Buffer.from(latin1, "latin1").toString())}`);
  for (const [label, options] of [["utf8", "utf8"], ["null", null], ["encoding undefined", { encoding: undefined }], ["extra keys", { other: 1 }]]) {
    console.log(`${label}: ${typeof fs.mkdtempSync("eu-", options)}`);
  }
  const pbuf = await fsp.mkdtemp("pb-", "buffer");
  console.log(`promises buffer: isBuffer=${Buffer.isBuffer(pbuf)} name=${isName("pb-", pbuf.toString())}`);
  const phex = await fsp.mkdtemp("ph-", { encoding: "hex" });
  console.log(`promises hex: name=${isName("ph-", Buffer.from(phex, "hex").toString())}`);
  const cbuf = await new Promise((res, rej) => fs.mkdtemp("cb-", "buffer", (e, d) => (e ? rej(e) : res(d))));
  console.log(`callback buffer: isBuffer=${Buffer.isBuffer(cbuf)} name=${isName("cb-", cbuf.toString())}`);

  // A template ending in X warns once per process, on every platform.
  fs.mkdtempSync("aX");
  await fsp.mkdtemp("bX");
  fs.mkdtempSync("plain-");
  // ...and is why: Windows and glibc replace exactly the six X's node
  // appends, keeping the prefix's own, while macOS's mkdtemp(3) replaces the
  // whole trailing run (keeping all six by chance is 1 in 62^6). Up to the
  // fix oam replaced six everywhere.
  for (const [label, xs] of [["sync", fs.mkdtempSync("xXXXXXX")], ["promises", await fsp.mkdtemp("yXXXXXX")]]) {
    console.log(
      `${label} prefix of X's: length=${xs.length} keptPrefix=${xs.slice(1, 7) === "XXXXXX"}` +
      ` alnum=${/^[xy][A-Za-z0-9]{12}$/.test(xs)}`,
    );
  }
  await new Promise((res) => setImmediate(res));
  console.log(`warnings: ${JSON.stringify(warnings)}`);
} finally {
  process.chdir(home);
  fs.rmSync(base, { recursive: true, force: true });
}
