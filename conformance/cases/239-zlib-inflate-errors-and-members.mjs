// node:zlib's inflaters, as zlib runs them (#166). oam's were flate2's
// decoders, and three things differed:
//
// - a back-reference from before the start of the output decoded to zeros
//   (miniz's wrapping dictionary), where zlib fails "invalid distance too far
//   back" -- and a gzip or zlib checksum computed over those zeros passed;
// - gunzip stopped after the first gzip member (the sync and callback forms
//   dropped the rest; createGunzip() failed the valid input);
// - decode errors had no `code` / `errno`, and a truncated stream was not an
//   error at all.
//
// Every row prints the outcome: the output, or the error's class, code, errno
// and own property names. The message is printed too, except for corrupt
// deflate DATA, where oam cannot name the defect zlib names (see
// docs/node-divergences.md); the distance check is the exception, and is
// printed.
import zlib from "node:zlib";

const hex = (s) => Buffer.from(s, "hex");
const gz = (s) => zlib.gzipSync(s);
function adler32(buf) {
  let a = 1;
  let b = 0;
  for (const x of buf) {
    a = (a + x) % 65521;
    b = (b + a) % 65521;
  }
  return ((b << 16) | a) >>> 0;
}
function crc32(buf) {
  let c = ~0;
  for (const x of buf) {
    c ^= x;
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c >>> 0;
}

const shape = (e, withMessage = true) =>
  `${e.constructor.name} ${e.code} ${e.errno}` +
  (withMessage ? ` ${JSON.stringify(e.message)}` : "") +
  ` [${Object.getOwnPropertyNames(e).join(",")}]`;
const show = (label, fn, withMessage = true) => {
  try {
    console.log(label, "->", JSON.stringify(fn().toString("latin1")));
  } catch (e) {
    console.log(label, "throws", shape(e, withMessage));
  }
};

// Raw deflate: the literal "a", then a 3-byte copy from distance 5. The
// wrappers carry the checksums of what a wrapping dictionary would produce
// ("a\0\0\0"), so nothing but the distance check can refuse them.
const tooFar = hex("4b041200");
const zeros = Buffer.from([0x61, 0, 0, 0]);
const zlibTooFar = Buffer.concat([hex("789c"), tooFar, Buffer.alloc(4)]);
zlibTooFar.writeUInt32BE(adler32(zeros), zlibTooFar.length - 4);
const gzipTooFar = Buffer.concat([hex("1f8b0800000000000003"), tooFar, Buffer.alloc(8)]);
gzipTooFar.writeUInt32LE(crc32(zeros), gzipTooFar.length - 8);
gzipTooFar.writeUInt32LE(zeros.length, gzipTooFar.length - 4);

const two = Buffer.concat([gz("hello"), gz("world")]);
const packed = zlib.deflateSync("hello world hello world");
const badAdler = Buffer.from(packed);
badAdler[badAdler.length - 1] ^= 1;
const member = gz("hello world");
const badCrc = Buffer.from(member);
badCrc[badCrc.length - 8] ^= 1;
const badLen = Buffer.from(member);
badLen[badLen.length - 4] ^= 1;

console.log("-- a copy from before the start of the output");
show("inflateRawSync", () => zlib.inflateRawSync(tooFar));
show("inflateSync", () => zlib.inflateSync(zlibTooFar));
show("gunzipSync", () => zlib.gunzipSync(gzipTooFar));
show("unzipSync", () => zlib.unzipSync(gzipTooFar));
show("gunzipSync, into the previous member", () =>
  zlib.gunzipSync(Buffer.concat([gz("hello"), gzipTooFar])),
);

console.log("-- gzip members");
show("gunzipSync two", () => zlib.gunzipSync(two));
show("unzipSync two", () => zlib.unzipSync(two));
show("gunzipSync member + 00 + member", () =>
  zlib.gunzipSync(Buffer.concat([gz("hello"), hex("00"), gz("world")])),
);
show("gunzipSync member + JUNK", () =>
  zlib.gunzipSync(Buffer.concat([gz("hello"), Buffer.from("JUNK")])),
);
show("gunzipSync member + 1f", () => zlib.gunzipSync(Buffer.concat([gz("hello"), hex("1f")])));
show("inflateSync stream + JUNK", () =>
  zlib.inflateSync(Buffer.concat([zlib.deflateSync("hello"), Buffer.from("JUNK")])),
);
show("inflateRawSync stream + JUNK", () =>
  zlib.inflateRawSync(Buffer.concat([zlib.deflateRawSync("hello"), Buffer.from("JUNK")])),
);

console.log("-- headers and trailers");
show("gunzipSync not gzip", () => zlib.gunzipSync("not gzip at all"));
show("inflateSync not zlib", () => zlib.inflateSync("not zlib at all"));
show("inflateSync method 7", () => zlib.inflateSync(hex("7709")));
show("inflateSync window 2^16", () => zlib.inflateSync(hex("881c")));
show("inflateSync preset dictionary", () => zlib.inflateSync(hex("78bb00000001")));
show("gunzipSync method 7", () => zlib.gunzipSync(hex("1f8b0700000000000003")));
show("gunzipSync reserved flag", () => zlib.gunzipSync(hex("1f8b0820000000000003")));
show("inflateSync bad adler", () => zlib.inflateSync(badAdler));
show("gunzipSync bad crc", () => zlib.gunzipSync(badCrc));
show("gunzipSync bad length", () => zlib.gunzipSync(badLen));

console.log("-- truncation");
show("inflateSync cut in the data", () => zlib.inflateSync(packed.subarray(0, 8)));
show("inflateSync cut in the adler", () => zlib.inflateSync(packed.subarray(0, packed.length - 1)));
show("gunzipSync cut in the trailer", () => zlib.gunzipSync(member.subarray(0, member.length - 3)));
show("inflateRawSync stored block cut", () => zlib.inflateRawSync(hex("010500faff")));
for (const fn of ["gunzipSync", "inflateSync", "inflateRawSync", "unzipSync"]) {
  show(`${fn} empty`, () => zlib[fn](Buffer.alloc(0)));
}
show("unzipSync 1f", () => zlib.unzipSync(hex("1f")));

console.log("-- corrupt deflate data (code and errno)");
show("inflateRawSync reserved block type", () => zlib.inflateRawSync(hex("ffff")), false);

console.log("-- callback forms");
const viaCallback = (label, fn, input) =>
  new Promise((resolve) =>
    fn(input, (err, out) => {
      console.log(label, err ? `error ${shape(err)}` : JSON.stringify(out.toString()));
      resolve();
    }),
  );
await viaCallback("gunzip two", zlib.gunzip, two);
await viaCallback("unzip two", zlib.unzip, two);
await viaCallback("inflateRaw too far", zlib.inflateRaw, tooFar);
await viaCallback("inflate cut", zlib.inflate, packed.subarray(0, 8));

console.log("-- streams");
const viaStream = (label, make, input, size = input.length) =>
  new Promise((resolve) => {
    const stream = make();
    const out = [];
    stream.on("data", (c) => out.push(c));
    stream.on("error", (e) => {
      console.log(label, "error", shape(e));
      resolve();
    });
    stream.on("end", () => {
      console.log(label, "end", JSON.stringify(Buffer.concat(out).toString("latin1")));
      resolve();
    });
    let offset = 0;
    const step = () => {
      if (offset >= input.length) return stream.end();
      stream.write(input.subarray(offset, offset + size));
      offset += size;
      setImmediate(step);
    };
    step();
  });
await viaStream("createGunzip two", zlib.createGunzip, two);
await viaStream("createGunzip two, a byte per write", zlib.createGunzip, two, 1);
await viaStream("createUnzip two", zlib.createUnzip, two);
await viaStream("createGunzip member + 00 + member", zlib.createGunzip,
  Buffer.concat([gz("hello"), hex("00"), gz("world")]));
await viaStream("createGunzip member + JUNK", zlib.createGunzip,
  Buffer.concat([gz("hello"), Buffer.from("JUNK")]));
await viaStream("createInflate stream + JUNK", zlib.createInflate,
  Buffer.concat([zlib.deflateSync("hello"), Buffer.from("JUNK")]));
await viaStream("createInflateRaw too far", zlib.createInflateRaw, tooFar);
await viaStream("createInflate cut", zlib.createInflate, packed.subarray(0, 8));
await viaStream("createGunzip not gzip", zlib.createGunzip, Buffer.from("not gzip"));
for (const name of ["createGunzip", "createInflate", "createInflateRaw", "createUnzip"]) {
  await viaStream(`${name} no data`, zlib[name], Buffer.alloc(0));
}
