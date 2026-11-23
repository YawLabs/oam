// node 26's crypto.argon2 / argon2Sync (RFC 9106) and crypto.randomUUIDv7.
//
// argon2: the derived tags for all three variants, with a secret and
// associated data, with parallelism above 1, from a string / Buffer /
// ArrayBuffer message, node's parameter checks in node's order, and the
// async form's callback (called asynchronously). Associated data
// stays within the 32 bytes oam's argon2 accepts (docs/node-divergences.md).
// randomUUIDv7: the version / variant nibbles, the 48-bit millisecond
// timestamp, and the options checks. No random bytes are printed.
import crypto from "node:crypto";

const show = (label, fn) => {
  try {
    const r = fn();
    console.log(label, Buffer.isBuffer(r) ? r.toString("hex") : r);
  } catch (e) {
    console.log(label, "threw", e.name, e.code, e.message);
  }
};
const base = { message: "pw", nonce: Buffer.alloc(16, 1), parallelism: 1, tagLength: 32, memory: 64, passes: 3 };

for (const alg of ["argon2d", "argon2i", "argon2id"]) show(alg, () => crypto.argon2Sync(alg, base));
show("secret+ad", () =>
  crypto.argon2Sync("argon2id", { ...base, secret: Buffer.alloc(8, 2), associatedData: Buffer.alloc(12, 3) }));
show("ad32", () => crypto.argon2Sync("argon2id", { ...base, associatedData: Buffer.alloc(32, 3) }));
show("p2", () => crypto.argon2Sync("argon2i", { ...base, parallelism: 2, memory: 16, tagLength: 4 }));
show("arraybuffer", () => crypto.argon2Sync("argon2id", { ...base, message: new ArrayBuffer(3) }));
show("uint8array nonce", () => crypto.argon2Sync("argon2d", { ...base, nonce: new Uint8Array(8).fill(9) }));
show("long tag", () => crypto.argon2Sync("argon2id", { ...base, tagLength: 100 }).length);

show("bad alg", () => crypto.argon2Sync("argon2x", base));
show("alg type", () => crypto.argon2Sync(5, base));
show("params null", () => crypto.argon2Sync("argon2id", null));
show("message type", () => crypto.argon2Sync("argon2id", { ...base, message: 5 }));
show("nonce short", () => crypto.argon2Sync("argon2id", { ...base, nonce: Buffer.alloc(7) }));
show("parallelism 0", () => crypto.argon2Sync("argon2id", { ...base, parallelism: 0 }));
show("tagLength 3", () => crypto.argon2Sync("argon2id", { ...base, tagLength: 3 }));
show("memory low", () => crypto.argon2Sync("argon2id", { ...base, parallelism: 2, memory: 15 }));
show("passes 0", () => crypto.argon2Sync("argon2id", { ...base, passes: 0 }));
show("passes type", () => crypto.argon2Sync("argon2id", { ...base, passes: "1" }));
show("no callback", () => crypto.argon2("argon2id", base));

let sync = true;
crypto.argon2("argon2id", base, (err, derived) => {
  console.log("callback async", !sync, err, derived.toString("hex"));
});
sync = false;

const before = Date.now();
const id = crypto.randomUUIDv7();
const after = Date.now();
console.log("uuidv7 shape", /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id));
const ms = parseInt(id.replace(/-/g, "").slice(0, 12), 16);
console.log("uuidv7 timestamp", ms >= before && ms <= after);
console.log("uuidv7 distinct", crypto.randomUUIDv7() !== crypto.randomUUIDv7());
console.log("uuidv7 uncached", /^[0-9a-f-]{36}$/.test(crypto.randomUUIDv7({ disableEntropyCache: true })));
console.log("uuidv7 length", crypto.randomUUIDv7.length);
for (const bad of [5, null, [], { disableEntropyCache: 1 }]) show("uuidv7 options", () => crypto.randomUUIDv7(bad));
console.log("RSA_SSLV23_PADDING", crypto.constants.RSA_SSLV23_PADDING);
