// writeFile / appendFile in all four forms -- Sync, callback, fs/promises
// and FileHandle -- check their arguments in node's order: the callback,
// then the options, then the data, then the path or descriptor.
//
// node v22.22.2 (lib/fs.js, lib/internal/fs/promises.js): getOptions first --
// a string is the encoding, null / undefined / a function the defaults,
// anything else not an object ERR_INVALID_ARG_TYPE "options"; an encoding
// Buffer does not know (but "buffer") ERR_INVALID_ARG_VALUE; a signal that
// is not an AbortSignal ERR_INVALID_ARG_TYPE -- then `options.flush` a
// boolean, then the data: a string or a view, and for the promise forms any
// other iterable too, written chunk by chunk. oam checked the data first, so
// a call with bad data and bad options named the wrong argument; it ignored
// the options of the promise forms entirely, wrote `5` as the text "5", and
// joined an array of chunks with commas.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-260-"));
const file = path.join(dir, "f.txt");

function shape(e) {
  return `${e.constructor.name} ${e.code} ${JSON.stringify(e.message)}`;
}
function sync(label, fn) {
  try {
    const r = fn();
    console.log(label, "->", r === undefined ? "undefined" : JSON.stringify(r));
  } catch (e) {
    console.log(label, "!!", shape(e));
  }
}
async function promised(label, fn) {
  try {
    const r = await fn();
    console.log(label, "->", r === undefined ? "undefined" : JSON.stringify(r));
  } catch (e) {
    console.log(label, "rejects", shape(e));
  }
}
function settle(label, fn) {
  return new Promise((resolve) => {
    try {
      fn((...args) => {
        console.log(label, "cb", args[0] ? shape(args[0]) : `ok, ${args.length} argument(s)`);
        resolve();
      });
    } catch (e) {
      console.log(label, "!!", shape(e));
      resolve();
    }
  });
}
const content = () => JSON.stringify(fs.readFileSync(file, "latin1"));

const cases = [
  ["number options", [123, 5]],
  ["symbol options", ["x", Symbol("s")]],
  ["bad encoding", [123, { encoding: "bogus" }]],
  ["bad encoding string", ["x", "bogus"]],
  ["'buffer' encoding", ["x", "buffer"]],
  ["flush 1", [123, { flush: 1 }]],
  ["flush 'yes'", ["x", { flush: "yes" }]],
  ["signal 1", ["x", { signal: 1 }]],
  ["array options", [123, []]],
  ["good options, bad data", [123, "utf8"]],
];
for (const [name, args] of cases) {
  sync(`writeFileSync ${name}`, () => fs.writeFileSync(file, ...args));
  sync(`appendFileSync ${name}`, () => fs.appendFileSync(file, ...args));
  sync(`writeFileSync(12.5) ${name}`, () => fs.writeFileSync(12.5, ...args));
  sync(`writeFileSync(-1) ${name}`, () => fs.writeFileSync(-1, ...args));
  await settle(`writeFile ${name}`, (cb) => fs.writeFile(file, ...args, cb));
  await settle(`appendFile(-1) ${name}`, (cb) => fs.appendFile(-1, ...args, cb));
  await promised(`fsp.writeFile ${name}`, () => fsp.writeFile(file, ...args));
  await promised(`fsp.appendFile ${name}`, () => fsp.appendFile(file, ...args));
  fs.writeFileSync(file, "");
  const fh = await fsp.open(file, "r+");
  await promised(`fh.writeFile ${name}`, () => fh.writeFile(...args));
  await promised(`fh.appendFile ${name}`, () => fh.appendFile(...args));
  await promised(`fsp.writeFile(fh) ${name}`, () => fsp.writeFile(fh, ...args));
  await fh.close();
}
sync("writeFile no callback, bad options", () => fs.writeFile(file, "x", 5));
sync("writeFileSync mode 'zz'", () => fs.writeFileSync(file, "x", { mode: "zz" }));
sync("writeFileSync(-1, 'x', mode 'zz')", () => fs.writeFileSync(-1, "x", { mode: "zz" }));
sync("writeFileSync(-1, 'x', latin1 + mode 'zz')", () => fs.writeFileSync(-1, "x", { encoding: "latin1", mode: "zz" }));

// What the good forms write, and what their callbacks are given.
await settle("writeFile ok", (cb) => fs.writeFile(file, "ab", cb));
await settle("appendFile ok", (cb) => fs.appendFile(file, Buffer.from("cd"), cb));
console.log("file", content());
await promised("fsp.writeFile iterable", () => fsp.writeFile(file, ["ab", Buffer.from("cd"), new Uint8Array([0x65])]));
console.log("file", content());
await promised("fsp.appendFile async iterable", () =>
  fsp.appendFile(file, (async function* () { yield "6667"; yield Buffer.from("hi"); })(), "hex"));
console.log("file", content());
await promised("fsp.writeFile iterable of numbers", () => fsp.writeFile(file, [5]));
console.log("file", content());
{
  fs.writeFileSync(file, "ABCDEF");
  const fh = await fsp.open(file, "r+");
  await promised("fh.writeFile iterable", () => fh.writeFile(new Set(["xy", "z"])));
  await promised("fh.writeFile empty", () => fh.writeFile(""));
  await promised("fh.writeFile latin1", () => fh.writeFile("é", "latin1"));
  await fh.close();
  console.log("file", content());
  const ro = await fsp.open(file, "r");
  await promised("fh.writeFile empty on a read-only handle", () => ro.writeFile(Buffer.alloc(0)));
  await ro.close();
}

fs.rmSync(dir, { recursive: true, force: true });
