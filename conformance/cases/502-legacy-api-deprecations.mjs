// Deprecations node v22.22.2 emits at runtime that oam was missing (#368):
//   - DEP0040, the `punycode` module, once, when it is first loaded -- by
//     require, import or process.getBuiltinModule. node does not warn when a
//     file under node_modules loads it first; it looks for "/node_modules/" in
//     the stack, so on Windows, where the paths are written with backslashes,
//     it warns all the same (probed: v22.22.2 on win32 warns, on linux not).
//   - DEP0044 util.isArray and DEP0060 util._extend, each on its first call.
//   - DEP0121 net._setSimultaneousAccepts, on its first call.
//   - DEP0025 the `sys` module (util under its old name), when first loaded.
//   - DEP0179 crypto.Hash and DEP0181 crypto.Hmac called as constructors,
//     with or without `new`; createHash / createHmac stay silent.
//
// The printing goes to stderr, which the harness does not compare, so each
// scenario runs as a child and its stderr is printed here, with the pid and
// the program name in the one-time hint taken out.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-502-")));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(dir, rel)), { recursive: true });
  fs.writeFileSync(path.join(dir, rel), text);
};
const hear = `process.on("warning", (w) => console.log("heard", w.name, w.code, JSON.stringify(w.message)));\n`;

write("package.json", JSON.stringify({ name: "case-502", type: "commonjs" }));
write("node_modules/dep/package.json", JSON.stringify({ name: "dep", main: "index.js" }));
write("node_modules/dep/index.js", `module.exports = require("punycode").encode("münchen");\n`);
write("node_modules/dep/esm.mjs", `import punycode from "punycode";\nexport default punycode.decode("mnchen-3ya");\n`);
write(
  "punycode.js",
  `${hear}const punycode = require("punycode");
console.log(Object.keys(punycode).sort().join(), punycode.version);
console.log(punycode.encode("münchen"), punycode.toASCII("münchen.de"), punycode.toUnicode("xn--mnchen-3ya.de"));
console.log("same module:", require("punycode") === punycode, require("node:punycode") === punycode);
`,
);
write(
  "punycode-from-node-modules.js",
  `${hear}console.log("dep says", require("dep"));
console.log("then the program's own require:", typeof require("punycode").encode);
`,
);
write(
  "punycode-esm-from-node-modules.mjs",
  `${hear}import decoded from "dep/esm.mjs";\nconsole.log("dep says", decoded);\n`,
);
write("punycode-import.mjs", `${hear}import punycode from "punycode";\nconsole.log("imported", typeof punycode.encode);\n`);
write(
  "punycode-builtin.js",
  `${hear}console.log("getBuiltinModule", typeof process.getBuiltinModule("punycode").decode);\n`,
);
write(
  "util.js",
  `${hear}const util = require("util");
console.log("isArray", util.isArray.name, util.isArray.length, util.isArray([]), util.isArray({}), util.isArray(new Array(2)));
console.log("_extend", util._extend.name, util._extend.length);
const target = { a: 1 };
console.log(util._extend(target, { b: 2, a: 3 }) === target, JSON.stringify(target));
console.log(JSON.stringify(util._extend({ a: 1 }, null)), JSON.stringify(util._extend({ a: 1 }, "str")), JSON.stringify(util._extend({ a: 1 }, [9])));
const order = [];
util._extend({ set x(v) { order.push("x"); }, set y(v) { order.push("y"); } }, { x: 1, y: 2 });
console.log("assignment order", order.join());
`,
);
write(
  "net.js",
  `${hear}const net = require("net");
const f = net._setSimultaneousAccepts;
console.log(typeof f, f.name, f.length, Object.keys(net).includes("_setSimultaneousAccepts"));
console.log("returns", f(), f(true));
`,
);
write(
  "sys.js",
  `${hear}const sys = require("sys");
console.log("sys is util:", sys === require("util"), require("sys") === sys, require("node:sys") === sys);
const { builtinModules, isBuiltin } = require("module");
console.log("listed:", builtinModules.includes("sys"), isBuiltin("sys"), isBuiltin("node:sys"));
console.log("getBuiltinModule:", process.getBuiltinModule("sys") === sys);
`,
);
write(
  "sys-import.mjs",
  `${hear}import sys, { format } from "sys";\nimport util from "util";\nconsole.log("sys is util:", sys === util, format("%s!", "named"));\n`,
);
write(
  "hash.js",
  `${hear}const crypto = require("crypto");
const h = crypto.Hash("sha256");
console.log(h instanceof crypto.Hash, h.constructor === crypto.Hash, h.update("a").digest("hex"));
console.log(new crypto.Hash("sha1").update("a").digest("hex"));
console.log(crypto.Hash.name, crypto.Hash.length, crypto.createHash("md5") instanceof crypto.Hash);
console.log(crypto.Hmac.name, crypto.Hmac.length, crypto.createHmac("md5", "k") instanceof crypto.Hmac);
console.log(crypto.Hmac("sha256", "key").update("a").digest("hex"));
console.log(new crypto.Hmac("sha1", "key").update("a").digest("hex"));
for (const call of [() => crypto.Hash(), () => crypto.Hash(5), () => crypto.Hmac(), () => crypto.Hmac(5, "key")]) {
  try { call(); console.log("no throw"); } catch (e) { console.log(e.name, e.code, e.message); }
}
`,
);
write("create-hash.js", `${hear}const crypto = require("crypto");\nconsole.log(crypto.createHash("sha1").update("a").digest("hex"), crypto.createHmac("sha1", "k").update("a").digest("hex"));\n`);

const run = (file, flags = []) => {
  const child = spawnSync(process.execPath, [...flags, path.join(dir, file)], { encoding: "utf8", cwd: dir });
  console.log(`## ${[...flags, file].join(" ")}`);
  console.log(`exit ${child.status}`);
  for (const line of child.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
  for (const line of child.stderr.split(/\r?\n/)) {
    if (line === "") continue;
    const shown = line.replace(/\((node|oam):\d+\)/g, "(node:PID)").replace(/^\(Use `(node|oam) /, "(Use `node ");
    console.log(`  err | ${shown}`);
  }
};

try {
  for (const file of [
    "punycode.js",
    "punycode-from-node-modules.js",
    "punycode-esm-from-node-modules.mjs",
    "punycode-import.mjs",
    "punycode-builtin.js",
    "util.js",
    "net.js",
    "sys.js",
    "sys-import.mjs",
    "hash.js",
    "create-hash.js",
  ]) {
    run(file);
  }
  for (const file of ["punycode.js", "util.js", "net.js", "sys.js", "hash.js"]) run(file, ["--no-deprecation"]);
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
