// Two errors node throws with a `code` that oam threw without one.
//
// 1. `new URL(input)` for an input that does not parse. Node: a TypeError
//    whose message is exactly "Invalid URL", with own `code`
//    (ERR_INVALID_URL), `input`, and `base` when a base was passed. oam put
//    the input into the message and set nothing else, so the common
//
//      try { new URL(s) } catch (e) { if (e.code === 'ERR_INVALID_URL') ... }
//
//    rethrew. The argument handling around it is pinned too: no argument is
//    ERR_MISSING_ARGS, a `null` base is the base "null", and URL.parse /
//    URL.canParse answer null / false only for "does not parse".
//
// 2. `emitter.emit('error', x)` with no listener and an x that is not an
//    Error. Node: ERR_UNHANDLED_ERROR, x inspected into the message and kept
//    as `context`. oam threw a plain Error with String(x) in the message.
import { EventEmitter } from "node:events";

function protoOf(e) {
  const proto = Object.getPrototypeOf(e);
  for (const C of [TypeError, RangeError, Error]) {
    if (proto === C.prototype) return C.name + ".prototype";
  }
  return "a NodeError prototype";
}

function show(label, fn) {
  try {
    const value = fn();
    console.log(label, "->", value === null ? "null" : typeof value === "object" ? value.href : value);
  } catch (e) {
    console.log(label, "->", e.constructor.name, JSON.stringify(e.message));
    console.log(
      "   code:", e.code,
      "| own:", JSON.stringify(Object.getOwnPropertyNames(e)),
      "| enumerable:", JSON.stringify(Object.keys(e)),
    );
    console.log(
      "   input:", JSON.stringify(e.input),
      "| base:", JSON.stringify(e.base),
      "| context" in e ? "| context: " + JSON.stringify(e.context) : "| no context",
    );
    console.log(
      "   prototype:", protoOf(e),
      "| String:", String(e),
      "| stack head:", String(e.stack).split("\n")[0],
    );
  }
}

console.log("== new URL");
show("new URL('nope')", () => new URL("nope"));
show("new URL('http://[zz/')", () => new URL("http://[zz/"));
show("new URL('http://[zz/', base)", () => new URL("http://[zz/", "http://h.test/r"));
show("new URL('/a', 'nope')", () => new URL("/a", "nope"));
show("new URL('/a', undefined)", () => new URL("/a", undefined));
show("new URL('/a', null)", () => new URL("/a", null));
show("new URL('http://a.test/b', null)", () => new URL("http://a.test/b", null));
show("new URL('/a', 'http://h.test/r')", () => new URL("/a", "http://h.test/r"));
show("new URL(42)", () => new URL(42));
show("new URL()", () => new URL());
show("new URL(Symbol())", () => new URL(Symbol("s")));
show("new URL({ toString })", () => new URL({ toString: () => "http://x.test/p" }));

console.log("== href setter");
show("url.href = 'nope'", () => {
  const url = new URL("http://x.test/");
  url.href = "nope";
  return url;
});
show("url.href = 'http://y.test/q'", () => {
  const url = new URL("http://x.test/");
  url.href = "http://y.test/q";
  return url;
});

console.log("== URL.parse / URL.canParse");
show("URL.parse('nope')", () => URL.parse("nope"));
show("URL.parse('/a', 'http://h.test/')", () => URL.parse("/a", "http://h.test/"));
show("URL.parse('http://a.test/', null)", () => URL.parse("http://a.test/", null));
show("URL.parse()", () => URL.parse());
show("URL.canParse('nope')", () => URL.canParse("nope"));
show("URL.canParse('/a', 'http://h.test/')", () => URL.canParse("/a", "http://h.test/"));
show("URL.canParse('http://a.test/', null)", () => URL.canParse("http://a.test/", null));
show("URL.canParse()", () => URL.canParse());

console.log("== the validation idiom");
for (const input of ["nope", "http://ok.test/"]) {
  try {
    new URL(input);
    console.log(JSON.stringify(input), "valid");
  } catch (e) {
    if (e.code === "ERR_INVALID_URL") console.log(JSON.stringify(input), "invalid");
    else console.log(JSON.stringify(input), "RETHROWN", e.message);
  }
}

console.log("== fetch with a bad URL: the cause is URL's error");
try {
  await fetch("nope");
  console.log("no rejection");
} catch (e) {
  console.log(e.constructor.name, e.message);
  console.log("   cause:", e.cause?.constructor.name, e.cause?.message, e.cause?.code, JSON.stringify(e.cause?.input));
}

console.log("== emit('error') with no listener");
show("emit('error')", () => new EventEmitter().emit("error"));
show("emit('error', 'str')", () => new EventEmitter().emit("error", "str"));
show("emit('error', 42)", () => new EventEmitter().emit("error", 42));
show("emit('error', null)", () => new EventEmitter().emit("error", null));
show("emit('error', { a: 1 })", () => new EventEmitter().emit("error", { a: 1 }));
{
  // An Error is thrown as is.
  const real = new RangeError("the real one");
  try {
    new EventEmitter().emit("error", real);
    console.log("emit('error', err): no throw");
  } catch (e) {
    console.log("emit('error', err): same object", e === real, "| code:", e.code);
  }
}
{
  // With a listener nothing is thrown.
  const ee = new EventEmitter();
  ee.on("error", (x) => console.log("listener got", JSON.stringify(x)));
  console.log("emit returned", ee.emit("error", "str"));
}
