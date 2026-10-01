// An abort while fetch is still reading the bytes of a Blob that is not the
// runtime's own (its stream is all fetch can read, and undici sends a Blob's
// size as the content-length, so it reads the whole thing before the request
// goes out). node rejects at once with the signal's reason and sends nothing.
// oam checked the signal only before the read, so the request still went out
// and the fetch rejected late with a fresh AbortError instead of the reason.
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

let requests = 0;
const server = http.createServer((req, res) => {
  requests++;
  req.resume();
  req.on("end", () => res.end("ok"));
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const url = `http://127.0.0.1:${server.address().port}/`;

// A Blob by prototype only: a stream that takes 200 ms to give its 4 bytes.
const foreignBlob = () =>
  Object.create(Blob.prototype, {
    size: { value: 4 },
    type: { value: "" },
    stream: {
      value: () =>
        new ReadableStream({
          async pull(controller) {
            await new Promise((resolve) => setTimeout(resolve, 200));
            controller.enqueue(new Uint8Array([1, 2, 3, 4]));
            controller.close();
          },
        }),
    },
  });

async function attempt(label, reason) {
  const ac = new AbortController();
  setTimeout(() => ac.abort(reason), 50);
  const t0 = Date.now();
  try {
    const res = await fetch(url, { method: "POST", body: foreignBlob(), signal: ac.signal });
    console.log(label, "resolved", res.status, await res.text());
  } catch (e) {
    console.log(
      label,
      "rejected",
      e?.name,
      e?.message,
      "is the reason:",
      e === ac.signal.reason,
      "before the Blob was read:",
      Date.now() - t0 < 150,
    );
  }
}

await attempt("abort(error)", new Error("stop"));
await attempt("abort()", undefined);
// Give a request that did go out time to reach the server.
await new Promise((resolve) => setTimeout(resolve, 400));
console.log("requests the server saw:", requests);

// Not aborted: the Blob's bytes are sent.
{
  const res = await fetch(url, { method: "POST", body: foreignBlob() });
  console.log("not aborted:", res.status, await res.text(), "requests:", requests);
}
server.close();
