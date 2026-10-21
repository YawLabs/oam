// fs.openAsBlob gives a file-backed Blob (#373): nothing is read at the
// call; each read opens the file then, and fails NotReadableError when the
// file's size or modification time is not what it was at the open (node
// compares the size and the nanosecond part of the mtime). A slice, a Blob
// made with it as a part, a Response and a FormData body keep the file
// parts and fail the same way; a slice of no bytes reads as empty without
// touching the file. A directory (#335) fits the same model. And a large
// file costs no memory until it is read.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
const D = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-521-"));
const show = (p) =>
  p.then(
    (v) => JSON.stringify(v),
    (e) => `${e.constructor.name} ${e.name} code=${e.code} ${JSON.stringify(e.message)}`,
  );
const streamText = async (b) => {
  let s = "";
  for await (const c of b.stream()) s += Buffer.from(c).toString();
  return s;
};

const f = path.join(D, "f.txt");
fs.writeFileSync(f, "hello world");
const b = await fs.openAsBlob(f, { type: "Text/Plain" });
console.log("fresh:", b.size, JSON.stringify(b.type), await show(b.text()), await show(b.slice(6).text()));
console.log("as a part:", await show(new Blob(["<", b, ">"]).text()), await show(streamText(new Blob(["<", b, ">"]))));
const kept = new Blob([b]);
const head = b.slice(0, 5);
const none = b.slice(3, 3);
const twice = b.slice(1).slice(1, 4);
fs.writeFileSync(f, "HELLO WORLD!");
console.log("after a rewrite:");
for (const [label, p] of [
  ["text", () => b.text()],
  ["arrayBuffer", () => b.arrayBuffer().then((x) => x.byteLength)],
  ["bytes", () => b.bytes().then((x) => x.length)],
  ["stream", () => streamText(b)],
  ["new Blob([b]).text", () => kept.text()],
  ["slice(0, 5).text", () => head.text()],
  ["slice of a slice", () => twice.text()],
  ["slice(3, 3).text", () => none.text()],
  ["new Blob(['x', slice(3, 3)]).text", () => new Blob(["x", none]).text()],
  ["new Response(b).text", () => new Response(b).text()],
  ["Request body", () => new Request("http://x/", { method: "POST", body: b }).text()],
  ["FormData body", () => { const fd = new FormData(); fd.append("f", b, "f.txt"); return new Response(fd).text(); }],
]) {
  console.log(`  ${label}:`, await show(p()));
}
console.log("  sizes:", b.size, kept.size, head.size, none.size, twice.size);
{
  const r = b.stream().getReader();
  console.log("  a stream's read and closed:", await show(r.read()), await show(r.closed));
}

// The size and mtime at the open are the reference, not the last read.
const g = path.join(D, "g.txt");
fs.writeFileSync(g, "one");
const gb = await fs.openAsBlob(g);
console.log("before a change:", await show(gb.text()), "after an append:", (fs.appendFileSync(g, "two"), await show(gb.text())));

// The mtime alone.
const h = path.join(D, "h.txt");
fs.writeFileSync(h, "abc");
fs.utimesSync(h, 1000000000, 1000000000);
const hb = await fs.openAsBlob(h);
fs.utimesSync(h, 1000000000.5, 1000000000.5);
console.log("utimes:", await show(hb.text()));
// Rewritten to the same size with the same mtime: indistinguishable, so read.
const k = path.join(D, "k.txt");
fs.writeFileSync(k, "abc");
fs.utimesSync(k, 1000000000, 1000000000);
const kb = await fs.openAsBlob(k);
fs.writeFileSync(k, "xyz");
fs.utimesSync(k, 1000000000, 1000000000);
console.log("same size, same mtime:", await show(kb.text()));
// Removed.
const u = path.join(D, "u.txt");
fs.writeFileSync(u, "abc");
const ub = await fs.openAsBlob(u);
fs.unlinkSync(u);
console.log("removed:", await show(ub.text()), await show(ub.slice(0, 0).text()), await show(ub.slice(1, 2).text()));
// An empty file: the Blob still checks it; a slice of it has no file part.
const e = path.join(D, "e.txt");
fs.writeFileSync(e, "");
const eb = await fs.openAsBlob(e);
const eslice = eb.slice();
const ewrap = new Blob([eb]);
const emixed = new Blob([eb, "x"]).slice(0, 1);
console.log("empty file:", eb.size, await show(eb.text()));
fs.writeFileSync(e, "grown");
console.log("empty file grown:", await show(eb.text()), await show(eslice.text()), await show(ewrap.text()), await show(emixed.text()));

// A directory: the size the open's fstat said, and a change to it fails the read.
// The first row pins the mtime along with its add: NTFS applies a directory's
// mtime update lazily, so the add alone moved it in only 6-29 of 100 attempts
// here. Each side holds its own directory, so an unpinned add printed
// NotReadableError on one side and "" on the other and failed the
// byte-compare on the host's schedule rather than on any runtime's behaviour.
for (const [label, change] of [
  [
    "a file added, mtime pinned",
    (d) => {
      fs.writeFileSync(path.join(d, "x"), "x");
      fs.utimesSync(d, 1000000000, 1000000000);
    },
  ],
  ["utimes", (d) => fs.utimesSync(d, 1000000000, 1000000000)],
  ["removed", (d) => fs.rmdirSync(d)],
]) {
  const d = fs.mkdtempSync(path.join(D, "dir-"));
  const db = await fs.openAsBlob(d);
  const sameSize = db.size === fs.statSync(d).size;
  const before = await show(db.text());
  change(d);
  console.log(`directory, ${label}:`, sameSize, before, await show(db.text()), await show(new Blob([db]).text()), await show(db.slice(0, 0).text()));
}

// No memory until it is read.
{
  const big = path.join(D, "big.bin");
  const fd = fs.openSync(big, "w");
  fs.ftruncateSync(fd, 256 * 1024 * 1024);
  fs.closeSync(fd);
  const before = process.memoryUsage().rss;
  const blobs = [await fs.openAsBlob(big), await fs.openAsBlob(big)];
  const grew = process.memoryUsage().rss - before;
  console.log("a 256 MiB file, opened twice:", blobs[0].size, "memory grew < 64 MiB:", grew < 64 * 1024 * 1024);
  console.log("a slice of it reads:", (await blobs[1].slice(1000, 1010).bytes()).join(""));
}
fs.rmSync(D, { recursive: true, force: true });
