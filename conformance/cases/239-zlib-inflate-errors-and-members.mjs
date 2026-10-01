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

// finishFlush: node's finishing flush decides whether a stream that stops
// short is an error. Only Z_FINISH (the default) makes it "unexpected end of
// file"; Z_SYNC_FLUSH -- what axios and node-fetch decode response bodies
// with -- and the other flushes return what decoded. oam ignored the option,
// so every truncated body failed. The vector is node's gzip of 400 numbers,
// fixed here so every runtime cuts the same bytes.
console.log("-- finishFlush");
const { Z_NO_FLUSH, Z_PARTIAL_FLUSH, Z_SYNC_FLUSH, Z_FULL_FLUSH } = zlib.constants;
const ffGzip = hex(
  "1f8b080000000000000a1d95b91100310c021b2230e8efbfb11b2e21dbc42bf0c38009115a4421853c54a303f330834d" +
  "1c710bbe02299007aac108301f980356824db0179c0257e01e780dbd80f8200ea48482502c940595a03aa81b9a80f641" +
  "3bd025e211f116c14248081d221a9181a887a8417422868859c416e284b843be4632907a480d32129944e622ab902d64" +
  "1f721ab981bc87bc41bd4491282e4a850aa1e250d9a80a543f540f6a12b544eda2aed04fe87768365a818e878e4167a2" +
  "8be85a74177a849e436fa3cf4ffc306f304c8c88d162a230294c1ea61a6315bf8bc16c628e985bec2b2c85e561d5d808" +
  "6c3e6c0eb612dbc4f662a7b02bec1ef61af702c787e3e094b8202e1697852be1ea70ddb809dc3edc0eee127c8f0e5f02" +
  "0b7c92e3c0170dbe0c9fc8730cf8dac49818136be24cdc817c0d92e1837a8e01190932e95890e5736bdf5b9b18136be2" +
  "4cdc807a098a742c2815a890c3279a0daac2c7fa1c26c6c49a581357603c390e0cfaaa153eede7183032c1283a168c36" +
  "3126c6c49ab8bf086ec2739b98608a8e05330acc94e3c0ac06b34dccdf1d136be24cb867e5a2959b56ae5ab96be5b295" +
  "db567fdddcb772e1ca8d2b57aedcb972e9ea1aec17aee4730cd87243c3158d053b0bec92e3c06e1363624dac093b1f3b" +
  "1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b5f3b5f3b5f3b5f3b5f3b5f3b5f3b5f3b5f3b5f3b" +
  "5f3b5f3b5f3b5f3b3f3b3f3b3f3b3f3b3f3b3f3b3f3b3f3b3f3b3f3b3f3b3f3b3f3bbf2be83d390e7af442293c53cf31" +
  "d0cb845ed1b1d06b1363624cac89fb47cdabf606e2bfa15e516a2146414c390e6235c43631ff0e9a581367e216d22b48" +
  "94e320a9214578309fc3d35909a9e9303126d6c49af0d286a736bcb5e1b18d7f6dffb9f5de860737bcb8e1c90d6f6e78" +
  "74c3ab1b9eddb884f2d1e185664129390eca6828333cddcf3150b689313126d6c499b883ea35540c0ffd730c54915025" +
  "1d0b55f91b68ff036d624cac89337103f54ba849c742ad823ae4f0d7910d75853f91e7303126d6c49ab0f3b1f3b1f3b1" +
  "f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b1f3b9f800a4f683862f070000"
);
const ffRaw = ffGzip.subarray(10, ffGzip.length - 8);
const ffText = zlib.gunzipSync(ffGzip);
const ffZlib = Buffer.concat([hex("789c"), ffRaw, Buffer.alloc(4)]);
ffZlib.writeUInt32BE(adler32(ffText), ffZlib.length - 4);
const decoded = (out) => {
  const text = out.toString("latin1");
  const prefix = ffText.toString("latin1").startsWith(text);
  return `${out.length} bytes, ${prefix ? "a prefix" : "NOT a prefix"}, ends ${JSON.stringify(text.slice(-12))}`;
};
const showFF = (label, fn) => {
  try {
    console.log(label, "->", decoded(fn()));
  } catch (e) {
    console.log(label, "throws", shape(e));
  }
};
const ffCases = [
  ["inflateSync", ffZlib],
  ["gunzipSync", ffGzip],
  ["inflateRawSync", ffRaw],
  ["unzipSync", ffGzip],
];
for (const [fn, whole] of ffCases) {
  for (const cut of [whole.length >> 1, whole.length - 3, whole.length]) {
    const input = whole.subarray(0, cut);
    showFF(`${fn} ${cut}/${whole.length} Z_SYNC_FLUSH`, () =>
      zlib[fn](input, { finishFlush: Z_SYNC_FLUSH }),
    );
    showFF(`${fn} ${cut}/${whole.length} default`, () => zlib[fn](input));
  }
}
for (const flush of [Z_NO_FLUSH, Z_PARTIAL_FLUSH, Z_FULL_FLUSH]) {
  showFF(`gunzipSync cut, finishFlush ${flush}`, () =>
    zlib.gunzipSync(ffGzip.subarray(0, 300), { finishFlush: flush }),
  );
}
showFF("inflateSync empty Z_SYNC_FLUSH", () =>
  zlib.inflateSync(Buffer.alloc(0), { finishFlush: Z_SYNC_FLUSH }),
);
showFF("gunzipSync header only Z_SYNC_FLUSH", () =>
  zlib.gunzipSync(ffGzip.subarray(0, 5), { finishFlush: Z_SYNC_FLUSH }),
);
show("gunzipSync not gzip Z_SYNC_FLUSH", () =>
  zlib.gunzipSync("not gzip at all", { finishFlush: Z_SYNC_FLUSH }),
);
for (const [key, value] of [
  ["finishFlush", 9],
  ["finishFlush", -1],
  ["finishFlush", "2"],
  ["finishFlush", null],
  ["finishFlush", Infinity],
  ["flush", 6],
]) {
  show(`inflateSync ${key} ${String(value)}`, () =>
    zlib.inflateSync(zlib.deflateSync("hello"), { [key]: value }),
  );
  show(`createInflate ${key} ${String(value)}`, () => zlib.createInflate({ [key]: value }));
  show(`inflate ${key} ${String(value)}`, () =>
    zlib.inflate(zlib.deflateSync("hello"), { [key]: value }, () => {}),
  );
}
show("inflateSync finishFlush NaN", () =>
  zlib.inflateSync(zlib.deflateSync("hello"), { finishFlush: NaN }),
);
const ffCut = ffGzip.subarray(0, 400);
await new Promise((resolve) =>
  zlib.gunzip(ffCut, { finishFlush: Z_SYNC_FLUSH }, (err, out) => {
    console.log("gunzip cut Z_SYNC_FLUSH", err ? `error ${shape(err)}` : decoded(out));
    resolve();
  }),
);
await new Promise((resolve) =>
  zlib.unzip(ffCut, { finishFlush: Z_SYNC_FLUSH }, (err, out) => {
    console.log("unzip cut Z_SYNC_FLUSH", err ? `error ${shape(err)}` : decoded(out));
    resolve();
  }),
);
const viaStreamFF = (label, make, input, size = input.length) =>
  new Promise((resolve) => {
    const stream = make();
    const out = [];
    stream.on("data", (c) => out.push(c));
    stream.on("error", (e) => {
      console.log(label, "error", shape(e));
      resolve();
    });
    stream.on("end", () => {
      console.log(label, "end", decoded(Buffer.concat(out)));
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
const sf = { finishFlush: Z_SYNC_FLUSH };
await viaStreamFF("createGunzip cut Z_SYNC_FLUSH", () => zlib.createGunzip(sf), ffCut);
await viaStreamFF("createGunzip cut Z_SYNC_FLUSH, 64-byte writes", () => zlib.createGunzip(sf), ffCut, 64);
await viaStreamFF("createUnzip cut Z_SYNC_FLUSH", () => zlib.createUnzip(sf), ffCut);
await viaStreamFF("createInflate cut Z_SYNC_FLUSH", () => zlib.createInflate(sf), ffZlib.subarray(0, 400));
await viaStreamFF("createInflateRaw cut Z_SYNC_FLUSH", () => zlib.createInflateRaw(sf), ffRaw.subarray(0, 400));
await viaStreamFF("createGunzip cut default", () => zlib.createGunzip(), ffCut);
await viaStreamFF("createInflate no data Z_SYNC_FLUSH", () => zlib.createInflate(sf), Buffer.alloc(0));
