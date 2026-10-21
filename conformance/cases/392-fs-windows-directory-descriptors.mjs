// A directory opens on Windows, as it does under open(2) (#284). libuv asks
// CreateFileW for FILE_FLAG_BACKUP_SEMANTICS on every path, so node hands out
// a descriptor for a directory opened `r`, `r+` or `a`: fstat of it is a
// directory, close closes it, and what fails is the read or the write --
// `EISDIR: illegal operation on a directory, read` / `..., write`, with no
// path, or EBADF on a descriptor not opened for that direction. `w` is still
// EISDIR on the open and an exclusive open EEXIST, both naming the path. The
// whole-file forms follow from the same open: `appendFileSync(dir, "")` opens,
// writes nothing and succeeds, and `truncateSync(dir)` fails on the ftruncate.
// An open directory descriptor does not pin the name: the directory can be
// renamed or removed under it.
//
// Regression guard: oam opened through std without that flag, where Windows
// opens no directory at all. Every open node allows failed `EPERM: operation
// not permitted, open`, `appendFileSync(dir, "")` failed `EISDIR ..., write`,
// `openSync(dir, "ax")` was EPERM instead of EEXIST, and a stream over a
// directory failed before its 'open'.
//
// Windows-only by design: on POSIX the same calls answer from open(2)'s own
// rules. Elsewhere the case prints one line so node == oam still holds.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

if (process.platform !== "win32") {
  console.log("windows-only");
  process.exit(0);
}

const start = process.cwd();
const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-392-"));
process.chdir(made);
const D = process.cwd();
// Whichever row throws, the directory goes.
process.on("exit", () => {
  try {
    process.chdir(start);
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});
const root = D.slice(0, 3);
const share = "\\\\localhost\\" + D[0] + "$";
const scrub = (s) =>
  typeof s === "string"
    ? s
        .split(D).join("<D>")
        .split(made).join("<D>")
        .split(process.env.SystemRoot).join("<SYSTEM>")
        .split(share).join("<SHARE>")
        .split(root).join("<ROOT>")
    : s;
const shape = (e) =>
  [e.code, e.errno, e.syscall, "path=" + JSON.stringify(scrub(e.path)), scrub(e.message)].join(" ");
const value = (r) => "OK" + (r === undefined ? "" : " " + JSON.stringify(scrub(r)));
const show = (label, f) => {
  let line;
  try {
    line = value(f());
  } catch (e) {
    line = shape(e);
  }
  console.log(label + ": " + line);
};
// A row whose errno the host itself varies between runs: only the refusal,
// its syscall and the path it names are stable enough to compare (both of
// those are the same whichever errno the open came back with; code, errno
// and message are not).
const showRefused = (label, f) => {
  let line;
  try {
    f();
    line = "opened?!";
  } catch (e) {
    line = "refused " + [e.syscall, "path=" + JSON.stringify(scrub(e.path))].join(" ");
  }
  console.log(label + ": " + line);
};
const later = async (label, f) => {
  let line;
  try {
    line = value(await f());
  } catch (e) {
    line = shape(e);
  }
  console.log(label + ": " + line);
};
const called = (f) => new Promise((res, rej) => f((e, ...v) => (e ? rej(e) : res(v.length > 1 ? v : v[0]))));
// What a descriptor is, never its number.
const kind = (fd) => {
  const st = fs.fstatSync(fd);
  return (st.isDirectory() ? "dir" : "not-dir") + (st.isFile() ? " file" : "");
};

fs.mkdirSync("d");
fs.writeFileSync("d/a.txt", "a");
fs.mkdirSync("d/inner");
fs.mkdirSync("bare");

// --- openSync: which flags open a directory -----------------------------------
const spellings = [
  ["d", "d"],
  ["d\\", "d\\"],
  ["d/", "d/"],
  ["d/inner", "d/inner"],
  ["bare", "bare"],
  ["absolute", path.join(D, "d")],
  ["verbatim", "\\\\?\\" + path.join(D, "d")],
  [".", "."],
];
const flags = ["r", "rs", "r+", "rs+", "a", "a+", "as", "as+", "ax", "ax+", "w", "w+", "wx", "wx+"];
for (const [name, p] of spellings) {
  for (const flag of flags) {
    show("openSync " + name + " " + flag, () => {
      const fd = fs.openSync(p, flag);
      const k = kind(fd);
      fs.closeSync(fd);
      return k;
    });
  }
}
show("openSync d O_RDONLY", () => {
  const fd = fs.openSync("d", fs.constants.O_RDONLY);
  const k = kind(fd);
  fs.closeSync(fd);
  return k;
});
show("openSync d O_RDWR", () => {
  const fd = fs.openSync("d", fs.constants.O_RDWR);
  const k = kind(fd);
  fs.closeSync(fd);
  return k;
});

// --- the descriptor, sync -----------------------------------------------------
const buf8 = Buffer.alloc(8);
for (const flag of ["r", "r+", "a"]) {
  const fd = fs.openSync("d", flag);
  const row = (label, f) => show(flag + " " + label, f);
  row("typeof fd", () => typeof fd);
  row("fstatSync", () => {
    const st = fs.fstatSync(fd);
    return [st.isDirectory(), st.isFile(), st.isSymbolicLink(), st.size, (st.mode & 0o170000).toString(8)];
  });
  row("fstatSync bigint size", () => String(fs.fstatSync(fd, { bigint: true }).size));
  row("readSync empty buffer", () => fs.readSync(fd, Buffer.alloc(0)));
  row("readSync length 0", () => fs.readSync(fd, buf8, 0, 0, null));
  row("readSync", () => fs.readSync(fd, buf8, 0, 8, null));
  row("readSync at 0", () => fs.readSync(fd, buf8, 0, 8, 0));
  row("writeSync empty buffer", () => fs.writeSync(fd, Buffer.alloc(0)));
  row("writeSync buffer", () => fs.writeSync(fd, Buffer.from("x")));
  row("writeSync buffer at 0", () => fs.writeSync(fd, Buffer.from("x"), 0, 1, 0));
  row("writeSync empty string", () => fs.writeSync(fd, ""));
  row("writeSync string", () => fs.writeSync(fd, "x"));
  row("readvSync", () => fs.readvSync(fd, [Buffer.alloc(4)]));
  row("readvSync empty buffer", () => fs.readvSync(fd, [Buffer.alloc(0)]));
  row("writevSync", () => fs.writevSync(fd, [Buffer.from("x")]));
  row("writevSync empty buffer", () => fs.writevSync(fd, [Buffer.alloc(0)]));
  row("ftruncateSync", () => fs.ftruncateSync(fd));
  row("ftruncateSync 4", () => fs.ftruncateSync(fd, 4));
  row("fsyncSync", () => fs.fsyncSync(fd));
  row("fdatasyncSync", () => fs.fdatasyncSync(fd));
  row("fchmodSync 777", () => fs.fchmodSync(fd, 0o777));
  row("fchmodSync 444", () => fs.fchmodSync(fd, 0o444));
  row("fchownSync", () => fs.fchownSync(fd, 0, 0));
  row("futimesSync", () => {
    fs.futimesSync(fd, 1700000000, 1700000001);
    const st = fs.fstatSync(fd);
    return [st.atimeMs, st.mtimeMs];
  });
  row("readFileSync fd", () => fs.readFileSync(fd).length);
  row("readFileSync fd utf8", () => fs.readFileSync(fd, "utf8"));
  row("writeFileSync fd empty", () => fs.writeFileSync(fd, ""));
  row("writeFileSync fd", () => fs.writeFileSync(fd, "x"));
  row("appendFileSync fd empty", () => fs.appendFileSync(fd, ""));
  row("appendFileSync fd", () => fs.appendFileSync(fd, "x"));
  row("closeSync", () => fs.closeSync(fd));
  row("closeSync again", () => fs.closeSync(fd));
  row("fstatSync closed", () => fs.fstatSync(fd).size);
}
show("the directory is as it was", () => fs.readdirSync("d").sort());

// --- the descriptor, callback -------------------------------------------------
for (const flag of ["r", "r+", "a", "ax", "w"]) {
  await later("fs.open " + flag, async () => {
    const fd = await called((cb) => fs.open("d", flag, cb));
    const k = kind(fd);
    fs.closeSync(fd);
    return k;
  });
}
for (const flag of ["r", "r+", "a"]) {
  const fd = await called((cb) => fs.open("d", flag, cb));
  const row = (label, f) => later("callback " + flag + " " + label, () => called(f));
  await later("callback " + flag + " fstat", async () => (await called((cb) => fs.fstat(fd, cb))).isDirectory());
  await row("read length 0", (cb) => fs.read(fd, buf8, 0, 0, null, (e, n) => cb(e, n)));
  await row("read", (cb) => fs.read(fd, buf8, 0, 8, null, (e, n) => cb(e, n)));
  await row("write buffer", (cb) => fs.write(fd, Buffer.from("x"), (e, n) => cb(e, n)));
  await row("write string", (cb) => fs.write(fd, "x", (e, n) => cb(e, n)));
  await row("readv", (cb) => fs.readv(fd, [Buffer.alloc(4)], (e, n) => cb(e, n)));
  await row("writev", (cb) => fs.writev(fd, [Buffer.from("x")], (e, n) => cb(e, n)));
  await row("ftruncate", (cb) => fs.ftruncate(fd, cb));
  await row("fsync", (cb) => fs.fsync(fd, cb));
  await row("fdatasync", (cb) => fs.fdatasync(fd, cb));
  await row("fchmod", (cb) => fs.fchmod(fd, 0o777, cb));
  await row("fchown", (cb) => fs.fchown(fd, 0, 0, cb));
  await row("futimes", (cb) => fs.futimes(fd, 1700000000, 1700000001, cb));
  // The callback forms issue their one write even for no data.
  await row("writeFile fd empty", (cb) => fs.writeFile(fd, "", cb));
  await row("writeFile fd", (cb) => fs.writeFile(fd, "x", cb));
  await row("appendFile fd empty", (cb) => fs.appendFile(fd, "", cb));
  await row("appendFile fd", (cb) => fs.appendFile(fd, "x", cb));
  await row("close", (cb) => fs.close(fd, cb));
}

// --- the descriptor, fs.promises ----------------------------------------------
for (const flag of ["r", "r+", "rs+", "a", "a+", "as", "ax", "wx", "w", "w+"]) {
  await later("promises.open " + flag, async () => {
    const fh = await fs.promises.open("d", flag);
    const out = [typeof fh.fd, (await fh.stat()).isDirectory()];
    await fh.close();
    return out;
  });
}
for (const flag of ["r", "r+", "a"]) {
  const fh = await fs.promises.open("d", flag);
  const row = (label, f) => later("FileHandle " + flag + " " + label, f);
  await row("stat", async () => {
    const st = await fh.stat();
    return [st.isDirectory(), st.isFile(), st.size];
  });
  await row("read length 0", async () => (await fh.read(buf8, 0, 0, null)).bytesRead);
  await row("read", async () => (await fh.read(buf8, 0, 8, null)).bytesRead);
  await row("write empty buffer", async () => (await fh.write(Buffer.alloc(0))).bytesWritten);
  await row("write buffer", async () => (await fh.write(Buffer.from("x"))).bytesWritten);
  await row("write empty string", async () => (await fh.write("")).bytesWritten);
  await row("write string", async () => (await fh.write("x")).bytesWritten);
  await row("readv", async () => (await fh.readv([Buffer.alloc(4)])).bytesRead);
  await row("writev", async () => (await fh.writev([Buffer.from("x")])).bytesWritten);
  await row("readFile", async () => (await fh.readFile()).length);
  await row("readFile utf8", () => fh.readFile("utf8"));
  await row("writeFile empty", () => fh.writeFile(""));
  await row("writeFile", () => fh.writeFile("x"));
  await row("appendFile empty", () => fh.appendFile(""));
  await row("appendFile", () => fh.appendFile("x"));
  await row("truncate", () => fh.truncate());
  await row("truncate 4", () => fh.truncate(4));
  await row("sync", () => fh.sync());
  await row("datasync", () => fh.datasync());
  await row("chmod", () => fh.chmod(0o777));
  await row("chown", () => fh.chown(0, 0));
  await row("utimes", () => fh.utimes(1700000000, 1700000001));
  await row("close", () => fh.close());
  await row("fd after close", () => fh.fd);
  await row("close again", () => fh.close());
}

// --- a path that is a directory, whole-file forms -----------------------------
// A truncating open collides with the name before the directory's ACL is
// asked: one the caller may not write is EISDIR too, not EPERM.
const system = process.env.SystemRoot;
for (const flag of ["w", "w+", "wx"]) {
  show("openSync SystemRoot " + flag, () => fs.openSync(system, flag));
}
show("writeFileSync SystemRoot", () => fs.writeFileSync(system, "x"));
await later("promises.writeFile SystemRoot", () => fs.promises.writeFile(system, "x"));
await later("fs.writeFile SystemRoot empty", () => called((cb) => fs.writeFile(system, "", cb)));
for (const [name, p] of [["d", "d"], ["d\\", "d\\"], ["verbatim", "\\\\?\\" + path.join(D, "d")], ["bare", "bare"]]) {
  const row = (label, f) => show(label + " " + name, f);
  row("readFileSync", () => fs.readFileSync(p));
  row("readFileSync utf8", () => fs.readFileSync(p, "utf8"));
  row("readFileSync flag r+", () => fs.readFileSync(p, { flag: "r+" }));
  row("writeFileSync empty", () => fs.writeFileSync(p, ""));
  row("writeFileSync", () => fs.writeFileSync(p, "x"));
  row("writeFileSync empty flag a", () => fs.writeFileSync(p, "", { flag: "a" }));
  row("writeFileSync flag r+", () => fs.writeFileSync(p, "x", { flag: "r+" }));
  row("writeFileSync empty flag r+", () => fs.writeFileSync(p, "", { flag: "r+" }));
  row("appendFileSync empty", () => fs.appendFileSync(p, ""));
  row("appendFileSync empty buffer", () => fs.appendFileSync(p, Buffer.alloc(0)));
  row("appendFileSync", () => fs.appendFileSync(p, "x"));
  row("truncateSync", () => fs.truncateSync(p));
}
await later("promises.readFile d", () => fs.promises.readFile("d"));
await later("promises.writeFile d empty", () => fs.promises.writeFile("d", ""));
await later("promises.writeFile d", () => fs.promises.writeFile("d", "x"));
await later("promises.appendFile d empty", () => fs.promises.appendFile("d", ""));
await later("promises.appendFile d", () => fs.promises.appendFile("d", "x"));
await later("promises.truncate d", () => fs.promises.truncate("d"));
await later("fs.readFile d", () => called((cb) => fs.readFile("d", cb)));
await later("fs.writeFile d", () => called((cb) => fs.writeFile("d", "x", cb)));
await later("fs.appendFile d", () => called((cb) => fs.appendFile("d", "x", cb)));
await later("fs.appendFile d empty", () => called((cb) => fs.appendFile("d", "", cb)));
await later("fs.appendFile d empty buffer", () => called((cb) => fs.appendFile("d", Buffer.alloc(0), cb)));
await later("fs.writeFile d empty flag a", () => called((cb) => fs.writeFile("d", "", { flag: "a" }, cb)));
await later("fs.writeFile d empty flag r+", () => called((cb) => fs.writeFile("d", "", { flag: "r+" }, cb)));
await later("fs.writeFile d empty", () => called((cb) => fs.writeFile("d", "", cb)));
// A file, where the empty write has nothing to refuse it.
await later("fs.appendFile file empty", async () => {
  await called((cb) => fs.appendFile("d/a.txt", "", cb));
  await called((cb) => fs.appendFile("made.txt", "", cb));
  await called((cb) => fs.writeFile("made-w.txt", "", cb));
  const out = [fs.readFileSync("d/a.txt", "utf8"), fs.statSync("made.txt").size, fs.statSync("made-w.txt").size];
  fs.unlinkSync("made.txt");
  fs.unlinkSync("made-w.txt");
  return out;
});
// An options object is taken as given: one without a flag still opens "w"
// (or "a"), creating the file and truncating it.
for (const [name, options] of [["{}", {}], ["encoding", { encoding: "utf8" }], ["mode", { mode: 0o666 }], ["flush", { flush: false }], ["flag undefined", { flag: undefined }], ["string", "utf8"], ["null", null]]) {
  await later("fs.writeFile empty, options " + name, async () => {
    fs.writeFileSync("old.txt", "old");
    await called((cb) => fs.writeFile("old.txt", "", options, cb));
    await called((cb) => fs.writeFile("new.txt", "", options, cb));
    const out = [fs.readFileSync("old.txt", "utf8"), fs.existsSync("new.txt")];
    fs.unlinkSync("new.txt");
    return out;
  });
  await later("fs.appendFile empty, options " + name, async () => {
    fs.writeFileSync("old.txt", "old");
    await called((cb) => fs.appendFile("old.txt", Buffer.alloc(0), options, cb));
    await called((cb) => fs.appendFile("new.txt", "", options, cb));
    const out = [fs.readFileSync("old.txt", "utf8"), fs.existsSync("new.txt")];
    fs.unlinkSync("new.txt");
    return out;
  });
}
await later("fs.writeFile empty, flag wx on a file", () => called((cb) => fs.writeFile("old.txt", "", { flag: "wx" }, cb)));
await later("fs.writeFile empty, flag r on a file", () => called((cb) => fs.writeFile("old.txt", "", { flag: "r" }, cb)));
await later("fs.writeFile empty, missing parent", () => called((cb) => fs.writeFile("no-such/new.txt", "", cb)));
// A signal that has not fired changes nothing: the write is still issued.
for (const [name, f] of [
  ["fs.writeFile empty, flag r", (signal, cb) => fs.writeFile("old.txt", "", { flag: "r", signal }, cb)],
  ["fs.writeFile empty on d, flag a", (signal, cb) => fs.writeFile("d", "", { flag: "a", signal }, cb)],
  ["fs.appendFile empty on d", (signal, cb) => fs.appendFile("d", "", { signal }, cb)],
  ["fs.appendFile empty buffer on d", (signal, cb) => fs.appendFile("d", Buffer.alloc(0), { signal }, cb)],
  ["fs.appendFile empty on a file", (signal, cb) => fs.appendFile("old.txt", "", { signal }, cb)],
]) {
  await later(name + ", live signal", () => called((cb) => f(new AbortController().signal, cb)));
}
await later("fs.writeFile fd empty on a read-only file", async () => {
  const fd = fs.openSync("d/a.txt", "r");
  try {
    return await called((cb) => fs.writeFile(fd, "", cb));
  } finally {
    fs.closeSync(fd);
  }
});
await later("fs.truncate d", () => called((cb) => fs.truncate("d", cb)));
show("the directory is still as it was", () => fs.readdirSync("d").sort());

// --- streams ------------------------------------------------------------------
const events = (label, make) =>
  new Promise((res) => {
    const seen = [];
    const stream = make(seen);
    for (const name of ["open", "ready", "finish", "end"]) {
      stream.on(name, (v) => seen.push(name === "open" ? "open(" + typeof v + ")" : name));
    }
    stream.on("data", () => seen.push("data"));
    stream.on("error", (e) => seen.push("error " + shape(e)));
    stream.on("close", () => {
      console.log(label + ": " + seen.concat("close").join(", "));
      res();
    });
  });
await events("createReadStream d", () => fs.createReadStream("d"));
await events("createReadStream d flags r+", () => fs.createReadStream("d", { flags: "r+" }));
await events("createReadStream d start end", () => fs.createReadStream("d", { start: 0, end: 3 }));
await events("createReadStream fd", () => fs.createReadStream(null, { fd: fs.openSync("d", "r") }));
await events("createWriteStream d", () => {
  const w = fs.createWriteStream("d");
  w.end("x");
  return w;
});
await events("createWriteStream d flags a, end(x)", () => {
  const w = fs.createWriteStream("d", { flags: "a" });
  w.end("x");
  return w;
});
await events("createWriteStream d flags a, write(x)", (seen) => {
  const w = fs.createWriteStream("d", { flags: "a" });
  w.write("x", (e) => seen.push("write callback " + (e ? shape(e) : "OK")));
  return w;
});
await events("createWriteStream fd, end(x)", () => {
  const w = fs.createWriteStream(null, { fd: fs.openSync("d", "a") });
  w.end("x");
  return w;
});

// --- an open descriptor does not pin the directory -----------------------------
show("two descriptors on one directory", () => {
  const a = fs.openSync("d", "r");
  const b = fs.openSync("d", "r+");
  const c = fs.openSync("d", "a");
  const out = [a !== b, b !== c, kind(a), kind(b), kind(c)];
  for (const fd of [a, b, c]) fs.closeSync(fd);
  return out;
});
for (const flag of ["r", "r+", "a"]) {
  show("rename under a descriptor " + flag, () => {
    fs.mkdirSync("moving");
    const fd = fs.openSync("moving", flag);
    fs.renameSync("moving", "moved");
    const out = [fs.existsSync("moving"), fs.existsSync("moved"), kind(fd)];
    fs.closeSync(fd);
    fs.rmdirSync("moved");
    return out;
  });
  show("rmdir under a descriptor " + flag, () => {
    fs.mkdirSync("going");
    const fd = fs.openSync("going", flag);
    fs.rmdirSync("going");
    const out = [fs.existsSync("going"), kind(fd)];
    fs.mkdirSync("going");
    fs.closeSync(fd);
    out.push(fs.existsSync("going"));
    fs.rmdirSync("going");
    return out;
  });
  show("children under a descriptor " + flag, () => {
    const fd = fs.openSync("bare", flag);
    fs.writeFileSync("bare/f.txt", "f");
    fs.renameSync("bare/f.txt", "bare/g.txt");
    const listed = fs.readdirSync("bare");
    fs.unlinkSync("bare/g.txt");
    fs.mkdirSync("bare/sub");
    fs.rmdirSync("bare/sub");
    fs.closeSync(fd);
    return listed;
  });
}

// --- a share that is no disk has no root to open ------------------------------
if (fs.existsSync(share)) {
  for (const ipc of ["\\\\localhost\\IPC$", "\\\\localhost\\IPC$\\"]) {
    for (const flag of ["r", "r+", "a", "w"]) {
      show("openSync " + ipc + " " + flag, () => fs.openSync(ipc, flag));
    }
    // An exclusive create answers with whichever errno the SMB server has for
    // it on this attempt: measured 4 flips between EINVAL and EEXIST in ~272
    // runs of node alone -- same binary, same flags, both answers. The two
    // sides issue the same CREATE_NEW, so what the share reports depends on
    // the host's own IPC$ session state (services open and close one in the
    // background), not on either runtime: only the refusal is compared here.
    // The full errno shape stays covered by every flag above and by the disk
    // paths below, where the server's answer does not vary.
    for (const flag of ["ax", "wx"]) {
      showRefused("openSync " + ipc + " " + flag, () => fs.openSync(ipc, flag));
    }
    show("readFileSync " + ipc, () => fs.readFileSync(ipc));
    show("writeFileSync " + ipc, () => fs.writeFileSync(ipc, ""));
    show("appendFileSync " + ipc, () => fs.appendFileSync(ipc, ""));
    await later("promises.open " + ipc, () => fs.promises.open(ipc, "r"));
    await later("fs.appendFile " + ipc, () => called((cb) => fs.appendFile(ipc, "", cb)));
  }
}

// --- a drive or share root is a directory too ---------------------------------
// Read-only: whether a root opens for writing is the caller's privilege.
const roots = [root, "\\", "/"];
if (fs.existsSync(share)) roots.push(share, share + "\\");
for (const r of roots) {
  const label = JSON.stringify(scrub(r));
  show("openSync root " + label, () => {
    const fd = fs.openSync(r, "r");
    const out = [kind(fd), fs.fstatSync(fd).size, fs.statSync(r).size];
    try {
      fs.readSync(fd, buf8, 0, 8, null);
      out.push("read");
    } catch (e) {
      out.push(shape(e));
    }
    fs.closeSync(fd);
    return out;
  });
  await later("promises.open root " + label, async () => {
    const fh = await fs.promises.open(r, "r");
    const isDir = (await fh.stat()).isDirectory();
    await fh.close();
    return isDir;
  });
}
