// ReadableStream#pipeTo / #pipeThrough follow node's readableStreamPipeTo
// (the standard's ReadableStreamPipeTo): each step waits for the
// destination's `ready`, reads, and starts the write without waiting for
// it, so the destination's queue fills to its high-water mark and the
// source is pulled as far ahead as node pulls it. Errors and closes on
// either side are watched while a read or a write is pending.
//
// oam's pipe read one chunk, awaited that chunk's write, then read again:
// it never filled the destination's high-water mark, so the source's pulls
// trailed node's (w2@3 where node is at w2@5), and the microtasks a pipe
// takes differed. A destination that errored while the source had nothing
// to read hung the pipe, since nothing watched it. options.signal was
// ignored, and a duck-typed or locked destination, a bad options object or
// a bad signal were not refused with node's errors. Measured on node
// v22.22.2.
const delay = (ms) => new Promise((r) => setTimeout(r, ms));
const show = (tag, e) => console.log(tag, e?.constructor?.name, e?.code, String(e));
const ticks = (n, log, tag) => { let p = Promise.resolve(); for (let i = 1; i <= n; i++) { const k = i; p = p.then(() => log.push(`${tag}${k}`)); } return p; };

// Backpressure: the pipe keeps the destination filled to its high-water mark.
for (const srcHwm of [1, 0]) {
  let pulls = 0;
  const log = [];
  const src = new ReadableStream({ pull(c) { pulls++; if (pulls > 8) c.close(); else c.enqueue(pulls); } }, { highWaterMark: srcHwm });
  const dst = new WritableStream({ write(v) { log.push(`w${v}@${pulls}`); return delay(3); } }, { highWaterMark: 4 });
  await src.pipeTo(dst);
  console.log("backpressure src-hwm", srcHwm, log.join(" "));
}

// Microtask order of a synchronous pipe.
{
  const log = [];
  const src = new ReadableStream({ start(c) { c.enqueue("a"); c.enqueue("b"); c.close(); log.push("start"); } });
  const dst = new WritableStream({ write(v) { log.push(`write:${v}`); }, close() { log.push("close"); } });
  const p = src.pipeTo(dst).then(() => log.push("piped"));
  log.push("called");
  await ticks(14, log, "t");
  await p;
  console.log("order", log.join(" "), src.locked, dst.locked);
}

// Source errors: the destination is aborted, the pipe rejects with the error.
for (const preventAbort of [false, true]) {
  const log = [];
  let c;
  const src = new ReadableStream({ start(x) { c = x; } });
  const dst = new WritableStream({ write(v) { log.push(`write:${v}`); }, abort(r) { log.push(`abort:${r.message}`); }, close() { log.push("close"); } });
  const p = src.pipeTo(dst, { preventAbort });
  c.enqueue(1);
  await delay(1);
  c.error(new Error("src-boom"));
  const e = await p.catch((e) => e);
  console.log("src-error preventAbort", preventAbort, log.join(" "), e.message, src.locked, dst.locked);
}

// Destination errors: the source is cancelled.
for (const preventCancel of [false, true]) {
  const log = [];
  const src = new ReadableStream({ pull(c) { c.enqueue(1); }, cancel(r) { log.push(`cancel:${r.message}`); } });
  const dst = new WritableStream({ write() { throw new Error("dst-boom"); } });
  const e = await src.pipeTo(dst, { preventCancel }).catch((e) => e);
  console.log("dst-error preventCancel", preventCancel, log.join(" "), e.message, src.locked, dst.locked);
}

// A destination that errors while the source is waiting for data.
{
  const log = [];
  let wc;
  const src = new ReadableStream({ cancel(r) { log.push(`cancel:${r.message}`); } });
  const dst = new WritableStream({ start(c) { wc = c; } });
  const p = src.pipeTo(dst);
  await delay(1);
  wc.error(new Error("later"));
  const e = await p.catch((e) => e);
  console.log("dst-error idle", log.join(" "), e.message);
}

// Source closes: preventClose keeps the destination open.
for (const preventClose of [false, true]) {
  const log = [];
  const src = new ReadableStream({ start(c) { c.enqueue("x"); c.close(); } });
  const dst = new WritableStream({ write(v) { log.push(`write:${v}`); }, close() { log.push("close"); } });
  await src.pipeTo(dst, { preventClose });
  console.log("preventClose", preventClose, log.join(" "), dst.locked);
}

// The destination is already closed.
{
  const log = [];
  const dst = new WritableStream();
  await dst.close();
  const src = new ReadableStream({ cancel(r) { log.push(`cancel:${r?.code}`); } });
  const e = await src.pipeTo(dst).catch((e) => e);
  show("dst-closed", e);
  console.log("dst-closed", log.join(" "));
}

// An abort signal.
{
  const log = [];
  const ac = new AbortController();
  let pulls = 0;
  const src = new ReadableStream({ async pull(c) { pulls++; await delay(2); c.enqueue(pulls); }, cancel(r) { log.push(`cancel:${r?.name}`); } });
  const dst = new WritableStream({ write(v) { log.push(`write:${v}`); }, abort(r) { log.push(`abort:${r?.name}`); } });
  const p = src.pipeTo(dst, { signal: ac.signal });
  await delay(9);
  ac.abort();
  const e = await p.catch((e) => e);
  console.log("signal", log.slice(-2).join(" "), e?.name, src.locked, dst.locked);
  const pre = new AbortController();
  pre.abort(new Error("early"));
  const log2 = [];
  const e2 = await new ReadableStream({ cancel(r) { log2.push(`cancel:${r.message}`); } })
    .pipeTo(new WritableStream({ abort(r) { log2.push(`abort:${r.message}`); } }), { signal: pre.signal })
    .catch((e) => e);
  console.log("signal pre-aborted", log2.join(" "), e2.message);
}

// Argument and lock errors.
{
  const locked = new ReadableStream();
  locked.getReader();
  const lockedW = new WritableStream();
  lockedW.getWriter();
  for (const [tag, f] of [
    ["not-writable", () => new ReadableStream().pipeTo({ getWriter() {} })],
    ["options-num", () => new ReadableStream().pipeTo(new WritableStream(), 5)],
    ["bad-signal", () => new ReadableStream().pipeTo(new WritableStream(), { signal: {} })],
    ["locked-src", () => locked.pipeTo(new WritableStream())],
    ["locked-dst", () => new ReadableStream().pipeTo(lockedW)],
  ]) {
    try { const p = f(); show(tag, await p.then(() => "ok", (e) => e)); } catch (e) { show(tag + " sync-throw", e); }
  }
  for (const [tag, f] of [
    ["through-no-readable", () => new ReadableStream().pipeThrough({ writable: new WritableStream() })],
    ["through-no-writable", () => new ReadableStream().pipeThrough({ readable: new ReadableStream() })],
    ["through-locked", () => locked.pipeThrough(new TransformStream())],
    ["through-options", () => new ReadableStream().pipeThrough(new TransformStream(), 5)],
  ]) {
    try { f(); console.log(tag, "ok"); } catch (e) { show(tag, e); }
  }
}

// pipeThrough chains and stays lazy.
{
  let pulls = 0;
  const src = new ReadableStream({ pull(c) { pulls++; if (pulls > 3) c.close(); else c.enqueue(pulls); } }, { highWaterMark: 0 });
  const out = src.pipeThrough(new TransformStream({ transform(v, c) { c.enqueue(v * 10); } }));
  await delay(5);
  const before = pulls;
  const got = [];
  for await (const v of out) got.push(v);
  console.log("through", before, got.join(","), pulls);
}

// Microtask order through a pipeThrough chain, and of a write that throws.
{
  const log = [];
  let n = 0;
  const src = new ReadableStream({ pull(c) { n++; log.push(`pull${n}`); if (n > 3) c.close(); else c.enqueue(n); } });
  const t1 = new TransformStream({ transform(v, c) { log.push(`t1:${v}`); c.enqueue(v + 1); } });
  const t2 = new TransformStream({ transform(v, c) { log.push(`t2:${v}`); c.enqueue(v * 2); } });
  const out = src.pipeThrough(t1).pipeThrough(t2);
  const r = out.getReader();
  const reads = [];
  for (let i = 0; i < 4; i++) reads.push(r.read().then((x) => log.push(`read:${x.value}:${x.done}`)));
  await ticks(40, log, "");
  console.log(log.join(" "));
}
{
  const log = [];
  const src = new ReadableStream({ start(c) { for (let i = 0; i < 5; i++) c.enqueue(i); c.close(); } });
  const dst = new WritableStream({ write(v) { log.push(`w${v}`); if (v === 2) throw new Error("w2"); }, abort(r) { log.push("abort"); } }, { highWaterMark: 3 });
  const p = src.pipeTo(dst).catch((e) => log.push(`rej:${e.message}`));
  await ticks(30, log, "");
  await p;
  console.log(log.join(" "));
}
