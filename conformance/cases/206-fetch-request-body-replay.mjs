// A Request body that is not a caller's stream keeps its length, and can be
// sent again on a 307, however the Request got a stream of its own. Measured
// on node v22.22.2 first: undici keeps the body's source, so it sends
// `content-length` and replays the body on the redirect. oam sent any body
// with a stream as a chunked upload, which a 307 cannot replay, once
// `.body` had been read -- on the Request itself, before a `clone()` or
// before `new Request(request)` -- and always for a Blob that is not oam's
// own (one that only passes `instanceof Blob`).
//
//   - A string or an oam Blob body: content-length on both hops, even
//     after `.body` was read.
//   - A foreign Blob: its bytes with its `size` as the length; a stream that
//     comes out another length fails as undici's length check does, and a
//     chunk that is not bytes fails with node's error.
//   - A caller's ReadableStream still goes out chunked and still fails a
//     307 (undici's `source` is null for it).
//
// Not asserted: a Uint8Array body after `.body` was read. node v22.22.2
// sends it once and then fails the redirect with `Cannot perform
// ArrayBuffer.prototype.slice on a detached ArrayBuffer` (the stream `.body`
// made transferred the bytes away); oam replays it as it does a string.
import http from "node:http";

const log = [];
const srv = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    log.push([
      req.url,
      req.headers["content-length"] ?? null,
      req.headers["transfer-encoding"] ?? null,
      Buffer.concat(chunks).toString(),
    ]);
    if (req.url === "/r") {
      res.writeHead(307, { location: "/done" });
      res.end();
    } else res.end("ok");
  });
});
await new Promise((resolve) => srv.listen(0, "127.0.0.1", resolve));
const U = `http://127.0.0.1:${srv.address().port}/`;

const foreignBlob = (chunks, size) => ({
  __proto__: Blob.prototype,
  size,
  type: "",
  stream: () =>
    new ReadableStream({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(chunk);
        controller.close();
      },
    }),
  [Symbol.toStringTag]: "Blob",
});
const touched = (body) => {
  const request = new Request(`${U}r`, { method: "POST", body });
  void request.body;
  return request;
};

const cases = {
  "string, .body read": () => [touched("hello")],
  "oam Blob, .body read": () => [touched(new Blob(["hello"]))],
  "clone after .body": () => [touched("hello").clone()],
  "clone, clone's .body read": () => {
    const copy = new Request(`${U}r`, { method: "POST", body: "hello" }).clone();
    void copy.body;
    return [copy];
  },
  "new Request(request) after .body": () => [new Request(touched("hello"))],
  "Request input, init body": () => [touched("x"), { body: "hello" }],
  "foreign Blob": () => [`${U}r`, { method: "POST", body: foreignBlob([new Uint8Array([104, 105])], 2) }],
  "foreign Blob, string chunk": () => [`${U}r`, { method: "POST", body: foreignBlob(["hello"], 5) }],
  "foreign Blob in a Request": () => [
    new Request(`${U}r`, { method: "POST", body: foreignBlob(["hello"], 5) }),
  ],
  "foreign Blob, short": () => [`${U}r`, { method: "POST", body: foreignBlob([new Uint8Array(2)], 5) }],
  "foreign Blob, ArrayBuffer chunk": () => [
    `${U}r`,
    { method: "POST", body: foreignBlob([new ArrayBuffer(2)], 2) },
  ],
  "caller stream": () => [
    `${U}r`,
    { method: "POST", duplex: "half", body: new Blob(["hello"]).stream() },
  ],
};
for (const [label, make] of Object.entries(cases)) {
  log.length = 0;
  let out;
  try {
    const res = await fetch(...make());
    out = `${res.status} redirected=${res.redirected} ${await res.text()}`;
  } catch (e) {
    out = `${e.name}: ${e.message} | ${e.cause?.name}: ${e.cause?.message}`;
  }
  console.log(label.padEnd(34), out, JSON.stringify(log));
}
srv.close();
