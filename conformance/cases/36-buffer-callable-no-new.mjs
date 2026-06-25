// Buffer callable WITHOUT `new` (FIX 2 -- DEP0005 legacy constructor form).
// Node's Buffer is a FUNCTION, callable as `Buffer(...)` AND `new Buffer(...)`:
//   Buffer(number)            -> zero-filled allocation (Node v22)
//   Buffer(string[, enc])     -> from(string, enc)
//   Buffer(array)             -> from(array)
//   Buffer(typedArray/buffer) -> copy
//   Buffer(arrayBuffer[, off[, len]]) -> view over the same memory
//   Buffer(number, "enc")     -> ERR_INVALID_ARG_TYPE ("string" arg)
// The DEP0005 deprecation warning goes to STDERR (not compared here); stdout
// must match node byte-for-byte. Guards the CallableBuffer shim.

console.log("typeof:" + typeof Buffer);

// number -> zero-filled alloc.
const z = Buffer(10);
console.log("num:" + z.length + ":" + [...z].every((b) => b === 0));

// string (+ encoding) -> from.
console.log("str:" + Buffer("abc").toString());
console.log("hex:" + Buffer("6869", "hex").toString());

// array / typed array -> from (copy).
console.log("arr:" + [...Buffer([1, 2, 3])].join(","));
const u = new Uint8Array(3);
u[0] = 7; u[1] = 8; u[2] = 9;
console.log("u8:" + [...Buffer(u)].join(","));

// new Buffer(...) identical to Buffer(...).
console.log("new:" + [...new Buffer(3)].join(",") + ":" + (new Buffer(3) instanceof Buffer));

// arrayBuffer + offset + length -> view.
const ab = new ArrayBuffer(4);
const view = new Uint8Array(ab);
view[1] = 5; view[2] = 6;
console.log("ab:" + [...Buffer(ab, 1, 2)].join(","));

// instanceof + isBuffer + prototype methods survive on callable-produced bufs.
const b = Buffer("hello");
console.log("inst:" + (b instanceof Buffer) + ":" + (b instanceof Uint8Array));
console.log("isBuffer:" + Buffer.isBuffer(b) + ":" + Buffer.isBuffer(new Buffer(2)));
console.log("slice:" + b.slice(1, 3).toString() + ":" + (b.slice(1, 3) instanceof Buffer));
console.log("name:" + Buffer.name);

// Buffer(number, "encoding") routes to the string form and throws.
try {
  new Buffer(42, "utf8");
  console.log("numEnc:NO-THROW");
} catch (e) {
  console.log("numEnc:" + e.code);
}
