// A BYOB read of a byte stream (#372): read(view) fills at most the view it
// is given and leaves the rest of a queued chunk for the next read (the
// Streams standard's "fill pull-into descriptor from queue"); the view's
// buffer is transferred, and what comes back is a view of the view's own
// type over it. Views smaller than, equal to and larger than the queued
// chunk, several chunks into one view, a Uint16Array and a DataView, a view
// at an offset, a read waiting for an enqueue, the source answering through
// controller.byobRequest, close with a read pending, cancel and release,
// the argument checks -- and a Blob's stream read the same way.
const bytesOf = (v) => [...new Uint8Array(v.buffer, v.byteOffset, v.byteLength)];
const show = (r) =>
  r.value === undefined
    ? `undefined done=${r.done}`
    : `${r.value.constructor.name}(${r.value.length ?? r.value.byteLength}) [${bytesOf(r.value)}] offset=${r.value.byteOffset} buffer=${r.value.buffer.byteLength} done=${r.done}`;
const err = (e) => `${e.constructor.name} ${e.code} ${e.message}`;
const stream = (chunks, close = true) =>
  new ReadableStream({
    type: "bytes",
    start(c) {
      for (const x of chunks) c.enqueue(new Uint8Array(x));
      if (close) c.close();
    },
  });
async function run(label, chunks, views) {
  const reader = stream(chunks).getReader({ mode: "byob" });
  console.log(label);
  for (const v of views) {
    const view = typeof v === "number" ? new Uint8Array(v) : v;
    try {
      const r = await reader.read(view);
      console.log(`  ${show(r)} source detached=${view.buffer.detached}`);
    } catch (e) {
      console.log(`  rejects ${err(e)}`);
    }
  }
}
await run("a view smaller than the chunk", [[1, 2, 3]], [2, 2, 2]);
await run("a view equal to the chunk", [[1, 2, 3]], [3, 3]);
await run("a view larger than the chunk", [[1, 2, 3]], [5, 5]);
await run("several chunks into one view", [[1, 2], [3, 4], [5]], [4, 4, 4]);
await run("Uint16Array, an odd byte left at close", [[1, 2, 3]], [new Uint16Array(1), new Uint16Array(1), new Uint16Array(1)]);
await run("Uint16Array, larger", [[1, 2, 3, 4, 5]], [new Uint16Array(4), new Uint16Array(4)]);
await run("DataView", [[1, 2, 3]], [new DataView(new ArrayBuffer(2)), new DataView(new ArrayBuffer(4))]);
await run("a view at an offset", [[1, 2, 3]], [new Uint8Array(new ArrayBuffer(8), 2, 4), new Uint8Array(new ArrayBuffer(8), 2, 4)]);
await run("a Buffer", [[1, 2, 3]], [Buffer.alloc(2), Buffer.alloc(2)]);
{
  const reader = stream([[1, 2], [3, 4]]).getReader({ mode: "byob" });
  console.log("read with min 3:", show(await reader.read(new Uint8Array(4), { min: 3 })));
}

const pending = (p) =>
  Promise.race([p.then(show, (e) => `rejects ${err(e)}`), new Promise((res) => setTimeout(() => res("still pending"), 30))]);

// A read waiting for an enqueue takes what fits; the rest stays queued.
{
  let ctl;
  const reader = new ReadableStream({ type: "bytes", start(c) { ctl = c; } }).getReader({ mode: "byob" });
  const p = reader.read(new Uint8Array(2));
  console.log("byobRequest while a read waits:", ctl.byobRequest?.view?.byteLength, ctl.byobRequest === ctl.byobRequest);
  ctl.enqueue(new Uint8Array([9, 8, 7]));
  console.log("waiting read:", show(await p), "byobRequest after:", ctl.byobRequest);
  console.log("next read:", show(await reader.read(new Uint8Array(4))));
  const p3 = reader.read(new Uint8Array(4));
  ctl.close();
  console.log("close with a read pending:", await pending(p3));
  ctl.byobRequest.respond(0);
  console.log("after respond(0):", show(await p3));
  console.log("closed:", await reader.closed);
}

// A source that writes into the view it is given.
{
  let n = 0;
  const reader = new ReadableStream({
    type: "bytes",
    pull(c) {
      const req = c.byobRequest;
      if (n === 2) {
        c.close();
        req.respond(0);
        return;
      }
      req.view[0] = 65 + n;
      req.view[1] = 97 + n;
      n++;
      req.respond(2);
    },
  }).getReader({ mode: "byob" });
  const out = [];
  for (;;) {
    const r = await reader.read(new Uint8Array(3));
    out.push(show(r));
    if (r.done) break;
  }
  console.log("respond:", out.join(" | "));
}
// respondWithNewView, and respond's checks.
{
  const results = [];
  const reader = new ReadableStream({
    type: "bytes",
    pull(c) {
      const req = c.byobRequest;
      const view = req.view;
      for (const bad of [() => req.respond(0), () => req.respond(99), () => req.respondWithNewView(new Uint8Array(2))]) {
        try { bad(); results.push("ok"); } catch (e) { results.push(err(e)); }
      }
      const fresh = new Uint8Array(view.buffer, view.byteOffset, 1);
      fresh[0] = 42;
      req.respondWithNewView(fresh);
      try { req.respond(1); } catch (e) { results.push(err(e)); }
    },
  }).getReader({ mode: "byob" });
  console.log("respondWithNewView:", show(await reader.read(new Uint8Array(3))));
  for (const r of results) console.log("  ", r);
}

// Cancel with a read pending; release with a read pending.
{
  const reader = new ReadableStream({ type: "bytes" }).getReader({ mode: "byob" });
  const p = reader.read(new Uint8Array(2));
  await reader.cancel("why");
  console.log("cancel with a read pending:", show(await p));
}
{
  let ctl;
  const s = new ReadableStream({ type: "bytes", start(c) { ctl = c; } });
  const reader = s.getReader({ mode: "byob" });
  const p = reader.read(new Uint8Array(4));
  reader.releaseLock();
  console.log("release with a read pending:", await pending(p));
  console.log("closed after release:", await reader.closed.then(() => "resolved", (e) => err(e)));
  const d = s.getReader();
  const q = d.read();
  ctl.enqueue(new Uint8Array([5, 6]));
  console.log("a default read after:", show(await q));
}

// An enqueue of a chunk an unfilled element needs, then close.
{
  let ctl;
  const reader = new ReadableStream({ type: "bytes", start(c) { ctl = c; } }).getReader({ mode: "byob" });
  const p = reader.read(new Uint16Array(2));
  ctl.enqueue(new Uint8Array([1]));
  let closing;
  try { ctl.close(); closing = "closed"; } catch (e) { closing = err(e); }
  console.log("close with half an element:", closing, await pending(p));
}

// The argument checks.
{
  const reader = stream([[1]]).getReader({ mode: "byob" });
  for (const [label, args] of [
    ["no view", []],
    ["an object", [{}]],
    ["an ArrayBuffer", [new ArrayBuffer(2)]],
    ["a zero-length view", [new Uint8Array(0)]],
    ["a detached view", [(() => { const v = new Uint8Array(2); v.buffer.transfer(); return v; })()]],
    ["options null", [new Uint8Array(1), null]],
    ["options 5", [new Uint8Array(1), 5]],
    ["min 0", [new Uint8Array(1), { min: 0 }]],
    ["min 1.5", [new Uint8Array(2), { min: 1.5 }]],
    ["min '1'", [new Uint8Array(2), { min: "1" }]],
    ["min > length", [new Uint16Array(2), { min: 3 }]],
    ["min > byteLength (DataView)", [new DataView(new ArrayBuffer(2)), { min: 3 }]],
  ]) {
    try {
      const p = reader.read(...args);
      console.log(label, "->", await p.then(show, (e) => `rejects ${err(e)}`));
    } catch (e) {
      console.log(label, "-> throws", err(e));
    }
  }
}
for (const [label, make] of [
  ["mode 'x'", () => stream([[1]]).getReader({ mode: "x" })],
  ["options 'byob'", () => stream([[1]]).getReader("byob")],
  ["options null", () => stream([[1]]).getReader(null)],
  ["a locked stream", () => { const s = stream([[1]]); s.getReader(); return s.getReader({ mode: "byob" }); }],
  ["not a byte stream", () => new ReadableStream().getReader({ mode: "byob" })],
]) {
  try {
    const r = make();
    console.log(label, "->", typeof r.read === "function" ? "a reader" : r);
  } catch (e) {
    console.log(label, "-> throws", e.constructor.name, e.code);
  }
}
{
  const reader = stream([[1]]).getReader({ mode: "byob" });
  console.log("reader:", Object.prototype.toString.call(reader), typeof reader.read, typeof reader.releaseLock, typeof reader.cancel, reader.closed instanceof Promise);
  reader.releaseLock();
  console.log("read after release:", await reader.read(new Uint8Array(1)).then(show, (e) => `rejects ${err(e)}`));
}
{
  const s = stream([[1]]);
  s.getReader({ mode: "byob" });
  console.log("a byob reader locks:", s.locked);
}
{
  const reader = new ReadableStream({ type: "bytes", start(c) { c.error(new Error("boom")); } }).getReader({ mode: "byob" });
  console.log("an errored stream:", await reader.read(new Uint8Array(1)).then(show, (e) => `rejects ${e.message}`));
}

// A Blob's stream through a BYOB reader.
async function blobRead(label, blob, size) {
  const reader = blob.stream().getReader({ mode: "byob" });
  const out = [];
  for (;;) {
    const r = await reader.read(new Uint8Array(size));
    out.push(`${r.value.length}${r.done ? " done" : ""}:${new TextDecoder().decode(r.value)}`);
    if (r.done) break;
  }
  console.log(label, out.join(" | "));
}
await blobRead("Blob, view 2:", new Blob(["abcde"]), 2);
await blobRead("Blob, view 5:", new Blob(["abcde"]), 5);
await blobRead("Blob, view 8:", new Blob(["abcde"]), 8);
await blobRead("Blob of parts, view 4:", new Blob(["ab", "cdefg", "hi"]), 4);
await blobRead("empty Blob:", new Blob([]), 4);

// The other byte streams the runtime makes, read through a BYOB reader to
// the end: each answers the read pending at its end.
{
  const fs = await import("node:fs");
  const os = await import("node:os");
  const path = await import("node:path");
  const http = await import("node:http");
  const drain = async (stream, size) => {
    const reader = stream.getReader({ mode: "byob" });
    const out = [];
    for (;;) {
      const r = await reader.read(new Uint8Array(size));
      out.push(`${r.value?.length}${r.done ? " done" : ""}`);
      if (r.done) break;
    }
    return out;
  };
  const f = path.join(os.tmpdir(), `oam-case-520-${process.pid}`);
  fs.writeFileSync(f, Buffer.alloc(40000, 7));
  const fh = await fs.promises.open(f);
  console.log("FileHandle.readableWebStream:", (await drain(fh.readableWebStream(), 30000)).join(","));
  await fh.close();
  fs.unlinkSync(f);
  console.log("Response of bytes:", (await drain(new Response(new Uint8Array(10)).body, 4)).join(","));
  const iterable = (async function* () { yield "abc"; yield "defgh"; })();
  console.log("Response of an async iterable:", (await drain(new Response(iterable).body, 4)).join(","));
  const server = http.createServer((req, res) => res.end("x".repeat(9)));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const res = await fetch(`http://127.0.0.1:${server.address().port}/`);
  const reads = await drain(res.body, 4);
  const last = reads.pop();
  console.log("fetch body: total", reads.reduce((n, x) => n + parseInt(x, 10), 0), "each <= 4", reads.every((x) => parseInt(x, 10) <= 4), "last", last);
  server.close();
}
