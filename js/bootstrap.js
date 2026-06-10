// oam bootstrap: the JS half of the runtime surface. Evaluated at context
// creation today; compiled into the startup snapshot once that pipeline
// lands (same source, faster boot).
//
// M1 fetch subset: buffered bodies (no ReadableStream yet), plain-object
// Response/Headers shapes (real spec classes arrive with oam_web + WPT).
// Wire contract with crates/oam_core ops::fetch:
//   request:  JSON string {url, method, headers: [[k,v]], body}
//   response: {status, statusText, url, headers: [[k,v]], body}
"use strict";
(() => {
  const core = globalThis.__oam;

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
    let consumed = false;
    function takeBody() {
      if (consumed) throw new TypeError("Body already consumed");
      consumed = true;
      return raw.body;
    }
    return {
      status: raw.status,
      statusText: raw.statusText,
      ok: raw.status >= 200 && raw.status <= 299,
      url: raw.url,
      headers: makeHeaders(raw.headers),
      get bodyUsed() {
        return consumed;
      },
      text: async () => takeBody(),
      json: async () => JSON.parse(takeBody()),
    };
  }

  globalThis.fetch = async function fetch(input, init) {
    init = init || {};
    let headers = [];
    if (init.headers) {
      headers = Array.isArray(init.headers)
        ? init.headers.map(([k, v]) => [String(k), String(v)])
        : Object.entries(init.headers).map(([k, v]) => [String(k), String(v)]);
    }
    const request = {
      url: String(input),
      method: init.method ? String(init.method).toUpperCase() : "GET",
      headers,
      body: init.body == null ? null : String(init.body),
    };
    const raw = await core.fetch(JSON.stringify(request));
    return makeResponse(raw);
  };
})();
