// 'internal/util' shim for the vendored Node streams port
// (docs/design/streams-port.md section 5.3): exactly the names the vendored
// files destructure -- once, kEmptyObject, SymbolDispose, SymbolAsyncDispose.
"use strict";

function once(callback) {
  let called = false;
  return function (...args) {
    if (called) return;
    called = true;
    return Reflect.apply(callback, this, args);
  };
}

// A genuinely BLOCKING sleep, the way Node's internal one blocks: park the
// thread on an Atomics.wait against memory nobody will ever notify. Not a
// busy-loop (which would burn a core and skew the very timer tests that use
// this) and not a promise (the callers depend on the thread not advancing).
function sleep(msec) {
  // Validate BEFORE waiting. Atomics.wait treats a non-number timeout as
  // "wait forever", so an unvalidated sleep(undefined) hangs the thread
  // with no way out -- the failure mode is a dead process, not a bad value.
  if (typeof msec !== "number") {
    const err = new TypeError(
      `The "msec" argument must be of type number. Received ${
        msec === null ? "null" : typeof msec
      }`,
    );
    err.code = "ERR_INVALID_ARG_TYPE";
    throw err;
  }
  if (!Number.isInteger(msec) || msec < 0 || msec > 4294967295) {
    const err = new RangeError(
      `The value of "msec" is out of range. It must be an integer >= 0 and ` +
        `<= 4294967295. Received ${msec}`,
    );
    err.code = "ERR_OUT_OF_RANGE";
    throw err;
  }
  const shared = new Int32Array(new SharedArrayBuffer(4));
  Atomics.wait(shared, 0, 0, msec);
}

// One warning per feature, matching Node's emitExperimentalWarning.
const experimentalWarned = new Set();
function emitExperimentalWarning(feature) {
  if (experimentalWarned.has(feature)) return;
  experimentalWarned.add(feature);
  globalThis.process.emitWarning(
    `${feature} is an experimental feature and might change at any time`,
    "ExperimentalWarning",
  );
}

module.exports = {
  once,
  kEmptyObject: Object.freeze({ __proto__: null }),
  SymbolDispose: Symbol.dispose ?? Symbol.for("nodejs.dispose"),
  SymbolAsyncDispose: Symbol.asyncDispose ?? Symbol.for("nodejs.asyncDispose"),
  // stream.js destructures promisify.custom to tag pipeline/finished for
  // util.promisify. The SHARED registry symbol -- node_compat's promisify
  // looks up the same Symbol.for key (node_compat.js:3922), so promisified
  // vendored APIs keep working through the legacy util module.
  promisify: { custom: Symbol.for("nodejs.util.promisify.custom") },
  // Reachable only under --expose-internals. Each of these is oam's REAL
  // implementation re-exported under Node's internal name, never a stub
  // written to satisfy a test.
  sleep,
  emitExperimentalWarning,
  // Same shared registry symbol node_compat's promisify reads, so setting
  // it on a function here actually changes how promisify resolves.
  customPromisifyArgs: Symbol.for("nodejs.util.promisify.customArgs"),
  get deprecate() {
    return globalThis.__oamNode.get("util").deprecate;
  },
  // deprecate(), but silent unless --pending-deprecation is on: for APIs
  // Node has decided to deprecate but is not yet warning everyone about.
  get pendingDeprecate() {
    const { deprecate } = globalThis.__oamNode.get("util");
    return (fn, msg, code) =>
      globalThis.__oamPendingDeprecation ? deprecate(fn, msg, code) : fn;
  },
};
