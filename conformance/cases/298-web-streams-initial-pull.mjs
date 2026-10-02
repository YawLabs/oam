// A ReadableStream makes its INITIAL pull once start() settles, as the
// Streams standard asks (after the start algorithm resolves: "ReadableStream
// DefaultControllerCallPullIfNeeded") and node does: with no read pending it
// pulls until the queue reaches the high-water mark, and every enqueue and
// read asks again. oam waited for the first read() before it pulled at all,
// so a source with a high-water mark above 0 was never pre-filled, a pull
// that enqueued did not pull again, and read() of a high-water-mark-0
// stream called pull() a microtask late.
//
// Each scenario logs, against counted microtasks (t1..t6) and one macrotask,
// exactly when start() and pull() run and what desiredSize pull() sees:
// sync / thenable / async / timer-resolved start, high-water mark 0 vs >0, a
// pull that enqueues (sync and async), a size() strategy, start() that
// closes / errors / rejects, a pull that throws, read() issued before start
// settles, async iteration, byte streams (`type: 'bytes'`: high-water mark 0
// by default, desiredSize in bytes), tee() and ReadableStream.from() (a
// high-water mark of 0: the iterator is not advanced until a read).
// Measured on node v22.22.2. start() itself still runs synchronously in the
// constructor (case 234).
const ticks = (n, log, tag) => { let p = Promise.resolve(); for (let i = 1; i <= n; i++) { const k = i; p = p.then(() => log.push(`${tag}${k}`)); } return p; };
const macro = () => new Promise((r) => setTimeout(r, 0));

async function scenario(name, src, strategy, after) {
  const log = [];
  const s = { ...src };
  for (const k of ["start", "pull"]) if (src[k]) s[k] = (c) => { log.push(k + (k === "pull" ? `(ds=${c.desiredSize})` : "")); return src[k](c, log); };
  let rs;
  try { rs = new ReadableStream(s, strategy); } catch (e) { console.log(name.padEnd(28), "ctor threw", e.name, e.message); return; }
  log.push("ctor-returned");
  await ticks(6, log, "t");
  await macro();
  log.push("macro");
  if (after) await after(rs, log);
  console.log(name.padEnd(28), log.join(" "));
}

await scenario("sync-start hwm1", { start() {}, pull() {} });
await scenario("no-start hwm1", { pull() {} });
await scenario("sync-start hwm0", { start() {}, pull() {} }, { highWaterMark: 0 });
await scenario("hwm0 then read", { pull(c) { c.enqueue(1); } }, { highWaterMark: 0 }, async (rs, log) => {
  const r = rs.getReader(); const p = r.read(); log.push("read-called"); const v = await p; log.push("got" + v.value); await ticks(3, log, "u");
});
await scenario("start-returns-1", { start() { return 1; }, pull() {} });
await scenario("start-thenable", { start() { return { then(r) { r(); } }; }, pull() {} });
await scenario("async-start", { async start() {}, pull() {} });
await scenario("start-timer", { start(c, log) { return new Promise((r) => setTimeout(() => { log.push("start-resolve"); r(); }, 0)); }, pull() {} });
await scenario("start-enqueues hwm1", { start(c) { c.enqueue("a"); }, pull() {} });
await scenario("start-enqueues hwm2", { start(c) { c.enqueue("a"); }, pull() {} }, { highWaterMark: 2 });
await scenario("pull-enqueues hwm3", { pull(c) { c.enqueue("x"); } }, { highWaterMark: 3 });
await scenario("async-pull-enqueues hwm3", { async pull(c) { await null; c.enqueue("x"); } }, { highWaterMark: 3 });
await scenario("pull-nothing hwm3", { pull() {} }, { highWaterMark: 3 });
await scenario("start-closes", { start(c) { c.close(); }, pull() {} });
await scenario("start-errors", { start(c) { c.error(new Error("e")); }, pull() {} });
await scenario("start-rejects", { start() { return Promise.reject(new Error("r")); }, pull() {} });
await scenario("pull-rejects", { pull() { return Promise.reject(new Error("pr")); } }, undefined, async (rs, log) => {
  try { await rs.getReader().read(); } catch (e) { log.push("read-rejected:" + e.message); }
});
await scenario("pull-throws", { pull() { throw new Error("pt"); } }, undefined, async (rs, log) => {
  try { await rs.getReader().read(); } catch (e) { log.push("read-rejected:" + e.message); }
});
await scenario("size-strategy", { pull(c) { c.enqueue("abcd"); } }, { highWaterMark: 10, size: (ch) => ch.length });
// read issued before start settles, hwm 0
{
  const log = [];
  const rs = new ReadableStream({
    start() { log.push("start"); return new Promise((r) => setTimeout(() => { log.push("start-resolve"); r(); }, 0)); },
    pull(c) { log.push(`pull(ds=${c.desiredSize})`); c.enqueue("p"); },
  }, { highWaterMark: 0 });
  const r = rs.getReader();
  const v = await r.read();
  log.push("got:" + v.value);
  await ticks(3, log, "t");
  console.log("read-pending-before-start".padEnd(28), log.join(" "));
}
// iterate, hwm 1
{
  const log = [];
  let n = 0;
  const rs = new ReadableStream({ pull(c) { log.push("pull" + n); c.enqueue(n++); if (n > 4) c.close(); } });
  await ticks(3, log, "t");
  for await (const x of rs) { log.push("v" + x); }
  console.log("iterate hwm1".padEnd(28), log.join(" "));
}
// bytes type
await scenario("bytes default", { type: "bytes", start() {}, pull(c) {} });
await scenario("bytes hwm1", { type: "bytes", start() {}, pull(c) {} }, { highWaterMark: 1 });
await scenario("bytes hwm8 enq", { type: "bytes", pull(c) { c.enqueue(new Uint8Array(3)); } }, { highWaterMark: 8 });
await scenario("bytes read", { type: "bytes", pull(c) { c.enqueue(new Uint8Array([1, 2])); } }, undefined, async (rs, log) => {
  const r = rs.getReader(); const v = await r.read(); log.push("got" + v.value.length);
});

// tee: the branches pull on their own once started
{
  const log = [];
  let n = 0;
  const rs = new ReadableStream({ pull(c) { log.push("src-pull" + n); c.enqueue(n++); if (n > 2) c.close(); } }, { highWaterMark: 0 });
  const [a, b] = rs.tee();
  log.push("teed");
  await ticks(8, log, "t");
  await macro();
  log.push("macro");
  const ra = a.getReader(), rb = b.getReader();
  log.push("a" + JSON.stringify(await ra.read()));
  log.push("b" + JSON.stringify(await rb.read()));
  console.log("tee:", log.join(" "));
}
// from: no pull until read
{
  const log = [];
  function* g() { log.push("next0"); yield 0; log.push("next1"); yield 1; }
  const rs = ReadableStream.from(g());
  await ticks(4, log, "t");
  await macro();
  log.push("macro");
  const r = rs.getReader();
  log.push(JSON.stringify(await r.read()));
  await ticks(4, log, "u");
  console.log("from:", log.join(" "));
}
