// node:zlib's deflaters with the `dictionary` option. oam ignored it: a
// zlib stream came out without FDICT, and neither form copied from the
// dictionary. Now the window starts primed with it, as zlib's
// deflateSetDictionary does, and a zlib stream sets FDICT and carries the
// dictionary's Adler-32. gzip takes the option and does not use it, in
// node as here.
//
// The compressed bytes are miniz's, not zlib's (docs/node-divergences.md),
// so this prints what node defines -- the header, DICTID, what inflates
// with which dictionary, that the dictionary was used -- not the body.
import zlib from "node:zlib";
import util from "node:util";

const shape = (e) => `${e.constructor.name} ${e.code} ${e.errno} ${JSON.stringify(e.message)}`;
const show = (label, fn) => {
  try {
    console.log(label, "->", fn());
  } catch (e) {
    console.log(label, "throws", shape(e));
  }
};
function adler32(buf) {
  let a = 1;
  let b = 0;
  for (const x of buf) {
    a = (a + x) % 65521;
    b = (b + a) % 65521;
  }
  return ((b << 16) | a) >>> 0;
}

// Text whose second half repeats phrases from the dictionary.
const dictionary = Buffer.from(
  "the quick brown fox jumps over the lazy dog; pack my box with five dozen liquor jugs; " +
    "sphinx of black quartz, judge my vow; how vexingly quick daft zebras jump",
);
const data = Buffer.from(
  "judge my vow, the lazy dog said; five dozen liquor jugs and the quick brown fox; " +
    "how vexingly quick daft zebras jump over black quartz",
);

console.log("-- deflateSync");
const z = zlib.deflateSync(data, { dictionary });
console.log("header", z.subarray(0, 2).toString("hex"), "FDICT", (z[1] & 0x20) !== 0);
console.log("DICTID is the dictionary's Adler-32", z.readUInt32BE(2) === adler32(dictionary));
console.log("trailer is the data's Adler-32", z.readUInt32BE(z.length - 4) === adler32(data));
console.log("smaller than without", z.length - 4 < zlib.deflateSync(data).length);
show("inflate, dictionary", () => zlib.inflateSync(z, { dictionary }).equals(data));
show("inflate, none", () => zlib.inflateSync(z));
show("inflate, wrong", () => zlib.inflateSync(z, { dictionary: Buffer.from("nope") }));
for (const level of [-1, 0, 1, 2, 5, 6, 7, 9]) {
  const out = zlib.deflateSync(data, { dictionary, level });
  console.log(
    "level", level, out.subarray(0, 2).toString("hex"),
    zlib.inflateSync(out, { dictionary }).equals(data),
  );
}

console.log("-- deflateRawSync");
const raw = zlib.deflateRawSync(data, { dictionary });
console.log("smaller than without", raw.length < zlib.deflateRawSync(data).length);
show("inflateRaw, dictionary", () => zlib.inflateRawSync(raw, { dictionary }).equals(data));
show("inflateRaw, none", () => zlib.inflateRawSync(raw));

console.log("-- not used");
const empty = zlib.deflateSync(data, { dictionary: Buffer.alloc(0) });
console.log("empty dictionary", empty.subarray(0, 2).toString("hex"), empty.equals(zlib.deflateSync(data)));
console.log("gzip", zlib.gzipSync(data, { dictionary }).equals(zlib.gzipSync(data)));
console.log("ArrayBuffer", zlib.deflateSync(data, { dictionary: new Uint8Array(dictionary).buffer }).equals(z));

console.log("-- a dictionary past the window");
// Its last 32 KiB is the window; DICTID is the whole dictionary's.
const big = Buffer.alloc(50000);
for (let i = 0; i < big.length; i++) big[i] = 32 + ((i * 7919) % 95);
const bigData = Buffer.concat([big.subarray(big.length - 30000, big.length - 29000), Buffer.from("tail")]);
const bz = zlib.deflateSync(bigData, { dictionary: big });
console.log("DICTID", bz.readUInt32BE(2) === adler32(big));
console.log("used", bz.length < 200);
show("inflate", () => zlib.inflateSync(bz, { dictionary: big }).equals(bigData));
show("inflateRaw", () =>
  zlib.inflateRawSync(zlib.deflateRawSync(bigData, { dictionary: big }), { dictionary: big }).equals(bigData));

console.log("-- callback and stream");
await new Promise((resolve) =>
  zlib.deflate(data, { dictionary }, (err, out) => {
    console.log("deflate cb", err ? shape(err) : [out.subarray(0, 6).toString("hex"), zlib.inflateSync(out, { dictionary }).equals(data)]);
    resolve();
  }),
);
await new Promise((resolve) =>
  zlib.deflateRaw(data, { dictionary }, (err, out) => {
    console.log("deflateRaw cb", err ? shape(err) : zlib.inflateRawSync(out, { dictionary }).equals(data));
    resolve();
  }),
);
const streamed = (stream, input) => {
  const parts = [];
  stream.on("data", (c) => parts.push(c));
  const done = new Promise((resolve) => stream.on("end", () => resolve(Buffer.concat(parts))));
  for (let i = 0; i < input.length; i += 7) stream.write(input.subarray(i, i + 7));
  stream.end();
  return done;
};
const sz = await streamed(zlib.createDeflate({ dictionary }), data);
console.log("createDeflate", sz.subarray(0, 6).toString("hex"), zlib.inflateSync(sz, { dictionary }).equals(data));
const sr = await streamed(zlib.createDeflateRaw({ dictionary }), data);
console.log("createDeflateRaw", zlib.inflateRawSync(sr, { dictionary }).equals(data));
// One stream's output, inflated by a stream with the dictionary.
const back = await streamed(zlib.createInflate({ dictionary }), sz);
console.log("createInflate", back.equals(data));

console.log("-- the handle a subclass drives (pngjs's pattern)");
function handleRun(Class, opts, input) {
  function Sub() {
    Class.call(this, opts);
  }
  util.inherits(Sub, Class);
  const sub = new Sub();
  const out = Buffer.alloc(4096);
  sub._handle.writeSync(zlib.constants.Z_FINISH, input, 0, input.length, out, 0, out.length);
  return out.subarray(0, out.length - sub._writeState[0]);
}
const hz = handleRun(zlib.Deflate, { dictionary }, data);
console.log("Deflate handle", hz.subarray(0, 6).toString("hex"), zlib.inflateSync(hz, { dictionary }).equals(data));
console.log("Inflate handle", handleRun(zlib.Inflate, { dictionary }, z).equals(data));
const hr = handleRun(zlib.DeflateRaw, { dictionary }, data);
console.log("DeflateRaw handle", zlib.inflateRawSync(hr, { dictionary }).equals(data));
console.log("InflateRaw handle", handleRun(zlib.InflateRaw, { dictionary }, raw).equals(data));
