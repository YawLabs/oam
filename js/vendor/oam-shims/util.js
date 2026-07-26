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
};
