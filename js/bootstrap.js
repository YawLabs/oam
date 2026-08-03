// oam bootstrap: the JS half of the runtime surface. Evaluated at context
// creation today; compiled into the startup snapshot once that pipeline
// lands (same source, faster boot).
//
// M1 fetch subset: buffered bodies (no ReadableStream yet), plain-object
// Response/Headers shapes (real spec classes arrive with oam_web + WPT).
// Wire contract with crates/oam_core ops::fetch:
//   request:  JSON string {url, method, headers: [[k,v]], body}
//   response: {status, statusText, url, headers: [[k,v]], body}
// SNAPSHOT CONSTRAINT: this file is evaluated at BUILD time into the V8
// startup snapshot, where no native bindings exist. Anything from __oam
// must be looked up at CALL time, never captured at eval time.
"use strict";
(() => {

  // Minimal DOMException (AbortError/TimeoutError carriers) — defined
  // first so the abort primitives below can throw it.
  if (typeof globalThis.DOMException !== "function") {
    class DOMException extends Error {
      constructor(message = "", name = "Error") {
        super(message);
        this.name = name;
      }
    }
    globalThis.DOMException = DOMException;
  }

  // ----------------------------------------------- Event / EventTarget
  // The DOM event primitives AbortController is built on. Minimal but
  // spec-shaped: once listeners, stopImmediatePropagation, dispatchEvent
  // returning !defaultPrevented.
  class Event {
    constructor(type, init = {}) {
      this.type = String(type);
      this.bubbles = init.bubbles === true;
      this.cancelable = init.cancelable === true;
      this.defaultPrevented = false;
      this.target = null;
      this.currentTarget = null;
      this._stopImmediate = false;
      this.timeStamp = 0;
    }
    preventDefault() {
      if (this.cancelable) this.defaultPrevented = true;
    }
    stopPropagation() {}
    stopImmediatePropagation() {
      this._stopImmediate = true;
    }
  }
  globalThis.Event = Event;

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
      const list = this._listeners.get(type) ?? [];
      if (!list.some((e) => e.fn === listener)) {
        list.push({ fn: listener, once, resist });
        this._listeners.set(type, list);
      }
    }
    removeEventListener(type, listener) {
      const list = this._listeners.get(type);
      if (list) this._listeners.set(type, list.filter((e) => e.fn !== listener));
    }
    dispatchEvent(event) {
      event.target = this;
      event.currentTarget = this;
      const list = (this._listeners.get(event.type) ?? []).slice();
      for (const entry of list) {
        // Resist-marked listeners (Node's kResistStopPropagation) still run
        // after stopImmediatePropagation; everything else is skipped.
        if (event._stopImmediate && !entry.resist) continue;
        if (entry.once) this.removeEventListener(event.type, entry.fn);
        const handler = typeof entry.fn === "function" ? entry.fn : entry.fn.handleEvent;
        handler.call(this, event);
      }
      return !event.defaultPrevented;
    }
  }
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
  globalThis.AbortSignal = AbortSignal;

  class AbortController {
    constructor() {
      this.signal = new AbortSignal();
    }
    abort(reason) {
      this.signal._fire(reason);
    }
  }
  globalThis.AbortController = AbortController;

  // Headers (Fetch-standard subset): case-insensitive, repeated values
  // combine per the comma rule, iterable. Shared by fetch responses,
  // server requests, and the Response constructor.
  class Headers {
    constructor(init) {
      this._map = new Map();
      if (init === undefined || init === null) return;
      if (init instanceof Headers) {
        for (const [k, v] of init) this._map.set(k, v);
      } else if (typeof init[Symbol.iterator] === "function" && typeof init !== "string") {
        for (const pair of init) this.append(pair[0], pair[1]);
      } else {
        for (const key of Object.keys(init)) this.append(key, init[key]);
      }
    }
    append(name, value) {
      const key = String(name).toLowerCase();
      const text = String(value);
      this._map.set(key, this._map.has(key) ? `${this._map.get(key)}, ${text}` : text);
    }
    set(name, value) {
      this._map.set(String(name).toLowerCase(), String(value));
    }
    get(name) {
      const value = this._map.get(String(name).toLowerCase());
      return value === undefined ? null : value;
    }
    has(name) {
      return this._map.has(String(name).toLowerCase());
    }
    delete(name) {
      this._map.delete(String(name).toLowerCase());
    }
    forEach(fn, thisArg) {
      for (const [key, value] of this._map) fn.call(thisArg, value, key, this);
    }
    *entries() {
      yield* this._map.entries();
    }
    *keys() {
      yield* this._map.keys();
    }
    *values() {
      yield* this._map.values();
    }
    [Symbol.iterator]() {
      return this.entries();
    }
  }
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
        this._bytes = bytes;
        this._type = (options && typeof options.type === "string")
          ? options.type.toLowerCase() : "";
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
      get [Symbol.toStringTag]() { return "Blob"; }
    }
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
      get [Symbol.toStringTag]() { return "File"; }
    }
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
      get [Symbol.toStringTag]() { return "FormData"; }
    }
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
      get [Symbol.toStringTag]() { return "Request"; }
    }
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
      get [Symbol.toStringTag]() { return "BroadcastChannel"; }
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
    globalThis.BroadcastChannel = BroadcastChannel;
    globalThis.MessageEvent = globalThis.MessageEvent || MessageEvent;
  }

  function makeHeaders(pairs) {
    const headers = new Headers();
    for (const [name, value] of pairs) headers.append(name, value);
    return headers;
  }

  function makeResponse(raw) {
    const handle = raw.bodyHandle;
    let consumed = false;
    let bodyStream = null;

    // The body is a real ReadableStream over the wire handle: each pull is
    // one op, so chunks surface as the server flushes them (SSE / token
    // streaming). Lazy: responses whose body is never touched never spawn
    // a read op; the handle dies with the run's CoreRuntime.
    function ensureBody() {
      bodyStream ??= new ReadableStream({
        async pull(controller) {
          const chunk = await globalThis.__oam.fetchBodyRead(handle);
          if (chunk === undefined) controller.close();
          else controller.enqueue(chunk);
        },
        cancel() {
          globalThis.__oam.fetchBodyCancel(handle);
        },
      });
      return bodyStream;
    }

    async function drainBytes() {
      if (consumed) throw new TypeError("Body already consumed");
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

  globalThis.fetch = async function fetch(input, init) {
    init = init || {};
    const signal = init.signal;
    // Already-aborted: reject before touching the network (spec).
    if (signal?.aborted) {
      throw signal.reason ?? new globalThis.DOMException("This operation was aborted", "AbortError");
    }
    let headers = [];
    if (init.headers) {
      // Branch on iterability, not Array-ness: a Map (valid HeadersInit)
      // is not an Array, and Object.entries(map) is [] — auth headers were
      // silently dropped.
      const h = init.headers;
      headers =
        typeof h !== "string" && typeof h[Symbol.iterator] === "function"
          ? [...h].map(([k, v]) => [wellFormed(k), wellFormed(v)])
          : Object.entries(h).map(([k, v]) => [wellFormed(k), wellFormed(v)]);
    }
    const request = {
      url: wellFormed(input),
      method: init.method ? String(init.method).toUpperCase() : "GET",
      headers,
    };
    // Connection pin: an undici-style dispatcher (init.dispatcher) may carry a
    // connect.lookup hook used for DNS-rebind / SSRF pinning. oam owns the
    // transport (reqwest), so we resolve the hook HERE to an IP and pass it as
    // a pin: reqwest then dials that IP while keeping Host + TLS SNI = the
    // hostname. The dispatcher exposes the hook as `_oamConnectLookup` (set by
    // the oam:undici shim). No dispatcher / no hook = the normal path, no cost.
    const dispatcher = init.dispatcher;
    const connectLookup = dispatcher && dispatcher._oamConnectLookup;
    if (typeof connectLookup === "function") {
      let host = "";
      try {
        host = new globalThis.URL(request.url).hostname;
      } catch {
        host = "";
      }
      if (host) {
        const ip = await new Promise((resolve) => {
          let settled = false;
          const done = (value) => {
            if (!settled) {
              settled = true;
              resolve(value);
            }
          };
          try {
            connectLookup(host, { all: true }, (err, addrs) => {
              if (err) return done(null);
              if (Array.isArray(addrs) && addrs[0] && addrs[0].address) return done(addrs[0].address);
              if (typeof addrs === "string" && addrs) return done(addrs);
              done(null);
            });
          } catch {
            done(null);
          }
        });
        if (ip) request.pin = { host, ip };
      }
    }
    if (init.body != null) {
      if (init.body instanceof ArrayBuffer || ArrayBuffer.isView(init.body)) {
        const bytes = init.body instanceof ArrayBuffer
          ? new Uint8Array(init.body)
          : new Uint8Array(init.body.buffer, init.body.byteOffset, init.body.byteLength);
        let binary = "";
        for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
        request.body_base64 = btoa(binary);
      } else {
        request.body = wellFormed(init.body);
      }
    }
    const op = globalThis.__oam
      .fetch(JSON.stringify(request))
      .then(makeResponse, (e) => {
        // WHATWG: fetch() rejects with a TypeError on network failure.
        throw new TypeError(e && e.message ? e.message : String(e));
      });
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
  };

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
  globalThis.WebSocket = WebSocket;
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
