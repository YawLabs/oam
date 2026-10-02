// WritableStream and TransformStream settle start / write / ready /
// backpressure in node's order, and the runtime's own byte streams do not
// pull before a read asks. oam's WritableStream ignored its strategy --
// desiredSize was always 1 and `ready` waited on the write chain -- and its
// TransformStream ran transform() the moment a chunk was written, with
// nothing reading, where node holds it back: the readable side's high-water
// mark defaults to 0, so a transform waits for a read (backpressure).
//
// Pinned here, against counted microtasks: a writable's desiredSize and
// ready across queued writes, sync and async start(), a write that throws,
// abort with a write in flight, close with writes queued, releaseLock, the
// controller's error() and signal, a size() strategy; a transform's
// backpressure (default and high-water mark 2 strategies), flush, terminate,
// a throwing transform, cancel from the readable side; a readable closed
// with chunks still queued, cancel with a read pending; strategy
// and size() validation; and a byte stream the runtime builds (a Response body over
// an async iterable, a byte stream as undici's is) reading nothing until a
// read asks. Measured on node v22.22.2.
const ticks = (n, log, tag) => { let p = Promise.resolve(); for (let i = 1; i <= n; i++) { const k = i; p = p.then(() => log.push(`${tag}${k}`)); } return p; };
const macro = () => new Promise((r) => setTimeout(r, 0));
const err = (e) => e === undefined ? "undef" : `${e.name}:${e.code ?? ""}:${e.message}`;

// TransformStream: start, transform before any read?
{
  const log = [];
  const ts = new TransformStream({
    start() { log.push("start"); },
    transform(ch, c) { log.push("transform:" + ch); c.enqueue(ch); },
    flush() { log.push("flush"); },
  });
  const w = ts.writable.getWriter();
  log.push("ds=" + w.desiredSize);
  w.ready.then(() => log.push("ready"));
  const wp = w.write("a").then(() => log.push("write-done"));
  log.push("ds-after-write=" + w.desiredSize);
  await ticks(6, log, "t");
  await macro();
  log.push("macro");
  const r = ts.readable.getReader();
  const v = r.read();
  log.push("read-called");
  log.push(JSON.stringify(await v));
  await ticks(3, log, "u");
  console.log("transform:", log.join(" "));
}
// TransformStream readable hwm1: initial pull clears backpressure
{
  const log = [];
  const ts = new TransformStream({
    transform(ch, c) { log.push("transform:" + ch); c.enqueue(ch); },
  }, undefined, { highWaterMark: 1 });
  const w = ts.writable.getWriter();
  w.write("a").then(() => log.push("write-done"));
  await ticks(6, log, "t");
  await macro();
  log.push("macro");
  console.log("transform-rhwm1:", log.join(" "));
}
// WritableStream: start sync / async, writes before start settles, ready + desiredSize
{
  const log = [];
  const ws = new WritableStream({
    start() { log.push("start"); return new Promise((r) => setTimeout(() => { log.push("start-resolve"); r(); }, 0)); },
    write(ch) { log.push("write:" + ch); },
    close() { log.push("close"); },
  }, { highWaterMark: 2 });
  const w = ws.getWriter();
  log.push("ds=" + w.desiredSize);
  w.ready.then(() => log.push("ready"));
  w.write("a").then(() => log.push("a-done"));
  log.push("ds=" + w.desiredSize);
  w.write("b").then(() => log.push("b-done"));
  log.push("ds=" + w.desiredSize);
  w.ready.then(() => log.push("ready2"));
  await macro();
  await macro();
  log.push("macro");
  log.push("ds=" + w.desiredSize);
  await w.close();
  console.log("writable-async-start:", log.join(" "));
}
{
  const log = [];
  const ws = new WritableStream({
    start() { log.push("start"); },
    write(ch) { log.push("write:" + ch); },
  });
  const w = ws.getWriter();
  w.write("a").then(() => log.push("a-done"));
  log.push("written");
  await ticks(6, log, "t");
  console.log("writable-sync-start:", log.join(" "));
}

const track = (p, log, tag) => p.then(() => log.push(tag + ":ok"), (e) => log.push(tag + ":rej:" + err(e)));

// writable: write error, then later writes / close
{
  const log = [];
  const ws = new WritableStream({ write(c) { log.push("w" + c); if (c === 2) throw new Error("bad"); }, close() { log.push("close"); } }, { highWaterMark: 3 });
  const w = ws.getWriter();
  track(w.write(1), log, "1"); track(w.write(2), log, "2"); track(w.write(3), log, "3");
  log.push("ds=" + w.desiredSize);
  track(w.ready, log, "ready"); track(w.closed, log, "closed");
  await ticks(8, log, "t"); await macro();
  log.push("ds=" + w.desiredSize);
  track(w.close(), log, "close"); await macro();
  console.log("write-error:", log.join(" "));
}
// abort during in-flight write
{
  const log = [];
  let release;
  const ws = new WritableStream({ write(c) { log.push("w" + c); return new Promise((r) => { release = r; }); }, abort(r) { log.push("abort:" + r); }, close() { log.push("close"); } });
  const w = ws.getWriter();
  track(w.write("a"), log, "a"); track(w.write("b"), log, "b");
  await macro();
  track(w.abort("why"), log, "abort"); log.push("ds=" + w.desiredSize);
  track(w.ready, log, "ready"); track(w.closed, log, "closed");
  await ticks(3, log, "t"); release(); await macro();
  console.log("abort-inflight:", log.join(" "));
}
// close with queued writes, releaseLock, locked errors
{
  const log = [];
  const ws = new WritableStream({ write(c) { log.push("w" + c); }, close() { log.push("sinkclose"); } });
  const w = ws.getWriter();
  w.write("x"); w.write("y");
  track(w.close(), log, "close");
  track(w.close(), log, "close2");
  track(w.write("z"), log, "z");
  track(ws.close(), log, "ws.close");
  try { ws.getWriter(); } catch (e) { log.push("getWriter:" + err(e)); }
  await macro();
  log.push("ds=" + w.desiredSize);
  w.releaseLock();
  track(w.closed, log, "closed-after-release"); track(w.ready, log, "ready-after-release");
  track(w.write(1), log, "write-after-release");
  try { w.desiredSize; } catch (e) { log.push("ds:" + err(e)); }
  await macro();
  console.log("close-queued:", log.join(" "), ws.locked);
}
// controller.error and signal
{
  const log = [];
  let ctl;
  const ws = new WritableStream({ start(c) { ctl = c; }, write() { log.push("write"); } });
  const w = ws.getWriter();
  log.push("aborted=" + ctl.signal.aborted);
  ctl.error(new Error("ce"));
  track(w.write(1), log, "w"); track(w.ready, log, "ready"); track(w.closed, log, "closed");
  log.push("ds=" + w.desiredSize);
  await macro();
  const ws2 = new WritableStream({ write() {} });
  ws2.abort("r2").then(() => log.push("abort2 ok"));
  await macro();
  const ws3 = new WritableStream({ start(c) { ctl = c; } });
  ws3.abort("r3"); log.push("sig3=" + ctl.signal.aborted + "/" + ctl.signal.reason);
  console.log("controller-error:", log.join(" "));
}
// size strategy on writable
{
  const log = [];
  const ws = new WritableStream({ write(c) { log.push("w" + c); } }, { highWaterMark: 10, size: (c) => c.length });
  const w = ws.getWriter();
  log.push(w.desiredSize); w.write("abcd"); log.push(w.desiredSize); w.write("abcdefgh"); log.push(w.desiredSize);
  track(w.ready, log, "ready");
  await macro();
  log.push(w.desiredSize);
  console.log("ws-size:", log.join(" "));
}
// transform: flush, terminate, error, cancel
{
  const log = [];
  const ts = new TransformStream({ transform(c, ctl) { ctl.enqueue(c + c); }, flush(ctl) { log.push("flush"); ctl.enqueue("end"); } });
  const w = ts.writable.getWriter(); const r = ts.readable.getReader();
  w.write("a"); w.write("b"); track(w.close(), log, "close");
  for (;;) { const v = await r.read(); log.push(JSON.stringify(v)); if (v.done) break; }
  await macro();
  console.log("transform-flush:", log.join(" "));
}
{
  const log = [];
  const ts = new TransformStream({ transform(c, ctl) { ctl.enqueue(c); ctl.terminate(); } });
  const w = ts.writable.getWriter(); const r = ts.readable.getReader();
  track(w.write("a"), log, "a"); track(w.write("b"), log, "b"); track(w.closed, log, "closed");
  log.push(JSON.stringify(await r.read())); log.push(JSON.stringify(await r.read()));
  await macro();
  console.log("transform-terminate:", log.join(" "));
}
{
  const log = [];
  const ts = new TransformStream({ transform() { throw new Error("tx"); } });
  const w = ts.writable.getWriter(); const r = ts.readable.getReader();
  track(w.write("a"), log, "a"); track(r.read(), log, "read"); track(w.closed, log, "closed"); track(r.closed, log, "rclosed");
  await macro();
  console.log("transform-throws:", log.join(" "));
}
{
  const log = [];
  const ts = new TransformStream({ cancel(r) { log.push("tcancel:" + r); } });
  const w = ts.writable.getWriter();
  track(w.write("a"), log, "a");
  track(ts.readable.cancel("nope"), log, "cancel"); track(w.closed, log, "closed");
  await macro();
  console.log("transform-cancel:", log.join(" "));
}
{
  const log = [];
  const ts = new TransformStream({}, { highWaterMark: 2 }, { highWaterMark: 2 });
  const w = ts.writable.getWriter();
  for (const c of "abcde") track(w.write(c), log, c);
  log.push("ds=" + w.desiredSize);
  await ticks(6, log, "t"); await macro();
  log.push("ds=" + w.desiredSize);
  console.log("transform-hwm2:", log.join(" "));
}
// readable: close with queued chunks, reader.closed timing, cancel paths
{
  const log = [];
  let ctl;
  const rs = new ReadableStream({ start(c) { ctl = c; c.enqueue(1); c.enqueue(2); c.close(); }, cancel(r) { log.push("cancel:" + r); } });
  const r = rs.getReader();
  track(r.closed, log, "closed");
  log.push("ds=" + ctl.desiredSize);
  try { ctl.enqueue(3); } catch (e) { log.push("enq:" + err(e)); }
  log.push(JSON.stringify(await r.read())); await ticks(2, log, "t");
  log.push(JSON.stringify(await r.read())); await ticks(2, log, "u");
  log.push(JSON.stringify(await r.read()));
  console.log("close-queued-read:", log.join(" "));
}
{
  const log = [];
  const rs = new ReadableStream({ pull(c) { log.push("pull"); }, cancel(r) { log.push("cancel:" + r); return new Promise((res) => setTimeout(res, 0)); } });
  const r = rs.getReader();
  const p = r.read(); track(p.then((v) => log.push(JSON.stringify(v))), log, "read");
  track(r.cancel("x"), log, "cancel-done"); log.push("after-cancel");
  await ticks(3, log, "t"); await macro(); await macro();
  console.log("cancel-pending-read:", log.join(" "));
}
// strategy validation
for (const hwm of [-1, NaN, "3", "x"]) {
  try { const rs = new ReadableStream({ start(c) { console.log("hwm", JSON.stringify(hwm), c.desiredSize); } }, { highWaterMark: hwm }); } catch (e) { console.log("hwm", JSON.stringify(hwm), err(e)); }
  try { const ws = new WritableStream({}, { highWaterMark: hwm }); console.log("ws hwm", ws.getWriter().desiredSize); } catch (e) { console.log("ws hwm", JSON.stringify(hwm), err(e)); }
}
try { new ReadableStream({ type: "bytes", start(c) { c.enqueue("x"); } }); } catch (e) { console.log("bytes enqueue string:", err(e)); }

// Response(async iterable): generator not advanced until read
{
  const log = [];
  async function* g() { log.push("gen0"); yield "a"; log.push("gen1"); yield "b"; }
  const res = new Response(g());
  const body = res.body;
  await macro();
  log.push("macro");
  const r = body.getReader();
  const v = await r.read();
  log.push("got:" + Buffer.from(v.value).toString());
  console.log("response-iter:", log.join(" "));
}

// size(): its result is +size, and NaN / negative / Infinity errors the stream;
// a size that is not a function is the constructor's ERR_INVALID_ARG_TYPE.
for (const v of [-1, NaN, Infinity, "2"]) {
  const rs = new ReadableStream({ start(c) { try { c.enqueue("a"); console.log("size", v, "ok ds", c.desiredSize); } catch (e) { console.log("size", v, err(e)); } } }, { size: () => v });
  rs.getReader().read().then((r) => console.log(" read", JSON.stringify(r)), (e) => console.log(" read rej", err(e)));
  await new Promise((r) => setTimeout(r, 0));
}
{
  const ws = new WritableStream({}, { size: () => -1 });
  const w = ws.getWriter();
  w.write("a").then(() => console.log("ws ok"), (e) => console.log("ws rej", err(e)));
  await new Promise((r) => setTimeout(r, 0));
}
for (const s of [1, null, "x"]) {
  try { new ReadableStream({}, { size: s }); console.log("rs size", s, "ok"); } catch (e) { console.log("rs size", s, err(e)); }
  try { new WritableStream({}, { size: s }); console.log("ws size", s, "ok"); } catch (e) { console.log("ws size", s, err(e)); }
}

// Fractional sizes: the queue total drops by each size, floored at 0 and not
// reset when the queue empties, so the rounding shows as it does in node.
{
  let c;
  const rs = new ReadableStream({ start(x) { c = x; } }, { highWaterMark: 1, size: (v) => v });
  c.enqueue(0.1); c.enqueue(0.2); c.enqueue(0.3);
  const r = rs.getReader();
  const seen = [c.desiredSize];
  for (let i = 0; i < 3; i++) { await r.read(); seen.push(c.desiredSize); }
  console.log("readable desiredSize", seen.join(" "));
}
{
  const w = new WritableStream({ write() { return new Promise((r) => setTimeout(r, 1)); } }, { highWaterMark: 1, size: (v) => v }).getWriter();
  const seen = [];
  const ps = [w.write(0.1), w.write(0.2), w.write(0.3)];
  seen.push(w.desiredSize);
  for (const p of ps) { await p; seen.push(w.desiredSize); }
  console.log("writable desiredSize", seen.join(" "));
}
