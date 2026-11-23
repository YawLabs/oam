// node 26's crypto.createMac / crypto.getMacs: every MAC family node's
// OpenSSL lists, its option rules and its error shapes.
//
// Pins the tags (HMAC, CMAC over AES and 3DES, GMAC with 96-bit and other
// IV lengths, KMAC128/256 with customization and output length, keyed
// BLAKE2b/BLAKE2s with salt and personalization, SipHash-2-4 at 64 and 128
// bits, Poly1305), node's argument checks in node's order ("required",
// GMAC's non-empty iv, "not supported by MAC <name>"), and the OpenSSL
// errors for bad key / customization / salt / output lengths and cipher
// modes, with their library / reason / code fields. HMAC digests stay within
// the six oam's createHmac ships (docs/node-divergences.md).
import crypto from "node:crypto";
import { Transform } from "node:stream";

const key = Buffer.from("000102030405060708090a0b0c0d0e0f", "hex");
const show = (label, fn) => {
  try {
    console.log(label, fn());
  } catch (e) {
    console.log(label, "threw", e.name, e.code, e.message, e.library, e.reason);
  }
};
const tag = (alg, k, options, data = "abc") =>
  crypto.createMac(alg, k, options).update(data).final("hex");

console.log(crypto.getMacs().join(","));

show("hmac", () => tag("hmac", key, { digest: "sha256" }));
show("hmac == createHmac", () =>
  tag("hmac", key, { digest: "SHA-512" }) ===
    crypto.createHmac("sha512", key).update("abc").digest("hex"));
show("hmac empty key", () => tag("hmac", Buffer.alloc(0), { digest: "sha1" }, ""));
show("hmac KeyObject", () =>
  crypto.createMac("HMAC", crypto.createSecretKey(key), { digest: "sha1" })
    .update(Buffer.from("abc")).final("base64"));
show("cmac aes-128", () => tag("cmac", key, { cipher: "aes-128-cbc" }));
show("cmac aes-192", () => tag("cmac", Buffer.alloc(24, 3), { cipher: "aes-192-cbc" }));
show("cmac aes-256", () => tag("cmac", Buffer.alloc(32, 7), { cipher: "AES-256-CBC" }));
show("cmac 3des", () => tag("cmac", Buffer.alloc(24, 3), { cipher: "des-ede3-cbc" }));
show("gmac iv12", () => tag("gmac", key, { cipher: "aes-128-gcm", iv: Buffer.alloc(12) }));
show("gmac iv1", () => tag("gmac", key, { cipher: "aes-128-gcm", iv: Buffer.alloc(1) }));
show("gmac iv100", () => tag("gmac", key, { cipher: "aes-128-gcm", iv: Buffer.alloc(100, 2) }));
show("gmac 256", () => tag("gmac", Buffer.alloc(32, 7), { cipher: "aes-256-gcm", iv: Buffer.alloc(12, 1) }));
show("kmac128", () => tag("kmac128", key));
show("kmac-128 alias", () => tag("KMAC-128", key));
show("kmac128 custom", () => tag("kmac128", key, { customization: Buffer.from("cust"), outputLength: 20 }));
show("kmac128 out0", () => tag("kmac128", key, { outputLength: 0 }));
show("kmac256", () => tag("kmac256", key));
show("kmac key4", () => tag("kmac128", Buffer.alloc(4, 1)));
show("blake2bmac", () => tag("blake2bmac", key));
show("blake2bmac out32", () => tag("blake2bmac", key, { outputLength: 32 }));
show("blake2bmac salt", () => tag("blake2bmac", key, { salt: Buffer.alloc(16, 1) }));
show("blake2bmac personal", () => tag("blake2bmac", key, { customization: Buffer.alloc(16, 1) }));
show("blake2smac", () => tag("blake2smac", key));
show("blake2smac salt+personal", () =>
  tag("blake2smac", key, { salt: Buffer.alloc(8, 1), customization: Buffer.alloc(8, 2) }));
show("siphash", () => tag("siphash", key));
show("siphash 8", () => tag("siphash", key, { outputLength: 8 }));
show("poly1305", () => tag("poly1305", Buffer.alloc(32, 7)));
show("streamed", () => {
  const m = crypto.createMac("hmac", key, { digest: "sha1" });
  m.update(Buffer.from("a"));
  m.update("62", "hex");
  return m.final("base64url");
});
show("final buffer", () => crypto.createMac("hmac", key, { digest: "md5" }).final().length);
show("prototype", () => Object.getOwnPropertyNames(Object.getPrototypeOf(crypto.createMac("hmac", key, { digest: "sha1" }))).join(","));
show("is a stream", () => crypto.createMac("hmac", key, { digest: "sha1" }) instanceof Transform);

// Argument checks.
show("no digest", () => crypto.createMac("hmac", key));
show("no cipher", () => crypto.createMac("cmac", key, { digest: "sha1" }));
show("gmac no iv", () => crypto.createMac("gmac", key, { cipher: "aes-128-gcm", salt: Buffer.alloc(2) }));
show("unknown mac", () => crypto.createMac("nope", key, { digest: "sha1" }));
show("empty name", () => crypto.createMac("", key));
show("NUL name", () => crypto.createMac("hm\0ac", key));
show("name type", () => crypto.createMac(5, key));
show("string key", () => crypto.createMac("hmac", "str", { digest: "sha256" }));
show("options type", () => crypto.createMac("hmac", key, 5));
show("digest type", () => crypto.createMac("hmac", key, { digest: 5 }));
show("custom type", () => crypto.createMac("kmac256", key, { customization: "x" }));
show("iv unsupported", () => crypto.createMac("HMAC", key, { digest: "sha1", iv: Buffer.alloc(2) }));
show("salt unsupported", () => crypto.createMac("KMAC-128", key, { salt: Buffer.alloc(2) }));
show("outputLength unsupported", () => crypto.createMac("hmac", key, { digest: "sha256", outputLength: 10 }));
show("outputLength range", () => crypto.createMac("kmac128", key, { outputLength: -1 }));

// OpenSSL's parameter errors.
show("unknown digest", () => crypto.createMac("hmac", key, { digest: "nosuch" }));
show("unknown cipher", () => crypto.createMac("cmac", key, { cipher: "nosuch" }));
show("cmac ecb", () => crypto.createMac("cmac", key, { cipher: "aes-128-ecb" }));
show("cmac key", () => crypto.createMac("cmac", Buffer.alloc(32), { cipher: "aes-128-cbc" }));
show("gmac cbc", () => crypto.createMac("gmac", key, { cipher: "aes-128-cbc", iv: Buffer.alloc(12) }));
show("gmac key", () => crypto.createMac("gmac", Buffer.alloc(10), { cipher: "aes-128-gcm", iv: Buffer.alloc(12) }));
show("kmac key3", () => crypto.createMac("kmac128", Buffer.alloc(3, 1)));
show("kmac key513", () => crypto.createMac("kmac128", Buffer.alloc(513, 1)));
show("kmac custom513", () => crypto.createMac("kmac128", key, { customization: Buffer.alloc(513) }));
show("kmac out", () => crypto.createMac("kmac128", key, { outputLength: 2097152 }));
show("blake2b out0", () => crypto.createMac("blake2bmac", key, { outputLength: 0 }));
show("blake2b key65", () => crypto.createMac("blake2bmac", Buffer.alloc(65, 1)));
show("blake2b salt17", () => crypto.createMac("blake2bmac", key, { salt: Buffer.alloc(17) }));
show("blake2b personal17", () => crypto.createMac("blake2bmac", key, { customization: Buffer.alloc(17) }));
show("siphash out4", () => crypto.createMac("siphash", key, { outputLength: 4 }));
show("siphash key8", () => crypto.createMac("siphash", Buffer.alloc(8)));
show("poly1305 key16", () => crypto.createMac("poly1305", key));

// Use after final.
show("final twice", () => {
  const m = crypto.createMac("hmac", key, { digest: "sha256" });
  m.final();
  return m.final();
});
show("update after final", () => {
  const m = crypto.createMac("hmac", key, { digest: "sha256" });
  m.final();
  return m.update("x");
});
show("bad output encoding", () => crypto.createMac("hmac", key, { digest: "sha256" }).final("bogus"));
show("bad data", () => crypto.createMac("hmac", key, { digest: "sha1" }).update(5));
show("bad input encoding", () => crypto.createMac("hmac", key, { digest: "sha1" }).update("ab", "nope"));
