// What node's SystemError codes are instances of (#288). Most are made by
// makeSystemErrorWithCode, a per-code `class NodeError extends SystemError`:
// constructor.name 'NodeError', length 1, a prototype that owns only its
// constructor. fs.rm's ERR_FS_EISDIR is a stackless subclass of that code's
// class whose `constructor` answers SystemError itself (length 2, whose
// prototype owns toString too). Either way the error's name is
// 'SystemError', its own keys and its header unchanged.
//
// Regression guard: oam built every one with a single SystemError class, so
// constructor.name read 'SystemError' for all of them.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import util from "node:util";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-409-"));
const at = (p) => path.join(dir, p);
fs.mkdirSync(at("d/sub"), { recursive: true });
fs.mkdirSync(at("empty"));
fs.writeFileSync(at("f"), "x");

const show = (label, e) => {
  if (e === undefined) return console.log(label, "no error");
  const C = e.constructor;
  console.log(label, e.code, C.name, Object.getPrototypeOf(C).name, C.length,
    Object.getOwnPropertyNames(C.prototype), e.name, Object.keys(e), String(e).split(":")[0]);
};
const rejection = (p) => p.then(() => undefined, (e) => e);
const thrown = (f) => {
  try {
    f();
  } catch (e) {
    return e;
  }
};

show("cp into itself", await rejection(fs.promises.cp(at("d"), at("d/sub"), { recursive: true })));
show("cp dir onto file", await rejection(fs.promises.cp(at("d"), at("f"), { recursive: true })));
show("cp file onto dir", await rejection(fs.promises.cp(at("f"), at("empty"), { recursive: true })));
show("cp dir unrecursive", await rejection(fs.promises.cp(at("d"), at("new"))));
show("cp callback", await new Promise((r) => fs.cp(at("d"), at("d/sub"), { recursive: true }, r)) ?? undefined);
show("rmSync dir", thrown(() => fs.rmSync(at("empty"))));
show("promises.rm dir", await rejection(fs.promises.rm(at("empty"))));
show("getPriority", thrown(() => os.getPriority(2 ** 31 - 2)));

// One class per code, shared by every error of that code.
const a = await rejection(fs.promises.cp(at("d"), at("d/sub"), { recursive: true }));
const b = await rejection(fs.promises.cp(at("d"), at("d/sub"), { recursive: true }));
const c = await rejection(fs.promises.cp(at("d"), at("f"), { recursive: true }));
console.log("same class", a.constructor === b.constructor, a.constructor === c.constructor,
  Object.getPrototypeOf(a.constructor) === Object.getPrototypeOf(c.constructor));

// fs.rm's is node's stackless subclass of ERR_FS_EISDIR's class, whose
// constructor answers SystemError; every one is kIsNodeError, its own mark.
const chain = (e) => {
  const names = [];
  for (let p = Object.getPrototypeOf(e); p !== null; p = Object.getPrototypeOf(p)) names.push(p.constructor.name);
  return names.join(">");
};
const cpEisdir = await rejection(fs.promises.cp(at("d"), at("new")));
const rmEisdir = await rejection(fs.promises.rm(at("empty")));
console.log("rm chain", chain(rmEisdir), rmEisdir instanceof cpEisdir.constructor,
  Object.getOwnPropertySymbols(rmEisdir).map(String), Object.getOwnPropertySymbols(cpEisdir).map(String));
console.log("rm inspect", util.inspect(rmEisdir).split("\n")[0].split(":")[0]);
console.log("cp chain", chain(cpEisdir));

fs.rmSync(dir, { recursive: true, force: true });
