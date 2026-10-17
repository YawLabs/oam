// fs.ReadStream and fs.WriteStream with no `options.fs` go through the
// LIVE fs.open, fs.read, fs.write, fs.writev, fs.fsync and fs.close, as
// node's do (#356): a function assigned over one of them is the one the
// stream calls, from the next stream on, and putting the original back
// stops it.
//
// Regression guard: oam's streams opened, read, wrote and closed natively,
// so a patched fs.open (graceful-fs, mock-fs and test spies patch it) was
// never called.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-531-")));
process.chdir(root);
const names = ["open", "read", "write", "writev", "fsync", "close"];
const originals = Object.fromEntries(names.map((n) => [n, fs[n]]));
const log = [];
const fds = new Set();
const tag = (fd) => (fds.has(fd) ? "FD" : typeof fd);
const len = (v) => (Array.isArray(v) ? `[${v.map((b) => b.length)}]` : v.length);
fs.open = function (p, flags, mode, cb) {
  log.push(`open ${path.basename(p)} ${flags} ${mode}`);
  originals.open(p, flags, mode, (e, fd) => { if (!e) fds.add(fd); cb(e, fd); });
};
fs.read = function (fd, buf, off, n, pos, cb) {
  log.push(`read ${tag(fd)} ${buf.length} ${off} ${n} ${pos}`);
  originals.read(fd, buf, off, n, pos, cb);
};
fs.write = function (fd, buf, off, n, pos, cb) {
  log.push(`write ${tag(fd)} ${len(buf)} ${off} ${n} ${pos}`);
  originals.write(fd, buf, off, n, pos, cb);
};
fs.writev = function (fd, bufs, pos, cb) {
  log.push(`writev ${tag(fd)} ${len(bufs)} ${pos}`);
  originals.writev(fd, bufs, pos, cb);
};
fs.fsync = function (fd, cb) {
  log.push(`fsync ${tag(fd)}`);
  originals.fsync(fd, cb);
};
fs.close = function (fd, cb) {
  log.push(`close ${tag(fd)}`);
  originals.close(fd, cb);
};
const closed = (s) => new Promise((resolve) => s.on("close", resolve));

const ws = fs.createWriteStream("d.txt", { flush: true });
await new Promise((resolve) => ws.write("pat", resolve));
ws.cork(); ws.write("ch"); ws.write("ed"); ws.uncork();
ws.end();
await closed(ws);
const rs = fs.createReadStream("d.txt", { start: 1 });
const got = [];
rs.on("data", (c) => got.push(c.toString()));
await closed(rs);
console.log("patched", JSON.stringify(log), JSON.stringify(got));

// A patch made after the stream was made still counts: node looks each
// method up when it calls it.
log.length = 0;
for (const n of names) fs[n] = originals[n];
const late = fs.createReadStream("d.txt");
const seenClose = [];
fs.close = function (fd, cb) {
  seenClose.push(typeof fd);
  originals.close(fd, cb);
};
late.resume();
await closed(late);
fs.close = originals.close;
console.log("late close patch", JSON.stringify(seenClose), JSON.stringify(log));

// The originals back: nothing recorded.
const quiet = fs.createWriteStream("e.txt");
quiet.end("x");
await closed(quiet);
console.log("restored", JSON.stringify(log), fs.readFileSync("e.txt", "utf8"));

process.chdir(os.tmpdir());
fs.rmSync(root, { recursive: true, force: true });
