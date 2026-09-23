// oam bootstrap: the JS half of the runtime surface. Evaluated at context
// creation today; compiled into the startup snapshot once that pipeline
// lands (same source, faster boot).
//
// fetch: plain-object Response/Headers shapes (real spec classes arrive with
// oam_web + WPT) over a streamed body. Wire contract with crates/oam_core
// http_client::send (__oam.fetch / fetchContinue / fetchAbandon):
//   request:  JSON string {url, method, headers: [[k,v]],
//             body | body_base64 | body_stream, attempt_timeout_ms,
//             fetch_semantics, lookup_hook?}
//   response: {status, statusText, url, redirected, headers: [[k,v]],
//             bodyHandle} -- or, for a lookup_hook request,
//             {lookup: {token, host, port}}: run the hook, then
//             fetchContinue(token, JSON {ips}) or fetchAbandon(token)
// SNAPSHOT CONSTRAINT: this file is evaluated at BUILD time into the V8
// startup snapshot, where no native bindings exist. Anything from __oam
// must be looked up at CALL time, never captured at eval time.
"use strict";
(() => {

  // Every web class node ships carries a Symbol.toStringTag, so
  // `Object.prototype.toString.call(new Headers())` is '[object Headers]'
  // and not '[object Object]'. Libraries brand-check on exactly that
  // string: @sindresorhus/is (got 14's type guard) refuses a URL whose tag
  // reads 'Object', which failed every redirect got followed. node's
  // descriptor is Web IDL's -- a data property, not writable, not
  // enumerable, configurable -- and sits on the PROTOTYPE, so subclasses
  // inherit it.
  function brand(ctor, name) {
    Object.defineProperty(ctor.prototype, Symbol.toStringTag, {
      value: name,
      writable: false,
      enumerable: false,
      configurable: true,
    });
  }

  // Minimal DOMException (AbortError/TimeoutError carriers) — defined
  // first so the abort primitives below can throw it.
  if (typeof globalThis.DOMException !== "function") {
    // Legacy numeric codes. Userland (and Node's own tests) assert on
    // `code`, e.g. DataCloneError === 25; without it the property is
    // undefined.
    const LEGACY_CODES = {
      IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
      InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
      NotSupportedError: 9, InUseAttributeError: 10, InvalidStateError: 11,
      SyntaxError: 12, InvalidModificationError: 13, NamespaceError: 14,
      InvalidAccessError: 15, TypeMismatchError: 17, SecurityError: 18,
      NetworkError: 19, AbortError: 20, URLMismatchError: 21,
      QuotaExceededError: 22, TimeoutError: 23, InvalidNodeTypeError: 24,
      DataCloneError: 25,
    };
    class DOMException extends Error {
      constructor(message = "", name = "Error") {
        super(message);
        this.name = name;
      }
      get code() {
        return LEGACY_CODES[this.name] || 0;
      }
    }
    brand(DOMException, "DOMException");
    globalThis.DOMException = DOMException;
  }

  // ----------------------------------------------- Event / EventTarget
  // The DOM event primitives AbortController is built on. Minimal but
  // spec-shaped: once listeners, stopImmediatePropagation, dispatchEvent
  // returning !defaultPrevented.
  class Event {
    constructor(type, init = {}) {
      // `type` is a REQUIRED argument per spec; without the arity check
      // `new Event()` quietly produced an event of type "undefined".
      // (A Symbol type throws from String() below, as it should.)
      if (arguments.length === 0) {
        throw new TypeError(
          "Failed to construct 'Event': 1 argument required, but only 0 present.",
        );
      }
      // Template-literal coercion, NOT String(): the two differ on exactly
      // one input, and it is the one that matters here. String(symbol) is
      // special-cased to return "Symbol(desc)", so a Symbol type silently
      // became a usable event name; implicit coercion throws TypeError,
      // which is what the spec (and Node) do.
      // A non-object `options` is a mistake, not something to read
      // properties off: `new Event('x', 'once')` silently produced an
      // event with every option false instead of telling the caller.
      if (init !== undefined && init !== null && typeof init !== "object") {
        const err = new TypeError(
          `The "options" argument must be of type object. Received ${
            typeof init === "string" ? `type string ('${init}')` : `type ${typeof init} (${init})`
          }`,
        );
        err.code = "ERR_INVALID_ARG_TYPE";
        throw err;
      }
      this.type = `${type}`;
      this.bubbles = init.bubbles === true;
      this.cancelable = init.cancelable === true;
      this.defaultPrevented = false;
      this.target = null;
      this.currentTarget = null;
      this._stopImmediate = false;
      this.timeStamp = 0;
      // DOM-compatibility surface. Node exposes all of these on its Event
      // and real code feature-detects on them; oam left them undefined,
      // which reads as "not an Event" to anything checking.
      this.composed = init.composed === true;
      this.isTrusted = false;
      this.eventPhase = 0;
      this._stopPropagation = false;
    }
    // Legacy alias for !defaultPrevented, still widely read.
    get returnValue() {
      return !this.defaultPrevented;
    }
    // Legacy alias for `target`, kept alive by older DOM-shaped code.
    get srcElement() {
      return this.target;
    }
    // Node gives Event a custom inspect: at negative depth it collapses to
    // the bare class name rather than util.inspect's generic "[ClassName]",
    // so a nested event reads as `CustomEvent` in a dump.
    [Symbol.for("nodejs.util.inspect.custom")](depth, options) {
      const name = this.constructor?.name ?? "Event";
      if (depth < 0) return name;
      const fields = {
        type: this.type,
        defaultPrevented: this.defaultPrevented,
        cancelable: this.cancelable,
        timeStamp: this.timeStamp,
      };
      if ("detail" in this) fields.detail = this.detail;
      const inspect = globalThis.__oamNode?.get?.("util")?.inspect;
      const body = inspect
        ? inspect(fields, { ...options, depth: (options?.depth ?? 2) - 1 })
        : "{}";
      return `${name} ${body}`;
    }
    // cancelBubble is the legacy face of the stop-propagation flag: reading
    // it reports the flag, and assigning a TRUTHY value sets it (assigning
    // false is a no-op per spec -- the flag cannot be un-set). It used to be
    // a plain field, so stopPropagation() left it reading false and code
    // that checks cancelBubble to see whether propagation was halted got
    // the wrong answer.
    get cancelBubble() {
      return this._stopPropagation;
    }
    set cancelBubble(value) {
      if (value) this._stopPropagation = true;
    }
    preventDefault() {
      if (this.cancelable) this.defaultPrevented = true;
    }
    // The propagation path: empty unless the event is mid-dispatch, and a
    // single-element path in this non-DOM EventTarget (no tree to bubble
    // through). Was missing entirely, so composedPath() threw.
    composedPath() {
      return this.currentTarget ? [this.currentTarget] : [];
    }
    stopPropagation() {
      this._stopPropagation = true;
    }
    stopImmediatePropagation() {
      // Immediate also implies ordinary propagation is stopped.
      this._stopPropagation = true;
      this._stopImmediate = true;
    }
  }
  // Event-phase constants. The spec puts them on BOTH the constructor and
  // the prototype (`Event.NONE` and `ev.NONE`), and code compares
  // `ev.eventPhase === Event.AT_TARGET` -- against undefined, before this,
  // which is never true.
  const EVENT_PHASES = {
    NONE: 0,
    CAPTURING_PHASE: 1,
    AT_TARGET: 2,
    BUBBLING_PHASE: 3,
  };
  const defineEventPhases = (ctor) => {
    for (const [name, value] of Object.entries(EVENT_PHASES)) {
      const descriptor = { value, writable: false, enumerable: true, configurable: false };
      Object.defineProperty(ctor, name, descriptor);
      Object.defineProperty(ctor.prototype, name, descriptor);
    }
  };
  defineEventPhases(Event);
  brand(Event, "Event");
  globalThis.Event = Event;

  // CustomEvent: the standard way to carry a payload on an event, and a
  // global in every other runtime (Node has had it unflagged since v22).
  // oam simply did not define it, so `new CustomEvent('x', { detail })`
  // -- the documented way to pass data through an EventTarget -- threw
  // ReferenceError.
  class CustomEvent extends Event {
    constructor(type, init = {}) {
      // Checked HERE as well as in Event: the super() call below always
      // passes two arguments, so the base class's arity check can never
      // see that the caller supplied none.
      if (arguments.length === 0) {
        throw new TypeError(
          "Failed to construct 'CustomEvent': 1 argument required, but only 0 present.",
        );
      }
      super(type, init);
      // `detail` is readonly per spec and defaults to null, not undefined.
      Object.defineProperty(this, "detail", {
        value: init.detail === undefined ? null : init.detail,
        writable: false,
        enumerable: true,
        configurable: true,
      });
    }
  }
  Object.defineProperty(CustomEvent.prototype, Symbol.toStringTag, {
    value: "CustomEvent",
    configurable: true,
  });
  // Subclasses carry their own copies of the phase constants; inheriting
  // them from Event is not enough for `CustomEvent.NONE`, since the
  // non-writable own properties above do not surface as static members.
  defineEventPhases(CustomEvent);
  globalThis.CustomEvent = CustomEvent;

  // Drops a weakly-held listener's entry once the listener is collected, so a
  // long-lived target does not accumulate dead slots.
  const collectedListeners = typeof FinalizationRegistry === "function"
    ? new FinalizationRegistry(({ target, type, entry }) => {
        const owner = target.deref();
        if (!owner) return;
        const list = owner._listeners.get(type);
        if (list) owner._listeners.set(type, list.filter((e) => e !== entry));
      })
    : null;

  class EventTarget {
    constructor() {
      this._listeners = new Map(); // type -> [{ fn, once }]
    }
    addEventListener(type, listener, options) {
      if (typeof listener !== "function" && typeof listener?.handleEvent !== "function") return;
      // useCapture (boolean true) is a no-op in this non-DOM EventTarget; once only comes from options.once
      const once = options === true ? false : options?.once === true;
      // Node-internal resist flag (events.addAbortListener, vendored stream
      // operators): a resist-marked listener still fires after another
      // listener called stopImmediatePropagation().
      const resist = options !== true && options != null &&
        options[Symbol.for("oam.kResistStopPropagation")] === true;
      // Node-internal weak flag (kWeakHandler): the listener must NOT keep
      // itself -- or its closure -- alive. Honouring it takes a real WeakRef;
      // accepting the option and storing a strong reference anyway would
      // retain exactly what the caller asked to be droppable, which is the
      // opposite of what it was set for.
      const weak = options !== true && options != null &&
        options[Symbol.for("oam.kWeakHandler")] != null;
      const list = this._listeners.get(type) ?? [];
      if (!list.some((e) => listenerOf(e) === listener)) {
        const entry = weak
          ? { ref: new WeakRef(listener), once, resist }
          : { fn: listener, once, resist };
        list.push(entry);
        this._listeners.set(type, list);
        if (weak && collectedListeners) {
          // The registry must not become the thing that keeps the target
          // alive, so it holds the target weakly too.
          collectedListeners.register(listener, { target: new WeakRef(this), type, entry });
        }
      }
    }
    removeEventListener(type, listener) {
      const list = this._listeners.get(type);
      if (list) this._listeners.set(type, list.filter((e) => listenerOf(e) !== listener));
    }
    dispatchEvent(event) {
      event.target = this;
      event.currentTarget = this;
      // eventPhase reads AT_TARGET while listeners run and returns to NONE
      // afterwards. It was pinned at NONE throughout, so a handler asking
      // which phase it was in always got the "not dispatching" answer.
      event.eventPhase = 2; // AT_TARGET
      try {
        const list = (this._listeners.get(event.type) ?? []).slice();
        for (const entry of list) {
          // Resist-marked listeners (Node's kResistStopPropagation) still run
          // after stopImmediatePropagation; everything else is skipped.
          if (event._stopImmediate && !entry.resist) continue;
          // A weak listener whose target has been collected is simply gone.
          const fn = listenerOf(entry);
          if (fn === undefined) continue;
          if (entry.once) this.removeEventListener(event.type, fn);
          // A plain function listener is called with the TARGET as `this`;
          // an object listener is called with the LISTENER OBJECT, so
          // `handleEvent` can reach its own state. Both were getting the
          // target, which broke the object form's whole point.
          if (typeof fn === "function") {
            fn.call(this, event);
          } else {
            fn.handleEvent.call(fn, event);
          }
        }
      } finally {
        // Restored even if a listener throws, so a later reader never sees
        // a stale mid-dispatch phase.
        event.eventPhase = 0; // NONE
        event.currentTarget = null;
      }
      return !event.defaultPrevented;
    }
  }
  // A registered entry holds its listener either strongly (`fn`) or weakly
  // (`ref`); undefined means a weak one has been collected.
  function listenerOf(entry) {
    return entry.ref ? entry.ref.deref() : entry.fn;
  }
  brand(EventTarget, "EventTarget");
  globalThis.EventTarget = EventTarget;

  // ------------------------------------------- AbortController / Signal
  class AbortSignal extends EventTarget {
    constructor() {
      super();
      this.aborted = false;
      this.reason = undefined;
      this.onabort = null;
    }
    static abort(reason) {
      const signal = new AbortSignal();
      signal.aborted = true;
      signal.reason =
        reason ?? new globalThis.DOMException("This operation was aborted", "AbortError");
      return signal;
    }
    static timeout(ms) {
      const signal = new AbortSignal();
      globalThis.setTimeout(() => {
        signal._fire(new globalThis.DOMException("The operation timed out", "TimeoutError"));
      }, ms);
      return signal;
    }
    static any(signals) {
      const result = new AbortSignal();
      for (const signal of signals) {
        if (signal.aborted) {
          result._fire(signal.reason);
          return result;
        }
        signal.addEventListener("abort", () => result._fire(signal.reason), { once: true });
      }
      return result;
    }
    throwIfAborted() {
      if (this.aborted) throw this.reason;
    }
    _fire(reason) {
      if (this.aborted) return;
      this.aborted = true;
      this.reason =
        reason ?? new globalThis.DOMException("This operation was aborted", "AbortError");
      const event = new Event("abort");
      if (typeof this.onabort === "function") this.onabort.call(this, event);
      this.dispatchEvent(event);
    }
  }
  brand(AbortSignal, "AbortSignal");
  globalThis.AbortSignal = AbortSignal;

  class AbortController {
    constructor() {
      this.signal = new AbortSignal();
    }
    abort(reason) {
      this.signal._fire(reason);
    }
  }
  brand(AbortController, "AbortController");
  globalThis.AbortController = AbortController;

  // Headers (Fetch-standard subset): case-insensitive, repeated values
  // combine per the comma rule, iterable. Shared by fetch responses,
  // server requests, and the Response constructor.
  //
  // The store is a LIST of [lowercased name, value], not a Map, because
  // `set-cookie` is the one name the standard never combines: a cookie's
  // `Expires` attribute contains a comma, so `a=1, b=2` cannot be split back
  // into the two lines the server sent, and cookie-handling code was silently
  // reading one broken cookie where node gives it two. Every other name still
  // combines in place, so iteration order is unchanged for them: wire order,
  // which is what `oam.serve` writes back out. (The standard also sorts
  // iteration by name and node does; oam does not -- see
  // docs/node-divergences.md.)
  class Headers {
    constructor(init) {
      /** @type {Array<[string, string]>} */
      this._list = [];
      if (init === undefined || init === null) return;
      if (init instanceof Headers) {
        for (const [k, v] of init) this.append(k, v);
      } else if (typeof init[Symbol.iterator] === "function" && typeof init !== "string") {
        for (const pair of init) this.append(pair[0], pair[1]);
      } else {
        for (const key of Object.keys(init)) this.append(key, init[key]);
      }
    }
    append(name, value) {
      const key = String(name).toLowerCase();
      const text = String(value);
      if (key === "set-cookie") {
        this._list.push([key, text]);
        return;
      }
      const entry = this._list.find((e) => e[0] === key);
      if (entry === undefined) this._list.push([key, text]);
      else entry[1] = `${entry[1]}, ${text}`;
    }
    set(name, value) {
      const key = String(name).toLowerCase();
      const text = String(value);
      const at = this._list.findIndex((e) => e[0] === key);
      if (at < 0) {
        this._list.push([key, text]);
        return;
      }
      this._list[at][1] = text;
      // set() replaces every entry for the name; only set-cookie can repeat.
      if (key === "set-cookie") {
        this._list = this._list.filter((e, i) => e[0] !== key || i === at);
      }
    }
    get(name) {
      const key = String(name).toLowerCase();
      const values = this._list.filter((e) => e[0] === key).map((e) => e[1]);
      return values.length === 0 ? null : values.join(", ");
    }
    /** Every `set-cookie` line, uncombined (Fetch Standard, node 19.7+). */
    getSetCookie() {
      return this._list.filter((e) => e[0] === "set-cookie").map((e) => e[1]);
    }
    has(name) {
      const key = String(name).toLowerCase();
      return this._list.some((e) => e[0] === key);
    }
    delete(name) {
      const key = String(name).toLowerCase();
      this._list = this._list.filter((e) => e[0] !== key);
    }
    forEach(fn, thisArg) {
      for (const [key, value] of this._list.slice()) fn.call(thisArg, value, key, this);
    }
    *entries() {
      for (const [key, value] of this._list.slice()) yield [key, value];
    }
    *keys() {
      for (const [key] of this._list.slice()) yield key;
    }
    *values() {
      for (const [, value] of this._list.slice()) yield value;
    }
    [Symbol.iterator]() {
      return this.entries();
    }
  }
  brand(Headers, "Headers");
  globalThis.Headers = Headers;

  // Response constructor (the SERVING side; fetch's inbound responses come
  // from makeResponse below). Body: string | bytes | ReadableStream | null.
  class Response {
    constructor(body, init = {}) {
      this.status = init.status ?? 200;
      this.statusText = init.statusText ?? "";
      this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers);
      this._body = body ?? null;
      this.ok = this.status >= 200 && this.status <= 299;
    }
    static json(data, init = {}) {
      const response = new Response(JSON.stringify(data), init);
      if (!response.headers.has("content-type")) {
        response.headers.set("content-type", "application/json");
      }
      return response;
    }
    get body() {
      return this._body;
    }
    async text() {
      if (typeof this._body === "string") return this._body;
      if (this._body === null) return "";
      if (this._body instanceof Uint8Array) return new TextDecoder().decode(this._body);
      let out = "";
      for await (const chunk of this._body) {
        out += typeof chunk === "string" ? chunk : new TextDecoder().decode(chunk);
      }
      return out;
    }
    async json() {
      return JSON.parse(await this.text());
    }
  }
  brand(Response, "Response");
  globalThis.Response = Response;

  // ------------------------------------------------------------ structuredClone
  // Deep-clone primitive and JSON-structured values. ArrayBuffer transfer:
  // spec order is serialize-then-detach, so the clone COPIES each transfer
  // buffer while it is still readable (memo pre-seed) and detaches the
  // sources only after the whole clone succeeds -- a moved buffer and a
  // copied-then-detached one are indistinguishable to the caller, and a
  // failed clone leaves every source intact. Views share their (cloned or
  // transferred) backing buffer via the memo, like the spec's serializer.
  // Other transferable kinds (MessagePort etc.) still throw DataCloneError
  // in this pure-JS wave. Circular references handled via a WeakMap memo.
  if (typeof globalThis.structuredClone !== "function") {
    globalThis.structuredClone = function structuredClone(value, options) {
      const memo = new WeakMap();
      let transfers = null;
      if (options && options.transfer !== undefined) {
        // Array.from first: a Set (or any iterable) transfer list must work,
        // not silently no-op on a missing .length.
        transfers = Array.from(options.transfer);
        const seen = new Set();
        for (const t of transfers) {
          if (t === null || typeof t !== "object") {
            // WebIDL sequence<object>: non-object entries are a TypeError,
            // before any transferability classification.
            throw new TypeError("Value in the transfer list is not an object");
          }
          if (!(t instanceof ArrayBuffer)) {
            throw new DOMException(
              "structuredClone: only ArrayBuffer transferables are supported in oam wave-1",
              "DataCloneError",
            );
          }
          if (seen.has(t)) {
            throw new DOMException("Duplicate transferable in the transfer list", "DataCloneError");
          }
          seen.add(t);
          if (t.detached) {
            throw new DOMException("Cannot transfer a detached ArrayBuffer", "DataCloneError");
          }
        }
        // Copy while readable; sources detach only after the clone succeeds.
        for (const t of transfers) memo.set(t, t.slice(0));
      }
      function cloneInner(v) {
        if (v === null || (typeof v !== "object" && typeof v !== "function")) return v;
        if (memo.has(v)) return memo.get(v);
        // A file-backed Blob (fs.openAsBlob) is NOT cloneable in node -- it
        // holds a file reference, not bytes, so a clone would outlive the
        // guarantee. node throws a plain TypeError, not a DataCloneError:
        //   TypeError: Invalid state: File-backed Blobs are not cloneable
        // Marked with a cross-realm registered symbol because the marker is set
        // in node_compat's fs factory, which cannot see this scope. A symbol
        // key also keeps it out of Object.keys, so it introduces no structural
        // difference of its own. Checked HERE rather than at the entry point so
        // a blob nested inside a cloned object throws too, as node's does.
        if (v[Symbol.for("oam.blob.fileBacked")] === true) {
          throw new TypeError("Invalid state: File-backed Blobs are not cloneable");
        }
        if (v instanceof Date) { const c = new Date(v); memo.set(v, c); return c; }
        if (v instanceof RegExp) { const c = new RegExp(v.source, v.flags); memo.set(v, c); return c; }
        if (typeof ArrayBuffer !== "undefined" && v instanceof ArrayBuffer) {
          if (v.detached) {
            throw new DOMException("Cannot clone a detached ArrayBuffer", "DataCloneError");
          }
          const c = v.slice(0); memo.set(v, c); return c;
        }
        if (ArrayBuffer.isView(v)) {
          // Clone the WHOLE backing buffer through the memo so views over
          // the same (or a transferred) buffer share one clone, preserving
          // out.view.buffer === out.buf identity like node/the spec.
          const buf = cloneInner(v.buffer);
          const c = v instanceof DataView
            ? new DataView(buf, v.byteOffset, v.byteLength)
            : new v.constructor(buf, v.byteOffset, v.length);
          memo.set(v, c); return c;
        }
        if (v instanceof Map) {
          const c = new Map(); memo.set(v, c);
          for (const [k, val] of v) c.set(cloneInner(k), cloneInner(val));
          return c;
        }
        if (v instanceof Set) {
          const c = new Set(); memo.set(v, c);
          for (const val of v) c.add(cloneInner(val));
          return c;
        }
        if (Array.isArray(v)) {
          const c = new Array(v.length); memo.set(v, c);
          for (let i = 0; i < v.length; i++) c[i] = cloneInner(v[i]);
          return c;
        }
        // Plain object (or unknown class -- clone own enumerable props).
        const c = Object.create(Object.getPrototypeOf(v));
        memo.set(v, c);
        for (const key of Object.keys(v)) c[key] = cloneInner(v[key]);
        return c;
      }
      // Serialize first (primitives pass through); only on success detach
      // the transfer-list sources. A throw above leaves them intact.
      const out = (value === null || (typeof value !== "object" && typeof value !== "function"))
        ? value
        : cloneInner(value);
      if (transfers) for (const t of transfers) t.transfer();
      return out;
    };
  }

  // -------------------------------------------------------------------- Blob
  // WHATWG Blob: an immutable byte sequence with a type string. Backed by
  // a flat Uint8Array, zero-copy subarray for slice(). text/arrayBuffer/stream
  // return the expected types. size and type are read-only.
  if (typeof globalThis.Blob !== "function") {
    class Blob {
      constructor(parts, options) {
        let size = 0;
        const pieces = [];
        if (parts != null) {
          for (const part of parts) {
            if (typeof part === "string") {
              const enc = new TextEncoder().encode(part);
              pieces.push(enc);
              size += enc.length;
            } else if (part instanceof Blob) {
              pieces.push(part._bytes);
              size += part._bytes.length;
            } else if (part instanceof ArrayBuffer) {
              const view = new Uint8Array(part);
              pieces.push(view);
              size += view.length;
            } else if (ArrayBuffer.isView(part)) {
              const view = new Uint8Array(part.buffer, part.byteOffset, part.byteLength);
              pieces.push(view);
              size += view.length;
            } else {
              const enc = new TextEncoder().encode(String(part));
              pieces.push(enc);
              size += enc.length;
            }
          }
        }
        const bytes = new Uint8Array(size);
        let offset = 0;
        for (const piece of pieces) { bytes.set(piece, offset); offset += piece.length; }
        // NON-ENUMERABLE, and that is load-bearing rather than tidiness.
        // These were plain assignments, so a Blob's internals were own
        // ENUMERABLE properties: `Object.keys(blob)` returned
        // ["_bytes","_type"] where node returns [], and -- much worse --
        // `JSON.stringify(blob)` serialised the ENTIRE payload as a numeric
        // object ({"0":104,"1":105,...}) where node gives {}. Any log line or
        // API response carrying an object with a Blob in it dumped the whole
        // buffer: a memory blowup, and file contents in places they should
        // never reach. Same reasoning for `writable` -- `slice` below reassigns
        // `_bytes`, which needs the property to already exist and be writable
        // or the assignment would create a fresh enumerable one.
        Object.defineProperty(this, "_bytes", {
          value: bytes,
          writable: true,
          enumerable: false,
          configurable: true,
        });
        Object.defineProperty(this, "_type", {
          value: (options && typeof options.type === "string") ? options.type.toLowerCase() : "",
          writable: true,
          enumerable: false,
          configurable: true,
        });
      }
      get size() { return this._bytes.length; }
      get type() { return this._type; }
      slice(start, end, type) {
        const len = this._bytes.length;
        let s = start === undefined ? 0 : start < 0 ? Math.max(0, len + start) : Math.min(start, len);
        let e = end === undefined ? len : end < 0 ? Math.max(0, len + end) : Math.min(end, len);
        if (s >= e) s = e = 0;
        const result = new Blob([], { type: type !== undefined ? String(type) : this._type });
        result._bytes = this._bytes.subarray(s, e);
        return result;
      }
      async arrayBuffer() { return this._bytes.buffer.slice(this._bytes.byteOffset, this._bytes.byteOffset + this._bytes.length); }
      async bytes() { return this._bytes.slice(); }
      async text() { return new TextDecoder().decode(this._bytes); }
      stream() {
        const bytes = this._bytes;
        return new ReadableStream({
          start(controller) { controller.enqueue(bytes); controller.close(); },
        });
      }
      toString() { return "[object Blob]"; }
    }
    brand(Blob, "Blob");
    globalThis.Blob = Blob;
  }

  // -------------------------------------------------------------------- File
  if (typeof globalThis.File !== "function") {
    class File extends globalThis.Blob {
      constructor(parts, name, options) {
        super(parts, options);
        this._name = String(name);
        this._lastModified = (options && typeof options.lastModified === "number")
          ? options.lastModified : Date.now();
      }
      get name() { return this._name; }
      get lastModified() { return this._lastModified; }
      toString() { return "[object File]"; }
    }
    brand(File, "File");
    globalThis.File = File;
  }

  if (typeof globalThis.MessagePort === "undefined") {
    globalThis.MessagePort = class MessagePort {};
  }

  // ----------------------------------------------------------------- FormData
  // WHATWG FormData: a multipart/form-data key-value store. Supports multiple
  // values per key. File / Blob entries are included as-is; streaming encoding
  // lands with the fetch body rework.
  if (typeof globalThis.FormData !== "function") {
    class FormData {
      constructor() { this._entries = []; }
      append(name, value, filename) {
        this._entries.push([String(name), this._normalizeValue(value, filename)]);
      }
      set(name, value, filename) {
        const key = String(name);
        const val = this._normalizeValue(value, filename);
        let replaced = false;
        this._entries = this._entries.filter(([k]) => {
          if (k !== key) return true;
          if (!replaced) { replaced = true; return true; }
          return false;
        });
        if (!replaced) { this._entries.push([key, val]); }
        else {
          const idx = this._entries.findIndex(([k]) => k === key);
          this._entries[idx] = [key, val];
        }
      }
      get(name) {
        const entry = this._entries.find(([k]) => k === String(name));
        return entry ? entry[1] : null;
      }
      getAll(name) {
        return this._entries.filter(([k]) => k === String(name)).map(([, v]) => v);
      }
      has(name) { return this._entries.some(([k]) => k === String(name)); }
      delete(name) { this._entries = this._entries.filter(([k]) => k !== String(name)); }
      forEach(fn, thisArg) {
        for (const [key, value] of this._entries) fn.call(thisArg, value, key, this);
      }
      *entries() { yield* this._entries; }
      *keys() { for (const [k] of this._entries) yield k; }
      *values() { for (const [, v] of this._entries) yield v; }
      [Symbol.iterator]() { return this.entries(); }
      _normalizeValue(value, filename) {
        if (typeof globalThis.Blob !== "undefined" && value instanceof globalThis.Blob) return value;
        if (typeof value === "string") return value;
        return String(value);
      }
    }
    brand(FormData, "FormData");
    globalThis.FormData = FormData;
  }

  // ----------------------------------------------------------------- Request
  // WHATWG Request: the fetch()-input request object. Minimal but spec-shaped:
  // method, url, headers, body. Used by frameworks that pass Request objects
  // instead of URLs to fetch().
  if (typeof globalThis.Request !== "function") {
    class Request {
      constructor(input, init) {
        let url, method, headers, body;
        if (input instanceof Request) {
          url = input.url; method = input.method; headers = new Headers(input.headers);
          body = input._body;
        } else {
          url = String(input);
          method = "GET"; headers = new Headers(); body = null;
        }
        if (init) {
          if (init.method) method = String(init.method).toUpperCase();
          if (init.headers) headers = new Headers(init.headers);
          if (init.body != null) body = init.body;
        }
        this.url = url;
        this.method = method;
        this.headers = headers;
        this._body = body;
        this.bodyUsed = false;
      }
      get body() {
        if (this._body == null) return null;
        if (typeof ReadableStream !== "undefined" && this._body instanceof ReadableStream) return this._body;
        const bytes = typeof this._body === "string"
          ? new TextEncoder().encode(this._body)
          : this._body;
        return new ReadableStream({
          start(controller) { controller.enqueue(bytes); controller.close(); },
        });
      }
      async text() {
        if (this.bodyUsed) throw new TypeError("Body already consumed");
        this.bodyUsed = true;
        if (this._body == null) return "";
        if (typeof this._body === "string") return this._body;
        const bytes = this._body instanceof Uint8Array ? this._body
          : new Uint8Array(this._body);
        return new TextDecoder().decode(bytes);
      }
      async json() { return JSON.parse(await this.text()); }
      async arrayBuffer() {
        if (this.bodyUsed) throw new TypeError("Body already consumed");
        this.bodyUsed = true;
        if (this._body == null) return new ArrayBuffer(0);
        if (typeof this._body === "string") return new TextEncoder().encode(this._body).buffer;
        const bytes = this._body instanceof Uint8Array ? this._body : new Uint8Array(this._body);
        return bytes.buffer;
      }
      clone() {
        if (this.bodyUsed) throw new TypeError("Cannot clone a Request whose body has already been consumed");
        return new Request(this);
      }
    }
    brand(Request, "Request");
    globalThis.Request = Request;
  }

  // --------------------------------------------------------- BroadcastChannel
  // In-process BroadcastChannel: messages are dispatched synchronously within
  // the same oam runtime (single-process, single-thread). Cross-process
  // broadcast (via SharedArrayBuffer or IPC) lands with worker_threads.
  if (typeof globalThis.BroadcastChannel !== "function") {
    const _bcChannels = new Map(); // name -> Set<BroadcastChannel>
    class BroadcastChannel extends EventTarget {
      constructor(name) {
        super();
        this.name = String(name);
        this._closed = false;
        this.onmessage = null;
        this.onmessageerror = null;
        if (!_bcChannels.has(this.name)) _bcChannels.set(this.name, new Set());
        _bcChannels.get(this.name).add(this);
      }
      postMessage(message) {
        if (this._closed) throw new DOMException("BroadcastChannel is closed", "InvalidStateError");
        const cloned = globalThis.structuredClone ? globalThis.structuredClone(message) : message;
        const peers = _bcChannels.get(this.name);
        if (peers) {
          for (const peer of peers) {
            if (peer === this || peer._closed) continue;
            queueMicrotask(() => {
              const ev = new MessageEvent("message", { data: cloned });
              peer.dispatchEvent(ev);
              if (typeof peer.onmessage === "function") peer.onmessage(ev);
            });
          }
        }
      }
      close() {
        if (this._closed) return;
        this._closed = true;
        const peers = _bcChannels.get(this.name);
        if (peers) {
          peers.delete(this);
          if (peers.size === 0) _bcChannels.delete(this.name);
        }
      }
    }
    // MessageEvent: minimal shape for BroadcastChannel messages.
    class MessageEvent extends Event {
      constructor(type, init) {
        super(type, init);
        this.data = init ? init.data : undefined;
        this.origin = (init && init.origin) || "";
        this.lastEventId = (init && init.lastEventId) || "";
        this.source = null;
        this.ports = [];
      }
    }
    // No brand() here: node gives BroadcastChannel no tag of its own, so
    // Object.prototype.toString.call(new BroadcastChannel(n)) reads
    // "[object EventTarget]", inherited from its base.
    globalThis.BroadcastChannel = BroadcastChannel;
    brand(MessageEvent, "MessageEvent");
    globalThis.MessageEvent = globalThis.MessageEvent || MessageEvent;
  }

  function makeHeaders(pairs) {
    const headers = new Headers();
    for (const [name, value] of pairs) headers.append(name, value);
    return headers;
  }

  function makeResponse(raw, signal) {
    const handle = raw.bodyHandle;
    let consumed = false;
    let bodyStream = null;
    let streamController = null;

    // An abort AFTER the response head still ends the body, whether or not
    // anything is reading it: node errors the body with the abort reason and
    // destroys the connection, and stopping a large download part-way
    // through is the case an AbortController is normally reached for. The
    // listener therefore goes on here, at the head, not when the body stream
    // is first touched -- registered lazily, an abort on a body nobody had
    // read yet cancelled nothing, and the server kept streaming into a
    // connection oam held for the rest of the run (measured: node's server
    // sees the client leave at once, oam's never did). The chunks already
    // delivered stay delivered, as in node.
    //
    // The same path finishes an abort that landed BEFORE the head: the fetch
    // promise already rejected with the reason, and the response that turns
    // up later is cancelled on arrival instead of holding its connection.
    let bodyAborted = false;
    let onAbort = null;
    const abortReason = () =>
      signal.reason ?? new globalThis.DOMException("This operation was aborted", "AbortError");
    // Read to the end, failed or cancelled: nothing is left for an abort to
    // stop. Stop listening, so a signal shared by many fetches does not
    // collect one listener per response, and a late abort does not leave a
    // cancel tombstone for a handle that is already gone.
    function bodyOver() {
      if (onAbort) signal.removeEventListener("abort", onAbort);
      onAbort = null;
    }
    if (signal) {
      onAbort = () => {
        onAbort = null;
        bodyAborted = true;
        try {
          globalThis.__oam.fetchBodyCancel(handle);
        } catch {
          /* already drained */
        }
        if (streamController) {
          try {
            streamController.error(abortReason());
          } catch {
            /* already closed or errored */
          }
        }
      };
      if (signal.aborted) onAbort();
      else signal.addEventListener("abort", onAbort, { once: true });
    }

    // The body is a real ReadableStream over the wire handle: each pull is
    // one op, so chunks surface as the server flushes them (SSE / token
    // streaming). Lazy: responses whose body is never touched never spawn
    // a read op; the handle dies with the run's CoreRuntime.
    function ensureBody() {
      bodyStream ??= new ReadableStream({
        start(controller) {
          streamController = controller;
          // Aborted before anything asked for the body: it starts errored,
          // as node's does (a reader's first read rejects with the reason).
          if (bodyAborted) controller.error(abortReason());
        },
        async pull(controller) {
          let chunk;
          try {
            chunk = await globalThis.__oam.fetchBodyRead(handle);
          } catch (e) {
            if (bodyAborted) return;
            bodyOver();
            throw e;
          }
          // The read that was in flight when the abort landed returns here
          // against a stream that is already errored; closing or enqueuing
          // on it throws, and the throw would surface as a bogus rejection.
          if (bodyAborted) return;
          if (chunk === undefined) {
            bodyOver();
            controller.close();
          } else controller.enqueue(chunk);
        },
        cancel() {
          bodyOver();
          globalThis.__oam.fetchBodyCancel(handle);
        },
      });
      return bodyStream;
    }

    async function drainBytes() {
      if (consumed) throw new TypeError("Body already consumed");
      // Consuming a body whose fetch was already aborted fails before it
      // starts, with undici's own AbortError rather than the signal's
      // reason (measured on node v22.22.2: `ac.abort(new Error('why'))` then
      // `r.text()` rejects with `DOMException [AbortError]: The operation was
      // aborted.`). An abort that lands DURING the read rejects with the
      // reason, through the stream, in both.
      if (bodyAborted) throw new globalThis.DOMException("The operation was aborted.", "AbortError");
      consumed = true;
      const chunks = [];
      let total = 0;
      for await (const chunk of ensureBody()) {
        chunks.push(chunk);
        total += chunk.length;
      }
      const out = new Uint8Array(total);
      let offset = 0;
      for (const chunk of chunks) {
        out.set(chunk, offset);
        offset += chunk.length;
      }
      return out;
    }

    return {
      status: raw.status,
      statusText: raw.statusText,
      ok: raw.status >= 200 && raw.status <= 299,
      url: raw.url,
      redirected: raw.redirected === true,
      headers: makeHeaders(raw.headers),
      get body() {
        return ensureBody();
      },
      get bodyUsed() {
        return consumed || (bodyStream !== null && bodyStream.locked);
      },
      arrayBuffer: async () => (await drainBytes()).buffer,
      bytes: () => drainBytes(),
      text: async () => new TextDecoder().decode(await drainBytes()),
      json: async () => JSON.parse(new TextDecoder().decode(await drainBytes())),
    };
  }

  // Lone surrogates would survive JSON.stringify (escaped) but be rejected
  // by the Rust-side wire parser with a misleading "malformed request";
  // sanitize up front so bad strings degrade to U+FFFD like the web does.
  function wellFormed(value) {
    return String(value).toWellFormed();
  }

  // net's happy-eyeballs attempt timeout
  // (net.setDefaultAutoSelectFamilyAttemptTimeout) is process-wide, and node's
  // fetch connects with it too (undici passes none of its own). A net module
  // nobody has loaded cannot have changed it: node's 250 ms.
  function netAttemptTimeoutMs() {
    try {
      const net = globalThis.__oamNode?.cache?.get?.("net");
      const ms = net?.getDefaultAutoSelectFamilyAttemptTimeout?.();
      if (typeof ms === "number") return ms;
    } catch {
      // the default below
    }
    return 250;
  }

  // node's undici (and its https.Agent) connect with tls.connect, whose
  // SecureContext reads the LIVE tls.DEFAULT_MIN_VERSION / DEFAULT_MAX_VERSION
  // for every connection: `tls.DEFAULT_MAX_VERSION = 'TLSv1.2'` or
  // `--tls-max-v1.2` caps fetch and an option-less https.get too (measured on
  // v22.22.2). The loaded module's resolver gives the effective names,
  // validated exactly as tls.connect validates them (a default that is not a
  // version throws ERR_TLS_INVALID_PROTOCOL_VERSION); with node:tls never
  // loaded nobody could have reassigned them, so the initial values -- what
  // the --tls-* flags set -- stand. "" is a side with no bound of its own.
  function tlsDefaultVersions() {
    const resolve = globalThis.__oamNode?._resolveTlsVersions;
    if (typeof resolve === "function") return resolve({});
    return {
      min: globalThis.__oamTlsMinVersion || "",
      max: globalThis.__oamTlsMaxVersion || "",
    };
  }

  // The `hints` node's net.connect hands a connect.lookup hook
  // (lib/net.js lookupAndConnect): 0 on Windows, dns.ADDRCONFIG -- the
  // platform's AI_ADDRCONFIG -- elsewhere. Measured: node v22.22.2 passes
  // `{ family: undefined, hints, all: true }` with hints 0 on win32, 1024 on
  // macOS 26 arm64 and 32 (0x20) on Debian 12 x64 (glibc 2.36). FreeBSD's
  // 1024 is its <netdb.h> value, unmeasured. oam's own dns.ADDRCONFIG is
  // still 0 everywhere (a separate follow-up).
  function lookupHints() {
    const platform = globalThis.process?.platform;
    if (platform === "win32") return 0;
    if (platform === "darwin" || platform === "freebsd") return 1024;
    return 0x20;
  }

  // node lib/net.js lookupAndConnectMultiple (v22.22.2): keep the addresses
  // net would dial -- isIP(address) and family 4 or 6 -- in the hook's order,
  // and with none, fail on the FIRST entry. The statements are node's own, so
  // the TypeErrors are too: an empty list throws "Cannot destructure property
  // 'address' of 'addresses[0]' as it is undefined." (measured as fetch's
  // cause), and a string answer (`cb(null, ip, family)`, the non-`all` form)
  // walks its characters and fails as `Invalid IP address: undefined`.
  // Family interleaving and repeats are Rust's (net_connect).
  function pinAddresses(addresses, host, port) {
    const reg = globalThis.__oamNode;
    const { isIP } = reg.get("net");
    const ips = [];
    for (let i = 0, l = addresses.length; i < l; i++) {
      const address = addresses[i];
      const { address: ip, family: addressType } = address;
      if (isIP(ip) && (addressType === 4 || addressType === 6)) ips.push(ip);
    }
    if (ips.length > 0) return ips;
    const { address: firstIp, family: firstAddressType } = addresses[0];
    const { codes } = reg.get("internal/errors");
    if (!isIP(firstIp)) throw new codes.ERR_INVALID_IP_ADDRESS(firstIp);
    throw new codes.ERR_INVALID_ADDRESS_FAMILY(firstAddressType, host, port);
  }

  // One connect.lookup call, as net.connect makes it for a fetch: the host,
  // `{ family, hints, all: true }`, a callback. The first callback wins; an
  // error (or a synchronous throw) rejects with that value unchanged.
  function runConnectLookup(lookup, host, port) {
    return new Promise((resolve, reject) => {
      let settled = false;
      lookup(host, { family: undefined, hints: lookupHints(), all: true }, (err, addresses) => {
        if (settled) return;
        settled = true;
        if (err) {
          reject(err);
          return;
        }
        try {
          resolve(pinAddresses(addresses, host, port));
        } catch (e) {
          reject(e);
        }
      });
    });
  }

  // One call of an undici dispatcher's `connect` function, as undici's
  // Client makes it: the connector parameters and a callback taking
  // `(err, socket)`. The first callback wins; an error, or a synchronous
  // throw, rejects with that value unchanged. A socket handed back after
  // that is destroyed, as nothing will use it.
  function runConnector(connector, params) {
    return new Promise((resolve, reject) => {
      let settled = false;
      try {
        connector.fn.call(connector.self, params, (err, socket) => {
          if (settled) {
            if (!err && socket && typeof socket.destroy === "function") socket.destroy();
            return;
          }
          settled = true;
          if (err) {
            reject(err);
          } else if (socket == null || typeof socket.write !== "function" || typeof socket.on !== "function") {
            reject(new TypeError("the dispatcher's connect function returned no socket"));
          } else {
            resolve(socket);
          }
        });
      } catch (e) {
        if (!settled) {
          settled = true;
          reject(e);
        }
      }
    });
  }

  // The socket a connector handed back, as the connection of one request:
  // its bytes pumped through a pipe the parked fetch resumes on. It is the
  // request's alone -- nothing pools it -- so once the transport lets go of
  // the connection (or the socket closes) the socket is destroyed. An error
  // the socket reports is the fetch's cause when the exchange fails (a
  // connector that hands back a socket still connecting to a refused port
  // fails with that ECONNREFUSED, as in node).
  function supplySocket(socket) {
    let socketError = null;
    socket.on("error", (e) => {
      if (socketError === null) socketError = e;
    });
    const pipe = globalThis.__oamNode._pipeSocket(socket);
    const close = () => {
      pipe.stop();
      if (!socket.destroyed) socket.destroy();
    };
    pipe.outDone.then(close);
    return { id: pipe.id, close, error: () => socketError };
  }

  // The connection policy of the undici dispatcher a fetch rides (the
  // `dispatcher` option, else the global one): `{ connector }` for one whose
  // `connect` is a function (a connect object carrying socket or TLS options,
  // or an Agent `factory`, is turned into one), `{ refuse }` for one oam
  // cannot run faithfully -- a dispatch() override, interceptors, or an
  // object that is not one of the oam:undici shim's dispatchers -- since oam
  // would otherwise send the request without it.
  function dispatcherPolicy(dispatcher, holder) {
    if (holder && typeof holder.policy === "function") return holder.policy(dispatcher);
    const refuse = new Error(
      "a fetch dispatcher that is not one of oam's undici dispatchers is not supported: " +
        "oam cannot run its dispatch(); pass the connection policy as a `connect` function",
    );
    refuse.name = "NotSupportedError";
    refuse.code = "UND_ERR_NOT_SUPPORTED";
    return { refuse };
  }

  // WHATWG: fetch() rejects with a TypeError on a network failure, and node's
  // message is the bare "fetch failed" with the transport error underneath as
  // `cause` -- that is where `code` (ECONNREFUSED, ENOTFOUND, ...) lives and
  // what retry logic reads. The native op builds that error itself, in node's
  // shape: errno / code / syscall and the address that refused (or the
  // hostname that did not resolve), or node's AggregateError when every
  // address of a name refused.
  function fetchFailed(e) {
    // A --permission refusal is not a network failure: the initial URL's
    // denial reaches the caller as the ERR_ACCESS_DENIED error itself, and a
    // refusal raised later (a redirect hop's host, which the native loop
    // rejects with that same error; the connect.lookup hook's addresses,
    // checked when the fetch resumes) has to look the same or a policy
    // failure reads as an unreachable host.
    if (e instanceof Error && e.code === "ERR_ACCESS_DENIED") return e;
    return new TypeError("fetch failed", { cause: e instanceof Error ? e : new Error(String(e)) });
  }

  // The op, settled: a response, or -- for a fetch whose dispatcher carries a
  // connect.lookup hook -- a lookup request first. undici calls the hook for
  // every connection to a host name, redirect hops included, so the native
  // loop parks before it dials a name it has no addresses for and hands the
  // name up here. A hook that fails fails the fetch CLOSED (its error is the
  // cause, unchanged, as in node) and never falls back to system DNS; an
  // abort while parked drops the parked fetch.
  async function settleFetch(pending, lookup, signal, connector) {
    return makeResponse(await settleRaw(pending, lookup, signal, connector), signal);
  }

  // settleFetch's loop, ending at the op's raw payload (the response head
  // with its `bodyHandle`, and the `socket` / `tls` facts of the connection
  // it arrived on) instead of a Response.
  async function settleRaw(pending, lookup, signal, connector) {
    const internal = globalThis.__oam;
    const aborted = () =>
      signal.reason ?? new globalThis.DOMException("This operation was aborted", "AbortError");
    let raw;
    try {
      raw = await pending;
    } catch (e) {
      throw fetchFailed(e);
    }
    let resumed = false;
    while (raw && (raw.lookup || raw.connect)) {
      if (raw.connect) {
        // Connector mode: the dispatcher's `connect` function is asked for
        // this hop's connection, with the parameters undici's Client calls
        // its connector with, and the request goes over the socket it hands
        // back and nowhere else. A connector that fails (or throws) fails
        // the fetch with its error as the cause, as in node.
        const { token, host, hostname, protocol, port } = raw.connect;
        const abandon = () => internal.fetchAbandon(token);
        if (resumed && signal?.aborted) {
          abandon();
          throw aborted();
        }
        signal?.addEventListener("abort", abandon, { once: true });
        let socket;
        try {
          socket = await runConnector(connector, {
            host,
            hostname,
            protocol,
            port,
            servername: null,
            localAddress: null,
          });
        } catch (err) {
          abandon();
          throw new TypeError("fetch failed", { cause: err });
        } finally {
          signal?.removeEventListener("abort", abandon);
        }
        if (signal?.aborted) {
          abandon();
          socket.destroy();
          throw aborted();
        }
        resumed = true;
        const supplied = supplySocket(socket);
        try {
          raw = await internal.fetchSupply(token, supplied.id, socket.alpnProtocol === "h2");
        } catch (e) {
          supplied.close();
          throw fetchFailed(supplied.error() ?? e);
        }
        continue;
      }
      const { token, host, port } = raw.lookup;
      const abandon = () => internal.fetchAbandon(token);
      // Aborted while the previous hop was on the wire: node ends the fetch
      // there and never looks the redirect target up (measured). The FIRST
      // host is looked up even after an abort that follows fetch() in the
      // same tick -- undici has already started connecting (measured: one
      // call) -- so only a resumed fetch stops here. The abort race already
      // rejected the fetch with the reason.
      if (resumed && signal?.aborted) {
        abandon();
        throw aborted();
      }
      signal?.addEventListener("abort", abandon, { once: true });
      let ips;
      try {
        ips = await runConnectLookup(lookup, host, port);
      } catch (err) {
        abandon();
        throw new TypeError("fetch failed", { cause: err });
      } finally {
        signal?.removeEventListener("abort", abandon);
      }
      if (signal?.aborted) {
        abandon();
        // The abort race already rejected the fetch with the reason.
        throw aborted();
      }
      resumed = true;
      try {
        raw = await internal.fetchContinue(token, JSON.stringify({ ips }));
      } catch (e) {
        throw fetchFailed(e);
      }
    }
    return raw;
  }

  // A replaced `require('dns').lookup`: node's fetch dials through
  // net.connect / tls.connect, which call dns.lookup as it is at call time,
  // so a guard that replaces it vets every new connection a fetch opens
  // (measured on node v22.22.2, redirect hops included). undefined while
  // the dns module is not loaded (nothing can have replaced it) or its
  // lookup is still oam's own.
  function replacedDnsLookup() {
    const reg = globalThis.__oamNode;
    const dns = reg?.cache?.get?.("dns");
    if (!dns) return undefined;
    const lookup = dns.lookup;
    return lookup !== reg._dnsLookupOriginal ? lookup : undefined;
  }

  // The methods the Fetch Standard byte-uppercases. Anything else keeps the
  // caller's spelling: node sends `patch /p HTTP/1.1` for `{method: 'patch'}`
  // and `fooBar /p` for `{method: 'fooBar'}` (measured on v22.22.2), where
  // oam used to uppercase every method unconditionally.
  const NORMALIZED_METHODS = new Set(["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"]);

  // Request headers undici refuses to dispatch, and the error each one
  // raises. Measured on node v22.22.2 + undici 6.24.1 against a raw-socket
  // server: every one of these fails the fetch BEFORE anything reaches the
  // wire.
  //
  // `connection` is the one that turns on the VALUE, case-insensitively:
  // `close` and `keep-alive` are both accepted and go out lowercased
  // (`{connection: 'CLOSE'}` writes `connection: close`), and anything else --
  // notably `close, transfer-encoding`, the CL.TE evasion -- is refused.
  //
  // This is NOT the Fetch Standard's forbidden-header list: node sends
  // `via`, `date`, `dnt`, `origin`, `referer`, `cookie`, `cookie2`,
  // `accept-charset`, `set-cookie`, `trailer`, `te`, `proxy-*` and
  // `access-control-request-*` straight through (all measured), so oam does
  // too. `host` is the one node silently drops.
  //
  // Returns `[name, message]` to refuse with, or the value to send.
  function dispatchHeader(name, value) {
    switch (name) {
      case "transfer-encoding":
        return { refuse: ["InvalidArgumentError", "invalid transfer-encoding header"] };
      case "keep-alive":
        return { refuse: ["InvalidArgumentError", "invalid keep-alive header"] };
      case "upgrade":
        return { refuse: ["InvalidArgumentError", "invalid upgrade header"] };
      case "expect":
        return { refuse: ["NotSupportedError", "expect header not supported"] };
      case "connection": {
        const lower = value.toLowerCase();
        return lower === "close" || lower === "keep-alive"
          ? { value: lower }
          : { refuse: ["InvalidArgumentError", "invalid connection header"] };
      }
      default:
        return { value };
    }
  }

  globalThis.fetch = async function fetch(input, init) {
    return oamFetch(input, init, false);
  };

  // http.ClientRequest's own entry (node_compat.js): the same transport,
  // resolving to the raw payload -- the response head, its body handle and
  // the connection's facts -- and out of the user's reach: replacing
  // globalThis.fetch must not intercept http.request, which in node never
  // goes near fetch. `Headers` is the class the fetch path builds headers
  // with.
  Object.defineProperty(globalThis, "__oamFetchInternal", {
    value: Object.freeze({
      fetch: (input, init) => oamFetch(input, init, true),
      Headers,
    }),
    writable: false,
    enumerable: false,
    configurable: false,
  });

  async function oamFetch(input, init, rawPayload) {
    init = init || {};
    const signal = init.signal;
    // Already-aborted: reject before touching the network (spec).
    if (signal?.aborted) {
      throw signal.reason ?? new globalThis.DOMException("This operation was aborted", "AbortError");
    }
    // Internal callers that are not fetch in node (http.request, the http2
    // client, undici.request) opt out of every Fetch-level rule below.
    const fetchSemantics = init.__oamFetchSemantics !== false;
    // undici's DISPATCH-level rules are a smaller set that node applies to
    // `undici.request` as well, because both build the same internal Request:
    // the five hop-by-hop header refusals and the content-length check, but
    // NOT the Fetch-level ones. Measured on node v22.22.2 + undici 6.24.1:
    // `undici.request` throws `invalid transfer-engine header` /
    // `expect header not supported` / `Request body length does not match
    // content-length header` exactly as `fetch` does, and at the same time
    // SENDS a caller `host` (which fetch drops) and leaves the method alone.
    // `http.request` and the http2 client set these headers legitimately in
    // node and are not subject to either set.
    const dispatchSemantics = fetchSemantics || init.__oamDispatchSemantics === true;
    // fetch's `redirect` (RequestInit's RequestRedirect enum): the Request
    // constructor converts init first, so a value outside the enum is
    // refused before the URL is even parsed, with webidl's message.
    let redirectMode;
    if (fetchSemantics && init.redirect !== undefined) {
      redirectMode = String(init.redirect);
      if (redirectMode !== "follow" && redirectMode !== "manual" && redirectMode !== "error") {
        throw new TypeError(
          `Request constructor: ${redirectMode} is not an accepted type. Expected one of follow, manual, error.`,
        );
      }
    }
    const rawUrl = wellFormed(input);
    if (fetchSemantics) {
      // node parses the URL in the Request constructor, so a bad URL is a URL
      // error and not a network failure -- the caller can tell them apart.
      // oam reported both as `TypeError: fetch failed` with cause
      // `Error: builder error`, which named neither.
      let parsed;
      try {
        parsed = new URL(rawUrl);
      } catch {
        const cause = new TypeError("Invalid URL");
        cause.code = "ERR_INVALID_URL";
        throw new TypeError(`Failed to parse URL from ${rawUrl}`, { cause });
      }
      // node: `TypeError: Request cannot be constructed from a URL that
      // includes credentials` -- nothing reaches the wire. oam converted the
      // userinfo to `Authorization: Basic ...` and sent it, which is also
      // inconsistent with this slice's own redirect rule (a Location with
      // userinfo already fails as `cross origin not allowed ...`). The
      // http.request path keeps the conversion: there the userinfo IS node's
      // documented `auth` option.
      if (parsed.username !== "" || parsed.password !== "") {
        throw new TypeError(
          `Request cannot be constructed from a URL that includes credentials: ${rawUrl}`,
        );
      }
      if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
        throw new TypeError("fetch failed", { cause: new Error("unknown scheme") });
      }
    }
    let headers = [];
    if (init.headers) {
      // Branch on iterability, not Array-ness: a Map (valid HeadersInit)
      // is not an Array, and Object.entries(map) is [] — auth headers were
      // silently dropped.
      const h = init.headers;
      const pairs =
        typeof h !== "string" && typeof h[Symbol.iterator] === "function"
          ? [...h].map(([k, v]) => [wellFormed(k), wellFormed(v)])
          : Object.entries(h).map(([k, v]) => [wellFormed(k), wellFormed(v)]);
      if (!dispatchSemantics) {
        headers = pairs;
      } else {
        // Repeated names combine, as node's Headers does: two `x-d` entries
        // go out as one `x-d: 1, 2` line, not two lines. Fetch only --
        // `undici.request` sends them as the caller wrote them.
        let list = pairs;
        if (fetchSemantics) {
          const combined = new Headers();
          for (const [k, v] of pairs) combined.append(k, v);
          list = [...combined];
        }
        for (const [rawName, value] of list) {
          const name = fetchSemantics ? rawName : String(rawName).toLowerCase();
          // `host` is node's one silent drop, and it is fetch-only: node's
          // `undici.request` sends a caller host. Left through on a fetch, a
          // caller controls the authority a name-based virtual host, a cache
          // or an SSRF filter sees while the connection goes somewhere else.
          if (fetchSemantics && name === "host") continue;
          const verdict = dispatchHeader(name, value);
          if (verdict.refuse !== undefined) {
            const cause = new Error(verdict.refuse[1]);
            cause.name = verdict.refuse[0];
            throw new TypeError("fetch failed", { cause });
          }
          headers.push([rawName, verdict.value]);
        }
      }
    }
    const method = init.method ? String(init.method) : "GET";
    const request = {
      url: rawUrl,
      // Fetch-level normalisation only: `undici.request` and the http2
      // client send the method as written in node.
      method:
        fetchSemantics && NORMALIZED_METHODS.has(method.toUpperCase())
          ? method.toUpperCase()
          : method,
      headers,
      attempt_timeout_ms: netAttemptTimeoutMs(),
      // undici's Fetch-spec bad-port block on the initial URL.
      fetch_semantics: fetchSemantics,
    };
    // An https URL handshakes under node's live TLS defaults
    // (tlsDefaultVersions). A default that is not a version fails a fetch as
    // undici's tls.connect fails it -- `fetch failed` with that error as the
    // cause; http.request's entry has already thrown it synchronously, and
    // undici.request rejects with it as given.
    if (/^https:/i.test(rawUrl)) {
      let versions;
      try {
        versions = tlsDefaultVersions();
      } catch (e) {
        if (fetchSemantics) throw new TypeError("fetch failed", { cause: e });
        throw e;
      }
      request.tls_min_version = versions.min;
      request.tls_max_version = versions.max;
    }
    // http.request (through the internal entry only) gets a 3xx as the
    // response, as node's does: node's http client never follows a
    // redirect, and an application that vets a URL before requesting it
    // relies on that.
    if (rawPayload && init.__oamManualRedirect === true) request.redirect = "manual";
    // fetch's own `redirect: "manual"` returns the 3xx (its status, headers
    // and body; `redirected` false, `url` the request's) and `"error"` fails
    // on a redirect status with cause `unexpected redirect`, as node's do:
    // an application asking for either vets each hop itself, and the
    // target must not be requested behind its back.
    if (redirectMode === "manual" || redirectMode === "error") request.redirect = redirectMode;
    // http.request's own maxHeaderSize for the response heads (without it
    // the transport applies the process-wide limit).
    if (rawPayload && typeof init.__oamMaxHeaderSize === "number") {
      request.max_header_size = init.__oamMaxHeaderSize;
    }
    // An undici-style dispatcher may carry a connect.lookup hook -- the
    // DNS-rebind / SSRF pin. The oam:undici shim exposes it as
    // `_oamConnectLookup`. node honours that hook however the dispatcher was
    // installed, so the GLOBAL one counts too (undici.setGlobalDispatcher,
    // which plain fetch() and undici.fetch() both dispatch through);
    // `init.dispatcher` overrides it, as in node. No dispatcher / no hook =
    // the plain path, no cost -- the holder does not exist until a run imports
    // undici.
    //
    // A replaced dns.lookup is the same kind of hook (node's net.connect
    // calls it for every connection undici opens): the dispatcher's own hook
    // wins, as undici's connector calls that one instead.
    const holder = globalThis.__oamUndiciDispatcher;
    const dispatcher = init.dispatcher ?? holder?.current;
    const lookup = (dispatcher && dispatcher._oamConnectLookup) || replacedDnsLookup();
    if (typeof lookup === "function") request.lookup_hook = true;
    // A `connect` FUNCTION is asked for every connection the fetch makes
    // (connector mode, which wins over the lookup hook), and a dispatcher oam
    // cannot run faithfully fails the fetch rather than being ignored. Not for
    // http.request's internal entry: node's http.request never goes through
    // an undici dispatcher.
    let connector = null;
    if (!rawPayload && dispatcher != null) {
      const policy = dispatcherPolicy(dispatcher, holder);
      if (policy.refuse) throw new TypeError("fetch failed", { cause: policy.refuse });
      if (policy.connector) {
        connector = policy.connector;
        request.connect_hook = true;
      }
    }
    // Internal escape hatch: a request whose body is produced over time
    // rides an outbound body channel instead of a materialized body
    // (docs/design/streaming-bodies.md). Not part of the WHATWG surface --
    // http.ClientRequest sets it.
    if (typeof init.__oamBodyStream === "number") {
      request.body_stream = init.__oamBodyStream;
    } else if (init.body != null) {
      if (init.body instanceof ArrayBuffer || ArrayBuffer.isView(init.body)) {
        const bytes = init.body instanceof ArrayBuffer
          ? new Uint8Array(init.body)
          : new Uint8Array(init.body.buffer, init.body.byteOffset, init.body.byteLength);
        let binary = "";
        for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
        request.body_base64 = btoa(binary);
      } else {
        request.body = wellFormed(init.body);
        // node's "extract a body": a string body's Content-Type is
        // `text/plain;charset=UTF-8` unless the caller set one (measured).
        // Servers branch on it, and oam sent none at all.
        if (fetchSemantics && !headers.some((h) => h[0] === "content-type")) {
          headers.push(["content-type", "text/plain;charset=UTF-8"]);
        }
      }
    }
    // A caller `content-length` that disagrees with the body is refused, not
    // framed. hyper writes exactly the declared length, so a short one
    // SILENTLY TRUNCATED the body and still returned 200 -- data loss, and
    // the classic CL desync primitive if anything downstream re-frames.
    // node never dispatches either shape: a long one rejects with
    // `RequestContentLengthMismatchError: Request body length does not match
    // content-length header`, a short one hangs until its timeout (measured).
    // oam rejects both with node's long-form error.
    if (dispatchSemantics) {
      const declared = headers.find((h) => h[0].toLowerCase() === "content-length");
      if (declared !== undefined) {
        const want = Number(declared[1]);
        const have =
          request.body_base64 !== undefined
            ? atob(request.body_base64).length
            : request.body !== undefined
              ? new TextEncoder().encode(request.body).length
              : request.body_stream !== undefined
                ? null
                : 0;
        if (have !== null && (!Number.isInteger(want) || want < 0 || want !== have)) {
          const cause = new Error("Request body length does not match content-length header");
          cause.name = "RequestContentLengthMismatchError";
          throw new TypeError("fetch failed", { cause });
        }
      }
    }
    // Started synchronously: a malformed request or a --permission refusal
    // throws from here, as it always has.
    const pending = globalThis.__oam.fetch(JSON.stringify(request));
    if (rawPayload) return settleRaw(pending, lookup, signal, connector);
    const op = settleFetch(pending, lookup, signal, connector);
    if (!signal) return op;
    // Race the abort. Wave-1 divergence (documented): the underlying op
    // is not cancelled at the socket — the abort rejects the fetch
    // PROMISE promptly (the observable contract), the response is
    // discarded; full socket-level cancellation lands with the op-handle
    // rework.
    return Promise.race([
      op,
      new Promise((_resolve, reject) => {
        signal.addEventListener(
          "abort",
          () =>
            reject(
              signal.reason ?? new globalThis.DOMException("This operation was aborted", "AbortError"),
            ),
          { once: true },
        );
      }),
    ]);
  }

  // ---------------------------------------------------------- oam.serve
  // The web-standard server: oam.serve({ port, hostname, fetch(request) })
  // -> Promise<{ port, hostname, close() }>. The handler returns a
  // Response; a ReadableStream body streams to the client chunk-by-chunk
  // (the SSE/token path). Requests are dispatched CONCURRENTLY — the
  // accept loop never awaits a handler.
  function makeServerRequest(meta, host) {
    let bodyBytes = null;
    let consumed = false;
    const takeBody = () => {
      if (consumed) throw new TypeError("Body already consumed");
      consumed = true;
      bodyBytes ??= globalThis.__oam.node.httpRequestBody(meta.requestId);
      return bodyBytes;
    };
    return {
      method: meta.method,
      url: `http://${host}${meta.uri}`,
      headers: new Headers(meta.headers),
      get bodyUsed() {
        return consumed;
      },
      arrayBuffer: async () => takeBody().buffer,
      bytes: async () => takeBody(),
      text: async () => new TextDecoder().decode(takeBody()),
      json: async () => JSON.parse(new TextDecoder().decode(takeBody())),
    };
  }

  async function respondWith(requestId, response) {
    const node = globalThis.__oam.node;
    const status = response?.status ?? 200;
    const headerPairs = [];
    if (response?.headers) {
      response.headers.forEach((value, key) => headerPairs.push([key, value]));
    }
    const headersJson = JSON.stringify(headerPairs);
    const body = response?.body ?? response?._body ?? null;
    if (body !== null && typeof body === "object" && typeof body.getReader === "function") {
      // Streaming response: chunks flush as the handler produces them.
      const streamId = node.httpRespondStream(requestId, status, headersJson);
      const reader = body.getReader();
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          const bytes =
            typeof value === "string" ? new TextEncoder().encode(value) : value;
          await node.httpBodyPush(streamId, bytes);
        }
      } catch {
        // Client gone or source errored: stop pushing.
      } finally {
        node.httpBodyEnd(streamId);
      }
      return;
    }
    const bytes =
      body === null
        ? new Uint8Array(0)
        : typeof body === "string"
          ? new TextEncoder().encode(body)
          : body;
    node.httpRespond(requestId, status, headersJson, bytes);
  }

  async function serve(options) {
    const handler = typeof options === "function" ? options : options.fetch;
    if (typeof handler !== "function") {
      throw new TypeError("oam.serve requires a fetch(request) handler");
    }
    const hostname = options.hostname ?? "127.0.0.1";
    const node = globalThis.__oam.node;
    const bound = await node.httpServe(hostname, options.port ?? 0);
    const host = `${hostname}:${bound.port}`;

    const handleOne = async (meta) => {
      let response;
      try {
        response = await handler(makeServerRequest(meta, host));
      } catch (e) {
        const message = e && e.message ? e.message : String(e);
        response = new Response(`oam: handler error: ${message}`, { status: 500 });
      }
      await respondWith(meta.requestId, response);
    };

    (async () => {
      for (;;) {
        const meta = await node.httpAccept(bound.serverId);
        if (meta === undefined) break; // server closed
        void handleOne(meta);
      }
    })();

    return {
      port: bound.port,
      hostname,
      close() {
        node.httpClose(bound.serverId);
      },
    };
  }
  // Attached to the oam namespace post-restore (ops.rs installs `oam`
  // before this runs at runtime? No — snapshot time. Lazy attach instead).
  globalThis.__oamServe = serve;

  // ------------------------------------------------------------ CloseEvent
  if (typeof globalThis.CloseEvent !== "function") {
    class CloseEvent extends Event {
      constructor(type, init) {
        super(type, init);
        this.code = (init && init.code) || 0;
        this.reason = (init && init.reason) || "";
        this.wasClean = (init && init.wasClean) === true;
      }
    }
    globalThis.CloseEvent = CloseEvent;
  }

  // ------------------------------------------------------------ WebSocket
  const CONNECTING = 0;
  const OPEN = 1;
  const CLOSING = 2;
  const CLOSED = 3;

  class WebSocket extends EventTarget {
    static get CONNECTING() { return CONNECTING; }
    static get OPEN() { return OPEN; }
    static get CLOSING() { return CLOSING; }
    static get CLOSED() { return CLOSED; }

    constructor(url, protocols) {
      super();
      if (arguments.length === 0) throw new TypeError("WebSocket requires a url argument");
      const parsed = new URL(url);
      if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") {
        throw new DOMException(
          "WebSocket url must use ws: or wss: scheme",
          "SyntaxError",
        );
      }
      this._url = parsed.href;
      this._readyState = CONNECTING;
      this._protocol = "";
      this._extensions = "";
      this._binaryType = "blob";
      this._handle = null;
      this._bufferedAmount = 0;
      this.onopen = null;
      this.onmessage = null;
      this.onclose = null;
      this.onerror = null;

      const protoList = typeof protocols === "string" ? [protocols]
        : Array.isArray(protocols) ? protocols : [];

      const wire = JSON.stringify({ url: this._url, protocols: protoList });
      globalThis.__oam.wsConnect(wire).then(
        (result) => {
          this._handle = result.handle;
          this._protocol = result.protocol || "";
          this._extensions = result.extensions || "";
          this._readyState = OPEN;
          const ev = new Event("open");
          if (typeof this.onopen === "function") this.onopen(ev);
          this.dispatchEvent(ev);
          this._recvLoop();
        },
        (err) => {
          this._readyState = CLOSED;
          const ev = new Event("error");
          if (typeof this.onerror === "function") this.onerror(ev);
          this.dispatchEvent(ev);
          this._fireClose(1006, "", false);
        },
      );
    }

    get url() { return this._url; }
    get readyState() { return this._readyState; }
    get protocol() { return this._protocol; }
    get extensions() { return this._extensions; }
    get bufferedAmount() { return this._bufferedAmount; }
    get binaryType() { return this._binaryType; }
    set binaryType(v) {
      if (v === "blob" || v === "arraybuffer") this._binaryType = v;
    }

    get CONNECTING() { return CONNECTING; }
    get OPEN() { return OPEN; }
    get CLOSING() { return CLOSING; }
    get CLOSED() { return CLOSED; }

    send(data) {
      if (this._readyState === CONNECTING) {
        throw new DOMException(
          "WebSocket is not open: readyState 0 (CONNECTING)",
          "InvalidStateError",
        );
      }
      if (this._readyState !== OPEN) return;
      let isBinary = false;
      if (typeof data !== "string") {
        isBinary = true;
        if (ArrayBuffer.isView(data)) {
          data = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
        } else if (data instanceof ArrayBuffer) {
          data = new Uint8Array(data);
        }
      }
      globalThis.__oam.wsSend(this._handle, data, isBinary);
    }

    close(code, reason) {
      if (this._readyState === CLOSING || this._readyState === CLOSED) return;
      if (code !== undefined) {
        if (code !== 1000 && (code < 3000 || code > 4999)) {
          throw new DOMException(
            "Invalid close code: " + code,
            "InvalidAccessError",
          );
        }
      }
      this._readyState = CLOSING;
      globalThis.__oam.wsClose(
        this._handle,
        code === undefined ? 1000 : code,
        reason || "",
      );
    }

    async _recvLoop() {
      while (this._readyState === OPEN || this._readyState === CLOSING) {
        let frame;
        try {
          frame = await globalThis.__oam.wsRecv(this._handle);
        } catch {
          break;
        }
        if (frame === undefined) break;

        if (frame instanceof Uint8Array) {
          const data = this._binaryType === "arraybuffer"
            ? frame.buffer : frame;
          const ev = new MessageEvent("message", { data });
          if (typeof this.onmessage === "function") this.onmessage(ev);
          this.dispatchEvent(ev);
        } else if (frame.type === "text") {
          const ev = new MessageEvent("message", { data: frame.data });
          if (typeof this.onmessage === "function") this.onmessage(ev);
          this.dispatchEvent(ev);
        } else if (frame.type === "close") {
          this._fireClose(frame.code, frame.reason, true);
          break;
        }
      }
      if (this._handle !== null) {
        globalThis.__oam.wsDrop(this._handle);
        this._handle = null;
      }
      if (this._readyState !== CLOSED) {
        const wasClean = this._readyState === CLOSING;
        this._fireClose(wasClean ? 1000 : 1006, "", wasClean);
      }
    }

    _fireClose(code, reason, wasClean) {
      this._readyState = CLOSED;
      const ev = new CloseEvent("close", { code, reason, wasClean });
      if (typeof this.onclose === "function") this.onclose(ev);
      this.dispatchEvent(ev);
    }
  }
  brand(WebSocket, "WebSocket");
  globalThis.WebSocket = WebSocket;
})();

// Node's system-error classes, for the errors native ops reject with. The
// engine's settle path (crates/oam_engine/src/ops.rs sys_error /
// aggregate_error) calls these for every OpOutcome::NodeFailed and
// NodeAggregateFailed, and builds the same own properties natively only if
// they are unreachable.
//
// Locked globals, not `__oam` members: __oam does not exist while this file is
// snapshotted and ops::install replaces it after restore, and it is writable
// by user code. Every worker and fork isolate restores the same snapshot, so
// each has its own copy. Built from intrinsics captured HERE, never looked up
// at call time, so a script that replaces globalThis.Error or AggregateError
// changes nothing.
//
// Shapes measured on node v22.22.2 (lib/internal/errors.js):
// - ExceptionWithHostPort (a connect failure) and DNSException (a resolver
//   failure) are Error subclasses whose prototype carries only a `constructor`
//   getter answering `Error`: `err.constructor === Error`, yet
//   `Object.getPrototypeOf(err) === Error.prototype` is false. Own properties:
//   stack, message, then enumerable errno, code, syscall, address, port -- or
//   errno, code, syscall, hostname. `port` only when truthy. The stack header
//   is the plain `Error: <message>` (they are not kIsNodeError).
// - NodeAggregateError (every address of a multi-address connect failed):
//   `extends AggregateError` with `code` = errors[0].code and prototype getters
//   `constructor` (answering AggregateError) and [kIsNodeError]. Own
//   properties stack, errors, code; no own message; Object.keys ["code"];
//   String(err) "AggregateError"; stack header `AggregateError [CODE]: `.
// - Anything else (the fs shape: errno, code, syscall, path) is a plain Error,
//   as before.
// Stack frames are observable: util.inspect brackets an error with none
// (`[Error: ...] {`), and an uncaught throw of one is headed by the user's
// throw site rather than by the error's first frame. Node's connect and DNS
// errors are built in JS and print unbracketed, so those classes keep the
// factory frame. Node's fs errors are built by the binding (uvException) with
// no JS on the stack: an fs.readFile / readdir / access / unlink callback
// error and a createReadStream 'error' print bracketed, and
// `fs.readFile(p, (e) => { throw e })` is headed by the `throw e` line
// (measured on v22.22.2). So the plain-Error branch drops its frame, exactly
// as the native build it replaced did. fs/promises adds node's frames at its
// own boundary (node_compat.js asAlwaysRejecting), as node does in
// handleErrorFromBinding, not here.
(() => {
  const ErrorCtor = Error;
  const captureStackTrace = ErrorCtor.captureStackTrace;
  const AggregateErrorCtor = AggregateError;
  const SymbolIterator = Symbol.iterator;
  const kIsNodeError = Symbol("kIsNodeError");

  class ExceptionWithHostPort extends ErrorCtor {
    get ["constructor"]() {
      return ErrorCtor;
    }
  }

  class DNSException extends ErrorCtor {
    get ["constructor"]() {
      return ErrorCtor;
    }
  }

  // `fields` is a null-prototype record from the engine: message, code and,
  // each only when present, errno, syscall, path, hostname, address, port.
  function makeSysError(fields) {
    const message = fields.message;
    let err;
    if (fields.address !== undefined || fields.port !== undefined) {
      err = new ExceptionWithHostPort(message);
    } else if (fields.hostname !== undefined) {
      err = new DNSException(message);
    } else {
      err = new ErrorCtor(message);
      // Skipping makeSysError and everything above it leaves no frames: the
      // engine calls this with no JS below it.
      captureStackTrace(err, makeSysError);
    }
    if (fields.errno !== undefined) err.errno = fields.errno;
    err.code = fields.code;
    if (fields.syscall !== undefined) err.syscall = fields.syscall;
    if (fields.path !== undefined) err.path = fields.path;
    if (fields.hostname !== undefined) err.hostname = fields.hostname;
    if (fields.address !== undefined) err.address = fields.address;
    if (fields.port) err.port = fields.port;
    return err;
  }

  // Node passes `new SafeArrayIterator(errors)`: the children are read without
  // consulting an Array.prototype[Symbol.iterator] user code may have replaced.
  function listIterable(list) {
    return {
      [SymbolIterator]() {
        let i = 0;
        return {
          next() {
            return i < list.length
              ? { value: list[i++], done: false }
              : { value: undefined, done: true };
          },
        };
      },
    };
  }

  class NodeAggregateError extends AggregateErrorCtor {
    constructor(errors, message) {
      super(listIterable(errors), message);
      this.code = errors[0]?.code;
    }

    get [kIsNodeError]() {
      return true;
    }

    get ["constructor"]() {
      return AggregateErrorCtor;
    }
  }

  function makeAggregateError(errors) {
    const err = new NodeAggregateError(errors);
    // Node's prepareStackTrace writes `${name} [${code}]: ${message}` for a
    // kIsNodeError error. Rewrite line 0 only when it is the default render
    // (`AggregateError`, an empty message): a user's Error.prepareStackTrace
    // output is theirs to keep, exactly as in node. `stack` stays an accessor
    // after the assignment.
    try {
      const stack = err.stack;
      if (typeof stack === "string") {
        const nl = stack.indexOf("\n");
        const head = nl === -1 ? stack : stack.slice(0, nl);
        if (head === "AggregateError") {
          err.stack = `AggregateError [${err.code}]: ${nl === -1 ? "" : stack.slice(nl)}`;
        }
      }
    } catch {
      // A throwing user Error.prepareStackTrace: the error is still whole.
    }
    return err;
  }

  Object.defineProperty(globalThis, "__oamMakeSysError", {
    value: makeSysError,
    writable: false,
    enumerable: false,
    configurable: false,
  });
  Object.defineProperty(globalThis, "__oamMakeAggregateError", {
    value: makeAggregateError,
    writable: false,
    enumerable: false,
    configurable: false,
  });
})();

// Node reports an uncaught exception as util.inspect(err): the stack, then a
// block of any extra own properties (code/errno/syscall/permission/...). The
// Rust fatal path only holds V8's one-line message summary, so it calls this
// to build the body. Lookups are at CALL time -- this file is snapshotted
// before any of it exists.
(() => {
  // Non-enumerable: Node's test harness fails any test that leaks a new
  // enumerable global, and this is runtime plumbing, not API.
  function __oamFormatFatal(err) {
    try {
      const reg = globalThis.__oamNode;
      const util = reg && typeof reg.get === "function" ? reg.get("util") : null;
      if (util && typeof util.inspect === "function") {
        return util.inspect(err, { colors: false, depth: 2 });
      }
    } catch {
      // fall through to the stack
    }
    try {
      if (err instanceof Error && typeof err.stack === "string") return err.stack;
    } catch {
      // a thrown Proxy can trap `stack`
    }
    try {
      return String(err);
    } catch {
      return "<unprintable exception>";
    }
  }
  Object.defineProperty(globalThis, "__oamFormatFatal", {
    value: __oamFormatFatal,
    writable: true,
    enumerable: false,
    configurable: true,
  });
})();

// Source-position remap for transpiled files, Node --enable-source-maps
// style but on by default: oam's codegen REFLOWS .ts/.tsx/.cts (Node's
// strip-only pipeline preserves positions), so without this every
// err.stack cites codegen line numbers. V8 materializes err.stack through
// this hook on first access; the default V8 format is reproduced
// byte-for-byte (header = ToString(err), one "\n    at <frame>" line per
// CallSite via the CallSite's own toString) with exactly one change: when
// the runtime source-map registry has a mapping for a frame's
// file:line:column (__oam.mapPosition, populated only for transpiled
// sources), the generated position is rewritten to the source position.
// Plain .js frames never have a registry entry and pass through verbatim.
// Userland assigning its own Error.prepareStackTrace overrides this, same
// as Node. SNAPSHOT CONSTRAINT: __oam is looked up at CALL time.
(() => {
  Error.prepareStackTrace = function (err, frames) {
    let head;
    try {
      head = `${err}`;
    } catch {
      head = "<error>";
    }
    let out = head;
    for (const frame of frames) {
      let text = `${frame}`;
      try {
        const internal = globalThis.__oam;
        const map = internal && internal.mapPosition;
        const file = typeof frame.getFileName === "function" ? frame.getFileName() : null;
        const line = typeof frame.getLineNumber === "function" ? frame.getLineNumber() : null;
        const col = typeof frame.getColumnNumber === "function" ? frame.getColumnNumber() : null;
        if (typeof map === "function" && file && line && col) {
          const mapped = map(file, line, col);
          if (mapped) {
            const generated = `:${line}:${col}`;
            const at = text.lastIndexOf(generated);
            if (at !== -1) {
              text =
                text.slice(0, at) +
                `:${mapped[0]}:${mapped[1]}` +
                text.slice(at + generated.length);
            }
          }
        }
      } catch {
        // remap must never break stack formatting
      }
      out += `\n    at ${text}`;
    }
    return out;
  };
})();
