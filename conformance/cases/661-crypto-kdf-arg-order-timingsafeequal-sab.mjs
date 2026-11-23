// crypto.pbkdf2 / crypto.scrypt check their arguments before the callback,
// and crypto.timingSafeEqual takes any ArrayBuffer, pinned against node on
// the same host.
//
// node validates pbkdf2's digest (a string), password, salt, iterations and
// keylen, and scrypt's password, salt, keylen and options, BEFORE it looks
// at the callback, so a bad argument is reported as itself even when the
// callback is missing. oam checked the callback first and handed every
// other failure to the callback. timingSafeEqual refused a SharedArrayBuffer
// and another realm's ArrayBuffer (an `instanceof ArrayBuffer` test), which
// node compares as any other buffer.
import crypto from "node:crypto";
import vm from "node:vm";

const report = (label, fn) => {
  try {
    const value = fn();
    console.log(label, "returned", value instanceof Uint8Array ? value.toString("hex") : value);
  } catch (err) {
    console.log(label, err.name, err.code, err.message);
  }
};

const shared = new SharedArrayBuffer(4);
new Uint8Array(shared).set([1, 2, 3, 4]);
report("tse shared", () => crypto.timingSafeEqual(shared, new Uint8Array([1, 2, 3, 4])));
report("tse shared view", () => crypto.timingSafeEqual(new Uint8Array([1, 2, 3, 5]), new Uint8Array(shared)));
report("tse other realm", () => crypto.timingSafeEqual(vm.runInNewContext("new Uint8Array([9]).buffer"), new Uint8Array([9]).buffer));
report("tse not binary 1", () => crypto.timingSafeEqual({}, shared));
report("tse not binary 2", () => crypto.timingSafeEqual(shared, "abcd"));
report("tse length", () => crypto.timingSafeEqual(shared, new ArrayBuffer(3)));

// pbkdf2: each bad argument with no callback, then a bad callback.
report("pbkdf2 iterations", () => crypto.pbkdf2("p", "s", -1, 32, "sha256"));
report("pbkdf2 iterations fraction", () => crypto.pbkdf2("p", "s", 1.5, 32, "sha256", () => {}));
report("pbkdf2 iterations string", () => crypto.pbkdf2("p", "s", "1", 32, "sha256"));
report("pbkdf2 keylen", () => crypto.pbkdf2("p", "s", 1, -1, "sha256", "x"));
report("pbkdf2 password", () => crypto.pbkdf2(1, "s", 1, 32, "sha256"));
report("pbkdf2 salt", () => crypto.pbkdf2("p", {}, 1, 32, "sha256", "x"));
report("pbkdf2 digest type", () => crypto.pbkdf2("p", "s", 1, 32, 1));
report("pbkdf2 digest is the callback", () => crypto.pbkdf2("p", "s", 1, 32, () => {}));
report("pbkdf2 no callback", () => crypto.pbkdf2("p", "s", 1, 32, "sha256"));
report("pbkdf2 unknown digest, no callback", () => crypto.pbkdf2("p", "s", 1, 32, "nope"));
report("pbkdf2Sync iterations 0", () => crypto.pbkdf2Sync("p", "s", 0, 32, "sha256"));
report("pbkdf2Sync iterations 2**31", () => crypto.pbkdf2Sync("p", "s", 2 ** 31, 32, "sha256"));
report("pbkdf2Sync digest missing", () => crypto.pbkdf2Sync("p", "s", 1, 32));
report("pbkdf2Sync shared", () => crypto.pbkdf2Sync(shared, new DataView(shared), 1, 8, "sha256"));

// scrypt: the options are checked before the callback; with four arguments
// the fourth is the callback and there are no options.
report("scrypt keylen", () => crypto.scrypt("p", "s", -1));
report("scrypt password", () => crypto.scrypt(1, "s", 16));
report("scrypt N range", () => crypto.scrypt("p", "s", 16, { N: -1 }, "x"));
report("scrypt N type", () => crypto.scrypt("p", "s", 16, { N: "16" }, "x"));
report("scrypt N and cost", () => crypto.scrypt("p", "s", 16, { N: 16, cost: 16 }, "x"));
report("scrypt maxmem", () => crypto.scrypt("p", "s", 16, { maxmem: -1 }, "x"));
report("scrypt four arguments", () => crypto.scrypt("p", "s", 16, { N: -1 }));
report("scrypt null options", () => crypto.scrypt("p", "s", 16, null, "x"));
report("scrypt N not a power of 2, no callback", () => crypto.scrypt("p", "s", 16, { N: 3 }, "x"));
report("scryptSync r and blockSize", () => crypto.scryptSync("p", "s", 16, { r: 8, blockSize: 8 }));
report("scryptSync r fraction", () => crypto.scryptSync("p", "s", 16, { r: 1.5 }));
report("scryptSync zero is the default", () => crypto.scryptSync("password", "salt", 8, { N: 1024, r: 0, p: 0 }));
report("scryptSync shared", () => crypto.scryptSync(shared, new Uint8Array(shared), 8, { N: 16 }));
try {
  crypto.scrypt("p", "s", 16, { N: 2 ** 20 }, () => {});
  console.log("scrypt over maxmem", "returned");
} catch (err) {
  // The message text is oam's own (docs/node-divergences.md).
  console.log("scrypt over maxmem", err.name, err.code);
}

crypto.pbkdf2("password", "salt", 2, 20, "sha1", (err, key) => {
  console.log("pbkdf2 callback", err, key.toString("hex"));
  crypto.scrypt("password", "salt", 8, { N: 16 }, (err2, key2) => {
    console.log("scrypt callback", err2, key2.toString("hex"));
    crypto.scrypt("password", "salt", 8, (err3, key3) => {
      console.log("scrypt callback, no options", err3, key3.toString("hex"));
    });
  });
});
