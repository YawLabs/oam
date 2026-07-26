// __oamVendor: the mini-CJS loader hosting the vendored Node streams port
// (docs/design/streams-port.md section 3.3). Evaluated at snapshot time
// IMMEDIATELY after node_compat.js; at that point it only creates the
// registry and stores factories -- no natives, console, or process are
// touched until a factory executes at the first runtime require().
//
// Resolution order inside require(id):
//   1. cache (module object is cached BEFORE its body runs -- cache-before-
//      execute -- so Node's designed circular requires, readable <-> duplex
//      <-> writable and duplexpair -> 'stream', see partial exports exactly
//      as Node's own CJS loader provides);
//   2. a define()d factory (vendored files under their Node specifier
//      strings, shim modules under internal/* ids, and 'stream' +
//      'stream/promises' themselves -- so a vendored file requiring
//      'stream' gets the VENDORED module, never the legacy factory);
//   3. fallback: globalThis.__oamNode.get(id), resolved at call time, for
//      the public builtins the port consumes (events, buffer,
//      string_decoder, async_hooks).
"use strict";
(() => {
  const factories = new Map();
  const modules = new Map();

  const vendorRequire = (id) => {
    const cached = modules.get(id);
    if (cached !== undefined) return cached.exports;
    const factory = factories.get(id);
    if (factory === undefined) {
      return globalThis.__oamNode.get(id);
    }
    const module = { exports: {} };
    modules.set(id, module);
    try {
      factory(
        vendorRequire,
        module,
        module.exports,
        globalThis.process,
        vendor._primordials,
      );
    } catch (e) {
      // A failed factory must not leave a broken partial in the cache; the
      // next require retries from scratch. (Re-entrant circular requires
      // during the SAME factory run still see the partial, as intended.)
      modules.delete(id);
      throw e;
    }
    return module.exports;
  };

  const vendor = {
    _primordials: null, // installed by primordials.js, the next script
    define(id, factory) {
      if (factories.has(id)) {
        // Duplicate define is a build bug -- fail the snapshot build loudly.
        throw new Error(`__oamVendor.define: duplicate module id ${id}`);
      }
      factories.set(id, factory);
    },
    require: vendorRequire,
  };

  Object.defineProperty(globalThis, "__oamVendor", {
    value: vendor,
    writable: false,
    enumerable: false,
    configurable: false,
  });

  // --- Inline misc shims -------------------------------------------------
  // Small internal/* ids the vendored files require. Anything bigger lives
  // in its own oam-shims file (errors/validators/util). All of these defer
  // global/registry lookups to require time (runtime), never snapshot eval.
  const d = vendor.define;

  d("internal/util/debuglog", (require, module) => {
    // NODE_DEBUG plumbing is not wired; the callback that would deliver an
    // enabled logger is deliberately never invoked.
    const noop = () => {};
    noop.enabled = false;
    module.exports = { debuglog: () => noop };
  });

  d("internal/util/types", (require, module) => {
    module.exports = {
      isArrayBufferView: ArrayBuffer.isView,
      isUint8Array: (v) => v instanceof Uint8Array,
    };
  });

  d("internal/buffer", (require, module) => {
    // Node's FastBuffer: a Uint8Array view wearing Buffer.prototype,
    // constructed without the Buffer ctor's argument juggling.
    const { Buffer } = globalThis.__oamNode.get("buffer");
    function FastBuffer(arrayBuffer, byteOffset, length) {
      const view = new Uint8Array(arrayBuffer, byteOffset, length);
      Object.setPrototypeOf(view, Buffer.prototype);
      return view;
    }
    FastBuffer.prototype = Buffer.prototype;
    module.exports = { FastBuffer };
  });

  d("internal/event_target", (require, module) => {
    // Marker symbols consumed via listener-option objects; fresh symbols
    // keep the shapes intact (the weak-listener optimization they gate in
    // Node core does not exist here).
    module.exports = {
      kWeakHandler: Symbol("kWeak"),
      kResistStopPropagation: Symbol("kResistStopPropagation"),
    };
  });

  d("internal/abort_controller", (require, module) => {
    module.exports = {
      AbortController: globalThis.AbortController,
      AbortSignal: globalThis.AbortSignal,
    };
  });

  d("internal/events/abort_listener", (require, module) => {
    const SymbolDispose = Symbol.dispose ?? Symbol.for("nodejs.dispose");
    module.exports = {
      addAbortListener(signal, listener) {
        const events = globalThis.__oamNode.get("events");
        if (typeof events.addAbortListener === "function") {
          return events.addAbortListener(signal, listener);
        }
        signal.addEventListener("abort", listener, { once: true });
        return {
          [SymbolDispose]() {
            signal.removeEventListener("abort", listener);
          },
        };
      },
    };
  });

  d("internal/blob", (require, module) => {
    module.exports = {
      get Blob() {
        return globalThis.Blob;
      },
      isBlob: (v) =>
        typeof globalThis.Blob === "function" && v instanceof globalThis.Blob,
    };
  });

  d("internal/assert", (require, module) => {
    module.exports = function assert(value, message) {
      if (!value) {
        const err = new Error(message || "Assertion failed");
        err.code = "ERR_INTERNAL_ASSERTION";
        throw err;
      }
    };
  });

  d("internal/webstreams/adapters", (require, module) => {
    // Slice-2 stub: every adapter throws. oam's existing to/fromWeb bridges
    // get attached over the module-level entry points at registration time
    // (slice 3); the real adapter over js/streams.js is a later slice.
    const stub = (name) =>
      function () {
        const { codes } = require("internal/errors");
        throw new codes.ERR_METHOD_NOT_IMPLEMENTED(
          `${String(name)} (webstream adapter; oam bridges attach at registration)`,
        );
      };
    module.exports = new Proxy(
      {},
      {
        get: (_t, prop) => (typeof prop === "string" ? stub(prop) : undefined),
      },
    );
  });
})();
