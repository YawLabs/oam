// The open flags of fs.open / openSync / fs/promises.open and the `flag`
// option of the writeFile family.
//
// node v22.22.2 runs every one through stringToFlags: an int32 is O_* bits,
// null / undefined is "r", a string must be one of node's spellings (the
// "s" ones included) or it is ERR_INVALID_ARG_VALUE "flags" -- checked after
// the path and before the mode. writeFile and appendFile open the path with
// their flag: "wx" fails EEXIST on an existing file, "r+" overwrites in place
// and fails ENOENT on a missing one, "a" appends, "r" fails EBADF on the
// write. oam ignored the writeFile flag entirely (writeFileSync(p, d,
// {flag: 'wx'}) overwrote the file), opened an unknown flag read-only, and
// read the numeric O_WRONLY|O_CREAT|O_EXCL as a truncating "w".
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-265-"));
const p = path.join(dir, "f");
const q = path.join(dir, "new");
const content = (f) => (fs.existsSync(f) ? JSON.stringify(fs.readFileSync(f, "latin1")) : "missing");
const shape = (e) => `${e.code} ${e.syscall ?? "-"} ${JSON.stringify(String(e.message).split(dir).join("<dir>"))}`;

const { O_WRONLY, O_RDWR, O_CREAT, O_EXCL, O_APPEND, O_TRUNC } = fs.constants;
const flags = [
  "w", "a", "wx", "ax", "r+", "a+", "w+", "r", "rs+", "sr+", "as", "as+", "sa", "rs", "xw", "xa+",
  "zz", "", null, 0, O_WRONLY | O_CREAT | O_EXCL, O_RDWR | O_CREAT | O_EXCL, O_WRONLY | O_APPEND | O_CREAT,
  O_WRONLY | O_CREAT | O_TRUNC, O_RDWR, 1.5, 2 ** 31, true, {},
];
const forms = [
  ["writeFileSync", (f, o) => fs.writeFileSync(f, "XY", o)],
  ["writeFileSync(buffer)", (f, o) => fs.writeFileSync(f, Buffer.from("XY"), o)],
  ["appendFileSync", (f, o) => fs.appendFileSync(f, "XY", o)],
  ["writeFile", (f, o) => new Promise((res, rej) => fs.writeFile(f, "XY", o, (e) => (e ? rej(e) : res())))],
  ["fsp.writeFile", (f, o) => fsp.writeFile(f, "XY", o)],
  ["fsp.appendFile", (f, o) => fsp.appendFile(f, "XY", o)],
];
for (const flag of flags) {
  const label = typeof flag === "object" && flag !== null ? "{}" : JSON.stringify(flag);
  for (const [form, run] of forms) {
    const out = [];
    for (const target of [p, q]) {
      fs.writeFileSync(p, "ABCD");
      fs.rmSync(q, { force: true });
      try {
        await run(target, { flag });
        out.push("ok " + content(target));
      } catch (e) {
        out.push("!! " + shape(e) + " " + content(target));
      }
    }
    console.log(form, label, "| existing:", out[0], "| new:", out[1]);
  }
}
for (const [label, run] of [
  ["empty string, flag r", () => fs.writeFileSync(p, "", { flag: "r" })],
  ["empty buffer, flag r", () => fs.writeFileSync(p, Buffer.alloc(0), { flag: "r" })],
  ["bad flag and bad mode", () => fs.writeFileSync(p, "x", { flag: "zz", mode: "q" })],
  ["bad path and bad flag", () => fs.writeFileSync(12.5, "x", { flag: "zz" })],
  ["callback form, bad flag", () => fs.writeFile(p, "x", { flag: "zz" }, () => console.log("  called back"))],
]) {
  try {
    run();
    console.log(label, "ok");
  } catch (e) {
    console.log(label, "!!", shape(e));
  }
}

fs.writeFileSync(p, "AB");
for (const [fl, mode] of [["zz", undefined], ["r", "zz"], ["zz", "zz"], ["", undefined], [1.5, undefined], [null, undefined], ["as", undefined], ["rs", undefined], [{}, undefined], ["r", 0o644], ["r", -1]]) {
  const label = `${typeof fl === "object" && fl !== null ? "{}" : JSON.stringify(fl)} ${JSON.stringify(mode)}`;
  try {
    fs.closeSync(fs.openSync(p, fl, mode));
    console.log("openSync", label, "ok");
  } catch (e) {
    console.log("openSync", label, "!!", shape(e));
  }
  try {
    await (await fsp.open(p, fl, mode)).close();
    console.log("fsp.open", label, "ok");
  } catch (e) {
    console.log("fsp.open", label, "rejects", shape(e));
  }
  try {
    await new Promise((res, rej) => fs.open(p, fl, mode, (e, fd) => (e ? rej(e) : (fs.closeSync(fd), res()))));
    console.log("open", label, "ok");
  } catch (e) {
    console.log("open", label, "!!", shape(e));
  }
  try {
    fs.openSync(12.5, fl, mode);
  } catch (e) {
    console.log("openSync(12.5)", label, "!!", shape(e));
  }
}
for (const [label, run] of [
  ["open(p, 'zz') -- two arguments: the second is the callback", () => fs.open(p, "zz")],
  ["open(p, 'r', 5)", () => fs.open(p, "r", 5)],
  ["open(p, 'zz', 5)", () => fs.open(p, "zz", 5)],
  ["open(p, 'zz', undefined, 5)", () => fs.open(p, "zz", undefined, 5)],
  ["open(p)", () => fs.open(p)],
]) {
  try {
    run();
    console.log(label, "ok");
  } catch (e) {
    console.log(label, "!!", shape(e));
  }
}
console.log("lengths", fs.open.length, fs.openSync.length, fsp.open.length);

fs.rmSync(dir, { recursive: true, force: true });
