// Buffer numeric read/write argument validation: Node throws coded errors
// (ERR_OUT_OF_RANGE / ERR_BUFFER_OUT_OF_BOUNDS / ERR_INVALID_ARG_TYPE) with
// exact messages; a bare V8 error or a silent no-op diverges. Guards the
// shared checkBounds/checkValue/varint validation layer.
const cap = (fn) => {
  try {
    fn();
    return "NO-THROW";
  } catch (e) {
    return `${e.code}|${e.name}|${e.message}`;
  }
};
const b = Buffer.alloc(8);

// Offset bounds + non-integer + buffer-too-short.
console.log(cap(() => Buffer.alloc(2).readUInt8(5)));
console.log(cap(() => b.readUInt8(1.5)));
console.log(cap(() => Buffer.alloc(2).readDoubleBE(0)));
console.log(cap(() => Buffer.alloc(0).readUInt16BE()));
console.log(cap(() => b.readUInt32BE(7)));

// Write value range (NaN / non-numeric pass through and mask, per Node).
console.log(cap(() => b.writeUInt8(256, 0)));
console.log(cap(() => b.writeInt8(-129, 0)));
console.log(cap(() => b.writeUInt16BE(70000, 0)));
console.log(cap(() => Buffer.alloc(2).writeUInt8("x", 0)));
console.log(cap(() => Buffer.alloc(2).writeUInt8(NaN, 0)));

// Variable-width family: byteLength range, offset, value range (incl. >2**32).
console.log(cap(() => b.readUIntLE(0, 7)));
console.log(cap(() => b.readUIntBE("", 3)));
console.log(cap(() => b.writeUIntBE(2 ** 24, 0, 3)));
console.log(cap(() => b.writeUIntLE(2 ** 48, 0, 6)));

// BigInt value range.
console.log(cap(() => b.writeBigUInt64BE(2n ** 64n, 0)));
console.log(cap(() => b.writeBigInt64BE(2n ** 63n, 0)));

// alloc size validation.
console.log(cap(() => Buffer.alloc(-1)));
console.log(cap(() => Buffer.alloc(Infinity)));
console.log(cap(() => Buffer.allocUnsafe("a")));

// Successful round-trips still work.
b.writeUInt32BE(0xdeadbeef, 0);
b.writeBigUInt64BE(1n, 0);
console.log(b.readUInt32BE(0).toString(16), b.readUIntBE(0, 3).toString(16));
