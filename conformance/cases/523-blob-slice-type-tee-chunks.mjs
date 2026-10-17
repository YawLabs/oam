// Blob as node's (#375): tee() of a byte stream gives the second branch its
// own copy of each chunk, and both branches are byte streams; a file-backed
// Blob refuses structuredClone with ERR_INVALID_STATE, a plain Blob (and a
// slice of a file-backed one) clones; slice() converts its arguments as
// WebIDL `long long`s and its content type as the constructor's (an omitted
// one is "", not the Blob's); the constructor's checks and endings; and a
// Blob's stream gives one chunk per part, a file part in 64 KiB chunks.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
const D = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-523-"));
const desc = (e) => `${e.constructor.name} ${e.code} ${JSON.stringify(e.message)}`;
const t = (label, fn) => {
  try {
    console.log(label, "->", JSON.stringify(fn()));
  } catch (e) {
    console.log(label, "-> throws", desc(e));
  }
};

// tee
{
  const [a, b] = new Blob(["hello"]).stream().tee();
  const ca = await a.getReader().read();
  ca.value[0] = 88;
  const cb = await b.getReader().read();
  console.log("Blob stream tee: a", ca.value[0], "b", cb.value[0], "same object", ca.value === cb.value, "same buffer", ca.value.buffer === cb.value.buffer);
}
{
  const s = new ReadableStream({ type: "bytes", start(c) { c.enqueue(new Uint8Array([1, 2, 3])); c.close(); } });
  const [a, b] = s.tee();
  const r = await a.getReader({ mode: "byob" }).read(new Uint8Array(2));
  const y = await b.getReader().read();
  console.log("byte stream tee: a byob read", r.value.length, "b", y.value.length, y.value.constructor.name);
}
{
  const o = { n: 1 };
  const [a, b] = new ReadableStream({ start(c) { c.enqueue(o); c.close(); } }).tee();
  const x = await a.getReader().read();
  const y = await b.getReader().read();
  console.log("default stream tee shares:", x.value === o, y.value === o);
  try { a.getReader({ mode: "byob" }); } catch (e) { console.log("  and its branches are not byte streams:", e.code); }
}
{
  const r1 = new Response(new Blob(["abc"]));
  const r2 = r1.clone();
  const x = await r1.body.getReader().read();
  x.value[0] = 90;
  console.log("Response.clone of a Blob body:", new TextDecoder().decode((await r2.body.getReader().read()).value));
}

// structuredClone
const f = path.join(D, "f");
fs.writeFileSync(f, "abc");
const fb = await fs.openAsBlob(f);
for (const [label, v] of [["file-backed", fb], ["nested", { x: [fb] }], ["a slice of it", fb.slice(1)], ["a Blob of it", new Blob([fb])]]) {
  try {
    const c = structuredClone(v);
    console.log(`structuredClone ${label}: ok`, c instanceof Blob ? `${c.size} ${JSON.stringify(await c.text())}` : "");
  } catch (e) {
    console.log(`structuredClone ${label}:`, desc(e), "own", Object.keys(e).join("|"));
  }
}
{
  const plain = new Blob(["xy", new Uint8Array(3)], { type: "a/b" });
  const c = structuredClone({ plain });
  console.log("plain clone:", c.plain instanceof Blob, c.plain !== plain, c.plain.size, c.plain.type, JSON.stringify(await c.plain.text()));
  const chunks = [];
  for await (const x of c.plain.stream()) chunks.push(x.length);
  console.log("  its chunks:", chunks.join(","));
  const file = structuredClone(new File(["f"], "n.txt", { type: "t/x", lastModified: 5 }));
  console.log("  a File:", Object.prototype.toString.call(file), file.size, file.type);
}

// slice
const B = new Blob(["hello"], { type: "x/y" });
t("slice().type", () => B.slice().type);
t("slice(0, 1, null).type", () => B.slice(0, 1, null).type);
t("slice(0, 1, undefined).type", () => B.slice(0, 1, undefined).type);
t("slice(0, 1, 'A/B').type", () => B.slice(0, 1, "A/B").type);
t("slice(0, 1, '\\u00e9').type", () => B.slice(0, 1, "é").type);
t("slice(0, 1, 1).type", () => B.slice(0, 1, 1).type);
t("slice(0, 1, {}).type", () => B.slice(0, 1, {}).type);
for (const [s, e] of [
  [NaN, Infinity], [NaN, NaN], [1, NaN], [-Infinity, 3], [1.9, 3.9], [-1.5, undefined], ["2", "4"],
  [null, null], [undefined, -1], [true, 3], [2 ** 64 + 2, 4], [-(2 ** 53), 2], [2, 1], [[], [3]], [-3, -1],
  [2 ** 63, 3], [-(2 ** 63), 2], [0.5, 4.9999],
]) {
  t(`slice(${typeof s === "object" && s ? "[]" : String(s)}, ${Array.isArray(e) ? "[3]" : String(e)})`, () => {
    const x = B.slice(s, e);
    return [x.size, x.type];
  });
}
t("slice(1n)", () => B.slice(1n).size);
t("slice(Symbol())", () => B.slice(Symbol()).size);
console.log("slice(1.9, 3.9) text:", await B.slice(1.9, 3.9).text());
t("own keys: a Blob, a slice, a File", () => [Object.keys(B), Object.keys(B.slice()), Object.keys(new File(["a"], "n"))]);
t("slice's prototype is not Blob.prototype", () => [Object.getPrototypeOf(B.slice()) === Blob.prototype, B.slice() instanceof Blob]);
t("Blob.prototype's enumerable keys", () => Object.keys(Blob.prototype));
t("File slice", () => { const x = new File(["ab"], "n", { type: "x/y" }).slice(); return [Object.prototype.toString.call(x), x.constructor.name, x.type]; });

// The constructor.
t("new Blob(null)", () => new Blob(null).size);
t("new Blob('ab')", () => new Blob("ab").size);
t("new Blob({})", () => new Blob({}).size);
t("new Blob(undefined)", () => new Blob(undefined).size);
t("new Blob(a Set)", () => new Blob(new Set(["ab", "c"])).size);
t("type 123", () => new Blob([], { type: 123 }).type);
t("type null", () => new Blob([], { type: null }).type);
t("type ABC", () => new Blob([], { type: "ABC" }).type);
t("type non-ASCII", () => new Blob([], { type: "aé" }).type);
t("type with a tab", () => new Blob([], { type: "a\tb" }).type);
t("options 5", () => new Blob([], 5).type);
t("options null", () => new Blob([], null).type);
t("options a function", () => new Blob([], () => {}).type);
t("endings 'x'", () => new Blob([], { endings: "x" }).type);
console.log("endings native:", JSON.stringify(await new Blob(["a\nb\r\nc\r"], { endings: "native" }).text()) === JSON.stringify(process.platform === "win32" ? "a\r\nb\r\nc\r" : "a\nb\nc\r"));
t("endings transparent", () => new Blob(["a\nb\r\nc"], { endings: "transparent" }).size);
t("endings native on bytes", () => new Blob([new Uint8Array([10])], { endings: "native" }).size);
t("parts of other types", () => new Blob([1, null, undefined, {}]).size);
{
  const u = new Uint8Array([1, 2, 3]);
  const b = new Blob([u]);
  u[0] = 9;
  console.log("a part is copied at construction:", (await b.bytes())[0]);
}

// Chunks.
const chunks = async (b) => {
  const out = [];
  for await (const c of b.stream()) out.push(c.length);
  return out.join(",");
};
console.log("chunks of parts:", await chunks(new Blob(["ab", new Uint8Array(70000), "cd"])));
console.log("chunks of strings:", await chunks(new Blob(["a", "b", "c"])));
console.log("chunks of one large part:", await chunks(new Blob([new Uint8Array(200000)])));
const inner = new Blob(["ab", "cd"]);
console.log("chunks of nested Blobs:", await chunks(new Blob([new Blob([inner, "x"]), inner])));
console.log("chunks of a slice:", await chunks(new Blob(["abc", "def", "ghi"]).slice(1, 8)));
console.log("chunks of a nested slice:", await chunks(new Blob([inner, "", "ef"]).slice(1, 5)));
console.log("chunks of empty parts:", await chunks(new Blob(["", "a", new Uint8Array(0), ""])));
const big = path.join(D, "big");
fs.writeFileSync(big, Buffer.alloc(200000, 1));
const bigb = await fs.openAsBlob(big);
console.log("chunks of a file:", await chunks(bigb));
console.log("chunks of a file slice:", await chunks(bigb.slice(10, 150000)));
console.log("chunks of a file in a Blob:", await chunks(new Blob(["x", bigb, "y"])));
{
  const r = bigb.stream().getReader({ mode: "byob" });
  const o = [];
  for (let i = 0; i < 5; i++) {
    const x = await r.read(new Uint8Array(100000));
    o.push(x.value.length + (x.done ? " done" : ""));
  }
  console.log("byob reads of a file:", o.join(","));
}
fs.rmSync(D, { recursive: true, force: true });
