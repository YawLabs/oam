// fetch() decodes a response body by undici's content-encoding rules (#143).
// node's fetch (undici 6.24.1) reads every content-encoding line, joined and
// split on commas, lowercased and trimmed, and decodes gzip / x-gzip, deflate
// (zlib-wrapped or raw, told apart by the first byte) and br, stacked, last
// coding first, up to five; a sixth fails the fetch at the response head. Any
// other token -- an empty one, `identity` -- leaves the body undecoded.
// HEAD, 204 and 304 responses are never decoded. A truncated body is the data
// that decodes, not an error; an empty one is empty. A multi-member gzip body
// is every member. Chunks come out at most 16 KiB each, however far a small
// frame inflates.
//
// oam used to decode an exact lowercase single `gzip` or `deflate` (zlib)
// through reqwest and nothing else: x-gzip, GZIP, raw deflate, br and stacked
// codings arrived compressed, and a truncated gzip body was an error.
//
// The server is a raw socket writing fixed bytes (compressed once by node's
// zlib, embedded below), so the only runtime under test is the fetch client:
// oam's zlib compresses to different bytes and its http server drops a 304's
// content-length. oam strips content-encoding and content-length from a
// DECODED response (divergence #32), so those headers are printed only where
// node and oam both keep them: undecoded bodies and HEAD / 204 / 304. A decode
// failure is printed as `rejected` only: node's is a TypeError "terminated",
// oam's a plain Error.
import net from "node:net";

const text = "The quick brown fox jumps over the lazy dog. ".repeat(200);
const b64 = {
  gzip:
    "H4sIAAAAAAAACu3K4RWBUAAG0FW+CUzTAuQpUY8SZXpzOOfe37fpS57rtb3lNNfPlEvdMqzjY0l9lzmvvuR+/O451+6QRpZl" +
    "WZZlWZZlWZZlWZZlWZZlWZZlWZZlWZb/M/8AaS+LkygjAAA=",
  deflate:
    "eJztyuEVgVAABtBVvglM0wLkKVGPEmV6czjn3t+36Uue67W95TTXz5RL3TKs42NJfZc5r77kfvzuOdfukEaWZVmWZVmWZVmW" +
    "ZVmWZVmWZVmWZVmWZVmW/zP/AIZBny0=",
  deflateRaw:
    "7crhFYFQAAbQVb4JTNMC5ClRjxJlenM4597ft+lLnuu1veU018+US90yrONjSX2XOa++5H787jnX7pBGlmVZlmVZlmVZlmVZ" +
    "lmVZlmVZlmVZlmVZlv8z/wA=",
  br:
    "GycjiCwOeNPQlV2XELsXK6nK0JLMjK1BXObyNsgZnp4Ke4MNOHBIIG80uEGnFc4cHieqKTjCqdUA2KfB",
  multiMember:
    "H4sIAAAAAAAACkvLLCouUQAA/HrxHAYAAAAfiwgAAAAAAAAKK05Nzs9LAQBpER+2BgAAAA==",
  gzipThenBr:
    "CzWAH4sIAAAAAAAACu3K4RWBUAAG0FW+CUzTAuQpUY8SZXpzOOfe37fpS57rtb3lNNfPlEvdMqzjY0l9lzmvvuR+/O451+6Q" +
    "RpZlWZZlWZZlWZZlWZZlWZZlWZZlWZZlWZb/M/8AaS+LkygjAAAD",
  fiveGzip:
    "H4sIAAAAAAAACpPv5mAAA67J758lMDCvO/l+vqqBwXrrn3X7VjUefL33d7HXZ3G/n1+D6/7qrn1t/iX5f03Fj6AZL/+Wz54pxcR13OzKu5hNfloHpiQuT47pkQywZ0h95CHgATQSAI6bNfxbAAAA",
  junk:
    "H4sIAAAAAAAACstIzcnJBwCGphA2BQAAAGp1bmsh",
  zeros:
    "H4sIAAAAAAACCu3BAQEAAACCIP+vbkhAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" +
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB8G2pARxEAAEAA",
};
const fix = Object.fromEntries(Object.entries(b64).map(([k, v]) => [k, Buffer.from(v, "base64")]));

// path -> [status, [header lines], body]
const routes = {
  "/gzip": [200, ["content-encoding: gzip"], fix.gzip],
  "/x-gzip": [200, ["content-encoding: x-gzip"], fix.gzip],
  "/GZIP": [200, ["content-encoding: GZIP"], fix.gzip],
  "/deflate-zlib": [200, ["content-encoding: deflate"], fix.deflate],
  "/deflate-raw": [200, ["content-encoding: deflate"], fix.deflateRaw],
  "/br": [200, ["content-encoding: br"], fix.br],
  "/multi-member": [200, ["content-encoding: gzip"], fix.multiMember],
  "/two-lines": [200, ["content-encoding: gzip", "content-encoding: br"], fix.gzipThenBr],
  "/stacked": [200, ["content-encoding: gzip, br"], fix.gzipThenBr],
  "/five": [200, ["content-encoding: gzip, gzip, x-gzip, gzip, gzip"], fix.fiveGzip],
  "/identity-first": [200, ["content-encoding: identity, gzip"], fix.gzip],
  "/trailing-comma": [200, ["content-encoding: gzip,"], fix.gzip],
  "/identity-last": [200, ["content-encoding: gzip, identity"], fix.gzip],
  "/unknown": [200, ["content-encoding: compress"], fix.gzip],
  "/six": [200, ["content-encoding: gzip, gzip, gzip, gzip, gzip, gzip"], fix.gzip],
  "/empty-gzip": [200, ["content-encoding: gzip"], Buffer.alloc(0)],
  "/202": [202, ["content-encoding: gzip"], Buffer.alloc(0)],
  "/truncated": [200, ["content-encoding: gzip"], fix.gzip.subarray(0, 60)],
  "/junk": [200, ["content-encoding: gzip"], fix.junk],
  "/zeros": [200, ["content-encoding: gzip"], fix.zeros],
};

const server = net.createServer((socket) => {
  let head = "";
  socket.setEncoding("latin1");
  socket.on("data", (chunk) => {
    head += chunk;
    if (!head.includes("\r\n\r\n")) return;
    socket.removeAllListeners("data");
    const [method, path] = head.split(" ");
    let status;
    let lines;
    let body;
    let length;
    if (path === "/204") {
      [status, lines, body, length] = [204, ["content-encoding: gzip"], Buffer.alloc(0), null];
    } else if (path === "/304") {
      [status, lines, body, length] = [304, ["content-encoding: gzip"], Buffer.alloc(0), fix.gzip.length];
    } else {
      [status, lines, body] = routes[path] ?? [404, [], Buffer.alloc(0)];
      length = body.length;
    }
    const wire = [`HTTP/1.1 ${status} Whatever`, ...lines];
    if (length !== null) wire.push(`content-length: ${length}`);
    wire.push("connection: close", "", "");
    socket.write(wire.join("\r\n"), "latin1");
    if (method !== "HEAD" && body.length > 0) socket.write(body);
    socket.end();
  });
  socket.on("error", () => {});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const base = `http://127.0.0.1:${server.address().port}`;

function describe(bytes) {
  const s = Buffer.from(bytes).toString("utf8");
  if (s === text) return "text";
  if (bytes.length === 0) return "''";
  if (bytes[0] === 0x1f && bytes[1] === 0x8b) return `${bytes.length} bytes, still gzip`;
  return `${bytes.length} bytes ${JSON.stringify(s.slice(0, 32))}`;
}

async function get(path, { headers = false, method = "GET" } = {}) {
  try {
    const res = await fetch(base + path, { method });
    const out = [path, method, res.status];
    if (headers) out.push(`ce=${res.headers.get("content-encoding")}`, `cl=${res.headers.get("content-length")}`);
    try {
      out.push(describe(new Uint8Array(await res.arrayBuffer())));
    } catch {
      out.push("rejected: true");
    }
    console.log(out.join(" "));
  } catch (e) {
    console.log(path, method, e.constructor.name, e.message, "cause:", e.cause?.message);
  }
}

for (const path of ["/gzip", "/x-gzip", "/GZIP", "/deflate-zlib", "/deflate-raw", "/br", "/multi-member", "/two-lines", "/stacked", "/five"]) {
  await get(path);
}
for (const path of ["/trailing-comma", "/identity-first", "/identity-last", "/unknown"]) {
  await get(path, { headers: true });
}
for (const path of ["/six", "/empty-gzip", "/202", "/truncated", "/junk"]) {
  await get(path);
}
await get("/gzip", { method: "HEAD", headers: true });
await get("/204", { headers: true });
await get("/304", { headers: true });

// A 4 MiB body of zeros, 4 KiB of gzip: read chunk by chunk.
{
  const res = await fetch(base + "/zeros");
  let total = 0;
  let largest = 0;
  for await (const chunk of res.body) {
    total += chunk.length;
    largest = Math.max(largest, chunk.length);
  }
  console.log("/zeros total", total, "largest <= 16384:", largest <= 16384, "largest > 0:", largest > 0);
}

server.close();
