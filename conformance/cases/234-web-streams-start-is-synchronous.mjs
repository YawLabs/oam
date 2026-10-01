// The underlying source / sink / transformer's start() runs synchronously,
// inside the stream's constructor (WHATWG Streams: the start algorithm is
// performed during setup and its RESULT is what becomes a promise). oam ran
// it on a microtask, so the idiom
//
//   let controller;
//   const body = new ReadableStream({ start(c) { controller = c; } });
//   controller.enqueue(chunk);
//
// threw "Cannot read properties of undefined (reading 'enqueue')". The MCP
// SDK's streamable-HTTP server transport is written exactly that way, so a
// server hosted on oam answered `initialize` with an empty event stream.
//
// What this pins: start() has run by the time the constructor returns, for
// all three stream classes; a chunk enqueued right after construction is
// read back; a throw from start() is the constructor's throw; a rejected
// start() promise errors the stream instead; and pull() / write() still wait
// for an asynchronous start().

const log = [];

// 1) start() has run when the constructor returns.
{
  let ran = false;
  new ReadableStream({ start() { ran = true; } });
  console.log("ReadableStream start sync:", ran);
}
{
  let ran = false;
  new WritableStream({ start() { ran = true; } });
  console.log("WritableStream start sync:", ran);
}
{
  let ran = false;
  new TransformStream({ start() { ran = true; } });
  console.log("TransformStream start sync:", ran);
}

// 2) The captured controller is usable at once.
{
  let controller;
  const rs = new ReadableStream({ start(c) { controller = c; } });
  console.log("controller captured:", typeof controller?.enqueue);
  controller.enqueue(new TextEncoder().encode("first"));
  controller.enqueue(new TextEncoder().encode("second"));
  controller.close();
  const dec = new TextDecoder();
  const seen = [];
  for await (const chunk of rs) seen.push(dec.decode(chunk));
  console.log("read back:", seen.join(","));
}
{
  let controller;
  const ts = new TransformStream({ start(c) { controller = c; } });
  console.log("transform controller captured:", typeof controller?.enqueue);
  controller.enqueue("from-start");
  const reader = ts.readable.getReader();
  console.log("transform read:", JSON.stringify(await reader.read()));
}

// 3) A throw from start() is the constructor's throw.
for (const [name, make] of [
  ["ReadableStream", (start) => new ReadableStream({ start })],
  ["WritableStream", (start) => new WritableStream({ start })],
  ["TransformStream", (start) => new TransformStream({ start })],
]) {
  try {
    make(() => { throw new RangeError("start threw"); });
    console.log(name, "constructor: no throw");
  } catch (e) {
    console.log(name, "constructor threw:", e.name, e.message);
  }
}

// 4) A rejected start() promise does not throw; it errors the stream.
{
  const rs = new ReadableStream({
    start() { return Promise.reject(new RangeError("start rejected")); },
  });
  console.log("rejected start constructed");
  try {
    await rs.getReader().read();
    console.log("read: no rejection");
  } catch (e) {
    console.log("read rejected:", e.name, e.message);
  }
}

// 5) pull() waits for an asynchronous start().
{
  const rs = new ReadableStream({
    async start() {
      log.push("start begins");
      await new Promise((r) => setTimeout(r, 5));
      log.push("start ends");
    },
    pull(c) {
      log.push("pull");
      c.enqueue("pulled");
      c.close();
    },
  });
  log.push("constructed");
  const reader = rs.getReader();
  const first = await reader.read();
  log.push("read " + first.value);
  console.log("readable order:", log.join(" > "));
}

// 6) write() waits for an asynchronous sink start().
{
  log.length = 0;
  const ws = new WritableStream({
    async start() {
      log.push("start begins");
      await new Promise((r) => setTimeout(r, 5));
      log.push("start ends");
    },
    write(chunk) { log.push("write " + chunk); },
    close() { log.push("close"); },
  });
  log.push("constructed");
  const writer = ws.getWriter();
  await writer.write("a");
  await writer.close();
  console.log("writable order:", log.join(" > "));
}
