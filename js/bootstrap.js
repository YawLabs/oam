// oam bootstrap: the JS half of the runtime surface. Evaluated at context
// creation today; compiled into the startup snapshot once that pipeline
// lands (same source, faster boot).
//
// fetch: plain-object Response/Headers shapes (real spec classes arrive with
// oam_web + WPT) over a streamed body. Wire contract with crates/oam_core
// http_client::send (__oam.fetch / fetchContinue / fetchAbandon /
// fetchCancel):
//   request:  JSON string {url, method, headers: [[k,v]],
//             body | body_base64 | body_stream, attempt_timeout_ms,
//             fetch_semantics, dispatch_semantics, lookup_hook?, lookup_pool?,
//             connect_timeout_ms?, alpn? ("http1" when absent | "allow_h2" | "none")}
//   response: {status, statusText, url, redirected, headers: [[k,v]],
//             bodyHandle} -- or, for a lookup_hook request,
//             {lookup: {token, host, port}} when a new connection needs the
//             hook: run it, then fetchContinue(token, JSON {ips}) or
//             fetchAbandon(token)
//   cancel:   fetch(request, id) registers the fetch under `id`;
//             fetchCancel(id) takes it off the wire while it has no
//             response head (after the head, fetchBodyCancel(bodyHandle))
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
    static timeout(delay) {
      // node: validateUint32(delay, 'delay', false) -- a number, an integer,
      // and within uint32, each with its own coded error. The codes live in
      // node_compat.js, which is evaluated after this file, so they are
      // looked up at call time.
      const codes = globalThis.__oamNode.get("internal/errors").codes;
      if (typeof delay !== "number") {
        throw new codes.ERR_INVALID_ARG_TYPE("delay", "number", delay);
      }
      if (!Number.isInteger(delay)) {
        throw new codes.ERR_OUT_OF_RANGE("delay", "an integer", delay);
      }
      if (delay < 0 || delay > 4294967295) {
        throw new codes.ERR_OUT_OF_RANGE("delay", ">= 0 && <= 4294967295", delay);
      }
      const signal = new AbortSignal();
      // The runtime's setTimeout, not the global: a replaced global (fake
      // timers, which return a number) neither runs this timer nor has an
      // unref() to call -- node arms it with its internal timers too.
      const setTimer = globalThis.__oamNode._setTimeout ?? globalThis.setTimeout;
      const timer = setTimer(() => {
        signal._fire(
          new globalThis.DOMException("The operation was aborted due to timeout", "TimeoutError"),
        );
      }, delay);
      // node unrefs this timer (lib/internal/abort_controller.js
      // setWeakAbortSignalTimeout): a timeout signal alone never keeps the
      // process alive. Ref'd, `fetch(url, { signal: AbortSignal.timeout(5000) })`
      // held the process open for the full five seconds after the response
      // had been read.
      if (typeof timer?.unref === "function") timer.unref();
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
  // reading one broken cookie where node gives it two. Every other name
  // combines in place. The list keeps the order the names arrived in (wire
  // order, or the order a handler set them), which is what `oam.serve`
  // writes back out and what a fetch sends; ITERATION is sorted by name, as
  // the standard's "sort and combine" says and node does (#175), so every
  // script that walks a Headers sees node's order.
  class Headers {
    constructor(init) {
      /** @type {Array<[string, string]>} */
      this._list = [];
      // The sorted view iteration reads, rebuilt after a change.
      this._sorted = null;
      if (init === undefined || init === null) return;
      if (init instanceof Headers) {
        // The stored list, in its order: iterating would sort it, and a
        // Response built from a handler's Headers must reach the wire in the
        // order the handler set them. Nothing a script can iterate differs.
        for (const [k, v] of init._list) this._append(k, v);
      } else if (typeof init[Symbol.iterator] === "function" && typeof init !== "string") {
        for (const pair of init) this.append(pair[0], pair[1]);
      } else {
        for (const key of Object.keys(init)) this.append(key, init[key]);
      }
    }
    // A name and a value are ByteStrings, as webidl makes them in node: a
    // code unit above 0xFF is refused with node's TypeError (#174), since
    // the wire carries one byte per code point. `_append` is the unchecked
    // entry for lists oam built itself (a fetched response's latin1 head,
    // oam.serve's request head, a copy of another Headers).
    append(name, value) {
      const [key, text] = headerEntry("append", name, value);
      this._append(key, text);
    }
    _append(name, value) {
      const key = String(name).toLowerCase();
      const text = String(value);
      this._sorted = null;
      if (key === "set-cookie") {
        this._list.push([key, text]);
        return;
      }
      const entry = this._list.find((e) => e[0] === key);
      if (entry === undefined) this._list.push([key, text]);
      else entry[1] = `${entry[1]}, ${text}`;
    }
    set(name, value) {
      let [key, text] = headerEntry("set", name, value);
      key = key.toLowerCase();
      this._sorted = null;
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
      this._sorted = null;
      this._list = this._list.filter((e) => e[0] !== key);
    }
    // The list sorted by name, set-cookie lines kept apart in the order they
    // came (a stable sort of the stored list does both).
    _sortedList() {
      this._sorted ??= this._list.slice().sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
      return this._sorted;
    }
    // Live, as webidl's iterable is in node: each step reads the sorted list
    // as it is now, so a name added or removed mid-walk is seen.
    forEach(fn, thisArg) {
      for (let i = 0; i < this._sortedList().length; i++) {
        const [key, value] = this._sortedList()[i];
        fn.call(thisArg, value, key, this);
      }
    }
    entries() {
      return headersIterator(this, "key+value");
    }
    keys() {
      return headersIterator(this, "key");
    }
    values() {
      return headersIterator(this, "value");
    }
    [Symbol.iterator]() {
      return headersIterator(this, "key+value");
    }
  }
  Object.defineProperty(Headers.prototype, Symbol.iterator, { enumerable: false });
  brand(Headers, "Headers");
  globalThis.Headers = Headers;

  // undici's Headers.append / set: name and value as ByteStrings, the value
  // stripped of leading and trailing HTTP whitespace, then a name that is not
  // a token or a value holding NUL, CR or LF refused with undici's texts
  // (measured on node v22.22.2).
  const HEADER_NAME_TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
  function headerEntry(method, name, value) {
    const key = toByteString(name);
    const text = toByteString(value).replace(/^[\t\n\r ]+|[\t\n\r ]+$/g, "");
    if (!HEADER_NAME_TOKEN.test(key)) {
      throw new TypeError(`Headers.${method}: "${key}" is an invalid header name.`);
    }
    if (/[\0\r\n]/.test(text)) {
      throw new TypeError(`Headers.${method}: "${text}" is an invalid header value.`);
    }
    return [key, text];
  }

  // node's `Headers Iterator`: an index into the sorted list, read afresh on
  // every next().
  const HeadersIteratorPrototype = Object.create(
    Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())),
    {
      next: {
        value: function next() {
          const state = headersIterators.get(this);
          if (state === undefined) throw new TypeError("Illegal invocation");
          const list = state.target._sortedList();
          if (state.index >= list.length) return { value: undefined, done: true };
          const [key, value] = list[state.index++];
          const out = state.kind === "key" ? key : state.kind === "value" ? value : [key, value];
          return { value: out, done: false };
        },
        writable: true,
        enumerable: true,
        configurable: true,
      },
      [Symbol.toStringTag]: { value: "Headers Iterator", configurable: true },
    },
  );
  const headersIterators = new WeakMap();
  function headersIterator(target, kind) {
    const iterator = Object.create(HeadersIteratorPrototype);
    headersIterators.set(iterator, { target, kind, index: 0 });
    return iterator;
  }

  // ------------------------------------------------------------------ bodies
  // The Fetch Standard's "extract a body", once, for `fetch`, `Request` and
  // `Response`. Each of them used to handle a string and bytes and hand
  // everything else to `String()`, so a Blob or a FormData went out as its
  // `[object ...]` name, a URLSearchParams as text/plain, and
  // `new Response(blob).text()` threw (#154).
  //
  // A body is `{ bytes, stream, type, kind, used }`: `bytes` when the whole
  // of it is known up front (everything but a stream), `stream` when it is
  // the caller's ReadableStream -- or, later, the stream `.body` made from
  // the bytes, which from then on IS the body. `type` is the content-type
  // the source implies, null when it implies none.
  //
  // `copy`: Request and Response keep the body, so a caller's buffer is
  // copied, as the standard says (measured: mutating the array after
  // `new Response(u8)` does not change `text()`). `fetch` encodes it before
  // it returns, and skips the copy.
  //
  // An async iterable that is not a ReadableStream is read through undici's
  // ReadableStreamFrom (lib/core/util.js): each value through Buffer.from --
  // an ArrayBuffer is its bytes, a number or a plain object fails with
  // Buffer.from's ERR_INVALID_ARG_TYPE -- and an empty one is skipped
  // (measured on node v22.22.2 by the undici lane's
  // fetch_streams_a_readable_stream_or_async_iterable_body). A
  // ReadableStream's chunks reach the socket as they are (uploadChunk).
  function iterableStream(iterable) {
    let iterator;
    return new globalThis.ReadableStream({
      // A byte stream, as undici's ReadableStreamFrom builds it: its
      // high-water mark is 0, so the iterator is not advanced until a read
      // (the upload, or a reader of `.body`) asks for bytes.
      type: "bytes",
      start() {
        iterator = iterable[Symbol.asyncIterator]();
      },
      // Pulls until it has a non-empty chunk (or the end): node's hangs on an
      // empty chunk followed by more (docs/node-divergences.md), oam skips it.
      async pull(controller) {
        for (;;) {
          const { done, value } = await iterator.next();
          if (done) {
            controller.close();
            return;
          }
          const B = globalThis.Buffer;
          let bytes;
          if (B !== undefined) {
            bytes = B.isBuffer(value) ? value : B.from(value);
          } else if (typeof value === "string") {
            bytes = new TextEncoder().encode(value);
          } else {
            bytes = new Uint8Array(ArrayBuffer.isView(value) ? value.buffer : value);
          }
          if (bytes.byteLength) {
            controller.enqueue(new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength));
            return;
          }
        }
      },
      async cancel(reason) {
        await iterator?.return?.(reason);
      },
    });
  }

  function extractBody(object, copy) {
    const state = { bytes: null, stream: null, type: null, kind: "bytes", used: false };
    const Stream = globalThis.ReadableStream;
    if (object instanceof Stream) {
      state.kind = "stream";
      state.stream = object;
    } else if (object instanceof globalThis.Blob) {
      state.kind = "blob";
      if (object.type !== "") state.type = object.type;
      // oam's own Blob holds its bytes, and they never change. Anything else
      // that passes for one is read through its stream.
      if (object._bytes instanceof Uint8Array) state.bytes = object._bytes;
      else {
        state.stream = object.stream();
        // What fetch sends as its content-length, as undici does.
        state.size = object.size;
      }
    } else if (object instanceof ArrayBuffer || ArrayBuffer.isView(object)) {
      const view =
        object instanceof ArrayBuffer
          ? new Uint8Array(object)
          : new Uint8Array(object.buffer, object.byteOffset, object.byteLength);
      state.bytes = copy ? view.slice() : view;
    } else if (object instanceof globalThis.FormData) {
      state.kind = "formdata";
      const boundary = multipartBoundary();
      state.bytes = multipartBytes(object, boundary);
      state.type = `multipart/form-data; boundary=${boundary}`;
    } else if (
      typeof globalThis.URLSearchParams === "function" &&
      object instanceof globalThis.URLSearchParams
    ) {
      state.kind = "params";
      state.bytes = new TextEncoder().encode(object.toString());
      state.type = "application/x-www-form-urlencoded;charset=UTF-8";
    } else if (
      object !== null &&
      typeof object === "object" &&
      typeof object[Symbol.asyncIterator] === "function"
    ) {
      // undici takes any async iterable as a streamed body (measured: an
      // async generator goes out chunked), which is also how a node Readable
      // is sent.
      state.kind = "stream";
      state.stream = iterableStream(object);
    } else {
      // A string, and anything else through String(): a USVString, so a lone
      // surrogate becomes U+FFFD, which is also what encoding it would do.
      state.kind = "string";
      // `text` lets fetch hand the transport the string as it is.
      state.text = wellFormed(object);
      state.bytes = new TextEncoder().encode(state.text);
      state.type = "text/plain;charset=UTF-8";
    }
    if (state.stream !== null && (state.stream.locked || state.stream._disturbed === true)) {
      // node's text for a Request body too.
      throw new TypeError("Response body object should not be disturbed or locked");
    }
    return state;
  }

  // The multipart/form-data encoding of a FormData, as undici writes it
  // (measured on node v22.22.2, byte for byte apart from the boundary):
  // line breaks in a name or a string value become CRLF; CR, LF and `"` in a
  // name or a filename are percent-escaped; a file part always carries a
  // Content-Type, `application/octet-stream` when the file has none.
  function multipartBoundary() {
    const digits = String(Math.floor(Math.random() * 1e11)).padStart(11, "0");
    return `----formdata-oam-0${digits}`;
  }

  function multipartBytes(form, boundary) {
    const encoder = new TextEncoder();
    const crlf = (text) => text.replace(/\r?\n|\r/g, "\r\n");
    const escape = (text) =>
      text.replace(/\n/g, "%0A").replace(/\r/g, "%0D").replace(/"/g, "%22");
    const parts = [];
    for (const [name, value] of form) {
      const head = `--${boundary}\r\nContent-Disposition: form-data; name="${escape(crlf(name))}"`;
      if (typeof value === "string") {
        parts.push(encoder.encode(`${head}\r\n\r\n${crlf(value)}\r\n`));
      } else {
        const filename = value.name ? `; filename="${escape(value.name)}"` : "";
        const type = value.type || "application/octet-stream";
        parts.push(encoder.encode(`${head}${filename}\r\nContent-Type: ${type}\r\n\r\n`));
        parts.push(value._bytes);
        parts.push(encoder.encode("\r\n"));
      }
    }
    parts.push(encoder.encode(`--${boundary}--\r\n`));
    return concatBytes(parts);
  }

  function concatBytes(chunks) {
    let total = 0;
    for (const chunk of chunks) total += chunk.length;
    const out = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      out.set(chunk, offset);
      offset += chunk.length;
    }
    return out;
  }

  // The body of each Request and Response a script constructs: a state as
  // above, or null for no body. Kept off the object, so `Object.keys` and
  // `JSON.stringify` never see the payload.
  const bodyStates = new WeakMap();

  // Extract `object` as `owner`'s body, and give `headers` the content-type
  // it implies unless the caller set one.
  function initBody(owner, object, headers) {
    const state = object === null || object === undefined ? null : extractBody(object, true);
    bodyStates.set(owner, state);
    if (state !== null && state.type !== null && !headers.has("content-type")) {
      headers.set("content-type", state.type);
    }
    return state;
  }

  // Already read, or being read by someone else.
  function bodyUnusable(state) {
    return (
      state.used ||
      (state.stream !== null && (state.stream.locked || state.stream._disturbed === true))
    );
  }

  // `.body`: the caller's stream, or one made over the bytes on first ask.
  function bodyStreamOf(state) {
    if (state.stream === null) {
      const bytes = state.used ? null : state.bytes;
      state.stream = new globalThis.ReadableStream({
        type: "bytes", // undici's body streams are byte streams
        start(controller) {
          if (bytes !== null && bytes.length > 0) controller.enqueue(bytes);
          controller.close();
        },
      });
      // Already read through text() and friends: what is left is a stream
      // nobody can read, as node's is (it stays locked to the reader that
      // drained it).
      if (state.used) state.stream.getReader();
    }
    return state.stream;
  }

  // Read the whole body, once. A second read, or a read of a body whose
  // stream somebody else holds or has read from, is node's TypeError.
  async function consumeBody(owner) {
    const state = bodyStates.get(owner);
    if (state === null || state === undefined) return new Uint8Array(0);
    if (bodyUnusable(state)) {
      throw new TypeError("Body is unusable: Body has already been read");
    }
    if (state.stream === null) {
      state.used = true;
      return state.bytes;
    }
    // The reader is kept: a consumed body's stream stays locked, as node's.
    const reader = state.stream.getReader();
    const chunks = [];
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      if (!(value instanceof Uint8Array)) throw new TypeError("Received non-Uint8Array chunk");
      chunks.push(value);
    }
    return concatBytes(chunks);
  }

  // A second body with the same content, for clone(): the bytes are shared
  // (nothing writes to them), a stream is teed.
  function cloneBody(state) {
    if (state === null) return null;
    const copy = { ...state };
    if (state.stream !== null) [state.stream, copy.stream] = state.stream.tee();
    return copy;
  }

  // The Body mixin both classes share. `bodyUsed` is a prototype getter, as
  // in node, not an own property.
  function installBody(Class) {
    const define = (name, descriptor) =>
      Object.defineProperty(Class.prototype, name, {
        enumerable: true,
        configurable: true,
        ...descriptor,
      });
    const method = (name, value) => define(name, { value, writable: true });
    define("body", {
      get() {
        const state = bodyStates.get(this);
        return state === null || state === undefined ? null : bodyStreamOf(state);
      },
    });
    define("bodyUsed", {
      get() {
        const state = bodyStates.get(this);
        if (state === null || state === undefined) return false;
        return state.used || (state.stream !== null && state.stream._disturbed === true);
      },
    });
    method("text", async function text() {
      return new TextDecoder().decode(await consumeBody(this));
    });
    method("json", async function json() {
      return JSON.parse(new TextDecoder().decode(await consumeBody(this)));
    });
    method("arrayBuffer", async function arrayBuffer() {
      const bytes = await consumeBody(this);
      return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
    });
    method("bytes", async function bytes() {
      return (await consumeBody(this)).slice();
    });
    method("blob", async function blob() {
      const type = bodyMimeType(this.headers);
      return new globalThis.Blob([await consumeBody(this)], { type });
    });
  }

  // The type a body's `blob()` carries: the content-type parsed and
  // re-serialised as the MIME Sniffing Standard says, "" when it does not
  // parse (undici's bodyMimeType; measured: `A/B; x=1` reads `a/b;x=1`).
  const MIME_TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
  const MIME_QUOTED_VALUE = /^[\t\x20-\x7e\x80-\xff]*$/;
  function bodyMimeType(headers) {
    const value = headers.get("content-type");
    return value === null ? "" : (serializeMimeType(value) ?? "");
  }
  function serializeMimeType(input) {
    const text = input.replace(/^[\t\n\r ]+|[\t\n\r ]+$/g, "");
    const slash = text.indexOf("/");
    if (slash <= 0) return null;
    const type = text.slice(0, slash);
    let pos = slash + 1;
    let end = text.indexOf(";", pos);
    if (end === -1) end = text.length;
    const subtype = text.slice(pos, end).replace(/[\t\n\r ]+$/, "");
    if (!MIME_TOKEN.test(type) || !MIME_TOKEN.test(subtype)) return null;
    let out = `${type.toLowerCase()}/${subtype.toLowerCase()}`;
    const seen = new Set();
    pos = end;
    while (pos < text.length) {
      pos++; // the ";"
      while (pos < text.length && /[\t\n\r ]/.test(text[pos])) pos++;
      let stop = pos;
      while (stop < text.length && text[stop] !== ";" && text[stop] !== "=") stop++;
      const name = text.slice(pos, stop).toLowerCase();
      pos = stop;
      if (pos >= text.length) break;
      if (text[pos] === ";") continue;
      pos++; // the "="
      let value;
      if (text[pos] === '"') {
        value = "";
        pos++;
        while (pos < text.length) {
          const ch = text[pos++];
          if (ch === '"') break;
          if (ch === "\\") {
            if (pos >= text.length) {
              value += "\\";
              break;
            }
            value += text[pos++];
          } else value += ch;
        }
        while (pos < text.length && text[pos] !== ";") pos++;
      } else {
        stop = text.indexOf(";", pos);
        if (stop === -1) stop = text.length;
        value = text.slice(pos, stop).replace(/[\t\n\r ]+$/, "");
        pos = stop;
        if (value === "") continue;
      }
      if (name !== "" && MIME_TOKEN.test(name) && MIME_QUOTED_VALUE.test(value) && !seen.has(name)) {
        seen.add(name);
        const shown =
          value !== "" && MIME_TOKEN.test(value) ? value : `"${value.replace(/["\\]/g, "\\$&")}"`;
        out += `;${name}=${shown}`;
      }
    }
    return out;
  }

  // Response constructor (the SERVING side; fetch's inbound responses come
  // from makeResponse below). Body: anything extractBody takes, or null.
  class Response {
    constructor(body, init = {}) {
      this.status = init.status ?? 200;
      this.statusText = init.statusText ?? "";
      // A copy, as in node: the body's content-type lands on this response's
      // headers, not on a Headers object the caller may use again.
      this.headers = new Headers(init.headers);
      this.ok = this.status >= 200 && this.status <= 299;
      initBody(this, body, this.headers);
    }
    static json(data, init = {}) {
      const headers = new Headers(init.headers);
      if (!headers.has("content-type")) headers.set("content-type", "application/json");
      return new Response(JSON.stringify(data), { ...init, headers });
    }
    // A constructed response is "default"; one fetch() returned is "basic"
    // (makeResponse).
    get type() {
      return "default";
    }
    clone() {
      const state = bodyStates.get(this);
      if (state !== null && state !== undefined && bodyUnusable(state)) {
        throw new TypeError("Response.clone: Body has already been consumed.");
      }
      const copy = new Response(null, this);
      bodyStates.set(copy, cloneBody(state ?? null));
      return copy;
    }
  }
  Object.defineProperty(Response.prototype, "clone", { enumerable: true });
  installBody(Response);
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
          type: "bytes", // node's Blob#stream() is a byte stream
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
      // A Blob entry is a File, as the standard says: named `filename`, or
      // the File's own name, or "blob" (measured on node). The multipart
      // encoding writes that name.
      _normalizeValue(value, filename) {
        if (typeof globalThis.Blob !== "undefined" && value instanceof globalThis.Blob) {
          const isFile = value instanceof globalThis.File;
          if (isFile && filename === undefined) return value;
          const options = { type: value.type };
          if (isFile) options.lastModified = value.lastModified;
          return new globalThis.File([value], filename === undefined ? "blob" : filename, options);
        }
        if (typeof value === "string") return value;
        return String(value);
      }
    }
    brand(FormData, "FormData");
    globalThis.FormData = FormData;
  }

  // ----------------------------------------------------------------- Request
  // WHATWG Request, as undici 6's constructor builds it (measured on node
  // v22.22.2): RequestInit is converted member by member in webidl order --
  // each enum refused with undici's text -- then the URL is parsed, a Request
  // input's fields are inherited, and `init` overrides them one by one. Every
  // attribute is a prototype getter over the state below, as in node; the
  // `signal` is a fresh one that follows the caller's. `fetch` builds its
  // request through this same constructor (#180), so `fetch(request, init)`
  // means what `new Request(request, init)` means.
  //
  // Kept and returned, and acted on only by the checks below: mode,
  // credentials, cache, integrity, keepalive, referrer, referrerPolicy.
  // node's fetch also turns `cache` into request headers, checks `integrity`
  // and sends `referrer`; oam does not (docs/node-divergences.md).
  const requestStates = new WeakMap();
  const REQUEST_ENUMS = {
    referrerPolicy: [
      "",
      "no-referrer",
      "no-referrer-when-downgrade",
      "same-origin",
      "origin",
      "strict-origin",
      "origin-when-cross-origin",
      "strict-origin-when-cross-origin",
      "unsafe-url",
    ],
    mode: ["navigate", "same-origin", "no-cors", "cors"],
    credentials: ["omit", "same-origin", "include"],
    cache: ["default", "no-store", "reload", "no-cache", "force-cache", "only-if-cached"],
    redirect: ["follow", "manual", "error"],
    duplex: ["half"],
  };
  // undici's normalizedMethodRecords: these six are uppercased, any other
  // token is kept as written (`patch` goes out as `patch`).
  const REQUEST_NORMALIZED_METHODS = new Set(["DELETE", "GET", "HEAD", "OPTIONS", "POST", "PUT"]);
  const REQUEST_FORBIDDEN_METHODS = new Set(["CONNECT", "TRACE", "TRACK"]);
  const HTTP_TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
  let patchMethodWarned = false;
  // undici's one-time warning for its first HTTP/2 connection
  // (client-h2.js connectH2, 6.24.1, the version node v22.22.2 bundles),
  // which only a dispatcher with `allowH2` can open.
  let h2ExperimentalWarned = false;
  function warnH2Experimental() {
    if (h2ExperimentalWarned) return;
    h2ExperimentalWarned = true;
    globalThis.process?.emitWarning?.("H2 support is experimental, expect them to change at any time.", {
      code: "UNDICI-H2",
    });
  }

  // webidl's ByteString: a code unit above 0xFF is refused.
  function toByteString(value) {
    const text = String(value);
    for (let i = 0; i < text.length; i++) {
      const code = text.charCodeAt(i);
      if (code > 255) {
        throw new TypeError(
          `Cannot convert argument to a ByteString because the character at index ${i} has a value of ${code} which is greater than 255.`,
        );
      }
    }
    return text;
  }

  // RequestInit, converted as undici's webidl dictionary converter does: in
  // member order, reading each member once.
  function convertRequestInit(init) {
    if (init === undefined || init === null) return { hasKey: false };
    if (typeof init !== "object" && typeof init !== "function") {
      let shown;
      try {
        shown = String(init);
      } catch {
        shown = "Symbol";
      }
      throw new TypeError(
        `Request constructor: Expected ${shown} to be one of: Null, Undefined, Object.`,
      );
    }
    const out = {};
    const read = (key, convert) => {
      const value = init[key];
      if (value !== undefined) out[key] = convert(value, key);
    };
    const oneOf = (value, key) => {
      const text = String(value);
      if (!REQUEST_ENUMS[key].includes(text)) {
        throw new TypeError(
          `Request constructor: ${text} is not an accepted type. Expected one of ${REQUEST_ENUMS[key].join(", ")}.`,
        );
      }
      return text;
    };
    const same = (value) => value;
    read("method", toByteString);
    read("headers", same);
    read("body", same);
    read("referrer", (value) => String(value).toWellFormed());
    read("referrerPolicy", oneOf);
    read("mode", oneOf);
    read("credentials", oneOf);
    read("cache", oneOf);
    read("redirect", oneOf);
    read("integrity", String);
    read("keepalive", Boolean);
    read("signal", same);
    read("window", same);
    read("duplex", oneOf);
    read("dispatcher", same);
    // "init has a key": decides whether a Request input's navigation state
    // and referrer reset, and whether its headers are refilled. undici asks
    // it of the converted dictionary, not of the object it was given: a
    // member counts when it is one of RequestInit's and is not undefined,
    // own or inherited, and an unknown key never counts.
    out.hasKey = Object.keys(out).length !== 0;
    return out;
  }

  // A signal that follows `parent`: aborted with its reason, now or later.
  // The listener holds the follower weakly and is removed once the follower
  // is collected, so a long-lived signal shared by many requests does not
  // collect one listener per request (node does the same).
  const followCleanup =
    typeof FinalizationRegistry === "function"
      ? new FinalizationRegistry(({ parent, listener }) => {
          parent.removeEventListener("abort", listener);
        })
      : null;
  function followSignal(parent) {
    const controller = new AbortController();
    if (parent === null || parent === undefined) return controller.signal;
    if (
      typeof parent !== "object" ||
      typeof parent.aborted !== "boolean" ||
      typeof parent.addEventListener !== "function"
    ) {
      throw new TypeError(
        "Failed to construct 'Request': member signal is not of type AbortSignal.",
      );
    }
    if (parent.aborted) {
      controller.abort(parent.reason);
      return controller.signal;
    }
    // Held through the signal, which is what the request keeps: the
    // controller itself is unreachable once this returns.
    const follower = controller.signal;
    const ref = new WeakRef(follower);
    const listener = () => ref.deref()?._fire(parent.reason);
    parent.addEventListener("abort", listener, { once: true });
    followCleanup?.register(follower, { parent, listener });
    return follower;
  }

  // undici's cause for a URL that does not parse: node's URL error, whose
  // message is "Invalid URL" alone, with own `code` and `input` -- the shape
  // oam's `new URL()` throws too (case 237).
  function invalidUrlCause(input) {
    const cause = new TypeError("Invalid URL");
    cause.code = "ERR_INVALID_URL";
    cause.input = input;
    return cause;
  }

  function requestState(request) {
    const state = requestStates.get(request);
    if (state === undefined) throw new TypeError("Illegal invocation");
    return state;
  }

  // A copy of a Headers object's stored list, in its stored order.
  function copyHeaders(from) {
    const headers = new Headers();
    for (const [name, value] of from._list) headers._list.push([name, value]);
    return headers;
  }

  // The Request class fetch builds its own request with. A script may
  // replace or delete `globalThis.Request` (a polyfill's installGlobals
  // does), and node's fetch goes on using undici's own class, so fetch must
  // not read the global.
  let OamRequest = null;
  if (typeof globalThis.Request !== "function") {
    class Request {
      constructor(input, init = undefined) {
        if (arguments.length < 1) {
          throw new TypeError("Request constructor: 1 argument required, but 0 found.");
        }
        const fromRequest = input instanceof Request;
        if (!fromRequest) input = String(input).toWellFormed();
        const options = convertRequestInit(init);
        let state;
        let signal = null;
        let fallbackMode = null;
        if (!fromRequest) {
          let parsed;
          try {
            parsed = new URL(input);
          } catch {
            throw new TypeError(`Failed to parse URL from ${input}`, {
              cause: invalidUrlCause(input),
            });
          }
          if (parsed.username !== "" || parsed.password !== "") {
            throw new TypeError(
              `Request cannot be constructed from a URL that includes credentials: ${input}`,
            );
          }
          state = {
            url: parsed.href,
            method: "GET",
            headers: null,
            mode: "no-cors",
            credentials: "same-origin",
            cache: "default",
            redirect: "follow",
            integrity: "",
            keepalive: false,
            referrer: "client",
            referrerPolicy: "",
            reloadNavigation: false,
            historyNavigation: false,
            dispatcher: options.dispatcher,
          };
          fallbackMode = "cors";
        } else {
          const source = requestState(input);
          state = { ...source, dispatcher: options.dispatcher ?? source.dispatcher };
          signal = source.signal;
        }
        if (options.window !== undefined && options.window !== null) {
          throw new TypeError("'window' option 'client' must be null");
        }
        if (options.hasKey) {
          if (state.mode === "navigate") state.mode = "same-origin";
          state.reloadNavigation = false;
          state.historyNavigation = false;
          state.referrer = "client";
          state.referrerPolicy = "";
        }
        if (options.referrer !== undefined) {
          if (options.referrer === "") state.referrer = "no-referrer";
          else {
            let parsed;
            try {
              parsed = new URL(options.referrer);
            } catch {
              throw new TypeError(`Referrer "${options.referrer}" is not a valid URL.`, {
                cause: invalidUrlCause(options.referrer),
              });
            }
            state.referrer =
              parsed.protocol === "about:" && parsed.hostname === "client" ? "client" : parsed.href;
          }
        }
        if (options.referrerPolicy !== undefined) state.referrerPolicy = options.referrerPolicy;
        const mode = options.mode ?? fallbackMode;
        if (mode === "navigate") {
          throw new TypeError("Request constructor: invalid request mode navigate.");
        }
        if (mode !== null) state.mode = mode;
        if (options.credentials !== undefined) state.credentials = options.credentials;
        if (options.cache !== undefined) state.cache = options.cache;
        if (state.cache === "only-if-cached" && state.mode !== "same-origin") {
          throw new TypeError("'only-if-cached' can be set only with 'same-origin' mode");
        }
        if (options.redirect !== undefined) state.redirect = options.redirect;
        if (options.integrity !== undefined) state.integrity = options.integrity;
        if (options.keepalive !== undefined) state.keepalive = options.keepalive;
        if (options.method !== undefined) {
          const method = options.method;
          const upper = method.toUpperCase();
          if (REQUEST_NORMALIZED_METHODS.has(upper)) state.method = upper;
          else {
            if (!HTTP_TOKEN.test(method)) {
              throw new TypeError(`'${method}' is not a valid HTTP method.`);
            }
            if (REQUEST_FORBIDDEN_METHODS.has(upper)) {
              throw new TypeError(`'${method}' HTTP method is unsupported.`);
            }
            state.method = method;
          }
          if (state.method === "patch" && !patchMethodWarned) {
            patchMethodWarned = true;
            globalThis.process?.emitWarning?.(
              "Using `patch` is highly likely to result in a `405 Method Not Allowed`. `PATCH` is much more likely to succeed.",
              { code: "UNDICI-FETCH-patch" },
            );
          }
        }
        if (options.signal !== undefined) signal = options.signal;
        state.signal = followSignal(signal);
        if (mode === "no-cors" && !["GET", "HEAD", "POST"].includes(state.method)) {
          throw new TypeError(`'${state.method} is unsupported in no-cors mode.`);
        }
        // The input's headers carry over unless init names its own.
        if (options.hasKey && options.headers !== undefined) {
          state.headers = copyHeaders(new Headers(options.headers));
        } else if (fromRequest) {
          state.headers = copyHeaders(requestState(input).headers);
        } else {
          state.headers = new Headers();
        }
        const headers = state.headers;
        const inputBody = fromRequest ? (bodyStates.get(input) ?? null) : null;
        const initBody = options.body ?? null;
        if (
          (initBody !== null || inputBody !== null) &&
          (state.method === "GET" || state.method === "HEAD")
        ) {
          throw new TypeError("Request with GET/HEAD method cannot have body.");
        }
        let body = null;
        if (initBody !== null) {
          if (
            state.keepalive &&
            typeof initBody === "object" &&
            !(initBody instanceof globalThis.Blob) &&
            (initBody instanceof globalThis.ReadableStream ||
              typeof initBody[Symbol.asyncIterator] === "function")
          ) {
            throw new TypeError("keepalive");
          }
          body = extractBody(initBody, true);
          if (body.type !== null && !headers.has("content-type")) {
            headers.append("content-type", body.type);
          }
        }
        const streamed = body ?? inputBody;
        if (streamed !== null && streamed.kind === "stream") {
          if (body !== null && options.duplex === undefined) {
            throw new TypeError("RequestInit: duplex option is required when sending a body.");
          }
          if (state.mode !== "same-origin" && state.mode !== "cors") {
            throw new TypeError(
              'If request is made from ReadableStream, mode should be "same-origin" or "cors"',
            );
          }
        }
        if (body === null && inputBody !== null) {
          // The input Request's body moves to this one, and the input reads
          // as used from here on (measured on node).
          if (bodyUnusable(inputBody)) {
            throw new TypeError(
              "Cannot construct a Request with a Request object that has already been used.",
            );
          }
          body = { ...inputBody };
          inputBody.used = true;
          inputBody.stream = null;
        }
        requestStates.set(this, state);
        bodyStates.set(this, body);
      }
      clone() {
        const state = requestState(this);
        const body = bodyStates.get(this) ?? null;
        // node's text, as it is.
        if (body !== null && bodyUnusable(body)) throw new TypeError("unusable");
        const copy = Object.create(Request.prototype);
        requestStates.set(copy, {
          ...state,
          headers: copyHeaders(state.headers),
          signal: followSignal(state.signal),
        });
        bodyStates.set(copy, cloneBody(body));
        return copy;
      }
    }
    const attribute = (name, get) =>
      Object.defineProperty(Request.prototype, name, {
        get,
        enumerable: true,
        configurable: true,
      });
    attribute("method", function method() {
      return requestState(this).method;
    });
    attribute("url", function url() {
      return requestState(this).url;
    });
    attribute("headers", function headers() {
      return requestState(this).headers;
    });
    attribute("destination", function destination() {
      requestState(this);
      return "";
    });
    attribute("referrer", function referrer() {
      const value = requestState(this).referrer;
      if (value === "no-referrer") return "";
      if (value === "client") return "about:client";
      return value;
    });
    attribute("referrerPolicy", function referrerPolicy() {
      return requestState(this).referrerPolicy;
    });
    attribute("mode", function mode() {
      return requestState(this).mode;
    });
    attribute("credentials", function credentials() {
      return requestState(this).credentials;
    });
    attribute("cache", function cache() {
      return requestState(this).cache;
    });
    attribute("redirect", function redirect() {
      return requestState(this).redirect;
    });
    attribute("integrity", function integrity() {
      return requestState(this).integrity;
    });
    attribute("keepalive", function keepalive() {
      return requestState(this).keepalive;
    });
    attribute("isReloadNavigation", function isReloadNavigation() {
      return requestState(this).reloadNavigation;
    });
    attribute("isHistoryNavigation", function isHistoryNavigation() {
      return requestState(this).historyNavigation;
    });
    attribute("signal", function signal() {
      return requestState(this).signal;
    });
    attribute("duplex", function duplex() {
      requestState(this);
      return "half";
    });
    Object.defineProperty(Request.prototype, "clone", { enumerable: true });
    installBody(Request);
    brand(Request, "Request");
    globalThis.Request = Request;
    OamRequest = Request;
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
    for (const [name, value] of pairs) headers._append(name, value);
    return headers;
  }

  // A body read that failed, as node's fetch reports it: `TypeError:
  // terminated`, with what failed as the `cause` (measured on v22.22.2), so
  // a caller can tell a truncated download from a corrupt payload:
  // - a corrupt encoding: the decoder's error, an Error with zlib's `errno`
  //   and `code` (-3, Z_DATA_ERROR) or brotli's. The op builds it whole;
  // - the connection ending inside the body of a kept-alive response:
  //   undici's `SocketError: other side closed`, whose `socket` describes the
  //   connection. oam fills in the addresses; undici's `bytesWritten` /
  //   `bytesRead` are not counted here, so they are left out rather than
  //   made up;
  // - the same inside a content-length body the server did not keep alive
  //   (`Connection: close`, HTTP/1.0): undici's
  //   ResponseContentLengthMismatchError. (Inside such a chunked body the
  //   body just ends: BODY_ENDED_AT_CLOSE, never an error.)
  // - a bad chunk-size line: undici's HTTPParserError (its `data`, the bytes
  //   that did not parse, is not available: undefined);
  // - a reset: node's `read ECONNRESET`, built whole by the op too.
  // `raw` is the response payload, for the connection's facts.
  // http_client/body.rs ENDED_AT_CLOSE_CODE.
  const BODY_ENDED_AT_CLOSE = "OAM_BODY_ENDED_AT_CLOSE";

  // `fetchSemantics` false (undici.request's entry): undici's request() body
  // fails with the error itself, not fetch's `TypeError: terminated` around
  // it (measured on node v22.22.2 + undici 6.29.0: a cut-off kept-alive body
  // rejects with SocketError, one not kept with
  // ResponseContentLengthMismatchError).
  function bodyTerminated(e, raw, fetchSemantics = true) {
    let cause = e;
    if (e instanceof Error && e.code === "UND_ERR_RES_CONTENT_LENGTH_MISMATCH") {
      cause = new undiciErrors.ResponseContentLengthMismatchError();
    } else if (e instanceof Error && e.code === "UND_ERR_SOCKET" && e.socket) {
      // The transport's close, with the connection's facts as they stood
      // (bytes written and read included).
      cause = undiciCause(e);
    } else if (e instanceof Error && e.code === "UND_ERR_SOCKET") {
      const { localAddr, remoteAddr } = raw.socket ?? {};
      cause = new undiciErrors.SocketError(e.message, {
        localAddress: localAddr?.address,
        localPort: localAddr?.port,
        remoteAddress: remoteAddr?.address,
        remotePort: remoteAddr?.port,
        remoteFamily: remoteAddr?.family,
        timeout: undefined,
      });
    } else if (e instanceof Error && typeof e.code === "string" && e.code.startsWith("HPE_")) {
      cause = new undiciErrors.HTTPParserError(e.message, e.code.slice(4));
    } else if (e instanceof Error && e.code === "UND_ERR_BODY_TIMEOUT") {
      // undici's bodyTimeout lapsed (the transport runs it): node's body
      // errors with `TypeError: terminated`, a BodyTimeoutError its cause.
      cause = undiciCause(e);
    }
    return fetchSemantics ? new TypeError("terminated", { cause }) : cause;
  }

  // `onBodyOver`: called once when the body has been read to its end, has
  // failed, or was cancelled.
  function makeResponse(raw, signal, fetchSemantics = true, onBodyOver) {
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
      if (onBodyOver) {
        const over = onBodyOver;
        onBodyOver = null;
        over();
      }
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
        // undici's response body is a byte stream: high-water mark 0, so
        // touching `.body` does not start a read -- the first read() does.
        type: "bytes",
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
            // A response the server did not keep alive, cut off inside its
            // chunked body: undici takes what arrived as the whole response.
            if (e instanceof Error && e.code === BODY_ENDED_AT_CLOSE) chunk = undefined;
            else {
              bodyOver();
              throw bodyTerminated(e, raw, fetchSemantics);
            }
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
      // Read already, or held by a reader someone took from `.body`.
      if (consumed || (bodyStream !== null && (bodyStream.locked || bodyStream._disturbed))) {
        throw new TypeError("Body is unusable: Body has already been read");
      }
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
      // node's fetch never produces a filtered response other than "basic":
      // it is the type under every `redirect` mode, a 3xx returned by
      // "manual" included (measured on v22.22.2; browsers answer
      // "opaqueredirect" there).
      type: "basic",
      status: raw.status,
      statusText: raw.statusText,
      ok: raw.status >= 200 && raw.status <= 299,
      url: raw.url,
      redirected: raw.redirected === true,
      headers: makeHeaders(raw.headers),
      get body() {
        return ensureBody();
      },
      // Read from, not merely locked: a reader that has not read yet leaves
      // the body unused, as in node.
      get bodyUsed() {
        return consumed || (bodyStream !== null && bodyStream._disturbed === true);
      },
      arrayBuffer: async () => (await drainBytes()).buffer,
      bytes: () => drainBytes(),
      text: async () => new TextDecoder().decode(await drainBytes()),
      json: async () => JSON.parse(new TextDecoder().decode(await drainBytes())),
      // The body's bytes, typed with the response's content-type (#154).
      blob: async function blob() {
        const type = bodyMimeType(this.headers);
        return new globalThis.Blob([await drainBytes()], { type });
      },
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
  // 1024 is its <netdb.h> value, unmeasured. The same values as oam's
  // dns.ADDRCONFIG off Windows.
  function lookupHints() {
    const platform = globalThis.process?.platform;
    if (platform === "win32") return 0;
    // bionic's AI_ADDRCONFIG is the BSD 0x400, not glibc's 0x20.
    if (platform === "darwin" || platform === "freebsd" || platform === "android") return 1024;
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
  // `{ family, hints, all: true }`, a callback. `answer` settles with the
  // first callback: the addresses, or the error (or a synchronous throw)
  // unchanged.
  //
  // The callback stays live after that (#169). node's lookupAndConnectMultiple
  // acts on every callback that arrives while the socket is still
  // connecting, so a later error fails the connection its answer was
  // opening -- as does a later answer node's address rules refuse, or the
  // hook throwing after it answered -- and one after the socket connected is
  // ignored. The first such failure is `late.error`, handed to
  // `late.onFail` once the fetch has resumed with the answer; onFail says
  // whether the connection was still being made, and when it was not the
  // failure is forgotten. A later answer that passes the rules is ignored:
  // node starts a second connect on the same socket there, which fails with
  // a platform-specific code (EISCONN on Windows), and oam does not.
  function runConnectLookup(lookup, host, port) {
    const late = { failed: false, error: undefined, onFail: null, over: false };
    let settled = false;
    let resolveAnswer, rejectAnswer;
    const answer = new Promise((resolve, reject) => {
      resolveAnswer = resolve;
      rejectAnswer = reject;
    });
    const lateFailure = (e) => {
      if (late.over || late.failed) return;
      late.failed = true;
      late.error = e;
      if (late.onFail !== null && !late.onFail()) {
        late.failed = false;
        late.error = undefined;
        late.over = true;
      }
    };
    const callback = (err, addresses) => {
      if (!settled) {
        settled = true;
        if (err) {
          late.over = true;
          rejectAnswer(err);
          return;
        }
        try {
          resolveAnswer(pinAddresses(addresses, host, port));
        } catch (e) {
          late.over = true;
          rejectAnswer(e);
        }
        return;
      }
      if (err) {
        lateFailure(err);
        return;
      }
      try {
        pinAddresses(addresses, host, port);
      } catch (e) {
        lateFailure(e);
      }
    };
    try {
      lookup(host, { family: undefined, hints: lookupHints(), all: true }, callback);
    } catch (e) {
      if (!settled) {
        settled = true;
        late.over = true;
        rejectAnswer(e);
      } else {
        lateFailure(e);
      }
    }
    return { answer, late };
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

  // undici's error classes (lib/core/errors.js, 6.24.1 -- the copy node
  // v22.22.2 bundles), defined here because fetch raises them with no undici
  // import in sight: node's fetch IS undici, so the cause of a refused request
  // is an `InvalidArgumentError` with `code` `UND_ERR_INVALID_ARG`, and code
  // that tells failures apart reads that `code` or asks `instanceof`. The
  // oam:undici shim exports these same constructors (js/undici.js), so
  // `e.cause instanceof undici.errors.InvalidArgumentError` holds.
  //
  // The shape is undici's, measured: every class sets `name` and `code` in
  // its constructor and assigns `message` after `super(message)` -- so the
  // own names read stack, message, name, code with a message and stack, name,
  // code, message when the default is used -- and brands the instance with
  // `Symbol.for('undici.error.<code>')`, which a static Symbol.hasInstance
  // checks instead of the prototype chain. The symbols are registered ones,
  // which is what lets `instanceof` hold between two copies of undici: an
  // error from node's bundled copy is an instance of the npm package's class,
  // and here an oam error is an instance of a node_modules undici's.
  // A subclass inherits the check, as in undici.
  //
  // Built from intrinsics captured here, as __oamMakeSysError is: a script
  // that replaces globalThis.Error changes nothing.
  const undiciErrors = (() => {
    const ErrorCtor = Error;
    const mark = (code) => Symbol.for(`undici.error.${code}`);
    const kUndiciError = mark("UND_ERR");
    class UndiciError extends ErrorCtor {
      constructor(message) {
        super(message);
        this.name = "UndiciError";
        this.code = "UND_ERR";
      }
      static [Symbol.hasInstance](instance) {
        return instance && instance[kUndiciError] === true;
      }
      [kUndiciError] = true;
    }
    // One undici error class: `className` is the constructor's name and
    // `name` the instance's (they differ for two of them), `fill` sets what a
    // class adds from its further constructor arguments.
    function define(className, Base, code, defaultMessage, name = className, fill = null) {
      const kMark = mark(code);
      return {
        [className]: class extends Base {
          constructor(message, ...rest) {
            super(message);
            this.name = name;
            this.message = message || defaultMessage;
            this.code = code;
            if (fill !== null) fill(this, ...rest);
          }
          static [Symbol.hasInstance](instance) {
            return instance && instance[kMark] === true;
          }
          [kMark] = true;
        },
      }[className];
    }
    const AbortError = define("AbortError", UndiciError, "UND_ERR_ABORT", "The operation was aborted");
    const kHTTPParserError = mark("UND_ERR_HTTP_PARSER");
    class HTTPParserError extends ErrorCtor {
      constructor(message, code, data) {
        super(message);
        this.name = "HTTPParserError";
        this.code = code ? `HPE_${code}` : undefined;
        this.data = data ? data.toString() : undefined;
      }
      static [Symbol.hasInstance](instance) {
        return instance && instance[kHTTPParserError] === true;
      }
      [kHTTPParserError] = true;
    }
    const retryFill = (self, statusCode, { headers, data }) => {
      self.statusCode = statusCode;
      self.data = data;
      self.headers = headers;
    };
    const kSecureProxyConnectionError = mark("UND_ERR_PRX_TLS");
    class SecureProxyConnectionError extends UndiciError {
      // undici passes `{ cause, ...options }` on to UndiciError, which takes
      // a message alone: `cause` is the assignment below, an enumerable own
      // property set after `code`.
      constructor(cause, message) {
        super(message);
        this.name = "SecureProxyConnectionError";
        this.message = message || "Secure Proxy Connection failed";
        this.code = "UND_ERR_PRX_TLS";
        this.cause = cause;
      }
      static [Symbol.hasInstance](instance) {
        return instance && instance[kSecureProxyConnectionError] === true;
      }
      [kSecureProxyConnectionError] = true;
    }
    // undici brands this one through a prototype getter, not an own field.
    const kMessageSizeExceededError = mark("UND_ERR_WS_MESSAGE_SIZE_EXCEEDED");
    class MessageSizeExceededError extends UndiciError {
      constructor(message) {
        super(message);
        this.name = "MessageSizeExceededError";
        this.message = message || "Max decompressed message size exceeded";
        this.code = "UND_ERR_WS_MESSAGE_SIZE_EXCEEDED";
      }
      static [Symbol.hasInstance](instance) {
        return instance && instance[kMessageSizeExceededError] === true;
      }
      get [kMessageSizeExceededError]() {
        return true;
      }
    }
    // In the order undici's module exports them.
    return Object.freeze({
      AbortError,
      HTTPParserError,
      UndiciError,
      HeadersTimeoutError: define("HeadersTimeoutError", UndiciError, "UND_ERR_HEADERS_TIMEOUT", "Headers Timeout Error"),
      HeadersOverflowError: define("HeadersOverflowError", UndiciError, "UND_ERR_HEADERS_OVERFLOW", "Headers Overflow Error"),
      BodyTimeoutError: define("BodyTimeoutError", UndiciError, "UND_ERR_BODY_TIMEOUT", "Body Timeout Error"),
      RequestContentLengthMismatchError: define(
        "RequestContentLengthMismatchError",
        UndiciError,
        "UND_ERR_REQ_CONTENT_LENGTH_MISMATCH",
        "Request body length does not match content-length header",
      ),
      ConnectTimeoutError: define("ConnectTimeoutError", UndiciError, "UND_ERR_CONNECT_TIMEOUT", "Connect Timeout Error"),
      ResponseStatusCodeError: define(
        "ResponseStatusCodeError",
        UndiciError,
        "UND_ERR_RESPONSE_STATUS_CODE",
        "Response Status Code Error",
        "ResponseStatusCodeError",
        (self, statusCode, headers, body) => {
          self.body = body;
          self.status = statusCode;
          self.statusCode = statusCode;
          self.headers = headers;
        },
      ),
      InvalidArgumentError: define("InvalidArgumentError", UndiciError, "UND_ERR_INVALID_ARG", "Invalid Argument Error"),
      InvalidReturnValueError: define(
        "InvalidReturnValueError",
        UndiciError,
        "UND_ERR_INVALID_RETURN_VALUE",
        "Invalid Return Value Error",
      ),
      // `name` is the base class's, as in undici.
      RequestAbortedError: define("RequestAbortedError", AbortError, "UND_ERR_ABORTED", "Request aborted", "AbortError"),
      ClientDestroyedError: define("ClientDestroyedError", UndiciError, "UND_ERR_DESTROYED", "The client is destroyed"),
      ClientClosedError: define("ClientClosedError", UndiciError, "UND_ERR_CLOSED", "The client is closed"),
      InformationalError: define("InformationalError", UndiciError, "UND_ERR_INFO", "Request information"),
      SocketError: define("SocketError", UndiciError, "UND_ERR_SOCKET", "Socket error", "SocketError", (self, socket) => {
        self.socket = socket;
      }),
      NotSupportedError: define("NotSupportedError", UndiciError, "UND_ERR_NOT_SUPPORTED", "Not supported error"),
      ResponseContentLengthMismatchError: define(
        "ResponseContentLengthMismatchError",
        UndiciError,
        "UND_ERR_RES_CONTENT_LENGTH_MISMATCH",
        "Response body length does not match content-length header",
      ),
      BalancedPoolMissingUpstreamError: define(
        "BalancedPoolMissingUpstreamError",
        UndiciError,
        "UND_ERR_BPL_MISSING_UPSTREAM",
        "No upstream has been added to the BalancedPool",
        "MissingUpstreamError",
      ),
      ResponseExceededMaxSizeError: define(
        "ResponseExceededMaxSizeError",
        UndiciError,
        "UND_ERR_RES_EXCEEDED_MAX_SIZE",
        "Response content exceeded max size",
      ),
      RequestRetryError: define(
        "RequestRetryError",
        UndiciError,
        "UND_ERR_REQ_RETRY",
        "Request retry error",
        "RequestRetryError",
        retryFill,
      ),
      ResponseError: define("ResponseError", UndiciError, "UND_ERR_RESPONSE", "Response error", "ResponseError", retryFill),
      SecureProxyConnectionError,
      MessageSizeExceededError,
    });
  })();
  // Locked, like __oamMakeSysError: the oam:undici shim reads the classes
  // from here, and user code cannot swap them for others.
  Object.defineProperty(globalThis, "__oamUndiciErrors", {
    value: undiciErrors,
    writable: false,
    enumerable: false,
    configurable: false,
  });

  // The connection policy of the undici dispatcher a fetch rides (the
  // `dispatcher` option, else the global one): `{ connector }` for one whose
  // `connect` is a function (a connect object carrying socket or TLS options,
  // or an Agent `factory`, is turned into one), `{ refuse }` for one oam
  // cannot run faithfully -- a dispatch() override, interceptors, or an
  // object that is not one of the oam:undici shim's dispatchers -- since oam
  // would otherwise send the request without it.
  function dispatcherPolicy(dispatcher, holder, request) {
    if (holder && typeof holder.policy === "function") return holder.policy(dispatcher, request);
    const refuse = new undiciErrors.NotSupportedError(
      "a fetch dispatcher that is not one of oam's undici dispatchers is not supported: " +
        "oam cannot run its dispatch(); pass the connection policy as a `connect` function",
    );
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
    const cause = e instanceof Error ? e : new Error(String(e));
    return new TypeError("fetch failed", { cause: undiciCause(cause) });
  }

  // A failure undici itself raises reaches JS as the op's coded error (the
  // op has no JS classes to build); here it becomes the undici class node's
  // cause is an instance of, constructed the way undici constructs it.
  // Anything else is returned as it came.
  function undiciCause(e) {
    switch (e.code) {
      // A response head over node's limit (send.rs response_head_overflow):
      // undici's parser raises it with no message of its own.
      case "UND_ERR_HEADERS_OVERFLOW":
        return new undiciErrors.HeadersOverflowError();
      // undici's connect timeout ran out (connector.rs ConnectTimedOut,
      // which words the message).
      case "UND_ERR_CONNECT_TIMEOUT":
        return new undiciErrors.ConnectTimeoutError(e.message);
      // undici's headersTimeout / bodyTimeout, which the transport runs
      // (send.rs headers_deadline, body.rs `timed`).
      case "UND_ERR_HEADERS_TIMEOUT":
        // No message argument, as undici builds them: its class default
        // makes `message` an own enumerable key (name, code, message).
        return new undiciErrors.HeadersTimeoutError();
      case "UND_ERR_BODY_TIMEOUT":
        return new undiciErrors.BodyTimeoutError();
      // A response head the parser refused (bridge.rs head_parse_error,
      // which words it as undici does).
      case "HPE_INVALID_STATUS":
      case "HPE_INVALID_CONSTANT":
      case "HPE_INVALID_HEADER_TOKEN":
        return new undiciErrors.HTTPParserError(e.message, e.code.slice(4));
      // The peer closed the connection before the head (transport.rs
      // OpOutcome::SocketClosed): undici's SocketError, with the socket it
      // describes.
      case "UND_ERR_SOCKET": {
        const facts = e.socket || {};
        return new undiciErrors.SocketError(e.message, {
          localAddress: facts.localAddress,
          localPort: facts.localPort,
          remoteAddress: facts.remoteAddress,
          remotePort: facts.remotePort,
          remoteFamily: facts.remoteFamily,
          timeout: undefined,
          bytesWritten: facts.bytesWritten,
          bytesRead: facts.bytesRead,
        });
      }
      default:
        return e;
    }
  }

  // The op, settled: a response, or -- for a fetch whose dispatcher carries a
  // connect.lookup hook -- a lookup request first. undici calls the hook for
  // every connection to a host name, redirect hops included, so the native
  // loop parks before it dials a name it has no addresses for and hands the
  // name up here. A hook that fails fails the fetch CLOSED (its error is the
  // cause, unchanged, as in node) and never falls back to system DNS; an
  // abort while parked drops the parked fetch.
  // `fetchSemantics` false is undici.request's entry (see makeResponse).
  async function settleFetch(pending, lookup, signal, connector, fetchSemantics, onBodyOver) {
    return makeResponse(await settleRaw(pending, lookup, signal, connector), signal, fetchSemantics, onBodyOver);
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
        // Aborted before the fetch parked: the listener will never fire (see
        // the lookup branch below), so drop the parked fetch now.
        if (signal?.aborted) abandon();
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
        if (socket.alpnProtocol === "h2") warnH2Experimental();
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
      // An abort that landed before the fetch parked has already fired, so
      // the listener above never will: drop the parked fetch now, or it
      // keeps its streamed body's receiver -- and every write blocked on it
      // -- for as long as the hook takes to answer, forever if it never
      // does. The hook is still asked, as node asks it.
      if (signal?.aborted) abandon();
      const lookupRun = runConnectLookup(lookup, host, port);
      const { late } = lookupRun;
      let ips;
      try {
        ips = await lookupRun.answer;
      } catch (err) {
        abandon();
        throw new TypeError("fetch failed", { cause: err });
      } finally {
        signal?.removeEventListener("abort", abandon);
      }
      if (signal?.aborted) {
        late.over = true;
        abandon();
        // The abort race already rejected the fetch with the reason.
        throw aborted();
      }
      // The hook failed the connection before anything was dialled (a
      // second callback in the same call, or a throw after answering): node
      // never connects.
      if (late.failed) {
        late.over = true;
        abandon();
        throw new TypeError("fetch failed", { cause: late.error });
      }
      resumed = true;
      // From here a failure reaches the dial the answer opens: it is
      // abandoned if it has not connected yet.
      late.onFail = () => internal.fetchLookupFail(token);
      try {
        raw = await internal.fetchContinue(token, JSON.stringify({ ips }));
      } catch (e) {
        if (late.failed) throw new TypeError("fetch failed", { cause: late.error });
        throw fetchFailed(e);
      } finally {
        late.over = true;
      }
    }
    // A redirect whose Location does not parse: node's cause is the error
    // `new URL(location, currentURL)` throws there -- a TypeError with `code`,
    // `input` and `base`, in that order (measured on v22.22.2) -- and the op
    // hands both strings up so it can be built here. Not through `new URL`:
    // the op has already decided the Location does not parse, by the parser
    // it follows redirects with.
    if (raw && raw.invalidLocation) {
      const cause = new TypeError("Invalid URL");
      cause.code = "ERR_INVALID_URL";
      cause.input = raw.invalidLocation.input;
      cause.base = raw.invalidLocation.base;
      throw new TypeError("fetch failed", { cause });
    }
    // An answer over HTTP/2: only an `allowH2` dispatcher's connection can
    // have negotiated it.
    if (raw?.tls?.alpnProtocol === "h2") warnH2Experimental();
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
  // Returns `{ refuse: [name, message] }` -- the undici error class (see
  // undiciErrors) and its message -- or `{ value }`, the value to send.
  function dispatchHeader(name, value) {
    // undici's isValidHeaderValue: a control character other than HTAB, or
    // DEL, cannot go on the wire (a code point above U+00FF never got this
    // far: the Headers it came through refused it).
    if (/[^\t\x20-\x7e\x80-\xff]/.test(value)) {
      return { refuse: ["InvalidArgumentError", `invalid ${name} header`] };
    }
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

  // Ids for `__oam.fetch(request, id)` / `fetchCancel(id)`.
  let fetchCancelIds = 0;

  function bytesToBase64(bytes) {
    let binary = "";
    for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
    return btoa(binary);
  }

  // A fetch with a streamed body, as undici sends it (measured on node
  // v22.22.2 + undici 6.29.0): the request is dispatched at once -- a bad
  // port, a failed lookup or a refused connect fail the fetch while the
  // source is still idle -- and the source is read from the start (fetch
  // reads it ahead of the connect, where undici.request waits for the
  // connection). Its AsyncWriter writes the head with the first
  // non-empty chunk, and the transport does the same (`defer_head`): a body
  // that ends with none goes as no body at all, `content-length: 0` on a
  // method that expects a payload. The body follows over an outbound
  // channel -- chunked unless the caller declared a content-length
  // (`declared`, checked as it goes) -- and a write resolves once the
  // transport has the chunk, so the source is read no faster than the
  // socket takes it. A body that fails -- the stream errors, a chunk node
  // would not write, the length disagrees -- fails the fetch with
  // `TypeError: fetch failed` and that error as its cause. An abort, or a
  // response whose body is over while the upload is still going, stops
  // reading the source without cancelling it, as node's does.
  const PAYLOAD_METHODS_STREAMED = new Set(["POST", "PUT", "PATCH", "QUERY", "PROPFIND", "PROPPATCH"]);
  function fetchUpload(stream, request, declared, signal, dispatch) {
    const node = globalThis.__oam.node;
    return new Promise((resolve, reject) => {
      let stopped = false;
      let failure = null;
      let written = 0;
      const aborted = () =>
        signal.reason ?? new globalThis.DOMException("This operation was aborted", "AbortError");
      if (signal?.aborted) return reject(aborted());
      const channel = node.fetchBodyChannelNew();
      request.body_stream = channel;
      request.defer_head = {
        empty_content_length: PAYLOAD_METHODS_STREAMED.has(request.method) && declared === null,
      };
      const stop = () => {
        stopped = true;
        signal?.removeEventListener("abort", onAbort);
      };
      const dropChannel = () => {
        try {
          node.fetchBodyChannelCancel(channel);
        } catch {
          /* the request already took and finished it */
        }
      };
      const mismatch = () => new undiciErrors.RequestContentLengthMismatchError();
      const fail = (err) => {
        if (stopped) return;
        failure = err;
        stop();
        dropChannel();
      };
      const onAbort = () => {
        if (stopped) return;
        stop();
        dropChannel();
      };
      signal?.addEventListener("abort", onAbort, { once: true });
      const onBodyOver = () => {
        if (stopped) return;
        stop();
        dropChannel();
      };
      let op;
      try {
        op = dispatch(onBodyOver);
      } catch (e) {
        op = Promise.reject(e);
      }
      op.then(resolve, (e) => {
        const cause = failure;
        stop();
        dropChannel();
        reject(cause !== null ? new TypeError("fetch failed", { cause }) : e);
      });
      const pump = async () => {
        const reader = stream.getReader();
        for (;;) {
          const step = await reader.read();
          if (stopped) return;
          if (step.done) break;
          const bytes = uploadChunk(step.value);
          if (!bytes.byteLength) continue;
          if (declared !== null && written + bytes.byteLength > declared) throw mismatch();
          written += bytes.byteLength;
          const more = await node.fetchBodyChannelWrite(channel, bytes).then(null, () => false);
          if (stopped) return;
          if (more === false) return stop();
        }
        if (declared !== null && written !== declared) throw mismatch();
        stop();
        node.fetchBodyChannelEnd(channel);
      };
      // undici refuses a bad port in mainFetch, before the body is touched;
      // the transport makes the same refusal, and the source is left alone.
      if (!(request.fetch_semantics && fetchBadPort(request.url))) pump().catch(fail);
    });
  }

  // The Fetch spec's bad ports, as undici's requestBadPort checks them
  // (oam_core http_client::prepare::BAD_PORTS is the same list).
  const FETCH_BAD_PORTS = new Set([
    1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101, 102,
    103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427, 465,
    512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990, 993,
    995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668,
    6669, 6679, 6697, 10080,
  ]);
  function fetchBadPort(href) {
    let url;
    try {
      url = new URL(href);
    } catch {
      return false;
    }
    return (url.protocol === "http:" || url.protocol === "https:") && url.port !== "" &&
      FETCH_BAD_PORTS.has(Number(url.port));
  }

  // One chunk of a streamed upload as bytes, with node's verdict on each
  // kind (measured on node v22.22.2): a string is its UTF-8, any typed array
  // or DataView its bytes as they lie in memory; an ArrayBuffer fails at the
  // socket write, and anything else at undici's Buffer.byteLength.
  function uploadChunk(value) {
    if (typeof value === "string") return new TextEncoder().encode(value);
    if (ArrayBuffer.isView(value)) {
      return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    }
    if (value instanceof ArrayBuffer) {
      const error = new TypeError(
        'The "chunk" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received an instance of ArrayBuffer',
      );
      error.code = "ERR_INVALID_ARG_TYPE";
      throw error;
    }
    // node's own error for it, from the same call.
    globalThis.Buffer?.byteLength(value);
    throw new TypeError("Received non-Uint8Array chunk");
  }

  // fetch's own request: node's fetch begins with `new Request(input, init)`
  // and sends what that Request holds, so a Request input is unwrapped field
  // by field with `init` winning, every constructor check applies to a plain
  // URL too (a GET with a body, a bad method, `mode: 'navigate'` ...), and
  // they run before anything else -- a bad URL beats an aborted signal.
  // Returns the url and the init oamFetch goes on with: the method, the
  // headers in their stored order, the body (taken: a Request input reads as
  // used, unless `init` brought its own body), the request's signal and
  // redirect mode, and the dispatcher.
  function fetchRequest(input, init) {
    const request = new OamRequest(input, init);
    const state = requestStates.get(request);
    const flat = {
      method: state.method,
      headers: state.headers._list.map(([name, value]) => [name, value]),
      signal: state.signal,
      redirect: state.redirect,
      dispatcher: state.dispatcher,
      // Read by fetchDefaultHeaders.
      mode: state.mode,
      cache: state.cache,
    };
    const body = bodyStates.get(request) ?? null;
    // A Blob that is not oam's own: its bytes are read before the request
    // goes out (oamFetch), as `flat.body` stays its stream until then.
    let collect = false;
    if (body !== null) {
      // The Request already put the body's content-type in its headers.
      //
      // Only a caller's stream is sent as a stream. Any other body has a
      // known length and can be sent again on a 307 or 308, and undici keeps
      // it so (its `source`) even once `.body`, `clone()` or
      // `new Request(request)` has given it a stream: node sends its
      // content-length and replays it on the redirect. The stream `.body`
      // made over the bytes counts for nothing unless someone has read from
      // it, which the Request constructor has already refused.
      const untouched =
        body.stream === null || (!body.stream.locked && body.stream._disturbed !== true);
      if (body.kind !== "stream" && body.bytes !== null && untouched) {
        flat.body = body.text ?? body.bytes;
      } else if (body.stream !== null) {
        flat.body = body.stream;
        flat.duplex = "half";
        // undici sends a Blob's size as its content-length, and reads it
        // again for a redirect.
        if (body.kind === "blob" && untouched) collect = { size: body.size };
      } else {
        flat.body = body.text ?? body.bytes;
      }
      bodyStates.set(request, null);
    }
    // oam's own knobs (`__oamBodyStream` and the like, which tests and
    // internal callers set) ride along untouched.
    for (const key of Object.keys(init ?? {})) {
      if (key.startsWith("__oam")) flat[key] = init[key];
    }
    return { url: state.url, init: flat, collect };
  }

  // A foreign Blob's bytes, read off its stream: undici sends them with the
  // Blob's `size` as the content-length (measured on node v22.22.2), each
  // chunk converted as a streamed upload's is, and a stream that comes out
  // a different length fails as undici's length check fails it.
  //
  // The fetch's `signal` is honoured while the bytes are read: an abort
  // rejects at once with its reason, stops reading the Blob (its reader is
  // cancelled with that reason) and sends nothing, as node's fetch does
  // (measured on v22.22.2: an abort 50 ms into a Blob whose stream takes
  // 200 ms rejects with the reason at once, and no request reaches the
  // server).
  async function collectBlobStream(stream, size, signal) {
    const reader = stream.getReader();
    const chunks = [];
    const reasonOf = () =>
      signal.reason ?? new globalThis.DOMException("This operation was aborted", "AbortError");
    let onAbort = null;
    const aborted = signal
      ? new Promise((_resolve, reject) => {
          onAbort = () => reject(reasonOf());
          signal.addEventListener("abort", onAbort, { once: true });
        })
      : null;
    // Never left unhandled: it settles only on an abort, and is raced below.
    aborted?.catch(() => {});
    try {
      for (;;) {
        let step;
        try {
          step = aborted ? await Promise.race([reader.read(), aborted]) : await reader.read();
        } catch (e) {
          if (signal?.aborted) {
            reader.cancel(signal.reason).catch(() => {});
            throw reasonOf();
          }
          throw new TypeError("fetch failed", { cause: e });
        }
        const { value, done } = step;
        if (done) break;
        try {
          chunks.push(uploadChunk(value));
        } catch (e) {
          throw new TypeError("fetch failed", { cause: e });
        }
      }
    } finally {
      if (onAbort) signal.removeEventListener("abort", onAbort);
    }
    const bytes = concatBytes(chunks);
    if (bytes.length !== Number(size)) {
      // undici's class (#177), as for a caller content-length that disagrees.
      throw new TypeError("fetch failed", { cause: new undiciErrors.RequestContentLengthMismatchError() });
    }
    return bytes;
  }

  // The request headers undici's fetch adds of its own (fetch/index.js
  // httpNetworkOrCacheFetch, and core/request.js for the length), each only
  // when the caller did not set it -- bar `sec-fetch-mode`, which is always
  // the request's mode (#178). Measured on node v22.22.2 against a raw
  // socket. Fetch only: http.request and undici.request send none of them.
  // Where `host` goes and oam's `user-agent` are left as they are.
  const PAYLOAD_METHODS = new Set(["POST", "PUT", "PATCH", "QUERY", "PROPFIND", "PROPPATCH"]);
  const CONDITIONAL_HEADERS = [
    "if-modified-since",
    "if-none-match",
    "if-unmodified-since",
    "if-match",
    "if-range",
  ];
  function fetchDefaultHeaders(headers, request, init, upload) {
    const has = (name) => headers.some((h) => h[0].toLowerCase() === name);
    const add = (name, value) => {
      if (!has(name)) headers.push([name, value]);
    };
    // A caller's `connection` (close or keep-alive, lowercased by
    // dispatchHeader) wins; node writes its own right after `host`.
    if (!has("connection")) headers.unshift(["connection", "keep-alive"]);
    add("accept", "*/*");
    add("accept-language", "*");
    const mode = headers.findIndex((h) => h[0].toLowerCase() === "sec-fetch-mode");
    if (mode !== -1) headers.splice(mode, 1);
    headers.push(["sec-fetch-mode", init.mode ?? "cors"]);
    // A conditional request in the default cache mode is a no-store one
    // (undici, and the Fetch Standard's HTTP-network-or-cache fetch step 15),
    // so it says `pragma: no-cache` and `cache-control: no-cache`.
    let cache = init.cache;
    if (cache === "default" && CONDITIONAL_HEADERS.some(has)) cache = "no-store";
    if (cache === "no-store" || cache === "reload") {
      add("pragma", "no-cache");
      add("cache-control", "no-cache");
    } else if (cache === "no-cache") {
      add("cache-control", "max-age=0");
    }
    // A range request asks for the bytes as they are, so its offsets are not
    // into a compressed copy: undici appends `identity` to accept-encoding
    // (step 18), to a caller's own value too (`gzip` goes out as
    // `gzip, identity`), and the scheme's default list then stays out.
    if (has("range")) {
      const at = headers.findIndex((h) => h[0].toLowerCase() === "accept-encoding");
      if (at === -1) headers.push(["accept-encoding", "identity"]);
      else headers[at] = [headers[at][0], `${headers[at][1]}, identity`];
    }
    // undici's own list, per scheme: oam decodes br as well (#151).
    add("accept-encoding", /^https:/i.test(request.url) ? "br, gzip, deflate" : "gzip, deflate");
    // A request whose method carries a payload says it carries none: no
    // body, or one whose length is 0 (a stream's length is not known).
    // undici compares the method as written, so `patch` gets none.
    if (PAYLOAD_METHODS.has(request.method) && upload === null) {
      const empty =
        request.body_base64 !== undefined
          ? request.body_base64 === ""
          : request.body !== undefined
            ? request.body === ""
            : request.body_stream === undefined;
      if (empty) add("content-length", "0");
    }
  }

  async function oamFetch(input, init, rawPayload) {
    // Internal callers that are not fetch in node (http.request, the http2
    // client, undici.request) opt out of every Fetch-level rule below.
    const fetchSemantics = init?.__oamFetchSemantics !== false;
    let collect = false;
    if (fetchSemantics) ({ url: input, init, collect } = fetchRequest(input, init));
    init = init || {};
    const signal = init.signal;
    // Already-aborted: reject before touching the network (spec).
    if (signal?.aborted) {
      throw signal.reason ?? new globalThis.DOMException("This operation was aborted", "AbortError");
    }
    if (collect) init.body = await collectBlobStream(init.body, collect.size, signal);
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
    // fetch's `redirect`, validated by the Request constructor.
    const redirectMode = fetchSemantics ? init.redirect : undefined;
    const rawUrl = wellFormed(input);
    // The Request constructor has parsed the URL and refused userinfo (node's
    // `Request cannot be constructed from a URL that includes credentials`;
    // the http.request path keeps turning userinfo into Basic credentials,
    // as there it IS node's documented `auth` option). What is left is the
    // scheme, which node checks when it fetches.
    if (fetchSemantics && !/^https?:/i.test(rawUrl)) {
      throw new TypeError("fetch failed", { cause: new Error("unknown scheme") });
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
          // In the order the caller gave them, as undici writes them.
          list = combined._list;
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
            // fetch wraps it; `undici.request` rejects with the undici
            // error itself (measured on node v22.22.2 + undici 6.24.1).
            const cause = new undiciErrors[verdict.refuse[0]](verdict.refuse[1]);
            throw fetchSemantics ? new TypeError("fetch failed", { cause }) : cause;
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
      // undici's response-head count and refusal (fetch, undici.request).
      dispatch_semantics: dispatchSemantics,
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
    // ... and, the same way, exactly what was written and exactly what came
    // back: node's http client adds no `accept` / `user-agent` /
    // `accept-encoding` to a request and decodes no response body -- a
    // `content-encoding: gzip` response is the gzip bytes, with its
    // `content-encoding` and `content-length`, for the program to decode.
    // The Fetch client's negotiation is fetch()'s alone (and, for now,
    // undici.request's, which shares its entry).
    if (rawPayload && init.__oamRawExchange === true) {
      request.decode = false;
      request.default_headers = false;
    }
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
    // http.request's sent signal: fired once the transport has a connection
    // for the request, which is when node's 'finish' is due (#193).
    if (typeof init.__oamSentSignal === "number") {
      request.sent_signal = init.__oamSentSignal;
    }
    // undici.request's streamed body: its head waits for the first chunk
    // (fetchUpload says why).
    if (init.__oamDeferHead !== undefined && init.__oamDeferHead !== null) {
      request.defer_head = { empty_content_length: init.__oamDeferHead.emptyContentLength === true };
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
    if (typeof lookup === "function") {
      request.lookup_hook = true;
      // The pool the connections come from: the dispatcher's own, reused
      // across its fetches as undici's Agent reuses its sockets (the hook
      // runs once per new connection), or, with no dispatcher, the one
      // global fetch's default dispatcher would have. http.request never
      // gets here with a hook (a replaced dns.lookup sends it over an
      // agent's socket), and keeps a pool of its own if it ever does; so
      // does a fetch through a dispatcher already closed, whose pool is gone
      // and would never be closed again.
      if (!rawPayload && dispatcher?.closed !== true) {
        const id = dispatcher?._oamPoolId;
        request.lookup_pool = typeof id === "number" ? id : 0;
      }
    }
    // What the handshake with an https origin offers by ALPN, as each of
    // node's clients offers it (measured on v22.22.2 against an allowHTTP1
    // http2 server, #176): undici -- fetch() and undici.request -- offers
    // `http/1.1` alone (the transport's default) and so never speaks HTTP/2,
    // unless the dispatcher has `allowH2`, which adds `h2`; http.request's
    // https.Agent offers nothing at all. oam used to offer `h2, http/1.1` for
    // every one of them, so an h2-capable origin served fetch -- and
    // https.request, whose response still said HTTP/1.1 -- over HTTP/2.
    if (rawPayload) {
      if (init.__oamOfferNoAlpn === true) request.alpn = "none";
    } else if (dispatcher?._oamAllowH2 === true) {
      request.alpn = "allow_h2";
    }
    // undici gives a connection 10 s to be connected (for https, handshaken)
    // and then fails the request with its ConnectTimeoutError, where the
    // operating system would keep trying for 21 s (Windows) to two minutes
    // (Linux). The dispatcher's `connect.timeout` / `connectTimeout` replaces
    // the 10 s; 0 is no timeout. For everything that goes through undici in
    // node -- fetch() and undici.request -- and not for http.request or the
    // http2 client, which have no connect timeout there.
    if (dispatchSemantics) {
      const own = dispatcher?._oamConnectTimeout;
      request.connect_timeout_ms =
        own == null ? 10000 : typeof own === "number" && own > 0 ? own : 0;
    }
    // A `connect` FUNCTION is asked for every connection the fetch makes
    // (connector mode, which wins over the lookup hook), and a dispatcher oam
    // cannot run faithfully fails the fetch rather than being ignored. Not for
    // http.request's internal entry: node's http.request never goes through
    // an undici dispatcher.
    let connector = null;
    let policy = null;
    if (!rawPayload && dispatcher != null) {
      policy = dispatcherPolicy(dispatcher, holder, {
        url: rawUrl,
        headerNames: headers.map((h) => h[0]),
      });
      if (policy.refuse) throw new TypeError("fetch failed", { cause: policy.refuse });
      if (policy.connector) {
        connector = policy.connector;
        request.connect_hook = true;
      }
    }
    // undici's two per-phase limits, which every node fetch runs under: the
    // dispatcher's `headersTimeout` / `bodyTimeout` (the global one's when
    // none is passed), else undici's 300 s; 0 for none. `undici.request`
    // passes its own, already resolved. The transport runs both:
    // headersTimeout from the moment a connection has the request -- after
    // DNS, the connect and any TLS handshake or tunnel, and after a streamed
    // body has gone -- failing the fetch with UND_ERR_HEADERS_TIMEOUT and
    // closing that connection when the head is late; bodyTimeout on each read
    // of the body, failing it with UND_ERR_BODY_TIMEOUT. http.request (the
    // raw entry) has node's own timeouts instead.
    if (!rawPayload) {
      let headersTimeout = init.__oamHeadersTimeout;
      let bodyTimeout = init.__oamBodyTimeout;
      if (headersTimeout === undefined) {
        headersTimeout = policy?.headersTimeout ?? 300e3;
        bodyTimeout = policy?.bodyTimeout ?? 300e3;
      }
      if (headersTimeout > 0) request.headers_timeout_ms = headersTimeout;
      if (bodyTimeout > 0) request.body_timeout_ms = bodyTimeout;
    }
    // Internal escape hatch: a request whose body is produced over time
    // rides an outbound body channel instead of a materialized body
    // (docs/design/streaming-bodies.md). Not part of the WHATWG surface --
    // http.ClientRequest sets it.
    //
    // A ReadableStream body (or any async iterable) rides the same channel:
    // `upload` is pumped into it once the request has started.
    let upload = null;
    if (typeof init.__oamBodyStream === "number") {
      request.body_stream = init.__oamBodyStream;
      // undici.request frames a streamed body of unknown length chunked
      // whatever the method; the transport, asked nothing, sends a GET, HEAD
      // or CONNECT with no body at all (hyper's rule).
      if (init.__oamChunked === true) headers.push(["transfer-encoding", "chunked"]);
      // An iterable, not a Readable: undici follows a redirect with it.
      if (init.__oamIterableBody === true) request.iterable_body = true;
    } else if (init.body != null) {
      let impliedType;
      if (typeof init.body === "string") {
        request.body = wellFormed(init.body);
        if (fetchSemantics) impliedType = "text/plain;charset=UTF-8";
      } else {
        const body = extractBody(init.body, false);
        if (body.stream !== null) {
          // node refuses a streamed body without `duplex: 'half'`. Fetch
          // only: undici.request takes a stream as it is.
          if (fetchSemantics && init.duplex !== "half") {
            throw new TypeError("RequestInit: duplex option is required when sending a body.");
          }
          upload = body.stream;
        } else {
          request.body_base64 = bytesToBase64(body.bytes);
        }
        // undici.request, which is not fetch, names the type of the two
        // bodies that carry one of their own -- a FormData's boundary is in
        // it -- and of nothing else.
        if (fetchSemantics || body.kind === "formdata" || body.kind === "blob") {
          impliedType = body.type;
        }
      }
      // node's "extract a body": the body's own Content-Type unless the
      // caller set one (measured) -- `text/plain;charset=UTF-8` for a string.
      // Servers branch on it, and oam sent none at all.
      if (
        typeof impliedType === "string" &&
        !headers.some((h) => h[0].toLowerCase() === "content-type")
      ) {
        headers.push(["content-type", impliedType]);
      }
    }
    if (fetchSemantics) fetchDefaultHeaders(headers, request, init, upload);
    // A caller `content-length` that disagrees with the body is refused, not
    // framed. hyper writes exactly the declared length, so a short one
    // SILENTLY TRUNCATED the body and still returned 200 -- data loss, and
    // the classic CL desync primitive if anything downstream re-frames.
    // node never dispatches either shape: a long one rejects with
    // `RequestContentLengthMismatchError: Request body length does not match
    // content-length header`, a short one hangs until its timeout (measured).
    // oam rejects both with node's long-form error.
    // A streamed body's declared length, checked as it is sent (fetchUpload).
    let streamedLength = null;
    if (dispatchSemantics) {
      const declared = headers.find((h) => h[0].toLowerCase() === "content-length");
      if (declared !== undefined && upload !== null) {
        const want = Number(declared[1]);
        if (!Number.isInteger(want) || want < 0) {
          throw new TypeError("fetch failed", { cause: new undiciErrors.RequestContentLengthMismatchError() });
        }
        streamedLength = want;
      }
      if (declared !== undefined) {
        const want = Number(declared[1]);
        const have =
          request.body_base64 !== undefined
            ? atob(request.body_base64).length
            : request.body !== undefined
              ? new TextEncoder().encode(request.body).length
              : request.body_stream !== undefined || upload !== null
                ? null
                : 0;
        if (have !== null && (!Number.isInteger(want) || want < 0 || want !== have)) {
          // No message argument, as undici constructs it: the class's
          // default, which makes `message` the last own name.
          const cause = new undiciErrors.RequestContentLengthMismatchError();
          throw fetchSemantics ? new TypeError("fetch failed", { cause }) : cause;
        }
      }
    }
    // Started synchronously: a malformed request or a --permission refusal
    // throws from here, as it always has. The id is what cancels it: while
    // the fetch has no response head, `cancel()` takes the request off the
    // wire -- the connection closes (an h2 stream resets), so the server sees
    // the client leave, as node's does. After the head it does nothing (the
    // body has its own cancel), and a fetch parked on its lookup hook or
    // connector is dropped by the abandon in settleRaw.
    const internal = globalThis.__oam;
    const cancelId = ++fetchCancelIds;
    const cancel = () => internal.fetchCancel(cancelId);
    if (rawPayload) {
      const pending = internal.fetch(JSON.stringify(request), cancelId);
      // http.ClientRequest cancels on abort() / destroy().
      if (typeof init.__oamCanceller === "function") init.__oamCanceller(cancel);
      return settleRaw(pending, lookup, signal, connector);
    }
    const dispatch = (onBodyOver) =>
      settleFetch(internal.fetch(JSON.stringify(request), cancelId), lookup, signal, connector, fetchSemantics, onBodyOver);
    // A streamed body goes out as it is produced, its head with its first
    // chunk (fetchUpload); any other is sent with the request.
    const op = upload === null ? dispatch() : fetchUpload(upload, request, streamedLength, signal, dispatch);
    if (!signal) return op;
    // Race the abort: it rejects the fetch with the reason at once, and
    // cancels the request. The cancelled op then fails, into a race that is
    // already decided. One listener, as node leaves one per fetch.
    return Promise.race([
      op,
      new Promise((_resolve, reject) => {
        signal.addEventListener(
          "abort",
          () => {
            reject(
              signal.reason ?? new globalThis.DOMException("This operation was aborted", "AbortError"),
            );
            cancel();
          },
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
      headers: makeHeaders(meta.headers),
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
    if (response?.headers instanceof Headers) {
      // The stored order -- the order the handler set them -- not the sorted
      // order iteration gives a script.
      for (const [key, value] of response.headers._list) headerPairs.push([key, value]);
    } else if (response?.headers) {
      response.headers.forEach((value, key) => headerPairs.push([key, value]));
    }
    const headersJson = JSON.stringify(headerPairs);
    // A constructed Response whose body is known bytes is written whole,
    // with a content-length; asking for its `.body` here would turn every
    // such answer into a stream. Anything else -- a fetched response passed
    // through, a plain object -- is read as before.
    const state = bodyStates.get(response);
    const body =
      state === undefined
        ? (response?.body ?? response?._body ?? null)
        : state === null || state.used
          ? null
          : (state.stream ?? state.bytes);
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

  // The event a WebSocket that fails to connect dispatches. Node v22 has the
  // class (undici's) and no `ErrorEvent` global, so there is none here
  // either: it is reachable only as an event's constructor.
  // Its five attributes are prototype getters, as undici's are. The Event
  // base still puts its own state (type, bubbles, target, ...) on every
  // instance as own enumerable properties, so Object.keys() of any oam
  // event, this one included, is not node's empty list.
  const kErrorEventInit = Symbol("kErrorEventInit");
  class ErrorEvent extends Event {
    constructor(type, init) {
      // undici's webidl check. Event's own would never see it: the super()
      // call below always passes two arguments.
      if (arguments.length === 0) {
        throw new TypeError("ErrorEvent constructor: 1 argument required, but 0 found.");
      }
      super(type, init);
      Object.defineProperty(this, kErrorEventInit, {
        value: {
          message: (init && init.message) || "",
          filename: (init && init.filename) || "",
          lineno: (init && init.lineno) || 0,
          colno: (init && init.colno) || 0,
          error: init ? init.error : undefined,
        },
      });
    }
    get message() { return this[kErrorEventInit].message; }
    get filename() { return this[kErrorEventInit].filename; }
    get lineno() { return this[kErrorEventInit].lineno; }
    get colno() { return this[kErrorEventInit].colno; }
    get error() { return this[kErrorEventInit].error; }
  }
  brand(ErrorEvent, "ErrorEvent");

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
        () => {
          this._readyState = CLOSED;
          // node (undici's failWebsocketConnection): an ErrorEvent whose
          // `message` is this fixed text and whose `error` is an Error
          // carrying it -- whatever failed, a refused connect or an answer
          // that is not a 101. It was a bare Event, with neither (#161).
          const message = "Received network error or non-101 status code.";
          const ev = new ErrorEvent("error", { error: new Error(message), message });
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
    if (fields.syscall === "listen") {
      // A server's listen error is node's uvExceptionWithHostPort, which
      // sets the code before the errno (measured on v22.22.2: keys code,
      // errno, syscall, address, port); every other is errno first.
      err.code = fields.code;
      if (fields.errno !== undefined) err.errno = fields.errno;
    } else {
      if (fields.errno !== undefined) err.errno = fields.errno;
      err.code = fields.code;
    }
    if (fields.syscall !== undefined) err.syscall = fields.syscall;
    if (fields.path !== undefined) err.path = fields.path;
    if (fields.dest !== undefined) err.dest = fields.dest;
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

  // node's coded errors (lib/internal/errors.js makeNodeErrorWithCode):
  // between an instance and Base.prototype sits one prototype per code,
  // whose `constructor` answers Base -- err.constructor.name is "Error" /
  // "TypeError" / "RangeError" -- whose toString renders
  // "Name [CODE]: message", and which is [kIsNodeError] -- what makes
  // prepareStackTrace below render the stack header as node's
  // `${name} [${code}]: ${message}` on the stack's first read (measured on
  // v22.22.2). The one registry for both kinds
  // of coded error oam raises -- node_compat.js's `codes` and the vendored
  // streams' internal/errors -- so two errors with one code share a
  // prototype whichever raised them. Made on first use of a code.
  const nodeErrorPrototypes = new Map(); // Base -> Map(code -> prototype)
  function nodeErrorPrototype(Base, code) {
    let byCode = nodeErrorPrototypes.get(Base);
    if (byCode === undefined) {
      byCode = new Map();
      nodeErrorPrototypes.set(Base, byCode);
    }
    let proto = byCode.get(code);
    if (proto === undefined) {
      class NodeError extends Base {
        toString() {
          return `${this.name} [${code}]: ${this.message}`;
        }

        get ["constructor"]() {
          return Base;
        }

        get [kIsNodeError]() {
          return true;
        }
      }
      proto = NodeError.prototype;
      byCode.set(code, proto);
    }
    return proto;
  }
  // The codes node declares with a message FUNCTION
  // (`E('ERR_X', (a, b) => ..., Base)`): node's NodeError sets `code` (a
  // class field) before the constructor body defines `message`, so their
  // own keys are stack, code, message -- and whatever the function sets on
  // `this` sits between the two. Every other code has a string message
  // (fixed, or a `%s` format), built with `super(message)`: stack, message,
  // code. Node's table, not something to derive: measured on v22.22.2 with
  // --expose-internals, by constructing every code oam names and reading its
  // own keys. Shared by both factories, node_compat.js's E() and the vendored
  // streams' makeCode, so an error of one code has one shape whichever
  // raised it.
  const functionMessageCodes = new Set([
    "ERR_ACCESS_DENIED",
    "ERR_BUFFER_OUT_OF_BOUNDS",
    "ERR_FALSY_VALUE_REJECTION",
    "ERR_HTTP2_STREAM_CANCEL",
    "ERR_INVALID_ADDRESS_FAMILY",
    "ERR_INVALID_ARG_TYPE",
    "ERR_INVALID_ARG_VALUE",
    "ERR_INVALID_CHAR",
    "ERR_INVALID_FILE_URL_PATH",
    "ERR_INVALID_RETURN_VALUE",
    "ERR_INVALID_URL",
    "ERR_INVALID_URL_SCHEME",
    "ERR_MISSING_ARGS",
    "ERR_MODULE_NOT_FOUND",
    "ERR_OUT_OF_RANGE",
    "ERR_PACKAGE_IMPORT_NOT_DEFINED",
    "ERR_PACKAGE_PATH_NOT_EXPORTED",
    "ERR_SOCKET_BAD_PORT",
    "ERR_TLS_CERT_ALTNAME_INVALID",
    "ERR_UNHANDLED_ERROR",
    "ERR_UNSUPPORTED_DIR_IMPORT",
    "ERR_UNSUPPORTED_ESM_URL_SCHEME",
  ]);
  const setHas = Set.prototype.has;
  Object.defineProperty(globalThis, "__oamNodeErrorCodeFirst", {
    value: (code) => setHas.call(functionMessageCodes, code),
    writable: false,
    enumerable: false,
    configurable: false,
  });
  // The brand itself, for Error.prepareStackTrace below: node renders a
  // kIsNodeError error's stack header from name, code and message.
  Object.defineProperty(globalThis, "__oamKIsNodeError", {
    value: kIsNodeError,
    writable: false,
    enumerable: false,
    configurable: false,
  });
  Object.defineProperty(globalThis, "__oamNodeErrorPrototype", {
    value: nodeErrorPrototype,
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
  // node's defaultPrepareStackTrace header (lib/internal/errors.js): for a
  // kIsNodeError error `${name} [${code}]: ${message}`, read when the stack
  // is first rendered (so a message or code set before that shows); for any
  // other the intrinsic Error.prototype.toString, never the error's own
  // toString -- a class that overrides toString does not change its stack,
  // and AssertionError, whose name carries the code while its stack is
  // rendered, does not get the code twice. Captured here, before user code.
  const kIsNodeError = globalThis.__oamKIsNodeError;
  const errorToString = Error.prototype.toString;
  Error.prepareStackTrace = function (err, frames) {
    let head;
    try {
      head =
        err !== null && typeof err === "object" && kIsNodeError in err
          ? `${err.name} [${err.code}]: ${err.message}`
          : errorToString.call(err);
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
