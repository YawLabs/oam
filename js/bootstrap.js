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
      const once = options === true ? false : options?.once === true;
      const list = this._listeners.get(type) ?? [];
      if (!list.some((e) => e.fn === listener)) {
        list.push({ fn: listener, once });
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
        if (entry.once) this.removeEventListener(event.type, entry.fn);
        const handler = typeof entry.fn === "function" ? entry.fn : entry.fn.handleEvent;
        handler.call(this, event);
        if (event._stopImmediate) break;
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
      body: init.body == null ? null : wellFormed(init.body),
    };
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
})();
