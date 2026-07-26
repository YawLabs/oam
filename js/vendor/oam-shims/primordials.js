// The primordials object handed to every vendored-file wrapper as its 5th
// parameter (docs/design/streams-port.md section 5.1). This is the exact
// 35-name union mechanically extracted from the vendored v22.22.2 sources
// (grep `= primordials` destructures) -- re-derive on every re-vendor, per
// js/vendor/node-streams/UPSTREAM step 3. Plain-JS stand-ins in the
// readable-stream ours/primordials.js style: same observable behavior, no
// tamper-proofing (oam does not freeze intrinsics).
"use strict";
(() => {
  const uncurryThis =
    (fn) =>
    (self, ...args) =>
      fn.apply(self, args);

  // ES2024; probe at snapshot-build time, polyfill if the pinned V8 predates it.
  const PromiseWithResolvers =
    typeof Promise.withResolvers === "function"
      ? Promise.withResolvers.bind(Promise)
      : () => {
          let resolve, reject;
          const promise = new Promise((res, rej) => {
            resolve = res;
            reject = rej;
          });
          return { promise, resolve, reject };
        };

  globalThis.__oamVendor._primordials = {
    ArrayIsArray: Array.isArray,
    ArrayPrototypeIndexOf: uncurryThis(Array.prototype.indexOf),
    ArrayPrototypePop: uncurryThis(Array.prototype.pop),
    ArrayPrototypePush: uncurryThis(Array.prototype.push),
    ArrayPrototypeSlice: uncurryThis(Array.prototype.slice),
    Boolean,
    Error,
    FunctionPrototypeCall: uncurryThis(Function.prototype.call),
    FunctionPrototypeSymbolHasInstance: (self, instance) =>
      Function.prototype[Symbol.hasInstance].call(self, instance),
    MathFloor: Math.floor,
    Number,
    NumberIsInteger: Number.isInteger,
    NumberIsNaN: Number.isNaN,
    NumberParseInt: Number.parseInt,
    ObjectDefineProperties: Object.defineProperties,
    ObjectDefineProperty: Object.defineProperty,
    ObjectGetOwnPropertyDescriptor: Object.getOwnPropertyDescriptor,
    ObjectKeys: Object.keys,
    ObjectSetPrototypeOf: Object.setPrototypeOf,
    Promise,
    PromisePrototypeThen: uncurryThis(Promise.prototype.then),
    PromiseReject: Promise.reject.bind(Promise),
    PromiseResolve: Promise.resolve.bind(Promise),
    PromiseWithResolvers,
    ReflectApply: Reflect.apply,
    ReflectOwnKeys: Reflect.ownKeys,
    SafeSet: Set,
    StringPrototypeToLowerCase: uncurryThis(String.prototype.toLowerCase),
    Symbol,
    SymbolAsyncIterator: Symbol.asyncIterator,
    SymbolFor: Symbol.for,
    SymbolHasInstance: Symbol.hasInstance,
    SymbolIterator: Symbol.iterator,
    SymbolSpecies: Symbol.species,
    TypedArrayPrototypeSet: uncurryThis(Uint8Array.prototype.set),
  };
})();
