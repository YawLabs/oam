// fs.close's result for a descriptor that is not open.
//
// node reports EBADF to fs.close's callback for a descriptor that was never
// opened or is already closed, the error closeSync throws; with no callback
// its default one throws it, an uncaught exception. oam called back null for
// both (fs.close went through the streams' forgiving close) and swallowed
// the failure when no callback was passed.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const shape = (e) =>
  `${e.constructor.name} ${e.code} ${e.syscall} ${JSON.stringify(e.message)} ${JSON.stringify(Object.keys(e))}`;
const uncaught = [];
process.on("uncaughtException", (e) => uncaught.push(shape(e)));

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-259-"));
const file = path.join(dir, "f.txt");
fs.writeFileSync(file, "x");

function close(label, fd) {
  return new Promise((resolve) => {
    let sync = true;
    fs.close(fd, (...args) => {
      console.log(label, sync ? "sync" : "async", args.length, args[0] ? shape(args[0]) : args[0]);
      resolve();
    });
    sync = false;
  });
}

await close("close(never opened, cb)", 987654);
const fd = fs.openSync(file, "r");
await close("close(fd, cb)", fd);
await close("close(fd, cb) again", fd);
try {
  fs.closeSync(fd);
} catch (e) {
  console.log("closeSync(fd) again", shape(e));
}
console.log("close.length", fs.close.length, fs.close.name);

// No callback: success is silent, a failure is an uncaught exception.
fs.close(fs.openSync(file, "r"));
fs.close(987654);
console.log("returned", fs.close(fs.openSync(file, "r")));
await new Promise((resolve) => setTimeout(resolve, 20));
console.log("uncaught", uncaught.length, uncaught.join(" | "));

fs.rmSync(dir, { recursive: true, force: true });
