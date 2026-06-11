// querystring: node-probed semantics.
import qs from "node:querystring";

console.log(qs.escape("a b!'()*~-._c"));
console.log(qs.stringify({ a: "x y", b: ["1", "2"], "c d": "?&=" }));
console.log(JSON.stringify(qs.parse("a=x+y&b=1&b=2&c%20d=%3F%26%3D&flag")));
console.log(qs.unescape("a%20b+c"));
console.log(qs.stringify({ u: "café €", n: 42, t: true, nil: null }));
console.log(JSON.stringify(qs.parse("a:1;b:2", ";", ":")));
console.log(JSON.stringify(qs.parse("")), qs.stringify({}));
