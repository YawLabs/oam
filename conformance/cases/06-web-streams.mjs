// Web streams: from/pipeThrough, TextDecoderStream split chars, tee.
const doubled = ReadableStream.from([1, 2, 3]).pipeThrough(
  new TransformStream({ transform: (v, c) => c.enqueue(v * 2) }),
);
const out = [];
for await (const v of doubled) out.push(v);
console.log(out.join(","));

const euro = ReadableStream.from([
  new Uint8Array([0x61, 0xe2]),
  new Uint8Array([0x82, 0xac, 0x62]),
]).pipeThrough(new TextDecoderStream());
let text = "";
for await (const part of euro) text += part;
console.log(text === "a€b", text.length);

const [t1, t2] = ReadableStream.from(["x", "y"]).tee();
const c1 = [];
const c2 = [];
for await (const v of t1) c1.push(v);
for await (const v of t2) c2.push(v);
console.log(c1.join(""), c2.join(""));

// Decoder/encoder primitives.
console.log(new TextDecoder().decode(new TextEncoder().encode("piñata")));
console.log(JSON.stringify(new TextDecoder().decode(new Uint8Array([0xe2]), { stream: true })));
