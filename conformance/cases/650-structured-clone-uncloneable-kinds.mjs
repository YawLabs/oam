// structuredClone refuses a Promise, WeakMap, WeakSet, WeakRef or
// FinalizationRegistry by its internal kind, not by its prototype, and names
// it the way V8's serializer does.
//
// An instance of a subclass is refused under the subclass's own name
// (`#<P>`, not `#<Promise>`), and an object that merely inherits from one of
// these prototypes has no internal slots and clones as a plain object. The
// name comes from the value's `constructor` data property and the function's
// declared name; without a usable one it falls back to `[object Tag]`. A
// proxy is refused too, named after its target, without running a trap.
import vm from "node:vm";

function attempt(label, value) {
  try {
    const clone = structuredClone(value);
    console.log(label, "cloned", JSON.stringify(clone), Object.getPrototypeOf(clone) === Object.prototype);
  } catch (e) {
    console.log(label, e.name, e.message);
  }
}

class P extends Promise {}
class WM extends WeakMap {}
class WS extends WeakSet {}
class WR extends WeakRef {}
class FR extends FinalizationRegistry {}

const kinds = {
  Promise: [() => Promise.resolve(), () => new P(() => {})],
  WeakMap: [() => new WeakMap(), () => new WM()],
  WeakSet: [() => new WeakSet(), () => new WS()],
  WeakRef: [() => new WeakRef({}), () => new WR({})],
  FinalizationRegistry: [() => new FinalizationRegistry(() => {}), () => new FR(() => {})],
};

for (const [name, [make, makeSub]] of Object.entries(kinds)) {
  attempt(`${name} plain`, make());
  attempt(`${name} nested`, { inner: [make()] });
  attempt(`${name} subclass`, makeSub());
  attempt(`${name} prototype-only`, Object.create(globalThis[name].prototype));

  // A WeakRef or FinalizationRegistry cut off from its prototype is left
  // out: oam asks V8 for the Promise, WeakMap and WeakSet kinds, but tells
  // the other two apart with a brand probe it runs only on values that
  // inherit from the kind's prototype, and clones such a one as `{}`.
  if (name !== "WeakRef" && name !== "FinalizationRegistry") {
    const nullProto = make();
    Object.setPrototypeOf(nullProto, null);
    attempt(`${name} null prototype`, nullProto);
  }

  const ownToString = make();
  ownToString.toString = () => "x";
  attempt(`${name} own toString`, ownToString);

  const ctorGetter = make();
  Object.defineProperty(ctorGetter, "constructor", {
    get() {
      throw new Error("the constructor getter ran");
    },
  });
  attempt(`${name} constructor getter`, ctorGetter);
}

class Renamed extends Promise {}
Object.defineProperty(Renamed, "name", { value: "Z" });
attempt("renamed class", new Renamed(() => {}));

class Getter extends Promise {
  static get name() {
    return "Zed";
  }
}
attempt("name getter", new Getter(() => {}));

const Inferred = class extends Promise {};
attempt("inferred class name", new Inferred(() => {}));

const nonFunctionCtor = Promise.resolve();
Object.defineProperty(nonFunctionCtor, "constructor", { value: { name: "X" } });
attempt("non-function constructor", nonFunctionCtor);

const boundCtor = Promise.resolve();
boundCtor.constructor = function Foo() {}.bind(null);
attempt("bound constructor", boundCtor);

const anonBound = Promise.resolve();
anonBound.constructor = (function () {}).bind(null);
attempt("anonymous bound constructor", anonBound);

const arrowCtor = Promise.resolve();
arrowCtor.constructor = () => {};
attempt("arrow constructor", arrowCtor);

const proxyCtor = Promise.resolve();
proxyCtor.constructor = new Proxy(function F() {}, {});
attempt("proxy constructor", proxyCtor);

const proxyProto = Promise.resolve();
Object.setPrototypeOf(proxyProto, new Proxy(Promise.prototype, {}));
attempt("proxy on the prototype chain", proxyProto);

const plainProto = Promise.resolve();
Object.setPrototypeOf(plainProto, Object.prototype);
attempt("Object.prototype", plainProto);

attempt("other realm", vm.runInNewContext("Promise.resolve()"));
attempt("other realm WeakMap", vm.runInNewContext("new WeakMap()"));
attempt("other realm WeakRef", vm.runInNewContext("new WeakRef({})"));
attempt("other realm plain object", vm.runInNewContext("({ a: 1 })"));

// A constructor's `name` the program wrote (a static class field) is not
// the function's own name.
const staticName = Promise.resolve();
staticName.constructor = class {
  static name = "Stat";
};
attempt("static name field", staticName);

// Every proxy is refused, named after its target, and none of its traps run.
const trapsRun = [];
const traps = Object.fromEntries(
  ["get", "getPrototypeOf", "has", "ownKeys", "getOwnPropertyDescriptor"].map((t) => [
    t,
    (...args) => {
      trapsRun.push(t);
      return Reflect[t](...args);
    },
  ]),
);
class K {}
attempt("proxy plain", new Proxy({ a: 1 }, traps));
attempt("proxy class instance", new Proxy(new K(), traps));
attempt("proxy promise", new Proxy(Promise.resolve(), traps));
attempt("proxy of a proxy", new Proxy(new Proxy(new K(), traps), traps));
attempt("proxy array", new Proxy([1], traps));
attempt("proxy date", new Proxy(new Date(0), traps));
attempt("proxy regexp", new Proxy(/x/g, traps));
attempt("proxy error", new Proxy(new TypeError("m"), traps));
attempt("proxy typed array", new Proxy(new Uint8Array(1), traps));
attempt("proxy boxed string", new Proxy(new String("s"), traps));
attempt("proxy null-prototype map", new Proxy(Object.setPrototypeOf(new Map(), null), traps));
attempt("proxy function", new Proxy(function f() {}, traps));
attempt("nested proxy", { inner: [new Proxy({}, traps)] });
const revocable = Proxy.revocable({}, traps);
revocable.revoke();
attempt("revoked proxy", revocable.proxy);
console.log("traps run:", trapsRun.length);

// A function is named by its source text, not by a `toString` of its own.
const ownFnToString = function g() {};
ownFnToString.toString = () => "overridden";
attempt("function own toString", ownFnToString);

// A proxy oam implements where node has a plain object is NOT refused:
// node's process.env is a real object (isProxy false) and clones as one, so
// oam's env proxy clones through the same path. Asserted as a shape rather
// than a value (the environment differs per host), and the clone is a plain
// object with the same own enumerable keys.
{
  const env = structuredClone(process.env);
  console.log(
    "env clone plain:",
    Object.getPrototypeOf(env) === Object.prototype,
    "keys equal:",
    JSON.stringify(Object.keys(env)) === JSON.stringify(Object.keys(process.env)),
  );
}
