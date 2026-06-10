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

  function makeHeaders(pairs) {
    const map = new Map();
    for (const [name, value] of pairs) {
      const key = String(name).toLowerCase();
      // Repeated headers combine per the spec's comma rule.
      map.set(key, map.has(key) ? map.get(key) + ", " + String(value) : String(value));
    }
    return {
      get: (name) => {
        const value = map.get(String(name).toLowerCase());
        return value === undefined ? null : value;
      },
      has: (name) => map.has(String(name).toLowerCase()),
      forEach: (fn) => {
        for (const [key, value] of map) fn(value, key);
      },
    };
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
    let raw;
    try {
      raw = await globalThis.__oam.fetch(JSON.stringify(request));
    } catch (e) {
      // WHATWG: fetch() rejects with a TypeError on network failure.
      throw new TypeError(e && e.message ? e.message : String(e));
    }
    return makeResponse(raw);
  };
})();
