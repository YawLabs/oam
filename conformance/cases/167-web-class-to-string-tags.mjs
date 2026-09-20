// Every web class node ships carries a Symbol.toStringTag, so
// Object.prototype.toString.call(x) names the class. Libraries brand-check
// on exactly that string: @sindresorhus/is, which got 14 uses to validate
// the URL it follows a redirect to, refuses a URL whose tag reads 'Object',
// so every got 14 redirect failed with "Option 'url': Expected values which
// are string, URL, or undefined. Received values of type Object".
//
// node's descriptor is Web IDL's: a data property on the PROTOTYPE, not
// writable, not enumerable, configurable. A getter is NOT the same thing --
// Object.getOwnPropertyDescriptor(C.prototype, Symbol.toStringTag).value is
// what a strict brand check reads.
//
// Only classes oam ships are listed; node's Performance, Crypto,
// SubtleCrypto, CryptoKey and MessageChannel have tags oam has no class for.
const named = [
  "URL", "URLSearchParams", "Headers", "Request", "Response",
  "AbortSignal", "AbortController", "FormData", "Blob", "File",
  "Event", "EventTarget", "CustomEvent", "MessageEvent", "DOMException",
  "TextEncoder", "TextDecoder", "ReadableStream", "WritableStream",
  "TransformStream", "CountQueuingStrategy", "ByteLengthQueuingStrategy",
  "WebSocket",
];
for (const name of named) {
  const ctor = globalThis[name];
  const d = Object.getOwnPropertyDescriptor(ctor.prototype, Symbol.toStringTag);
  console.log(
    name,
    d === undefined
      ? "NO TAG"
      : `value=${JSON.stringify(d.value)} get=${d.get !== undefined}` +
          ` w=${d.writable} e=${d.enumerable} c=${d.configurable}`,
  );
}
// Classes node leaves untagged: they read as the base class they extend.
for (const name of ["BroadcastChannel", "MessagePort", "TextEncoderStream", "TextDecoderStream"]) {
  const ctor = globalThis[name];
  const d = Object.getOwnPropertyDescriptor(ctor.prototype, Symbol.toStringTag);
  console.log(name, d === undefined ? "NO TAG" : `value=${JSON.stringify(d.value)}`);
}
console.log("---");
const bc = new BroadcastChannel("oam-conformance-tag");
const instances = [
  ["URL", new URL("https://example.test/p?q=1")],
  ["URLSearchParams", new URL("https://example.test/p?q=1").searchParams],
  ["Headers", new Headers()],
  ["AbortSignal", new AbortController().signal],
  ["AbortController", new AbortController()],
  ["Response", new Response("x")],
  ["Request", new Request("https://example.test/")],
  ["Blob", new Blob(["x"])],
  ["File", new File(["x"], "f.txt")],
  ["FormData", new FormData()],
  ["Event", new Event("e")],
  ["EventTarget", new EventTarget()],
  ["CustomEvent", new CustomEvent("e")],
  ["DOMException", new DOMException("m", "AbortError")],
  ["TextEncoder", new TextEncoder()],
  ["TextDecoder", new TextDecoder()],
  ["ReadableStream", new ReadableStream()],
  ["WritableStream", new WritableStream()],
  ["TransformStream", new TransformStream()],
  ["BroadcastChannel", bc],
  ["process", process],
];
for (const [name, value] of instances) {
  console.log(name, Object.prototype.toString.call(value));
}
bc.close();
console.log("---");
// @sindresorhus/is (got 14's type guard) is exactly this.
const is = (value) => Object.prototype.toString.call(value).slice(8, -1);
const url = new URL("https://example.test/hello?q=redirected");
console.log("is(url)", is(url), "accepted", is(url) === "URL" || typeof url === "string");
console.log("tag on the instance itself", url[Symbol.toStringTag]);
console.log("own tag on the instance", Object.prototype.hasOwnProperty.call(url, Symbol.toStringTag));
console.log("String(url)", String(url));
