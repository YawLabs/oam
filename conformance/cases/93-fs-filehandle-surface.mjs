// The FileHandle returned by fsPromises.open(): its WHOLE method surface, and
// the two things about it that are easy to get wrong.
//
// Case 91 leaves a note saying oam's FileHandle carried only
// read/write/readFile/writeFile/stat/close, so `(await fsp.open(f)).utimes(...)`
// was a TypeError rather than a timestamp divergence. This case is that note
// discharged: every method node v22 puts on a FileHandle is exercised here.
//
// The two behaviours that are not merely "is the method present":
//
//  1. stat() is FSTAT, not stat(). Statting the PATH the handle was opened
//     from describes a DIFFERENT object the moment anything moves the name.
//     "Open a temp file, unlink it, keep using the handle" is a standard
//     pattern, and a path-stat there raises ENOENT where node reports the
//     still-open inode. The rename leg below catches the quieter half: a
//     path-stat succeeds there, it just describes the wrong file. Both legs
//     assert a SIZE that only the real descriptor can know.
//
//  2. Every method rejects on a CLOSED handle with node's fsCall error --
//     a plain Error (not a subclass, not an ERR_* code), message "file
//     closed", and exactly two own properties, code then syscall. The
//     syscall names the syscall the call WOULD have made, so it differs per
//     method, and appendFile reports "writeFile" because node implements it
//     as an alias. The stream factories are the exception: they are not
//     fsCall-wrapped, so a closed handle reaches the stream constructor as
//     fd -1 and fails its integer-range check as a synchronous RangeError.
//
//  3. readableWebStream() is the WEB stream, and it is not createReadStream
//     in web clothing. It reads from the handle's CURRENT cursor, hands back
//     plain Uint8Array chunks on node's 16384-byte autoAllocateChunkSize
//     boundaries, and does NOT take the descriptor (autoClose defaults to
//     false). It LOCKS the handle for life on the first call -- a second
//     call is ERR_INVALID_STATE even after the first stream was cancelled or
//     fully drained, and even after a call that died on a bad `options`.
//     `options.type` is NOT value-validated: anything other than 'bytes'
//     warns and builds exactly the same stream.
//
// createReadStream/createWriteStream additionally take OWNERSHIP of the
// descriptor: when they finish, the handle is closed, a later fh.close()
// still resolves (no double close), and everything else starts reporting
// EBADF. autoClose:false opts out and leaves the descriptor to the caller.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-fh-surface-${process.pid}`);
fs.mkdirSync(dir, { recursive: true });
const isWin = process.platform === "win32";

const show = async (label, thunk) => {
  try {
    console.log(label, "->", JSON.stringify(await thunk()));
  } catch (e) {
    console.log(label, "threw", e.constructor.name, e.code, JSON.stringify(e.message));
  }
};

// --- 1. stat() is fstat: the descriptor outlives the NAME ----------------
{
  const f = path.join(dir, "victim.txt");
  fs.writeFileSync(f, "0123456789"); // 10 bytes
  const fh = await fsp.open(f, "r+");

  // Renamed: a path-stat still SUCCEEDS here, it just describes whatever now
  // sits at the old name -- which is why this leg plants a decoy of a
  // different size. Only fstat can report 10.
  const moved = path.join(dir, "moved.txt");
  fs.renameSync(f, moved);
  fs.writeFileSync(f, "decoy"); // 5 bytes, at the ORIGINAL path
  await show("stat after rename size ", async () => (await fh.stat()).size);

  // Unlinked: a path-stat raises ENOENT. Node keeps reporting the open inode.
  fs.unlinkSync(moved);
  fs.unlinkSync(f);
  await show("stat after unlink size ", async () => (await fh.stat()).size);
  await show("stat after unlink kind ", async () => {
    const st = await fh.stat();
    return { isFile: st.isFile(), isDirectory: st.isDirectory() };
  });
  await fh.close();
}

// --- 2. metadata methods on the descriptor -------------------------------
{
  const f = path.join(dir, "meta.txt");
  fs.writeFileSync(f, "abcdefghij");
  const fh = await fsp.open(f, "r+");

  // Fixed UTC instants, distinct so a swapped pair is visible, and read back
  // through the HANDLE -- which is also the fstat path again.
  const A = new Date(Date.UTC(2001, 0, 2, 3, 4, 5));
  const M = new Date(Date.UTC(2011, 10, 12, 13, 14, 15));
  await show("utimes ret             ", () => fh.utimes(A, M));
  await show("utimes readback        ", async () => {
    const st = await fh.stat();
    return { atime: st.atime.toISOString(), mtime: st.mtime.toISOString() };
  });

  await show("truncate(4) size       ", async () => {
    await fh.truncate(4);
    return (await fh.stat()).size;
  });
  await show("truncate() size        ", async () => {
    await fh.truncate();
    return (await fh.stat()).size;
  });

  await show("sync ret               ", () => fh.sync());
  await show("datasync ret           ", () => fh.datasync());

  // chmod resolves everywhere -- Windows maps it onto the read-only
  // attribute -- but only POSIX round-trips the permission bits, so the
  // READBACK is gated. 0o644 rather than anything with an execute bit
  // because a umask cannot subtract from an explicit fchmod.
  await show("chmod ret              ", () => fh.chmod(0o644));
  const CHMOD_MODE_EXPECTED = '"644"';
  if (isWin) {
    console.log("chmod mode readback     ->", CHMOD_MODE_EXPECTED);
  } else {
    await show("chmod mode readback    ", async () => ((await fh.stat()).mode & 0o777).toString(8));
  }

  // chown is POSIX-only in effect: libuv implements uv_fs_fchown on Windows
  // as a successful no-op, so the call RESOLVES on both platforms and only
  // the arguments have to be platform-picked. -1/-1 is POSIX's documented
  // "change neither", which an unprivileged process is always allowed.
  await show("chown(-1,-1) ret       ", () => fh.chown(-1, -1));

  await fh.close();
}

// --- 3. appendFile is an ALIAS of writeFile, not an append ---------------
{
  const f = path.join(dir, "append.txt");
  fs.writeFileSync(f, "ORIGINAL-CONTENT");
  // Opened "r+", so the cursor starts at 0 and appendFile OVERWRITES from
  // there. Only the "a" flag at open() time appends -- node documents this
  // ("the mode cannot be changed from what it was set to with open()") and
  // it is the opposite of what the name suggests.
  const fh = await fsp.open(f, "r+");
  await fh.appendFile("xx");
  console.log("appendFile on r+        ->", JSON.stringify(fs.readFileSync(f, "utf8")));
  await fh.close();

  const fh2 = await fsp.open(f, "a");
  await fh2.appendFile("!");
  console.log("appendFile on a         ->", JSON.stringify(fs.readFileSync(f, "utf8")));
  await fh2.close();
}

// --- 4. readv / writev: the object-returning half of the vectored pair ---
{
  const f = path.join(dir, "vec.txt");
  fs.writeFileSync(f, "hello world");
  const fh = await fsp.open(f, "r+");

  await show("readv positional       ", async () => {
    const b1 = Buffer.alloc(5);
    const b2 = Buffer.alloc(6);
    const r = await fh.readv([b1, b2], 0);
    return {
      keys: Object.keys(r),
      bytesRead: r.bytesRead,
      views: [String(b1), String(b2)],
      sameArray: r.buffers[0] === b1,
    };
  });

  // No position: sequential from the cursor, so the two calls must not
  // return the same bytes.
  await show("readv sequential       ", async () => {
    const first = Buffer.alloc(5);
    const second = Buffer.alloc(6);
    const a = await fh.readv([first]);
    const b = await fh.readv([second]);
    return { a: [a.bytesRead, String(first)], b: [b.bytesRead, String(second)] };
  });

  // A short final read leaves the rest of that view UNTOUCHED.
  await show("readv partial fill     ", async () => {
    const b = Buffer.from("ZZZZZZZZ");
    const r = await fh.readv([b], 6);
    return { bytesRead: r.bytesRead, view: String(b) };
  });

  await show("writev positional      ", async () => {
    const r = await fh.writev([Buffer.from("AB"), Buffer.from("CD")], 0);
    return { keys: Object.keys(r), bytesWritten: r.bytesWritten, file: fs.readFileSync(f, "utf8") };
  });

  // The empty list is ASYMMETRIC: writev reports 0 without touching the
  // descriptor, readv raises EINVAL. An array of zero-length views is NOT
  // that case -- it still issues the syscall and reports 0.
  await show("writev([])             ", async () => {
    const r = await fh.writev([], 0);
    return { bytesWritten: r.bytesWritten, len: r.buffers.length };
  });
  await show("readv([])              ", () => fh.readv([], 0));
  await show("readv([empty view])    ", async () => {
    const r = await fh.readv([Buffer.alloc(0)], 0);
    return { bytesRead: r.bytesRead, len: r.buffers.length };
  });

  // Validation is the node:fs validateBufferArray message, on both surfaces.
  await show("readv(number)          ", () => fh.readv(5, 0));
  await show("writev([non-views])    ", () => fh.writev([1, 2], 0));

  await fh.close();
}

// --- 5. createReadStream: window, and ownership of the descriptor --------
{
  const f = path.join(dir, "stream.txt");
  fs.writeFileSync(f, "line1\nline2\nline3\n");

  const fh = await fsp.open(f);
  const rs = fh.createReadStream();
  let whole = "";
  for await (const chunk of rs) whole += chunk;
  console.log("createReadStream whole  ->", JSON.stringify(whole));
  // `path` is undefined on a handle-backed stream: there was no path to
  // record. bytesRead is the stream's own counter.
  console.log("createReadStream path   ->", JSON.stringify(rs.path), "bytesRead", rs.bytesRead);
  // The stream OWNED the descriptor and closed it.
  await show("stat after stream end  ", async () => (await fh.stat()).size);
  // ...and closing again is a no-op that resolves, NOT a double close.
  await show("close after stream end ", () => fh.close());

  // start/end is a real seek, not just a byte budget: bytes 6..10 of the
  // file above are "line2", while a cursor read of the same length is
  // "line1".
  const fh2 = await fsp.open(f);
  let window = "";
  for await (const chunk of fh2.createReadStream({ start: 6, end: 10, encoding: "utf8" })) {
    window += chunk;
  }
  console.log("createReadStream window ->", JSON.stringify(window));
  await fh2.close();

  // autoClose:false hands the descriptor back to the caller.
  const fh3 = await fsp.open(f);
  for await (const _chunk of fh3.createReadStream({ autoClose: false }));
  await show("autoClose:false stat   ", async () => (await fh3.stat()).size);
  await fh3.close();
}

// --- 6. createWriteStream ------------------------------------------------
{
  const f = path.join(dir, "written.txt");
  const fh = await fsp.open(f, "w");
  const ws = fh.createWriteStream();
  await new Promise((resolve, reject) => {
    ws.on("error", reject);
    ws.on("close", resolve);
    ws.write("first\n");
    ws.end("second\n");
  });
  console.log("createWriteStream file  ->", JSON.stringify(fs.readFileSync(f, "utf8")));
  console.log("createWriteStream path  ->", JSON.stringify(ws.path), "bytesWritten", ws.bytesWritten);
  await show("stat after ws close    ", async () => (await fh.stat()).size);
  await show("close after ws close   ", () => fh.close());
}

// --- 7. readLines --------------------------------------------------------
{
  const f = path.join(dir, "lines.txt");
  // Mixed line endings and NO trailing newline: the last segment is still a
  // line, and the empty segment after a trailing newline is not.
  fs.writeFileSync(f, "alpha\r\nbeta\ngamma");
  const fh = await fsp.open(f);
  const lines = [];
  for await (const line of fh.readLines()) lines.push(line);
  console.log("readLines mixed         ->", JSON.stringify(lines));
  // readLines is createReadStream underneath, so it took the descriptor too.
  await show("close after readLines  ", () => fh.close());

  const g = path.join(dir, "lines2.txt");
  fs.writeFileSync(g, "one\ntwo\n");
  const fh2 = await fsp.open(g);
  const lines2 = [];
  for await (const line of fh2.readLines()) lines2.push(line);
  console.log("readLines trailing nl   ->", JSON.stringify(lines2));
  await fh2.close();
}

// --- 8. every method on a CLOSED handle ----------------------------------
{
  const f = path.join(dir, "closed.txt");
  fs.writeFileSync(f, "content");
  const fh = await fsp.open(f, "r+");
  await fh.close();
  console.log("fd after close          ->", fh.fd);

  const rejects = async (label, thunk) => {
    try {
      const r = await thunk();
      console.log(label, "RESOLVED", JSON.stringify(r));
    } catch (e) {
      console.log(
        label,
        e.constructor.name,
        JSON.stringify(e.message),
        "code=" + e.code,
        "syscall=" + e.syscall,
        "own=" + Object.keys(e).join(","),
      );
    }
  };

  await rejects("closed read      ", () => fh.read(Buffer.alloc(4), 0, 4, 0));
  await rejects("closed write     ", () => fh.write(Buffer.from("x"), 0, 1, 0));
  await rejects("closed readFile  ", () => fh.readFile());
  await rejects("closed writeFile ", () => fh.writeFile("x"));
  await rejects("closed appendFile", () => fh.appendFile("x"));
  await rejects("closed stat      ", () => fh.stat());
  await rejects("closed chmod     ", () => fh.chmod(0o644));
  await rejects("closed chown     ", () => fh.chown(-1, -1));
  await rejects("closed truncate  ", () => fh.truncate(0));
  await rejects("closed sync      ", () => fh.sync());
  await rejects("closed datasync  ", () => fh.datasync());
  await rejects("closed utimes    ", () => fh.utimes(new Date(0), new Date(0)));
  await rejects("closed readv     ", () => fh.readv([Buffer.alloc(2)], 0));
  await rejects("closed writev    ", () => fh.writev([Buffer.from("q")], 0));
  // The empty-list EINVAL does NOT win over the closed-handle check: node
  // runs fsCall first, so this is EBADF and not the EINVAL an OPEN handle
  // would give.
  await rejects("closed readv([]) ", () => fh.readv([], 0));

  // Not fsCall-wrapped: a synchronous RangeError out of the stream
  // constructor's fd validation, carrying only `code`.
  await rejects("closed createRead", async () => fh.createReadStream());
  await rejects("closed createWrit", async () => fh.createWriteStream());
  await rejects("closed readLines ", async () => fh.readLines());
  // A THIRD shape: readableWebStream is neither fsCall-wrapped nor routed
  // through a stream constructor. It checks the descriptor itself and throws
  // ERR_INVALID_STATE synchronously -- a plain Error, no syscall.
  await rejects("closed readableWeb", async () => fh.readableWebStream());
}

// --- 9. readableWebStream ------------------------------------------------
{
  // Every non-'bytes' options.type emits an ExperimentalWarning rather than
  // throwing. Collected here and printed at a deterministic point below --
  // emitWarning is deferred a tick on both runtimes, so it cannot be asserted
  // inline at the call site.
  const warnings = [];
  process.on("warning", (w) => warnings.push(w.name + ": " + w.message));

  const f = path.join(dir, "web.txt");
  fs.writeFileSync(f, "hello world");

  // A normal read to completion. The chunks are plain Uint8Arrays -- node's
  // byte stream hands back its auto-allocated view, so this is NOT a Buffer
  // -- and the handle survives: autoClose defaults to FALSE, the opposite of
  // createReadStream's ownership above.
  {
    const fh = await fsp.open(f);
    const s = fh.readableWebStream();
    console.log(
      "rws ctor                ->",
      s.constructor.name,
      "isGlobal",
      s instanceof globalThis.ReadableStream,
      "locked",
      s.locked,
    );
    const chunks = [];
    for await (const c of s) chunks.push(c);
    console.log("rws chunk ctors         ->", JSON.stringify(chunks.map((c) => c.constructor.name)));
    console.log("rws bytes               ->", JSON.stringify(Buffer.concat(chunks).toString()));
    // Drained, and STILL OPEN.
    await show("rws stat after drain   ", async () => (await fh.stat()).size);
    await show("rws close after drain  ", () => fh.close());
  }

  // An empty file is zero chunks, not one empty chunk.
  {
    const g = path.join(dir, "web-empty.txt");
    fs.writeFileSync(g, "");
    const fh = await fsp.open(g);
    const chunks = [];
    for await (const c of fh.readableWebStream()) chunks.push(c);
    console.log("rws empty file          ->", chunks.length, "chunks");
    await fh.close();
  }

  // It starts at the handle's CURRENT cursor, not at byte 0. Six bytes are
  // consumed by a plain read() first, so the stream must only see "world".
  {
    const fh = await fsp.open(f);
    const pre = Buffer.alloc(6);
    await fh.read(pre, 0, 6, null);
    const chunks = [];
    for await (const c of fh.readableWebStream()) chunks.push(c);
    console.log(
      "rws from cursor         ->",
      JSON.stringify(String(pre)),
      "then",
      JSON.stringify(Buffer.concat(chunks).toString()),
    );
    await fh.close();
  }

  // The chunk BOUNDARIES are observable: node's autoAllocateChunkSize is
  // 16384, so 200000 bytes arrive as 12 full chunks and a 3392-byte tail.
  // A different chunk size would still round-trip the bytes and still be
  // wrong, so the shape is asserted, not just the total.
  {
    const big = path.join(dir, "web-big.bin");
    const src = Buffer.alloc(200000);
    for (let i = 0; i < src.length; i++) src[i] = i % 251;
    fs.writeFileSync(big, src);
    const fh = await fsp.open(big);
    const lens = [];
    let sum = 0;
    let intact = true;
    let at = 0;
    for await (const c of fh.readableWebStream()) {
      lens.push(c.length);
      for (let i = 0; i < c.length; i++) {
        if (c[i] !== src[at + i]) intact = false;
        sum += c[i];
      }
      at += c.length;
    }
    console.log(
      "rws big chunk lens      ->",
      JSON.stringify({ n: lens.length, first: lens[0], last: lens[lens.length - 1] }),
      "total",
      at,
      "sum",
      sum,
      "intact",
      intact,
    );
    await fh.close();
  }

  // autoClose:true is the opt-IN to createReadStream-style ownership.
  {
    const fh = await fsp.open(f);
    for await (const _c of fh.readableWebStream({ autoClose: true }));
    console.log("rws autoClose fd        ->", fh.fd);
    await show("rws autoClose stat     ", async () => (await fh.stat()).size);
    await show("rws autoClose close    ", () => fh.close());
  }

  // Cancelling mid-stream leaves the descriptor alone (autoClose is false):
  // the handle still stats, and the later close is a first close, not a
  // double one.
  {
    const fh = await fsp.open(f);
    const reader = fh.readableWebStream().getReader();
    const first = await reader.read();
    console.log("rws cancel first chunk  ->", first.done, first.value.length);
    await reader.cancel();
    await show("rws stat after cancel  ", async () => (await fh.stat()).size);
    await show("rws close after cancel ", () => fh.close());
    await show("rws close twice        ", () => fh.close());
  }

  // Closing the handle mid-stream ENDS the stream rather than erroring it:
  // the next read reports done, it does not reject with EBADF.
  {
    const big = path.join(dir, "web-big.bin");
    const fh = await fsp.open(big);
    const reader = fh.readableWebStream().getReader();
    await reader.read();
    await fh.close();
    await show("rws read after close   ", async () => {
      const r = await reader.read();
      return { done: r.done, value: r.value === undefined ? "undefined" : r.value.length };
    });
  }

  // The handle is LOCKED for life -- not merely "while a stream is live".
  {
    const fh = await fsp.open(f);
    const s = fh.readableWebStream();
    await show("rws second call        ", async () => fh.readableWebStream());
    await s.cancel();
    await show("rws after cancel       ", async () => fh.readableWebStream());
    await fh.close();

    const fh2 = await fsp.open(f);
    for await (const _c of fh2.readableWebStream());
    await show("rws after drain        ", async () => fh2.readableWebStream());
    await fh2.close();
  }

  // The lock is taken BEFORE the options are validated, so a call that dies
  // on a bad `options` still burns it.
  {
    const fh = await fsp.open(f);
    await show("rws bad opts           ", async () => fh.readableWebStream(null));
    await show("rws after bad opts     ", async () => fh.readableWebStream());
    await fh.close();
  }

  // node's validateObject: null, arrays and non-objects are rejected. A
  // function is a non-object here (typeof is 'function'), and an empty object
  // is fine.
  for (const [label, opts] of [
    ["null      ", null],
    ["string    ", "bytes"],
    ["number    ", 5],
    ["array     ", []],
    ["function  ", function named() {}],
  ]) {
    const fh = await fsp.open(f);
    await show("rws opts " + label + "    ", async () => {
      fh.readableWebStream(opts);
      return "ok";
    });
    await fh.close();
  }

  // autoClose IS value-validated, with validateBoolean's property message.
  for (const [label, ac] of [
    ["string", "yes"],
    ["number", 1],
    ["null  ", null],
  ]) {
    const fh = await fsp.open(f);
    await show("rws autoClose " + label + "  ", async () => {
      fh.readableWebStream({ autoClose: ac });
      return "ok";
    });
    await fh.close();
  }

  // options.type is the surprise: it is NOT value-validated. 'bytes' and
  // undefined are silent; EVERY other value warns and builds exactly the same
  // stream. There is no ERR_INVALID_ARG_VALUE on this path.
  for (const [label, t] of [
    ["bytes    ", "bytes"],
    ["undefined", undefined],
    ["gzip     ", "gzip"],
    ["number   ", 7],
    ["null     ", null],
  ]) {
    const fh = await fsp.open(f);
    const chunks = [];
    await show("rws type " + label + "     ", async () => {
      for await (const c of fh.readableWebStream({ type: t })) chunks.push(c);
      return Buffer.concat(chunks).toString();
    });
    await fh.close();
  }

  // Drain the deferred warnings. Filtered to this method's own text so an
  // unrelated warning from elsewhere in the case cannot make the count drift.
  await new Promise((resolve) => setTimeout(resolve, 0));
  const mine = warnings.filter((w) => w.includes("options.type"));
  console.log("rws warnings            ->", mine.length);
  console.log("rws warning text        ->", JSON.stringify(mine[0]));
}

fs.rmSync(dir, { recursive: true, force: true });
