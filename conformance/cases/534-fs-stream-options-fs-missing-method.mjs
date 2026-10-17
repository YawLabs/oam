// A file stream whose `options.fs` lacks a method it reaches for after the
// constructor (#356): node calls `stream[kFs].<method>(...)` regardless, so
// the call fails with a TypeError naming it, and the descriptor
// `options.fs.open` handed back is left alone.
//
// Regression guard: oam treated a missing `options.fs` method like one of
// its own and fell back to the native, so a stream with autoClose: false
// and no `options.fs.close` closed, natively, whatever real descriptor had
// the number the custom fs gave out.
//
// The constructor validates close only when autoClose is true, fsync only
// when flush is true, and checks nothing again later; each row reaches a
// method the constructor did not check.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-534-")));
process.chdir(root);
fs.writeFileSync("victim.txt", "victim-data");
const row = (label, ...rest) => console.log(label, JSON.stringify(rest));
const victimOpen = (fd) => {
  try {
    return fs.readSync(fd, Buffer.alloc(6), 0, 6, 0);
  } catch (e) {
    return e.code;
  }
};
const closed = (s) => new Promise((resolve) => s.on("close", resolve));
const errors = (s, log) => s.on("error", (e) => log.push(`${e.name}: ${e.message}`));

// A read stream with autoClose: false and no close, destroyed: the close
// is `stream[kFs].close`, which is not a function.
{
  const vfd = fs.openSync("victim.txt", "r");
  const log = [];
  const vfs = {
    open: (p, fl, m, cb) => setImmediate(cb, null, vfd),
    read: (fd, b, o, l, p, cb) => setImmediate(cb, null, 0, b),
  };
  const rs = fs.createReadStream("/virtual/file", { autoClose: false, fs: vfs });
  errors(rs, log);
  rs.on("open", () => rs.destroy());
  await closed(rs);
  row("read-no-close", log, victimOpen(vfd), rs.fd === vfd);
  try { fs.closeSync(vfd); } catch {}
}

// The same, with autoClose turned on after the constructor.
{
  const vfd = fs.openSync("victim.txt", "r");
  const log = [];
  const vfs = {
    open: (p, fl, m, cb) => setImmediate(cb, null, vfd),
    read: (fd, b, o, l, p, cb) => setImmediate(cb, null, 0, b),
  };
  const rs = fs.createReadStream("/virtual/file", { autoClose: false, fs: vfs });
  rs.autoClose = true;
  errors(rs, log);
  rs.on("open", () => rs.destroy());
  await closed(rs);
  row("read-autoclose-later", log, victimOpen(vfd));
  try { fs.closeSync(vfd); } catch {}
}

// A write stream with autoClose: false and no close, destroyed.
{
  const vfd = fs.openSync("victim.txt", "r");
  const log = [];
  const vfs = {
    open: (p, fl, m, cb) => setImmediate(cb, null, vfd),
    write: (fd, b, o, l, p, cb) => setImmediate(cb, null, l, b),
  };
  const ws = fs.createWriteStream("/virtual/file", { autoClose: false, fs: vfs });
  errors(ws, log);
  ws.on("open", () => ws.destroy());
  await closed(ws);
  row("write-no-close", log, victimOpen(vfd), ws.fd === vfd);
  try { fs.closeSync(vfd); } catch {}
}

// A write stream given flush after the constructor, with no fsync: the
// sync is `stream[kFs].fsync`, and the close is never reached.
{
  const vfd = fs.openSync("victim.txt", "r");
  const log = [];
  const vfs = {
    open: (p, fl, m, cb) => setImmediate(cb, null, vfd),
    write: (fd, b, o, l, p, cb) => setImmediate(cb, null, l, b),
    close: (fd, cb) => { log.push("close"); setImmediate(cb, null); },
  };
  const ws = fs.createWriteStream("/virtual/file", { fs: vfs });
  ws.flush = true;
  errors(ws, log);
  ws.on("open", () => ws.destroy());
  await closed(ws);
  row("write-flush-no-fsync", log, victimOpen(vfd));
  try { fs.closeSync(vfd); } catch {}
}

// open taken away after the constructor: `stream[kFs].open` fails the
// construction.
{
  const log = [];
  const vfs = {
    open: (p, fl, m, cb) => setImmediate(cb, null, 99),
    read: (fd, b, o, l, p, cb) => setImmediate(cb, null, 0, b),
    close: (fd, cb) => { log.push(`close ${fd}`); setImmediate(cb, null); },
  };
  const rs = fs.createReadStream("/virtual/file", { fs: vfs });
  delete vfs.open;
  errors(rs, log);
  await closed(rs);
  row("open-removed", log, rs.fd);
}

process.chdir(os.tmpdir());
fs.rmSync(root, { recursive: true, force: true });
