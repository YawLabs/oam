// Abort plumbing as node's (#401), in four parts.
//
// 1. Only an undefined reason is "no reason": `ac.abort(null)` and
//    `AbortSignal.abort(null)` keep null, and so does everything that
//    carries the reason on (throwIfAborted, AbortSignal.any, an
//    events.once / timers/promises AbortError's cause, fetch's rejection).
//    A fetch body being read is the exception: undici errors it with its
//    own AbortError, "The operation was aborted.". util.aborted resolves
//    with the abort Event, not the reason.
// 2. util.inspect heads an error with its constructor when that is not
//    what `name` says (node's improveStack): `DOMException [AbortError]: x`,
//    `Foo [Error]: x`, and a null-prototype error `[Error: null prototype]`;
//    an error whose stack cannot be built (a Symbol name) is `[object
//    Error]`, not a throw; util.format / console.log's %s inspect an error
//    (hasBuiltInToString) instead of String()-ing it. A boxed BigInt or
//    Symbol, a Blob and a File inspect as node's, and so does an error
//    whose stack is not a string (walked with no depth limit, one entry
//    per line).
// 3. One internal AbortError class: events.once / on, timers/promises,
//    stream finished / pipeline / addAbortSignal / the operators, fs and
//    readline all reject with the same constructor; fs's writeFile family
//    honours an aborted signal before it opens anything.
// 4. MessageChannel, MessagePort and BroadcastChannel are globals, the same
//    constructors node:worker_threads exports, and a port is node's
//    NodeEventTarget: queued until started, on() handed the data and
//    addEventListener / onmessage a MessageEvent, delivered after the
//    microtasks, a close that reaches the other end behind what was sent;
//    MessagePort and MessageChannel are node's function constructors, and
//    a worker's parentPort lets the worker exit once its last 'message'
//    listener is gone.
//
// Regression guard: oam replaced a null reason with a DOMException, printed
// `AbortError: x` for a DOMException, rejected with a different AbortError
// class per module (and wrote the file under an aborted signal), and had no
// global MessageChannel (its ports were EventEmitters).
import util from "node:util";
import http from "node:http";
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import stream from "node:stream";
import { finished, pipeline } from "node:stream/promises";
import { once, on, EventEmitter } from "node:events";
import { setTimeout as sleep, setImmediate as immediate, setInterval as interval } from "node:timers/promises";
import rlp from "node:readline/promises";
import wt from "node:worker_threads";

const tick = (ms = 5) => new Promise((r) => setTimeout(r, ms));
const first = (s) => s.split("\n")[0];

console.log("-- 1. null reasons");
{
  const ac = new AbortController();
  ac.abort(null);
  console.log("abort(null)", ac.signal.reason);
  console.log("AbortSignal.abort(null)", AbortSignal.abort(null).reason);
  console.log("abort(undefined)", AbortSignal.abort(undefined).reason.name);
  const ac0 = new AbortController();
  ac0.abort(0);
  console.log("abort(0)", ac0.signal.reason);
  console.log("any", AbortSignal.any([AbortSignal.abort(null)]).reason);
  const ac2 = new AbortController();
  const any2 = AbortSignal.any([ac2.signal]);
  ac2.abort(null);
  console.log("any, later", any2.reason);
  try {
    AbortSignal.abort(null).throwIfAborted();
  } catch (e) {
    console.log("throwIfAborted threw", e);
  }
  await once(new EventEmitter(), "x", { signal: AbortSignal.abort(null) }).catch((e) =>
    console.log("once cause", e.cause, "cause" in e),
  );
  await sleep(1, 1, { signal: AbortSignal.abort(null) }).catch((e) => console.log("sleep cause", e.cause));
  // Already aborted: fetch rejects before it touches the network.
  await fetch("http://127.0.0.1:1/", { signal: AbortSignal.abort(null) }).catch((e) =>
    console.log("fetch rejected with", e),
  );
  console.log("Request signal", new Request("http://x.invalid/", { signal: AbortSignal.abort(null) }).signal.reason);

  // A body being read when abort(null) lands: undici's `reason ?? new
  // DOMException(...)` on the body path, so not null; any other reason,
  // falsy ones included, is kept.
  const server = http.createServer((_req, res) => {
    res.writeHead(200);
    res.write("x");
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${server.address().port}/`;
  const what = (e) =>
    e === null || typeof e !== "object" ? `${typeof e} ${String(e)}` : `${e.constructor.name} ${e.name} ${e.message} ${e.code}`;
  for (const reason of [null, 0, "why"]) {
    const ac = new AbortController();
    const res = await fetch(url, { signal: ac.signal });
    const reader = res.body.getReader();
    await reader.read();
    const pending = reader.read();
    ac.abort(reason);
    await pending.catch((e) => console.log("body abort", String(reason), what(e)));
    const ac2 = new AbortController();
    const res2 = await fetch(url, { signal: ac2.signal });
    ac2.abort(reason);
    await res2.body.getReader().read().catch((e) => console.log("body after abort", String(reason), what(e)));
  }
  server.closeAllConnections();
  server.close();

  // util.aborted: the abort Event (resolve is the listener itself), even
  // past a stopImmediatePropagation; undefined for an already-aborted
  // signal; node's argument checks, as rejections.
  for (const reason of [null, "why"]) {
    const ac = new AbortController();
    ac.signal.addEventListener("abort", (e) => e.stopImmediatePropagation());
    const p = util.aborted(ac.signal, {});
    ac.abort(reason);
    const ev = await p;
    console.log("util.aborted", String(reason), ev.constructor.name, ev.type, ev.target === ac.signal);
  }
  console.log("util.aborted already", await util.aborted(AbortSignal.abort(null), {}));
  for (const [signal, resource] of [[undefined, {}], [null, {}], [{}, {}], [new AbortController().signal, null], [new AbortController().signal, 1]]) {
    const p = util.aborted(signal, resource);
    await p.catch((e) => console.log("util.aborted", p instanceof Promise, e.name, e.code, e.message));
  }
}

console.log("-- 2. inspect headers");
{
  const show = (label, v) => console.log(label.padEnd(10), first(util.inspect(v)));
  show("dom", new DOMException("x", "AbortError"));
  show("dom bare", new DOMException("x"));
  show("dom clone", new DOMException("x", "DataCloneError"));
  show("dom odd", new DOMException("x", "NotAnError"));
  class Foo extends Error {}
  show("Foo", new Foo("x"));
  class BarError extends Error {}
  show("BarError", new BarError("x"));
  class Baz extends TypeError {
    constructor(m) {
      super(m);
      this.name = "Baz";
    }
  }
  show("Baz", new Baz("x"));
  class Qux extends Error {
    get name() {
      return "QuxError";
    }
  }
  show("Qux", new Qux("x"));
  class MyAbortError extends Error {
    constructor() {
      super("m");
      this.name = "AbortError";
    }
  }
  show("MyAbort", new MyAbortError());
  const renamed = new Error("y");
  renamed.name = "CustomError";
  show("renamed", renamed);
  show("null-proto", Object.setPrototypeOf(new Error("np"), null));
  show("null-proto2", Object.setPrototypeOf(new TypeError("np2"), null));
  const numName = new Error("q");
  numName.name = 5;
  show("num name", numName);
  const noStack = new RangeError("ns");
  noStack.stack = undefined;
  show("no stack", noStack);
  const bare = new DOMException("b", "AbortError");
  Object.defineProperty(bare, "stack", { value: "AbortError: b" });
  show("no frames", bare);
  const tagged = new Error("t");
  Object.defineProperty(tagged, Symbol.toStringTag, { value: "Tagz" });
  show("tagged", tagged);
  class TagErr extends Error {
    get [Symbol.toStringTag]() {
      return "Other";
    }
  }
  show("TagErr", new TagErr("x"));
  console.log(String(new DOMException("x", "AbortError")), "|", first(new DOMException("x", "AbortError").stack));
  // A stack V8 cannot build -- a Symbol name, a throwing name getter -- is
  // `[object Error]` (with the keys), as node's formatError falls back to.
  const symName = new Error("s");
  symName.name = Symbol("q");
  show("sym name", symName);
  class SymProto extends Error {}
  Object.defineProperty(SymProto.prototype, "name", { value: Symbol("p") });
  show("sym proto", new SymProto("s"));
  const throwName = new Error("s");
  Object.defineProperty(throwName, "name", {
    get() {
      throw new Error("nope");
    },
  });
  show("throw name", throwName);
  const readFirst = new Error("s");
  void readFirst.stack;
  Object.defineProperty(readFirst, "name", {
    get() {
      throw new Error("nope");
    },
  });
  show("read first", readFirst);
  const throwStack = new Error("s");
  Object.defineProperty(throwStack, "stack", {
    get() {
      throw new Error("st");
    },
  });
  show("throw stack", throwStack);
  const symStack = new Error("s");
  Object.defineProperty(symStack, "stack", { value: Symbol("st") });
  console.log("sym stack ", JSON.stringify(util.inspect(symStack)));
  try {
    symName.stack;
  } catch (e) {
    console.log("sym name stack threw", e.constructor.name);
  }
  symName.name = "Fine";
  console.log("then", first(symName.stack));
  // %s inspects what has a built-in toString -- an error, a boxed number,
  // a plain object -- and String()s the rest: a typed array, a URL, a
  // Buffer, an object with its own toString.
  const noFrames = (e) => {
    Object.defineProperty(e, "stack", { value: e.stack.split("\n")[0] });
    return e;
  };
  const fmt = (label, v) =>
    console.log(label.padEnd(10), JSON.stringify(util.format("%s", v).split("\n").filter((l) => !/^\s+at /.test(l)).join("\n")));
  fmt("fmt dom", new DOMException("x", "AbortError"));
  fmt("fmt Foo", new Foo("x"));
  fmt("fmt Error", new Error("x"));
  console.log("fmt frames", /\n {4}at /.test(util.format("%s", new Error("x"))));
  fmt("fmt ownTs", Object.assign(new Error("o"), { toString: () => "mine" }));
  fmt("fmt number", new Number(3));
  fmt("fmt string", new String("s"));
  fmt("fmt u8", new Uint8Array(2));
  fmt("fmt url", new URL("http://a/b"));
  fmt("fmt buf", Buffer.from("hi"));
  fmt("fmt map", new Map([[1, 2]]));
  fmt("fmt protoTs", Object.create({ toString: () => "pts" }));
  // A boxed BigInt and Symbol inspect as node's getBoxedBase spells them,
  // recognised by internal slot (so a nulled or swapped prototype still
  // reads as a box), the whole base coloured only when no keys follow; a
  // Blob and a File by node's custom inspect, at %s's depth 0 too.
  fmt("fmt bigint", Object(1n));
  fmt("fmt symbol", Object(Symbol("q")));
  const keyedBig = Object.assign(Object(2n), { x: 1 });
  const nullBig = Object.setPrototypeOf(Object(5n), null);
  const nullSym = Object.setPrototypeOf(Object(Symbol("z")), null);
  const nullNum = Object.setPrototypeOf(Object(5), null);
  class SubBig {}
  const subBig = Object.setPrototypeOf(Object(7n), SubBig.prototype);
  for (const [label, v, opts] of [
    ["box keyed", keyedBig],
    ["box null", [nullBig, nullSym, nullNum]],
    ["box sub", subBig],
    ["box sep", Object(123456789n), { numericSeparator: true }],
    ["box -0", [Object(-0), Object(Symbol.iterator), Object(Symbol())]],
    ["box -0sep", Object(-0), { numericSeparator: true }],
    ["box color", [Object(1n), Object(Symbol("c")), Object(1), Object("s"), keyedBig], { colors: true }],
    ["box fake", Object.create(Number.prototype)],
  ]) {
    console.log(label.padEnd(10), JSON.stringify(util.inspect(v, opts)));
  }
  const boxes = [Object(1n), Object(Symbol()), Object(1), Object(""), Object(false), 1n, Object.create(Number.prototype)];
  for (const k of ["isBigIntObject", "isSymbolObject", "isNumberObject", "isStringObject", "isBooleanObject", "isBoxedPrimitive"]) {
    console.log(k.padEnd(16), boxes.map((v) => util.types[k](v)).join());
  }
  class MyBlob extends Blob {}
  class Xlob extends Blob {}
  class Blobby extends Blob {}
  const blob = new Blob(["a"]);
  const file = new File(["xyz"], "a.b", { type: "t/p", lastModified: 5 });
  fmt("fmt blob", blob);
  fmt("fmt file", file);
  for (const [label, v, opts] of [
    ["blob", blob],
    ["blob d0", blob, { depth: 0 }],
    ["blob d-1", blob, { depth: -1 }],
    ["blob deep", { x: { y: { z: blob } } }],
    ["blob in", { b: blob, a: [blob] }],
    ["blob sub", { x: { y: { z: new MyBlob([]) } } }],
    ["blob sub0", new MyBlob(["ab"])],
    ["blob subs0", [new Xlob([]), new Blobby([]), new MyBlob([])], { depth: 0 }],
    ["blob color", blob, { colors: true }],
    ["blob loose", blob, { compact: false }],
    ["file", file],
    ["file deep", { x: { y: { z: file } } }],
    ["file d-1", file, { depth: -1, colors: true }],
    ["file keys", Object.keys(file)],
  ]) {
    console.log(label.padEnd(10), JSON.stringify(util.inspect(v, opts)));
  }
  // An error whose stack is not a string: node's getStackString hands it
  // to formatValue with no recurseTimes, so it is walked with no depth
  // limit and every non-empty object or array breaks one entry per line.
  const withStack = (s, ctor = Error) => {
    const e = new ctor("v");
    e.stack = s;
    return e;
  };
  const circ = { n: 1 };
  circ.self = circ;
  const selfRef = new Error("self");
  selfRef.stack = { err: selfRef };
  for (const [label, v, opts] of [
    ["stk obj", withStack({ a: 1 })],
    ["stk arr", withStack([1, 2])],
    ["stk deep", withStack({ a: { b: { c: { d: { e: { f: 1 } } } } } }), { depth: 0 }],
    ["stk two", withStack({ a: 1, b: "x" })],
    ["stk num", withStack(42)],
    ["stk null", withStack(null)],
    ["stk true", withStack(true)],
    ["stk empty", [withStack([]), withStack({})]],
    ["stk box", [withStack(Object(1n)), withStack(5n), withStack(Symbol("s"))]],
    ["stk keyed", Object.assign(withStack({ a: 1 }), { code: "X" })],
    ["stk nested", { x: { y: { z: withStack({ q: { r: 1 } }) } } }],
    ["stk long", withStack(Array.from({ length: 30 }, (_, i) => i))],
    ["stk coll", [withStack(new Map([[1, { a: 1 }]])), withStack(new Set([1, 2])), withStack(new Uint8Array(2))]],
    ["stk circ", [withStack(circ), selfRef]],
    ["stk loose", withStack({ a: 1 }), { compact: false }],
    ["stk tight", withStack({ a: 1, b: [1] }), { compact: true }],
    ["stk type", withStack([1, { q: 2 }], TypeError)],
    ["stk color", withStack({ a: 1 }), { colors: true }],
  ]) {
    console.log(label.padEnd(10), JSON.stringify(util.inspect(v, opts)));
  }
  fmt("fmt stk", withStack({ a: 1 }));
  console.log("O stk     ", JSON.stringify(util.format("%O", withStack([1]))));
  console.log("%s|%s", noFrames(new DOMException("c", "AbortError")), noFrames(new Foo("c")));
  // The [cause] line of an inspected AbortError, frames left out.
  await once(new EventEmitter(), "x", { signal: AbortSignal.abort() }).catch((err) =>
    console.log(util.inspect(err).split("\n").filter((l) => !/^\s+at /.test(l)).join("\n")),
  );
}

console.log("-- 3. one AbortError class");
{
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-417-")));
  try {
    const sig = () => AbortSignal.abort("why");
    const errs = [];
    const grab = async (name, fn) => {
      try {
        await fn();
        errs.push([name, null]);
      } catch (e) {
        errs.push([name, e]);
      }
    };
    await grab("events.once", () => once(new EventEmitter(), "x", { signal: sig() }));
    await grab("events.on", async () => {
      for await (const _ of on(new EventEmitter(), "x", { signal: sig() }));
    });
    await grab("events.on late", async () => {
      const ac = new AbortController();
      setTimeout(() => ac.abort("late"), 5);
      for await (const _ of on(new EventEmitter(), "x", { signal: ac.signal }));
    });
    await grab("on EventTarget", async () => {
      for await (const _ of on(new EventTarget(), "x", { signal: sig() }));
    });
    await grab("setTimeout", () => sleep(1, 1, { signal: sig() }));
    await grab("setImmediate", () => immediate(1, { signal: sig() }));
    await grab("setInterval", async () => {
      for await (const _ of interval(1, 1, { signal: sig() }));
    });
    await grab("finished", () => finished(new stream.PassThrough(), { signal: sig() }));
    await grab("pipeline", () =>
      pipeline(stream.Readable.from(["a"]), new stream.PassThrough(), { signal: sig() }),
    );
    await grab("addAbortSignal", () =>
      new Promise((_res, rej) => stream.addAbortSignal(sig(), new stream.PassThrough()).on("error", rej)),
    );
    await grab("Readable.map", () => stream.Readable.from([1]).map((x) => x, { signal: sig() }).toArray());
    await grab("fsp.readFile", () => fsp.readFile(new URL(import.meta.url), { signal: sig() }));
    await grab("fs.readFile", () =>
      new Promise((res, rej) =>
        fs.readFile(new URL(import.meta.url), { signal: sig() }, (e) => (e ? rej(e) : res())),
      ),
    );
    // fs.promises.watch: aborted before the first next(), and while waiting.
    await grab("fsp.watch", async () => {
      for await (const _ of fsp.watch(dir, { signal: sig() }));
    });
    await grab("fsp.watch late", async () => {
      const ac = new AbortController();
      setTimeout(() => ac.abort("late"), 5);
      for await (const _ of fsp.watch(dir, { signal: ac.signal }));
    });
    await grab("rl.question", () => {
      const rl = rlp.createInterface({ input: new stream.PassThrough() });
      return rl.question("", { signal: sig() }).finally(() => rl.close());
    });
    const base = errs[0][1].constructor;
    for (const [name, e] of errs) {
      console.log(
        name.padEnd(15),
        e === null
          ? "no error"
          : `${e.name} ${e.code} same=${e.constructor === base} keys=${JSON.stringify(Reflect.ownKeys(e))}` +
              ` cause=${String(e.cause)}`,
      );
    }
    console.log(base.name, base.length, Object.getPrototypeOf(base) === Error, new base().message);

    // The writeFile family under an aborted signal: nothing opened, nothing
    // written (a callback form answers synchronously); mid-iteration, the
    // chunks before the abort are written.
    const at = (f) => path.join(dir, f);
    const state = (f) => (fs.existsSync(at(f)) ? `"${fs.readFileSync(at(f), "utf8")}"` : "absent");
    const showAbort = (label, e) => console.log(label, e?.constructor === base, e?.code, String(e?.cause));
    fs.writeFileSync(at("kept"), "old");
    await fsp.writeFile(at("a"), "x", { signal: AbortSignal.abort("r1") }).catch((e) => showAbort("fsp.writeFile", e));
    await fsp.writeFile(at("kept"), "x", { signal: AbortSignal.abort("r2") }).catch((e) => showAbort("fsp.writeFile kept", e));
    await fsp.appendFile(at("b"), "x", { signal: AbortSignal.abort("r3") }).catch((e) => showAbort("fsp.appendFile", e));
    await fsp.writeFile(at(path.join("missing", "q")), "x", { signal: AbortSignal.abort("r4") }).catch((e) =>
      showAbort("fsp.writeFile missing dir", e),
    );
    let sync = true;
    fs.writeFile(at("c"), "x", { signal: AbortSignal.abort(null) }, (e) => showAbort(`fs.writeFile sync=${sync}`, e));
    fs.appendFile(at("kept"), "x", { signal: AbortSignal.abort("r5") }, (e) => showAbort(`fs.appendFile sync=${sync}`, e));
    const fd = fs.openSync(at("d"), "w");
    fs.writeFile(fd, "x", { signal: AbortSignal.abort("r6") }, (e) => showAbort(`fs.writeFile fd sync=${sync}`, e));
    sync = false;
    fs.closeSync(fd);
    const fh = await fsp.open(at("e"), "w");
    await fh.writeFile("x", { signal: AbortSignal.abort("r7") }).catch((e) => showAbort("FileHandle.writeFile", e));
    await fh.close();
    const ac = new AbortController();
    async function* chunks() {
      yield "one";
      ac.abort("mid");
      yield "two";
    }
    await fsp.writeFile(at("f"), chunks(), { signal: ac.signal }).catch((e) => showAbort("fsp.writeFile iterable", e));
    for (const f of ["a", "b", "c", "d", "e", "f", "kept"]) console.log(f, state(f));
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

console.log("-- 4. MessageChannel / MessagePort");
{
  for (const name of ["MessageChannel", "MessagePort", "BroadcastChannel"]) {
    const d = Object.getOwnPropertyDescriptor(globalThis, name);
    console.log(name, typeof d.value, d.writable, d.enumerable, d.configurable, d.value === wt[name]);
  }
  console.log(MessageChannel.length, BroadcastChannel.length, Object.keys(globalThis).filter((k) => /Message|Broadcast/.test(k)));
  // node's C++ function constructors: MessagePort.length 0, its own eight
  // members (enumerable) and no addEventListener of its own; a call without
  // `new` refused with node's codes.
  console.log(
    "MessagePort",
    MessagePort.length,
    Object.getPrototypeOf(MessagePort).name,
    Object.getOwnPropertyNames(MessagePort.prototype).sort().join(),
    Object.keys(MessagePort.prototype).join(),
    Object.getOwnPropertyNames(MessageChannel.prototype).join(),
  );
  for (const call of [() => MessageChannel(), () => MessageChannel.call({}), () => MessagePort()]) {
    try {
      call();
    } catch (e) {
      console.log(e.constructor.name, e.code, e.message, Reflect.ownKeys(e).join());
    }
  }
  class SubChannel extends MessageChannel {}
  const sub = new SubChannel();
  console.log("subclass", sub instanceof MessageChannel, sub.port1 instanceof MessagePort, Object.keys(sub).join());
  sub.port1.close();
  const chain = [];
  for (let p = Object.getPrototypeOf(new MessageChannel().port1); p; p = Object.getPrototypeOf(p)) {
    chain.push(p.constructor.name);
  }
  console.log(chain.join(" > "), Object.prototype.toString.call(new MessageChannel()));
  try {
    new MessagePort();
  } catch (e) {
    console.log(e.constructor.name, e.code, e.message, Reflect.ownKeys(e));
  }
  {
    const { port1, port2 } = new MessageChannel();
    console.log("keys", Object.keys(new MessageChannel()), port1.onmessage, port1.hasRef());
    try {
      port1.postMessage();
    } catch (e) {
      console.log(e.constructor.name, e.code, e.message);
    }
    for (const v of [() => {}, Symbol("s"), { f() {} }]) {
      try {
        port1.postMessage(v);
      } catch (e) {
        console.log(e.name, e.code, e.message);
      }
    }
    port1.close();
    port2.close();
  }
  // Queued until started; after the microtasks, before a later setImmediate;
  // a structured clone; on() gets the data, addEventListener an event.
  {
    const order = [];
    const { port1, port2 } = new MessageChannel();
    const sent = { d: new Date(0), m: new Map([[1, 2]]) };
    port1.postMessage("early");
    await tick();
    port2.on("message", (v) =>
      order.push(typeof v === "string" ? `on:${v}` : `on:${v === sent}:${v.d instanceof Date}:${v.m.get(1)}`),
    );
    port2.addEventListener("message", (e) =>
      order.push(`ael:${e.constructor.name}:${e.type}:${e.target === port2}:${e.data === sent}`),
    );
    port1.postMessage(sent);
    Promise.resolve().then(() => order.push("micro"));
    setImmediate(() => order.push("immediate"));
    await tick();
    console.log(order.join(" | "), port2.hasRef(), port2.listenerCount("message"));
    port1.close();
    await tick();
  }
  // Between two messages run every tick and microtask the first listener
  // queued, and no immediate: messages come before the check phase.
  {
    const out = [];
    const { port1, port2 } = new MessageChannel();
    port2.on("message", async (v) => {
      out.push(`m${v}`);
      process.nextTick(() => out.push(`tick${v}`));
      Promise.resolve().then(() => out.push(`p${v}`)).then(() => out.push(`pp${v}`));
      await null;
      out.push(`await${v}`);
    });
    setImmediate(() => out.push("immediate before"));
    port1.postMessage(1);
    port1.postMessage(2);
    setImmediate(() => out.push("immediate after"));
    await tick();
    console.log(out.join(" | "));
    port1.close();
    await tick();
  }
  // onmessage; a close from a handler still lets the rest of the batch out.
  {
    const { port1, port2 } = new MessageChannel();
    port1.postMessage("q");
    await tick();
    port2.onmessage = (e) => {
      console.log("onmessage", e.data, e.ports.length);
      if (e.data === "r") port2.close();
    };
    console.log(typeof port2.onmessage, port2.listenerCount("message"));
    port1.postMessage("r");
    port1.postMessage("s");
    await tick();
  }
  // close(): this end's queue goes; the other end hears of it behind what
  // was sent; close callbacks; hasRef until the close lands.
  {
    const { port1, port2 } = new MessageChannel();
    port2.on("message", (v) => console.log("p2 got", v));
    port2.on("close", (e) => console.log("p2 close", e.constructor.name, e.type));
    port1.on("close", () => console.log("p1 close"));
    port1.postMessage(1);
    port1.close();
    port1.postMessage(2);
    console.log("hasRef right after close", port1.hasRef(), port2.hasRef());
    await tick();
    console.log("hasRef after", port1.hasRef(), port2.hasRef());
    const c = new MessageChannel();
    c.port2.on("message", (v) => console.log("dropped?", v));
    c.port1.postMessage("a");
    c.port2.close(() => console.log("close cb 1"));
    c.port2.close(() => console.log("close cb 2"));
    await tick();
    const e = new MessageChannel();
    e.port1.postMessage("x");
    e.port1.close();
    e.port2.on("close", () => console.log("e close"));
    await tick();
    console.log("e not started yet");
    e.port2.start();
    await tick();
  }
  // Removing the last 'message' listener stops and unrefs the port; once();
  // emit(); receiveMessageOnPort.
  {
    const { port1, port2 } = new MessageChannel();
    const f = (v) => console.log("f", v);
    port2.on("message", f);
    port1.postMessage(1);
    await tick();
    port2.off("message", f);
    console.log("after off", port2.hasRef());
    port1.postMessage(2);
    await tick();
    port2.addEventListener("message", (ev) => console.log("ael", ev.data));
    await tick();
    port2.once("message", (v) => console.log("once", v));
    port1.postMessage(3);
    port1.postMessage(4);
    await tick();
    console.log("emit", port2.emit("message", "manual"), port2.emit("other", 1), port2.listenerCount("message"));
    port1.close();
    await tick();
    const r = new MessageChannel();
    r.port1.postMessage({ a: 1 });
    r.port1.postMessage(2);
    const take = () => JSON.stringify(wt.receiveMessageOnPort(r.port2));
    console.log("receive", take(), take(), take());
    r.port1.close();
    console.log("receive after close", take());
    try {
      wt.receiveMessageOnPort({});
    } catch (err) {
      console.log(err.constructor.name, err.code, err.message);
    }
    await tick();
  }
  // A listener that throws: the error is uncaught, the next listener runs.
  {
    const { port1, port2 } = new MessageChannel();
    process.once("uncaughtException", (err) => console.log("uncaught", err.message));
    port2.on("message", () => {
      throw new Error("boom");
    });
    port2.on("message", (v) => console.log("second listener", v));
    port1.postMessage("m");
    await tick(10);
    port1.close();
    await tick();
  }
  try {
    structuredClone(Symbol("t"));
  } catch (e) {
    console.log("structuredClone", e.name, e.message);
  }
  // A worker's parentPort: its first 'message' listener holds the worker
  // open, and the last one going -- once() after it ran, off(),
  // removeEventListener(), onmessage = null -- lets it exit; start()
  // alone holds nothing.
  const wdir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-417w-")));
  try {
    const workers = {
      once: "parentPort.once('message', (v) => parentPort.postMessage(v * 2));",
      off: "const f = (v) => { parentPort.postMessage(v * 2); parentPort.off('message', f); }; parentPort.on('message', f);",
      onmessage: "parentPort.onmessage = (e) => { parentPort.postMessage(e.data * 2); parentPort.onmessage = null; };",
      removeEventListener:
        "const f = (e) => { parentPort.postMessage(e.data * 2); parentPort.removeEventListener('message', f); }; parentPort.addEventListener('message', f);",
      start: "parentPort.start(); parentPort.postMessage('started ' + parentPort.hasRef());",
    };
    for (const [name, body] of Object.entries(workers)) {
      const file = path.join(wdir, `${name}.cjs`);
      fs.writeFileSync(file, `const { parentPort } = require("node:worker_threads");\n${body}\n`);
      await new Promise((resolve) => {
        const w = new wt.Worker(file);
        const got = [];
        // A worker still alive well after its listener went is the bug.
        const timer = setTimeout(() => {
          console.log("worker", name, "still alive", got.join());
          w.terminate();
        }, 5000);
        w.on("message", (m) => got.push(m));
        w.on("exit", (code) => {
          clearTimeout(timer);
          console.log("worker", name, "exit", code, got.join());
          resolve();
        });
        if (name !== "start") w.postMessage(21);
      });
    }
  } finally {
    fs.rmSync(wdir, { recursive: true, force: true });
  }
}
console.log("done");
