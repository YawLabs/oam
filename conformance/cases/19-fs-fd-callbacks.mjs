// Classic fd-based callback fs ops: open -> read -> close, and the
// promisify(fs.open) pattern chokidar/graceful-fs rely on. Regression guard
// for the gap where fs.open was undefined (broke playwright-core's chokidar).
import fs from "node:fs";
import { promisify } from "node:util";
import os from "node:os";
import path from "node:path";

const open = promisify(fs.open);
const close = promisify(fs.close);
console.log("promisify-ok", typeof open === "function", typeof close === "function");

const tmp = path.join(os.tmpdir(), "oam-conf-fdtest.txt");
const content = "fd round-trip line\n".repeat(10);
fs.writeFileSync(tmp, content);

const fd = await open(tmp, "r");
console.log("fd-type", typeof fd);
const buf = Buffer.alloc(content.length);
const bytesRead = await new Promise((res, rej) =>
  fs.read(fd, buf, 0, buf.length, 0, (e, n) => (e ? rej(e) : res(n))),
);
await close(fd);
console.log("read", bytesRead === content.length, buf.toString("utf8", 0, bytesRead) === content);
fs.unlinkSync(tmp);
