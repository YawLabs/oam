// undici -- oam ships a NATIVE undici-API shim, shadowing the npm package.
//
// Why a shim, not the real package: undici reimplements the HTTP/1.1+H2
// stack with an llhttp WASM parser and pulls node:sqlite, node:worker_threads,
// node:diagnostics_channel, etc. -- it neither loads nor adds anything over
// oam's web-standard fetch (reqwest+rustls). So `import 'undici'` resolves to
// this factory (registered as the virtual builtin "oam:undici"; the resolver
// maps the bare specifier 'undici' to it before the node_modules walk), the
// same approach Bun and Deno take.
//
// Surface: fetch, request, stream, Dispatcher/Agent/Pool/Client/BalancedPool,
// get/setGlobalDispatcher, errors, interceptors (no-op), and the web globals
// undici re-exports (Headers/Response/Request/FormData/fetch/...).
//
// Documented divergences (a shim over fetch cannot honor everything):
//  - A Dispatcher/Agent passed as `dispatcher` (undici) is accepted but its
//    connection-level hooks (connect.lookup, TLS opts, pooling) are NOT
//    applied -- oam's fetch owns the transport. Notably a custom
//    `connect.lookup` (used for DNS-rebind pinning) is a NO-OP; callers
//    relying on it for a security control must not assume it pins.
//  - Mock* (MockAgent/MockPool/...) are minimal stubs: constructible, but
//    they do not intercept requests.

(function oamUndiciModule(registry) {
  registry.factories["oam:undici"] = () => {
    const { Readable } = registry.get("stream");
    const EventEmitter = registry.get("events");

    const G = globalThis;

    // ---- errors -----------------------------------------------------------
    class UndiciError extends Error {
      constructor(message, code) {
        super(message);
        this.name = "UndiciError";
        this.code = code || "UND_ERR";
      }
    }
    function mkError(name, code) {
      return class extends UndiciError {
        constructor(message) {
          super(message || name, code);
          this.name = name;
          this.code = code;
        }
      };
    }
    const errors = {
      UndiciError,
      ConnectTimeoutError: mkError("ConnectTimeoutError", "UND_ERR_CONNECT_TIMEOUT"),
      HeadersTimeoutError: mkError("HeadersTimeoutError", "UND_ERR_HEADERS_TIMEOUT"),
      HeadersOverflowError: mkError("HeadersOverflowError", "UND_ERR_HEADERS_OVERFLOW"),
      BodyTimeoutError: mkError("BodyTimeoutError", "UND_ERR_BODY_TIMEOUT"),
      RequestContentLengthMismatchError: mkError("RequestContentLengthMismatchError", "UND_ERR_REQ_CONTENT_LENGTH_MISMATCH"),
      ResponseContentLengthMismatchError: mkError("ResponseContentLengthMismatchError", "UND_ERR_RES_CONTENT_LENGTH_MISMATCH"),
      RequestAbortedError: mkError("RequestAbortedError", "UND_ERR_ABORTED"),
      AbortError: mkError("AbortError", "UND_ERR_ABORTED"),
      InformationalError: mkError("InformationalError", "UND_ERR_INFO"),
      InvalidArgumentError: mkError("InvalidArgumentError", "UND_ERR_INVALID_ARG"),
      InvalidReturnValueError: mkError("InvalidReturnValueError", "UND_ERR_INVALID_RETURN_VALUE"),
      ClientDestroyedError: mkError("ClientDestroyedError", "UND_ERR_DESTROYED"),
      ClientClosedError: mkError("ClientClosedError", "UND_ERR_CLOSED"),
      SocketError: mkError("SocketError", "UND_ERR_SOCKET"),
      NotSupportedError: mkError("NotSupportedError", "UND_ERR_NOT_SUPPORTED"),
      BalancedPoolMissingUpstreamError: mkError("BalancedPoolMissingUpstreamError", "UND_ERR_BPL_MISSING_UPSTREAM"),
      ResponseStatusCodeError: class ResponseStatusCodeError extends UndiciError {
        constructor(message, statusCode, headers, body) {
          super(message || "Response Status Code Error", "UND_ERR_RESPONSE_STATUS_CODE");
          this.name = "ResponseStatusCodeError";
          this.statusCode = statusCode;
          this.headers = headers;
          this.body = body;
        }
      },
      RequestRetryError: class RequestRetryError extends UndiciError {
        constructor(message, code, { headers, data } = {}) {
          super(message, "UND_ERR_REQ_RETRY");
          this.name = "RequestRetryError";
          this.statusCode = code;
          this.headers = headers;
          this.data = data;
        }
      },
      SecureProxyConnectionError: mkError("SecureProxyConnectionError", "UND_ERR_PRX_TLS"),
    };

    // ---- undici-shaped response body -------------------------------------
    // request().body is a Readable streaming the response bytes, plus the
    // undici body-mixin helpers, all consuming the same stream.
    function makeBodyReadable(webStream) {
      const reader = webStream && typeof webStream.getReader === "function" ? webStream.getReader() : null;
      const r = new Readable({
        read() {
          if (!reader) {
            this.push(null);
            return;
          }
          reader.read().then(
            ({ done, value }) => {
              if (done) this.push(null);
              else this.push(G.Buffer.from(value));
            },
            (err) => this.destroy(err instanceof Error ? err : new Error(String(err))),
          );
        },
      });
      const collect = async () => {
        const chunks = [];
        for await (const c of r) chunks.push(G.Buffer.isBuffer(c) ? c : G.Buffer.from(c));
        return G.Buffer.concat(chunks);
      };
      r.text = async () => (await collect()).toString("utf8");
      r.json = async () => JSON.parse((await collect()).toString("utf8"));
      r.arrayBuffer = async () => {
        const b = await collect();
        return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
      };
      r.bytes = async () => new Uint8Array(await collect());
      if (typeof G.Blob === "function") {
        r.blob = async () => new G.Blob([await collect()]);
      }
      r.dump = async () => {
        // Drain and discard (undici's body.dump()).
        for await (const _ of r) { /* discard */ }
      };
      return r;
    }

    function headersToObject(headers) {
      const out = { __proto__: null };
      if (headers && typeof headers.forEach === "function") {
        headers.forEach((value, key) => {
          out[key] = key in out ? out[key] + ", " + value : value;
        });
      }
      return out;
    }

    // ---- request() --------------------------------------------------------
    // undici.request(url, opts) -> { statusCode, headers, trailers, body,
    // opaque, context }. Backed by global fetch.
    async function request(url, opts) {
      if (typeof url === "object" && url !== null && !(url instanceof G.URL)) {
        // request({ origin, path, method, ... }) form.
        opts = url;
        const origin = opts.origin || "";
        url = String(origin).replace(/\/$/, "") + (opts.path || "/");
      }
      opts = opts || {};
      const init = {
        method: opts.method || "GET",
        headers: opts.headers || undefined,
        body: opts.body != null ? opts.body : undefined,
        signal: opts.signal || undefined,
        redirect: opts.redirect || (opts.maxRedirections > 0 ? "follow" : undefined),
      };
      // undici allows a `query` object appended to the URL.
      if (opts.query && typeof opts.query === "object") {
        const u = new G.URL(String(url));
        for (const [k, v] of Object.entries(opts.query)) u.searchParams.set(k, String(v));
        url = u.toString();
      }
      const res = await G.fetch(String(url), init);
      return {
        statusCode: res.status,
        headers: headersToObject(res.headers),
        trailers: { __proto__: null },
        opaque: opts.opaque ?? null,
        context: {},
        body: makeBodyReadable(res.body),
      };
    }

    // undici.stream(url, opts, factory): pipe the response into the writable
    // the factory returns. Returns a promise resolving when piping completes.
    async function stream(url, opts, factory) {
      if (typeof opts === "function") {
        factory = opts;
        opts = {};
      }
      const { statusCode, headers, body, trailers, opaque } = await request(url, opts);
      const writable = factory({ statusCode, headers, opaque, trailers });
      await new Promise((resolve, reject) => {
        body.on("error", reject);
        writable.on("error", reject);
        writable.on("finish", resolve);
        writable.on("close", resolve);
        body.pipe(writable);
      });
      return { statusCode, headers, opaque, trailers };
    }

    // ---- dispatchers ------------------------------------------------------
    // All dispatchers delegate to request() -- oam's fetch owns the transport,
    // so connection-level options (connect.lookup, pooling, TLS) are accepted
    // and stored but NOT applied. See the module-level divergence note.
    class Dispatcher extends EventEmitter {
      constructor(options) {
        super();
        this._options = options || {};
        this.destroyed = false;
        this.closed = false;
      }
      // request(opts, handler?) -- callback form is rare; support the
      // promise form (returns the request() result) which is what fetch and
      // most callers use.
      request(opts, handler) {
        const p = request(opts && opts.origin ? opts : opts, undefined);
        if (typeof handler === "function") {
          p.then(
            (data) => handler(null, data),
            (err) => handler(err, null),
          );
          return;
        }
        return p;
      }
      async close(cb) {
        this.closed = true;
        if (typeof cb === "function") queueMicrotask(cb);
      }
      async destroy(err, cb) {
        if (typeof err === "function") { cb = err; err = null; }
        this.destroyed = true;
        this.closed = true;
        if (typeof cb === "function") queueMicrotask(cb);
      }
      // Web-fetch dispatch entry undici exposes; not used by oam's fetch
      // (which dispatches itself), but present so feature-detection passes.
      dispatch() {
        throw new errors.NotSupportedError(
          "undici Dispatcher.dispatch() is not supported on oam -- use fetch() or request()",
        );
      }
    }

    class Agent extends Dispatcher {}

    // Origin-bound dispatchers: resolve opts.path against the origin.
    class Client extends Dispatcher {
      constructor(origin, options) {
        super(options);
        this.origin = typeof origin === "string" ? origin : (origin && origin.toString()) || "";
      }
      request(opts, handler) {
        const merged = { ...opts, origin: this.origin };
        return super.request(merged, handler);
      }
    }
    class Pool extends Client {}
    class BalancedPool extends Dispatcher {
      constructor(upstreams = [], options) {
        super(options);
        this.upstreams = Array.isArray(upstreams) ? upstreams : [upstreams];
      }
      request(opts, handler) {
        const origin = this.upstreams[0] || "";
        return super.request({ ...opts, origin }, handler);
      }
    }

    // ---- global dispatcher ------------------------------------------------
    let globalDispatcher = new Agent();
    function setGlobalDispatcher(d) {
      if (!d || typeof d.request !== "function") {
        throw new errors.InvalidArgumentError("Argument agent must implement Agent");
      }
      globalDispatcher = d;
    }
    function getGlobalDispatcher() {
      return globalDispatcher;
    }

    // ---- interceptors (no-op pass-throughs) -------------------------------
    const passThrough = () => (dispatch) => dispatch;
    const interceptors = {
      redirect: passThrough,
      retry: passThrough,
      dump: passThrough,
      dns: passThrough,
      cache: passThrough,
      responseError: passThrough,
    };

    // ---- minimal Mock* stubs (constructible, non-intercepting) -----------
    class MockAgent extends Dispatcher {
      get() { return new MockPool(); }
      enableNetConnect() {}
      disableNetConnect() {}
      assertNoPendingInterceptors() {}
      deactivate() {}
      activate() {}
    }
    class MockPool extends Dispatcher {
      intercept() {
        return {
          reply() { return this; },
          replyWithError() { return this; },
          persist() { return this; },
          times() { return this; },
          delay() { return this; },
        };
      }
    }
    class MockClient extends MockPool {}

    // ---- web globals undici re-exports -----------------------------------
    const mod = {
      fetch: (input, init) => G.fetch(input, init),
      request,
      stream,
      Dispatcher,
      Agent,
      Client,
      Pool,
      BalancedPool,
      setGlobalDispatcher,
      getGlobalDispatcher,
      errors,
      interceptors,
      MockAgent,
      MockPool,
      MockClient,
      // Web-standard re-exports (oam ships these as globals).
      Headers: G.Headers,
      Response: G.Response,
      Request: G.Request,
      FormData: G.FormData,
      File: G.File,
      Blob: G.Blob,
      WebSocket: G.WebSocket,
      MessageEvent: G.MessageEvent,
      // Origin helpers (no-ops; oam fetch resolves absolute URLs).
      getGlobalOrigin: () => undefined,
      setGlobalOrigin: () => {},
    };
    return mod;
  };
})(globalThis.__oamNode);
