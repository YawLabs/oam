// Readable.prototype.compose (Node v22). Deterministic stdout; identical on
// node and oam. Ordered output via an explicit sequence to avoid scheduler race.
import { Readable, Transform } from "node:stream";

const out = [];
async function run() {
  // 1. compose(asyncGeneratorFn): pair chars
  {
    const s = Readable.from(["a", "b", "c", "d"]).compose(async function* (src) {
      let str = "";
      for await (const c of src) { str += c; if (str.length === 2) { yield str; str = ""; } }
    });
    out.push("gen:" + (await s.toArray()).join(","));
  }
  // 2. compose(Transform): passthrough
  {
    const s = Readable.from(["a", "b", "c", "d"]).compose(
      new Transform({ objectMode: true, transform(c, e, cb) { cb(null, c); } }),
    );
    out.push("xform:" + (await s.toArray()).join(","));
  }
  // 3. throw inside compose -> rejection
  {
    const s = Readable.from([1, 2, 3]).compose(async function* (src) {
      for await (const c of src) { if (c === 2) throw new Error("boom"); yield c; }
    });
    try { await s.toArray(); out.push("throw:none"); }
    catch (e) { out.push("throw:" + e.message); }
  }
  // 4. compose with already-aborted signal -> AbortError on composed stream
  {
    const ac = new AbortController();
    ac.abort();
    const s = Readable.from([1, 2, 3]).compose(async function* (src) {
      for await (const c of src) yield c;
    }, { signal: ac.signal });
    try { await s.toArray(); out.push("abort:none"); }
    catch (e) { out.push("abort:" + e.name); }
  }
  // 5. compose(Readable) -> ERR_INVALID_ARG_VALUE
  {
    try { Readable.from(["a"]).compose(Readable.from(["b"])); out.push("rdbl:none"); }
    catch (e) { out.push("rdbl:" + e.code); }
  }
  // 6. compose() -> ERR_INVALID_ARG_TYPE
  {
    try { Readable.from(["a"]).compose(); out.push("empty:none"); }
    catch (e) { out.push("empty:" + e.code); }
  }
}
run().then(() => { for (const line of out) console.log(line); });
