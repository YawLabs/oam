// fs.openAsBlob of a directory (#335): node's open and fstat of a directory
// succeed, so it gives a Blob of the size the fstat says -- 0 on Windows,
// the directory's own size on Linux and macOS -- and every read of a
// non-empty one fails NotReadableError. A file and a missing path for
// comparison, and the argument checks.
//
// And two things about any Blob's stream() found on the way: an empty Blob
// gives no chunk at all, and the chunks are the reader's own, not the
// Blob's bytes.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-407-"));
fs.mkdirSync(path.join(dir, "sub"));
fs.writeFileSync(path.join(dir, "f.txt"), "hello");
const show = (e) => `${e.name} ${e.code} | ${e.message.replaceAll(dir, "<T>")}${e.path ? " | path " + String(e.path).replaceAll(dir, "<T>") : ""}${e.syscall ? " | " + e.syscall : ""}`;
const row = async (label, f) => { try { console.log(label, "->", await f()); } catch (e) { console.log(label, "throws", show(e)); } };
for (const [label, p, opts] of [["dir", path.join(dir, "sub"), undefined], ["dot", ".", undefined], ["dir with type", path.join(dir, "sub"), { type: "x/y" }], ["file", path.join(dir, "f.txt"), undefined], ["missing", path.join(dir, "nope"), undefined]]) {
  let blob;
  await row(`openAsBlob ${label}`, async () => { blob = await fs.openAsBlob(p, opts); return `${blob.constructor.name} size=${blob.size} type=${JSON.stringify(blob.type)}`; });
  if (!blob) continue;
  await row(`  text`, async () => JSON.stringify(await blob.text()));
  await row(`  arrayBuffer`, async () => (await blob.arrayBuffer()).byteLength);
  await row(`  bytes`, async () => (await blob.bytes()).length);
  await row(`  slice(0,2).text`, async () => JSON.stringify(await blob.slice(0, 2).text()));
  await row(`  stream`, async () => { const chunks = []; for await (const c of blob.stream()) chunks.push(c.length); return JSON.stringify(chunks); });
  await row(`  slice(1).size`, () => blob.slice(1).size);
}
await row("openAsBlob no arg", () => fs.openAsBlob());
await row("openAsBlob bad type", () => fs.openAsBlob(path.join(dir, "f.txt"), { type: 5 }));
fs.rmSync(dir, { recursive: true, force: true });
const chunks = async (b) => {
  const out = [];
  for await (const c of b.stream()) out.push(c.length);
  return JSON.stringify(out);
};
console.log("stream of an empty Blob:", await chunks(new Blob([])));
console.log("stream of empty parts:", await chunks(new Blob(["", new Uint8Array(0)])));
console.log("stream of a slice to nothing:", await chunks(new Blob(["abc"]).slice(3)));
console.log("stream of an empty File:", await chunks(new File([], "f")));
const kept = new Blob(["abc"]);
for await (const c of kept.stream()) c[0] = 120;
console.log("a chunk written to leaves the Blob:", await kept.text());
