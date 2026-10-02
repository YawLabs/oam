// What an fs callback is called with on success.
//
// node's FSReqCallback::Resolve (src/node_file.cc, v22.22.2) passes the result
// only when there is one: an operation that produces nothing -- chmod,
// rename, unlink, mkdir, writeFile, ... -- calls back with the single argument
// null. oam called every one of them with (null, undefined), so a callback
// that looks at arguments.length, or a promisify-style wrapper that does,
// saw two arguments where node gives one.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-262-"));
const f = path.join(dir, "f");
fs.writeFileSync(f, "abc");

const ops = [
  ["chmod", (cb) => fs.chmod(f, 0o666, cb)],
  ["truncate", (cb) => fs.truncate(f, 1, cb)],
  ["copyFile", (cb) => fs.copyFile(f, f + "2", cb)],
  ["rename", (cb) => fs.rename(f + "2", f + "3", cb)],
  ["unlink", (cb) => fs.unlink(f + "3", cb)],
  ["access", (cb) => fs.access(f, cb)],
  ["mkdir", (cb) => fs.mkdir(path.join(dir, "m"), cb)],
  ["rmdir", (cb) => fs.rmdir(path.join(dir, "m"), cb)],
  ["utimes", (cb) => fs.utimes(f, 1, 1, cb)],
  ["link", (cb) => fs.link(f, path.join(dir, "hl"), cb)],
  ["chown", (cb) => fs.chown(f, -1, -1, cb)],
  ["lchown", (cb) => fs.lchown(f, -1, -1, cb)],
  ["writeFile", (cb) => fs.writeFile(f, "x", cb)],
  ["appendFile", (cb) => fs.appendFile(f, "y", cb)],
  ["rm", (cb) => fs.rm(path.join(dir, "hl"), cb)],
  ["stat", (cb) => fs.stat(f, cb)],
  ["readFile", (cb) => fs.readFile(f, "utf8", cb)],
  ["readdir", (cb) => fs.readdir(dir, cb)],
];
for (const [name, op] of ops) {
  await new Promise((resolve) =>
    op(function (...args) {
      const rest = args.slice(1).map((v) => (v instanceof fs.Stats ? "Stats" : JSON.stringify(v)));
      console.log(name, args.length, args[0], ...rest);
      resolve();
    }),
  );
}

// A failed read or write calls back with (err, 0, buffer) (review 3,
// finding 20: oam's fs.read called back with the error alone).
const fd = fs.openSync(f, "r");
fs.closeSync(fd);
for (const [name, op] of [
  ["read on a closed fd", (cb) => fs.read(fd, Buffer.alloc(2), 0, 2, 0, cb)],
  ["write on a closed fd", (cb) => fs.write(fd, Buffer.from("xy"), 0, 2, 0, cb)],
]) {
  await new Promise((resolve) =>
    op(function (...args) {
      console.log(name, args.length, args[0] && args[0].code, args[1], Buffer.isBuffer(args[2]));
      resolve();
    }),
  );
}

fs.rmSync(dir, { recursive: true, force: true });
