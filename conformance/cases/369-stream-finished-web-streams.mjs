// stream.finished() / stream/promises finished() on a web ReadableStream or
// WritableStream: node waits on the promise every web stream carries under
// Symbol.for('nodejs.webstream.isClosedPromise'), settled when the stream
// closes or errors. oam's web streams had none, so finished() threw
// "Cannot read properties of undefined (reading 'promise')" -- and the npm
// undici's fetch, which finalizes a response with finished(), never
// delivered a MockAgent reply's body (#206).
import { finished } from "node:stream";
import { finished as finishedPromise } from "node:stream/promises";

const k = Symbol.for("nodejs.webstream.isClosedPromise");
const log = (...args) => console.log(...args);
const tick = () => new Promise((r) => setTimeout(r, 5));
log("carried by", k in new ReadableStream(), k in new WritableStream(), k in new TransformStream());

{
  let c;
  const rs = new ReadableStream({ start(ctl) { c = ctl; } });
  finished(rs, (err) => log("readable closed:", err === undefined ? "no error" : err.message));
  c.enqueue("a");
  c.close();
  const reader = rs.getReader();
  log("read", JSON.stringify(await reader.read()));
  log("read", JSON.stringify(await reader.read()));
}
{
  let c;
  const rs = new ReadableStream({ start(ctl) { c = ctl; } });
  finished(rs, (err) => log("readable errored:", err && err.message));
  c.error(new Error("boom"));
  await tick();
}
{
  const rs = new ReadableStream({ start(ctl) { ctl.close(); } });
  await tick();
  finished(rs, (err) => log("closed before finished():", err === undefined ? "no error" : err.message));
  await tick();
}
{
  const rs = new ReadableStream({ start(ctl) { ctl.error(new Error("early")); } });
  await tick();
  finished(rs, (err) => log("errored before finished():", err && err.message));
  await tick();
}
{
  const rs = new ReadableStream();
  finished(rs, (err) => log("cancelled:", err === undefined ? "no error" : err.message));
  await rs.cancel("why");
  await tick();
}
{
  const ws = new WritableStream({ write() {} });
  const p = finishedPromise(ws).then(() => "resolved", (e) => "rejected " + e.message);
  const w = ws.getWriter();
  await w.write("x");
  await w.close();
  log("writable closed:", await p);
}
{
  const ws = new WritableStream();
  const p = finishedPromise(ws).then(() => "resolved", (e) => "rejected " + e.message);
  await ws.abort(new Error("stop"));
  log("writable aborted:", await p);
}
{
  const ws = new WritableStream({ write() { throw new Error("sink failed"); } });
  const p = finishedPromise(ws).then(() => "resolved", (e) => "rejected " + e.message);
  await ws.getWriter().write("x").catch(() => {});
  log("writable errored by its sink:", await p);
}
{
  const order = [];
  const rs = new ReadableStream({ start(ctl) { ctl.close(); } });
  finished(rs, () => order.push("finished"));
  rs.getReader().closed.then(() => order.push("reader.closed"));
  await tick();
  log("order", order.join(","));
}
{
  // A fetch-style body: a byte stream read to its end.
  const body = new Response("payload").body;
  const done = finishedPromise(body).then(() => "resolved", (e) => "rejected " + e.message);
  log("body", await new Response(body).text(), await done);
}
