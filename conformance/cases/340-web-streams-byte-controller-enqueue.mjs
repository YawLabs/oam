// A `type: 'bytes'` stream's controller checks a chunk the way node's
// ReadableByteStreamController.enqueue does, before anything is queued,
// and hands a default reader a Uint8Array over the chunk's bytes.
//
// oam ran the ArrayBufferView check as the queue's size function, so a
// rejected chunk ERRORED the stream (the next enqueue threw "Controller is
// already closed" and a read rejected with the first chunk's error), and
// with a read already waiting the check never ran at all: a string or a
// number reached the reader. Any view came back as the view it was
// enqueued as (an Int16Array, a DataView, a Buffer), where node always
// gives a Uint8Array. Also pinned: the closed-controller errors, which
// tell a closing controller from a stream that is no longer readable, and
// the coded errors' rendering. Measured on node v22.22.2.
const show = (tag, e) =>
  console.log(tag, e?.constructor?.name, e?.code, String(e), e instanceof TypeError);

// A rejected chunk leaves the stream readable.
{
  let c;
  const rs = new ReadableStream({ type: "bytes", start(x) { c = x; } });
  try { c.enqueue("str"); } catch (e) { show("bad-chunk", e); }
  try { c.enqueue(new Uint8Array([1])); console.log("next-enqueue ok", c.desiredSize); } catch (e) { show("next-enqueue", e); }
  const r = await rs.getReader().read().then((v) => v, (e) => e);
  console.log("read", Object.prototype.toString.call(r.value), r.value?.length, r.done);
}

// With a read waiting, a non-view chunk is refused just the same.
{
  let c;
  const rs = new ReadableStream({ type: "bytes", start(x) { c = x; } });
  const p = rs.getReader().read();
  await null; await null;
  for (const bad of [123, "str", {}, null, undefined, new ArrayBuffer(2), [1, 2]]) {
    try { c.enqueue(bad); console.log("accepted", typeof bad); } catch (e) { show("waiting-bad", e); }
  }
  c.enqueue(new Uint8Array([7]));
  const r = await p;
  console.log("waiting-read", Object.prototype.toString.call(r.value), [...r.value].join(","));
}

// A reader gets a Uint8Array over the chunk's bytes, queued or waiting.
for (const waiting of [false, true]) {
  let c;
  const rs = new ReadableStream({ type: "bytes", start(x) { c = x; } }, { highWaterMark: 64 });
  const reader = rs.getReader();
  const pending = waiting ? Array.from({ length: 6 }, () => reader.read()) : [];
  const src = new Uint8Array([9, 8, 7, 6, 5, 4]);
  const chunks = [
    new Int16Array([1, 2]),
    new DataView(new ArrayBuffer(3)),
    src.subarray(2, 5),
    Buffer.alloc(4, 3),
    new Float64Array(1),
    new BigInt64Array([5n]),
  ];
  for (const chunk of chunks) c.enqueue(chunk);
  console.log(waiting ? "waiting" : "queued", "desiredSize", c.desiredSize);
  c.close();
  const out = [];
  for (const p of pending) out.push(await p);
  for (;;) { const r = await reader.read(); if (r.done) break; out.push(r); }
  for (const r of out) {
    console.log(
      waiting ? "waiting" : "queued",
      Object.prototype.toString.call(r.value),
      Object.getPrototypeOf(r.value) === Uint8Array.prototype,
      r.value.byteOffset,
      r.value.byteLength,
      [...r.value].join(","),
    );
  }
}

// The byte stream reads the same bytes through for await.
{
  const rs = new ReadableStream({
    type: "bytes",
    start(c) { c.enqueue(new Uint16Array([0x0102])); c.enqueue(Buffer.alloc(2, 0x68)); c.close(); },
  });
  const seen = [];
  for await (const chunk of rs) seen.push(`${Object.prototype.toString.call(chunk)}:${[...chunk].join(",")}`);
  console.log("iterate", seen.join(" "));
}

// Closed, closing, cancelled and errored controllers, byte and default.
{
  let c;
  new ReadableStream({ type: "bytes", start(x) { c = x; } });
  c.close();
  try { c.enqueue("x"); } catch (e) { show("closed bad-chunk", e); }
  try { c.enqueue(new Uint8Array(1)); } catch (e) { show("closed enqueue", e); }
  try { c.close(); } catch (e) { show("closed close", e); }

  let q;
  new ReadableStream({ type: "bytes", start(x) { q = x; } });
  q.enqueue(new Uint8Array(2));
  q.close();
  try { q.enqueue(new Uint8Array(1)); } catch (e) { show("closing enqueue", e); }
  try { q.close(); } catch (e) { show("closing close", e); }

  let d;
  const cancelled = new ReadableStream({ type: "bytes", start(x) { d = x; } });
  d.enqueue(new Uint8Array(1));
  await cancelled.cancel();
  try { d.enqueue(new Uint8Array(1)); } catch (e) { show("cancelled enqueue", e); }
  try { d.close(); } catch (e) { show("cancelled close", e); }

  let g;
  new ReadableStream({ type: "bytes", start(x) { g = x; } });
  g.error(new Error("boom"));
  try { g.enqueue(new Uint8Array(1)); } catch (e) { show("errored enqueue", e); }

  let f;
  const plain = new ReadableStream({ start(x) { f = x; } });
  await plain.cancel();
  try { f.enqueue(1); } catch (e) { show("default enqueue", e); }
  try { f.close(); } catch (e) { show("default close", e); }
}

// The strategy errors render as node's coded errors.
for (const [tag, make] of [
  ["hwm", () => new ReadableStream({}, { highWaterMark: -1 })],
  ["size", () => new ReadableStream({}, { size: 5 })],
]) {
  try { make(); console.log(tag, "constructed"); } catch (e) { show(tag, e); }
}
{
  let c;
  const rs = new ReadableStream({ start(x) { c = x; } }, { size: () => NaN });
  try { c.enqueue(1); } catch (e) { show("size-NaN", e); }
  const r = await rs.getReader().read().then((v) => v, (e) => e);
  show("size-NaN read", r);
}
