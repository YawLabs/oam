// crypto: published vectors, streaming, encodings, KeyObject, timing-safe.
import { createHash, createHmac, createSecretKey, timingSafeEqual, getHashes } from "node:crypto";

console.log(createHash("sha256").update("abc").digest("hex"));
console.log(createHash("sha1").update("abc").digest("hex"));
console.log(createHash("md5").update("abc").digest("hex"));
console.log(createHash("sha512").update("abc").digest("base64").slice(0, 24));
const chunked = createHash("sha256").update("a").update("b");
const forked = chunked.copy();
console.log(chunked.update("c").digest("hex") === createHash("sha256").update("abc").digest("hex"));
console.log(forked.update("X").digest("hex") === createHash("sha256").update("abX").digest("hex"));
console.log(createHmac("sha256", "key").update("The quick brown fox jumps over the lazy dog").digest("hex"));
const keyObject = createSecretKey(Buffer.from("key"));
console.log(keyObject.type, keyObject.symmetricKeySize);
console.log(createHmac("sha256", keyObject).update("The quick brown fox jumps over the lazy dog").digest("hex").slice(0, 16));
console.log(timingSafeEqual(Buffer.from("same"), Buffer.from("same")), timingSafeEqual(Buffer.from("same"), Buffer.from("diff")));
console.log(getHashes().includes("sha256"));
const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode("abc"));
console.log(digest instanceof ArrayBuffer, Buffer.from(digest).toString("hex").slice(0, 16));
