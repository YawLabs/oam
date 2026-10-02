// node's per-encoding codecs on Buffer.prototype (lib/internal/buffer.js
// addBufferPrototypeMethods): the *Slice family and hex/base64/base64url/
// ucs2 *Write are C++ natives (StringSlice / StringWrite), ascii/latin1/utf8
// *Write JS wrappers over the same native write. Packages call them directly
// -- undici's body.text() is `buffer.utf8Slice(start, end)` -- and oam had
// none of the slices nor the four native writes, so undici 7 from npm threw
// "buffer.utf8Slice is not a function" on its first response body. Its three
// wrappers took the wrong defaults and errors. The decoded hex input also
// stops at the first pair that is not two hex DIGITS, which parseInt let
// through ("aG", " 1", "+1"). Measured on node v22.22.2.
import { Buffer } from "node:buffer";

const show = (label, f) => {
  try {
    console.log(label, JSON.stringify(f()));
  } catch (e) {
    console.log(label, "THROW", e.name, e.code, e.message, Object.keys(e).join());
  }
};

const SLICES = ["asciiSlice", "latin1Slice", "utf8Slice", "hexSlice", "base64Slice", "base64urlSlice", "ucs2Slice"];
const WRITES = ["asciiWrite", "latin1Write", "utf8Write", "hexWrite", "base64Write", "base64urlWrite", "ucs2Write"];

show("own", () => Object.getOwnPropertyNames(Buffer.prototype).filter((k) => /(Slice|Write)$/.test(k)).sort());
for (const m of [...SLICES, ...WRITES]) {
  const fn = Buffer.prototype[m];
  const desc = Object.getOwnPropertyDescriptor(Buffer.prototype, m);
  show(`${m} shape`, () => [fn.name, fn.length, typeof fn.prototype, desc.enumerable, desc.writable]);
  show(`${m} new`, () => new fn());
}

const b = Buffer.from("héllo wörld \u{1F600} end", "utf8");
for (const m of SLICES) {
  show(`${m}()`, () => b[m]());
  show(`${m}(2,9)`, () => b[m](2, 9));
  show(`${m}(9,2)`, () => b[m](9, 2));
  show(`${m}("3","5")`, () => b[m]("3", "5"));
  show(`${m}(1.7,4.2)`, () => b[m](1.7, 4.2));
  show(`${m}(NaN,undefined)`, () => b[m](NaN, undefined));
  show(`${m}(null,{})`, () => b[m](null, {}));
  show(`${m}(-0.5)`, () => b[m](-0.5));
  show(`${m}(-1)`, () => b[m](-1));
  show(`${m}(0,999)`, () => b[m](0, 999));
  show(`${m}(999)`, () => b[m](999));
  show(`${m}(0,Infinity)`, () => b[m](0, Infinity));
  show(`${m}(1n)`, () => b[m](1n));
  show(`${m} empty`, () => Buffer.alloc(0)[m](-1, 99));
  show(`${m} subarray`, () => Buffer.from("xxhello").subarray(2)[m](1, 3));
  show(`${m}.call(u16)`, () => Buffer.prototype[m].call(new Uint16Array([0x6968])));
  show(`${m}.call(dv)`, () => Buffer.prototype[m].call(new DataView(new Uint8Array([104, 105]).buffer)));
  show(`${m}.call({})`, () => Buffer.prototype[m].call({}));
}

const INPUT = {
  asciiWrite: "aé€x",
  latin1Write: "aé€x",
  utf8Write: "aé€x",
  hexWrite: "a1b2c3d4",
  base64Write: "YWJjZGVm",
  base64urlWrite: "YWJj-_8",
  ucs2Write: "aé€x",
};
for (const m of WRITES) {
  const s = INPUT[m];
  const run = (label, ...args) =>
    show(`${m}${label}`, () => {
      const out = Buffer.alloc(6);
      return [out[m](...args), [...out]];
    });
  run("(s)", s);
  run("(s,1)", s, 1);
  run("(s,1,2)", s, 1, 2);
  run("(s,2,99)", s, 2, 99);
  run("(s,6)", s, 6);
  run("(s,7)", s, 7);
  run("(s,1.5)", s, 1.5);
  run('(s,"2")', s, "2");
  run("(s,NaN,NaN)", s, NaN, NaN);
  run("(s,{})", s, {});
  run("(s,null)", s, null);
  run("(s,-0.5)", s, -0.5);
  run("(s,0,-1)", s, 0, -1);
  run("(s,0,-0.5)", s, 0, -0.5);
  run("(s,0,1.9)", s, 0, 1.9);
  run('(s,0,"3")', s, 0, "3");
  run("(s,undefined,3)", s, undefined, 3);
  run("(s,Infinity)", s, Infinity);
  run("(s,0,Infinity)", s, 0, Infinity);
  run("(s,1n)", s, 1n);
  run("(s,0,1n)", s, 0, 1n);
  run("(1)", 1);
  run("(1,9)", 1, 9);
  run('("")', "");
  show(`${m} empty`, () => Buffer.alloc(0)[m](s));
  show(`${m} empty,1`, () => Buffer.alloc(0)[m](s, 1));
  show(`${m}.call(u8)`, () => {
    const u = new Uint8Array(4);
    return [Buffer.prototype[m].call(u, s), [...u]];
  });
  show(`${m}.call(dv)`, () => Buffer.prototype[m].call(new DataView(new ArrayBuffer(4)), s));
  show(`${m}.call({})`, () => Buffer.prototype[m].call({}, s));
}
show("utf8Write splits no character", () => {
  const out = Buffer.alloc(5);
  return [out.utf8Write("ab€"), [...out]];
});
show("ucs2Write odd room", () => {
  const out = Buffer.alloc(5);
  return [out.ucs2Write("abc"), [...out]];
});
show("hexWrite odd", () => {
  const out = Buffer.alloc(4);
  return [out.hexWrite("abc"), [...out]];
});
for (const h of ["aG", "Ga11", "1g22", "a 11", "0x11", "11zz22", "+1", " 1", "A0fF"]) {
  show(`hex ${JSON.stringify(h)}`, () => [...Buffer.from(h, "hex")]);
  show(`hexWrite ${JSON.stringify(h)}`, () => Buffer.alloc(4).hexWrite(h));
}
// undici's chunksDecode: skip a UTF-8 BOM, then utf8Slice the rest.
const body = Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from('{"ok":true}')]);
show("utf8Slice after BOM", () => JSON.parse(body.utf8Slice(3, body.length)));
