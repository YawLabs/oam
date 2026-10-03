// console.table draws node's table: its box characters, an `(index)` column,
// a column per property (or per `properties`), `Values` for primitives,
// `(iteration index)` / `Key` / `Values` for a Map or a Set, each cell
// inspected as node inspects it and padded to its display width -- on the
// global console and on a `new Console()` (undici's MockAgent formats its
// pending interceptors that way, #206). oam printed `a | b` rows, and a
// Console instance had no table() at all.
import { Console } from "node:console";
import { Transform } from "node:stream";

console.table([{ a: 1, b: "x" }, { a: 2, c: true }]);
console.table([{ a: 1, b: "x" }, { a: 2, c: true }], ["a", "c"]);
console.table([1, "two", { three: 3 }]);
console.table({ first: { x: 1 }, second: { y: [1, 2, 3, 4, 5] } });
console.table(new Map([["k", { v: 1 }], [2, "two"]]));
console.table(new Set(["s", 1]));
console.table([{ wide: "中文", emoji: "❌", nested: { a: 1, b: 2, c: 3 } }]);
console.table([{ s: "❤" }, { s: "\u{1F44D}\u{1F3FD}" }, { s: "é" }, { s: "⌚" }, { s: "a​b" }]);
console.table([]);
console.table("not tabular");
console.table(42);
try {
  console.table([], "a");
} catch (e) {
  console.log(e.code, e.message);
}

const sink = new Transform({ transform(chunk, _enc, cb) { cb(null, chunk); } });
const logger = new Console({ stdout: sink, inspectOptions: { colors: false } });
logger.table([{ Method: "GET", Remaining: Infinity }]);
process.stdout.write(sink.read().toString());
const colored = new Console({ stdout: sink, inspectOptions: { colors: true } });
colored.table([{ n: 1, s: "x" }]);
console.log(JSON.stringify(sink.read().toString()));
