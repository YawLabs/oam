// The descriptor argument of the synchronous fs calls.
//
// node range- and type-checks an fd before it is used, and only a descriptor
// that passes reaches the OS: -1, 2**31, 1.5, NaN, "3" or undefined are
// ERR_OUT_OF_RANGE / ERR_INVALID_ARG_TYPE, never EBADF. oam handed every one
// of them to its native, which answered EBADF -- so code that tells "a bad
// argument" from "a closed descriptor" by err.code took the wrong branch.
//
// Every *Sync form checks in node's C++ binding, after the JS-side checks of
// its other arguments, and words the failure the C++ way: the range test comes
// before the integer test (-0.5 and 2**53 are ">= 0 && <= 2147483647", only
// NaN, +-Infinity and in-range fractions are "an integer"), digits are not
// grouped, and a non-number's tail is V8's (`Received function`, `Received
// Symbol(s)`, `type bigint (1)`). Pinned here for every fd-taking *Sync call,
// with the orderings that follow from it: readSync of length 0 and
// writevSync([]) return before the descriptor is looked at, readvSync([]) and
// writeSync("") do not. readSync checks its buffer, options, offset and length
// first, the offset even when the length is 0 (oam returned 0 for
// readSync(-1, B, -5, 0), where node refuses the offset).
//
// conformance/cases/247 covers the callback forms and the streams, which check
// in JS first.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const B = Buffer.alloc(4);
function shape(err) {
  const out = { name: err.constructor.name, code: err.code, message: err.message };
  if ("syscall" in err) out.syscall = err.syscall;
  out.keys = Object.keys(err);
  return JSON.stringify(out);
}
function sync(label, fn) {
  try {
    const value = fn();
    console.log(label, "ok", JSON.stringify(value === undefined ? "undefined" : typeof value === "object" ? "object" : value));
  } catch (err) {
    console.log(label, shape(err));
  }
}

class Foo {}
const bad = [
  ["-1", -1],
  ["2**31", 2 ** 31],
  ["-(2**31)-1", -(2 ** 31) - 1],
  ["2**53", 2 ** 53],
  ["1e21", 1e21],
  ["1.5", 1.5],
  ["-0.5", -0.5],
  ["NaN", NaN],
  ["Infinity", Infinity],
  ["-Infinity", -Infinity],
  ["'3'", "3"],
  ["quoted", "it's"],
  ["long", "x".repeat(40)],
  ["long quoted", "it's".repeat(10)],
  ["null", null],
  ["undefined", undefined],
  ["{}", {}],
  ["[]", []],
  ["Foo", new Foo()],
  ["null proto", Object.create(null)],
  ["own constructor", { constructor: { name: "X" } }],
  ["1n", 1n],
  ["Symbol", Symbol("s")],
  ["function", function named() {}],
  ["true", true],
];
const calls = {
  closeSync: (fd) => fs.closeSync(fd),
  fstatSync: (fd) => fs.fstatSync(fd),
  readSync: (fd) => fs.readSync(fd, B, 0, 1, null),
  writeSync: (fd) => fs.writeSync(fd, B, 0, 1, null),
  "writeSync(string)": (fd) => fs.writeSync(fd, "x"),
  readvSync: (fd) => fs.readvSync(fd, [B]),
  writevSync: (fd) => fs.writevSync(fd, [B]),
  fsyncSync: (fd) => fs.fsyncSync(fd),
  fdatasyncSync: (fd) => fs.fdatasyncSync(fd),
  ftruncateSync: (fd) => fs.ftruncateSync(fd, 0),
  fchmodSync: (fd) => fs.fchmodSync(fd, 0o644),
  fchownSync: (fd) => fs.fchownSync(fd, 0, 0),
  futimesSync: (fd) => fs.futimesSync(fd, 0, 0),
};
// Every value through one call, the common cases through all of them.
for (const [label, value] of bad) sync(`closeSync(${label})`, () => fs.closeSync(value));
for (const [name, fn] of Object.entries(calls)) {
  for (const [label, value] of [["-1", -1], ["1.5", 1.5], ["'3'", "3"], ["undefined", undefined]]) {
    sync(`${name}(${label})`, () => fn(value));
  }
}

// Orderings.
sync("readSync(-1, B, 0, 0)", () => fs.readSync(-1, B, 0, 0, null));
sync("readSync(-1, B, {})", () => fs.readSync(-1, B, {}));
sync("writeSync(-1, '')", () => fs.writeSync(-1, ""));
sync("writeSync(-1, empty Buffer)", () => fs.writeSync(-1, Buffer.alloc(0)));
sync("writevSync(-1, [])", () => fs.writevSync(-1, []));
sync("writevSync(-1, 'x')", () => fs.writevSync(-1, "x"));
sync("readvSync(-1, [])", () => fs.readvSync(-1, []));
sync("readvSync(-1, 'x')", () => fs.readvSync(-1, "x"));
sync("readSync(-1, B, 0, 99)", () => fs.readSync(-1, B, 0, 99, null));
// readSync's other arguments, all checked before the descriptor -- the offset
// even by a read of length 0, which returns 0 only once the offset passes.
for (const [label, offset] of [["-5", -5], ["'x'", "x"], ["1.5", 1.5], ["NaN", NaN], ["2**53", 2 ** 53], ["5", 5], ["4", 4]]) {
  sync(`readSync(-1, B, ${label}, 0)`, () => fs.readSync(-1, B, offset, 0, null));
  sync(`readSync(-1, B, ${label}, 1)`, () => fs.readSync(-1, B, offset, 1, null));
  sync(`readSync(-1, B, { offset: ${label}, length: 0 })`, () => fs.readSync(-1, B, { offset, length: 0 }));
  sync(`readSync(-1, B, { offset: ${label} })`, () => fs.readSync(-1, B, { offset }));
}
sync("readSync(-1, B, { offset: null })", () => fs.readSync(-1, B, { offset: null }));
sync("readSync(-1, B, null, 0)", () => fs.readSync(-1, B, null, 0, null));
sync("readSync(-1, B, undefined, 0)", () => fs.readSync(-1, B, undefined, 0, null));
sync("readSync(-1, B, 0)", () => fs.readSync(-1, B, 0));
sync("readSync(-1, B, [])", () => fs.readSync(-1, B, []));
sync("readSync(-1, B, 0, -1)", () => fs.readSync(-1, B, 0, -1, null));
sync("readSync(-1, 'x', 0, 0)", () => fs.readSync(-1, "x", 0, 0, null));
sync("readSync(-1, empty Buffer, 0, 1)", () => fs.readSync(-1, Buffer.alloc(0), 0, 1, null));
sync("readSync(-1, empty Buffer, 0, 0)", () => fs.readSync(-1, Buffer.alloc(0), 0, 0, null));
sync("futimesSync(-1, {}, 0)", () => fs.futimesSync(-1, {}, 0));
sync("fstatSync(-1, { bigint: true })", () => fs.fstatSync(-1, { bigint: true }));
sync("readFileSync(-1)", () => fs.readFileSync(-1));
sync("appendFileSync(-1, Buffer)", () => fs.appendFileSync(-1, Buffer.from("x")));
const warn = process.emitWarning;
process.emitWarning = () => {}; // DEP0081, once per process; stderr is not compared anyway
sync("truncateSync(-1)", () => fs.truncateSync(-1));
sync("truncateSync(1.5)", () => fs.truncateSync(1.5));
process.emitWarning = warn;

// A valid descriptor still works, and a closed one is EBADF.
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-fdval-"));
const file = path.join(dir, "f.txt");
const fd = fs.openSync(file, "w+");
sync("writeSync(fd, 'hello')", () => fs.writeSync(fd, "hello"));
sync("fstatSync(fd).size", () => fs.fstatSync(fd).size);
sync("readSync(fd, B, 0, 4, 0)", () => fs.readSync(fd, B, 0, 4, 0));
console.log("bytes", JSON.stringify(B.toString()));
// The options form defaults to the whole buffer; the positional form does not.
const R = Buffer.alloc(4);
sync("readSync(fd, R, { length: 2, position: 1 })", () => fs.readSync(fd, R, { length: 2, position: 1 }));
sync("readSync(fd, R, { offset: 1, position: 0 })", () => fs.readSync(fd, R, { offset: 1, position: 0 }));
sync("readSync(fd, R, 0, undefined, 0)", () => fs.readSync(fd, R, 0, undefined, 0));
console.log("bytes", JSON.stringify(R.toString()));
sync("ftruncateSync(fd, 2)", () => fs.ftruncateSync(fd, 2));
sync("fsyncSync(fd)", () => fs.fsyncSync(fd));
sync("fstatSync(fd).size", () => fs.fstatSync(fd).size);
sync("closeSync(fd)", () => fs.closeSync(fd));
sync("closeSync(fd) again", () => fs.closeSync(fd));
fs.rmSync(dir, { recursive: true, force: true });

// The C++ check's error is node's THROW_ERR_*: a plain RangeError /
// TypeError with `code` assigned after the message -- own keys stack,
// message, code, the builtin prototype, and no `[CODE]` in the stack header
// -- where the JS check (fs.read with a callback, case 247) throws its
// internal NodeError. oam built both the JS way.
function cppShape(label, fn) {
  try {
    fn();
    console.log(label, "no throw");
  } catch (err) {
    const builtin = err instanceof RangeError ? RangeError : TypeError;
    console.log(
      label,
      err.code,
      Reflect.ownKeys(err).filter((k) => typeof k === "string").join(","),
      "builtin prototype:",
      Object.getPrototypeOf(err) === builtin.prototype,
      JSON.stringify(String(err.stack).split("\n")[0]),
      JSON.stringify(String(err)),
    );
  }
}
cppShape("C++ readSync(-1, B)", () => fs.readSync(-1, B));
cppShape("C++ closeSync(2 ** 31)", () => fs.closeSync(2 ** 31));
cppShape("C++ fstatSync(1.5)", () => fs.fstatSync(1.5));
cppShape("C++ closeSync('1')", () => fs.closeSync("1"));
