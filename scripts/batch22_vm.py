#!/usr/bin/env python3
"""Enhance node:vm -- WeakSet context tracking, expression-first compile,
filename/lineOffset/columnOffset options, Symbol.toStringTag on contexts,
createScript alias."""

import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

OLD = r'''  // ----------------------------------------------------------------------- vm
  // vm module stub: Script.runInThisContext / runInNewContext cover the
  // most-used APIs (module bundlers, Jest, etc.).
  registry.factories.vm = () => {
    class Script {
      constructor(code, _options) {
        this._code = String(code);
        this._fn = null;
      }
      _compile() {
        if (!this._fn) {
          // eslint-disable-next-line no-new-func
          this._fn = new Function(`with(this){${this._code}}`);
        }
        return this._fn;
      }
      runInThisContext(_options) {
        return this._compile().call(globalThis);
      }
      runInContext(ctx, _options) {
        return this._compile().call(ctx != null ? ctx : globalThis);
      }
      runInNewContext(sandbox, _options) {
        const ctx = Object.assign(Object.create(null), sandbox);
        return this._compile().call(ctx);
      }
      createCachedData() { return new Uint8Array(0); }
    }
    function createContext(sandbox, _options) {
      return Object.assign(Object.create(null), sandbox || {});
    }
    function isContext(value) {
      return value !== null && typeof value === "object";
    }
    function runInThisContext(code, _options) {
      return new Script(code).runInThisContext();
    }
    function runInNewContext(code, sandbox, _options) {
      return new Script(code).runInNewContext(sandbox);
    }
    function runInContext(code, ctx, _options) {
      return new Script(code).runInContext(ctx);
    }
    function compileFunction(code, params, _options) {
      // eslint-disable-next-line no-new-func
      return new Function(...(params || []), code);
    }
    function measureMemory() {
      return Promise.resolve({ total: { jsMemoryEstimate: 0 } });
    }
    return {
      Script, createContext, isContext,
      runInThisContext, runInNewContext, runInContext,
      compileFunction, measureMemory,
    };
  };'''

NEW = r'''  // ----------------------------------------------------------------------- vm
  // vm module: Script.runInThisContext / runInNewContext / runInContext,
  // createContext with WeakSet tracking, expression-first compilation.
  // NOTE: this uses the with(this){...} pattern for sandboxing, which is
  // NOT true V8 context isolation -- it shares the same global heap.
  // Sufficient for template engines, config eval, and most bundler use.
  registry.factories.vm = () => {
    const _vmContexts = new WeakSet();

    class Script {
      constructor(code, options) {
        this._code = String(code);
        const opts = options != null && typeof options === "object" ? options : {};
        if (typeof options === "string") {
          this._filename = options;
        } else {
          this._filename = opts.filename || "evalmachine.<anonymous>";
        }
        this._lineOffset = Number(opts.lineOffset) || 0;
        this._columnOffset = Number(opts.columnOffset) || 0;
        this._fn = null;
      }
      _compile() {
        if (this._fn) return this._fn;
        const code = this._code;
        // Try expression form first so that 'x + 1' works without explicit
        // return.  Fall back to statement form on SyntaxError.
        try {
          // eslint-disable-next-line no-new-func
          this._fn = new Function(`with(this){return(${code})}`);
        } catch (_e) {
          // eslint-disable-next-line no-new-func
          this._fn = new Function(`with(this){${code}}`);
        }
        return this._fn;
      }
      runInThisContext(_options) {
        return this._compile().call(globalThis);
      }
      runInContext(ctx, _options) {
        return this._compile().call(ctx != null ? ctx : globalThis);
      }
      runInNewContext(sandbox, _options) {
        const ctx = createContext(sandbox || {});
        return this._compile().call(ctx);
      }
      createCachedData() { return new Uint8Array(0); }
    }

    function createContext(sandbox, _options) {
      const obj = sandbox != null && typeof sandbox === "object"
        ? sandbox
        : Object.create(null);
      if (!_vmContexts.has(obj)) {
        _vmContexts.add(obj);
        // Tag the context unless the caller already set Symbol.toStringTag.
        const desc = Object.getOwnPropertyDescriptor(obj, Symbol.toStringTag);
        if (!desc) {
          Object.defineProperty(obj, Symbol.toStringTag, {
            value: "Context",
            writable: false,
            enumerable: false,
            configurable: true,
          });
        }
      }
      return obj;
    }

    function isContext(value) {
      return value !== null && typeof value === "object" && _vmContexts.has(value);
    }

    function runInThisContext(code, _options) {
      return new Script(code, _options).runInThisContext();
    }
    function runInNewContext(code, sandbox, _options) {
      return new Script(code, _options).runInNewContext(sandbox);
    }
    function runInContext(code, ctx, _options) {
      return new Script(code, _options).runInContext(ctx);
    }
    function compileFunction(code, params, options) {
      const p = params || [];
      const opts = options != null && typeof options === "object" ? options : {};
      // eslint-disable-next-line no-new-func
      const fn = new Function(...p, code);
      if (opts.filename) fn._filename = opts.filename;
      return fn;
    }
    function measureMemory() {
      return Promise.resolve({ total: { jsMemoryEstimate: 0 } });
    }
    function createScript(code, options) {
      return new Script(code, options);
    }

    return {
      Script, createContext, isContext, createScript,
      runInThisContext, runInNewContext, runInContext,
      compileFunction, measureMemory,
    };
  };'''

assert OLD in src, "anchor not found -- vm factory text does not match"
src = src.replace(OLD, NEW, 1)
p.write_text(src, encoding="utf-8")
print("OK")
