// events.once is node's: an async function (#344), so the continuation of
// `await once(...)` runs after the microtasks the same emit queued, its kind
// is AsyncFunction and its length 2. An abort rejects with node's AbortError
// (code ABORT_ERR, the signal's reason as cause), already aborted or aborted
// while waiting, with and without a reason; a bad emitter or options reject
// as node's do.
//
// Regression guard: oam's once was a plain function (its continuation ran
// first) and rejected an abort with a DOMException (code 20, no cause).
import { once, EventEmitter } from "node:events";
const log = [];
const e = new EventEmitter();
const waiting = (async () => { await once(e, "x"); log.push("once continuation"); })();
e.on("x", () => {
  queueMicrotask(() => log.push("microtask"));
  Promise.resolve().then(() => 0).then(() => 0).then(() => log.push("third promise hop"));
});
e.emit("x");
await waiting;
await new Promise((r) => setTimeout(r, 5));
console.log(log.join(" | "));
console.log(Object.prototype.toString.call(once), once.length, once.name);
const show = (er) => console.log(er.constructor.name, er.name, JSON.stringify(er.code), String(er.cause), Object.keys(er), er.message);
await once(e, "z", { signal: AbortSignal.abort("why") }).catch(show);
await once(e, "z", { signal: AbortSignal.abort() }).catch(show);
const ac = new AbortController(); const p = once(e, "z", { signal: ac.signal }); ac.abort("later"); await p.catch(show);
const ac2 = new AbortController(); const p2 = once(e, "z", { signal: ac2.signal }); ac2.abort(); await p2.catch(show);
console.log(e.listenerCount("z"), e.listenerCount("error"));
await once(null, "x").catch((er) => console.log(er.constructor.name, er.code, er.message));
await once({}, "x").catch((er) => console.log(er.constructor.name, er.code, er.message));
await once(e, "x", null).catch((er) => console.log(er.code, er.message));
await once(e, "x", { signal: 1 }).catch((er) => console.log(er.code, er.message));
const et = new EventTarget(); const pe = once(et, "foo"); et.dispatchEvent(new Event("foo")); console.log((await pe)[0].type);
const pr = once(e, "x"); e.emit("error", new Error("bad")); await pr.catch((er) => console.log("rejected", er.message));
const pv = once(e, "v"); e.emit("v", 1, 2); console.log(await pv);
