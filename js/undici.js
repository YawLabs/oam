// undici -- oam ships a NATIVE undici-API shim, shadowing the npm package.
//
// Why a shim, not the real package: undici reimplements the HTTP/1.1+H2
// stack with an llhttp WASM parser and pulls node:sqlite, node:worker_threads,
// node:diagnostics_channel, etc. -- it neither loads nor adds anything over
// oam's web-standard fetch (oam's own hyper + rustls transport). So `import 'undici'` resolves to
// this factory (registered as the virtual builtin "oam:undici"; the resolver
// maps the bare specifier 'undici' to it before the node_modules walk), the
// same approach Bun and Deno take.
//
// Surface: fetch, request, stream, Dispatcher/Agent/Pool/Client/BalancedPool,
// buildConnector, get/setGlobalDispatcher, errors, interceptors (no-op), and
// the web globals undici re-exports (Headers/Response/Request/FormData/fetch/...).
//
// Supported transport control:
//  - A `connect` FUNCTION (`new Agent|Pool|Client({ connect(opts, cb) })`, a
//    custom connector) IS honored, as undici honors it: it is called with
//    undici's connector parameters ({ host, hostname, protocol, port,
//    servername, localAddress }) before every connection the request makes,
//    redirect hops included, and the request goes over the socket it hands
//    back and nowhere else -- a connector that refuses fails the request with
//    its error. A `connect` OBJECT carrying socket or TLS options (`ca`,
//    `checkServerIdentity`, `servername`, `rejectUnauthorized`, `family`,
//    `localAddress`, `socketPath`, ...) becomes such a function through
//    buildConnector, as in undici, so those options apply to the
//    net.connect / tls.connect it makes. An Agent's `factory` is honored the
//    same way: each origin's dispatcher from it decides that origin's
//    connections. (See the Dispatcher constructor's _oamConnect bridge and
//    globalThis.fetch's connector mode.)
//  - A Dispatcher/Agent with a `connect.lookup` hook IS honored, as undici
//    honors it: the hook is called before the fetch connects to a host name
//    -- the first request AND every redirect hop to another host -- and the
//    connection is pinned to the addresses it returns (Host header + TLS SNI
//    preserved; node's address filtering; never the environment proxy). A hook
//    error fails the fetch closed with that error as `cause`, never falling
//    back to system DNS. This makes the DNS-rebind / SSRF pin used by e.g.
//    @yawlabs/fetch-mcp a real control, not a no-op. (See the Dispatcher
//    constructor's _oamConnectLookup bridge and globalThis.fetch's lookup
//    continuation.) All five ways undici installs a dispatcher carry it, as
//    they do in node: fetch's `dispatcher` option, setGlobalDispatcher +
//    global fetch, undici.fetch, agent.request(), and
//    undici.request(url, { dispatcher }).
//
// Refused, never ignored: a dispatcher whose dispatch() is overridden (a
// subclass or a patched instance), one built with `interceptors`, and an
// object that is not one of this shim's dispatchers fail the request with
// NotSupportedError. oam runs a request itself rather than through
// dispatch(), so honouring anything that lives there is impossible, and
// dropping it could skip a policy the application put there.
//
// Documented divergences (a shim over fetch cannot honor everything):
//  - Other connection-level dispatcher options (connection pooling,
//    keep-alive tuning) are accepted but NOT applied -- oam's fetch owns the
//    transport beyond the connect hooks.
//  - Mock* (MockAgent/MockPool/...) are minimal stubs: constructible, but
//    they do not intercept requests.

(function oamUndiciModule(registry) {
  // The global dispatcher lives on a locked global, not in this closure:
  // globalThis.fetch is native and cannot reach a module-scope binding, and it
  // needs the dispatcher to find a connect.lookup hook installed with
  // setGlobalDispatcher. Same shape as bootstrap.js's __oamMakeSysError.
  // Created once per isolate, lazily, so a run that never imports undici pays
  // nothing and fetch's lookup falls through `undefined`.
  function globalDispatcherHolder() {
    var existing = globalThis.__oamUndiciDispatcher;
    if (existing !== undefined) return existing;
    var holder = { current: null };
    Object.defineProperty(globalThis, "__oamUndiciDispatcher", {
      value: holder,
      writable: false,
      enumerable: false,
      configurable: false,
    });
    return holder;
  }

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
        // The dispatcher carries the connect.lookup hook. undici enforces it
        // for request() too, not just fetch(): agent.request() and
        // undici.request(url, {dispatcher}) both consult it (measured on node
        // v22.22.2 + undici 6.24.1, where a refusing hook blocks all five
        // installation forms). Absent here, globalThis.fetch falls back to the
        // global dispatcher.
        dispatcher: opts.dispatcher || undefined,
        // undici.request is not fetch: node's has no Fetch-spec bad-port
        // block, so port 1 or 25 is dialled like any other, a caller `host`
        // header is SENT (fetch drops it) and the method is not normalised.
        __oamFetchSemantics: false,
        // It does share undici's dispatch-level header rules, because both
        // build the same internal Request: `transfer-encoding`, `keep-alive`,
        // `upgrade`, `expect` and a bad `connection` are refused, and a
        // `content-length` that disagrees with the body is refused rather
        // than framed (measured on node v22.22.2 + undici 6.24.1).
        __oamDispatchSemantics: true,
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

    // ---- connectors -------------------------------------------------------
    // undici's util.getServerName: the host without its port, '' for an IP
    // (not a valid server name, RFC 6066).
    function getServerName(host) {
      if (!host) return null;
      let name = String(host);
      if (name[0] === "[") {
        name = name.substring(1, name.indexOf("]"));
      } else {
        const idx = name.indexOf(":");
        if (idx !== -1) name = name.substring(0, idx);
      }
      return registry.get("net").isIP(name) ? "" : name;
    }

    // undici's buildConnector (lib/core/connect.js, 6.24.1): a connect
    // function over net.connect (http:) or tls.connect (https:) with the
    // given socket and TLS options, calling back once the socket is
    // connected (and for https, secured) or with the error that stopped it,
    // including undici's ConnectTimeoutError after `timeout` (10 s by
    // default). TLS sessions are not cached.
    function buildConnector(opts) {
      const { allowH2, maxCachedSessions, socketPath, timeout, session: customSession, ...rest } = opts || {};
      if (maxCachedSessions != null && (!Number.isInteger(maxCachedSessions) || maxCachedSessions < 0)) {
        throw new errors.InvalidArgumentError("maxCachedSessions must be a positive integer or zero");
      }
      const options = { path: socketPath, ...rest };
      const connectTimeout = timeout == null ? 10e3 : timeout;
      const h2 = allowH2 != null ? allowH2 : false;
      return function connect({ hostname, host, protocol, port, servername, localAddress, httpSocket }, callback) {
        let socket;
        if (protocol === "https:") {
          servername = servername || options.servername || getServerName(host) || null;
          port = port || 443;
          const tlsOptions = {
            highWaterMark: 16384,
            ...options,
            servername,
            localAddress,
            ALPNProtocols: h2 ? ["http/1.1", "h2"] : ["http/1.1"],
            port,
            host: hostname,
          };
          if (customSession) tlsOptions.session = customSession;
          if (httpSocket) tlsOptions.socket = httpSocket;
          socket = registry.get("tls").connect(tlsOptions);
        } else {
          if (httpSocket) throw new errors.InvalidArgumentError("httpSocket can only be sent on TLS update");
          port = port || 80;
          const netOptions = { highWaterMark: 64 * 1024, ...options, localAddress, port, host: hostname };
          if (netOptions.path === undefined) delete netOptions.path;
          socket = registry.get("net").connect(netOptions);
        }
        if (options.keepAlive == null || options.keepAlive) {
          const delay = options.keepAliveInitialDelay === undefined ? 60e3 : options.keepAliveInitialDelay;
          socket.setKeepAlive(true, delay);
        }
        let timer = null;
        const clearConnectTimeout = () => {
          if (timer !== null) {
            clearTimeout(timer);
            timer = null;
          }
        };
        if (connectTimeout) {
          timer = setTimeout(() => {
            timer = null;
            socket.destroy(new errors.ConnectTimeoutError(
              `Connect Timeout Error (attempted address: ${hostname}:${port}, timeout: ${connectTimeout}ms)`,
            ));
          }, connectTimeout);
          if (typeof timer.unref === "function") timer.unref();
        }
        socket.setNoDelay(true);
        socket.once(protocol === "https:" ? "secureConnect" : "connect", function () {
          queueMicrotask(clearConnectTimeout);
          if (callback) {
            const cb = callback;
            callback = null;
            cb(null, this);
          }
        });
        socket.on("error", function (err) {
          queueMicrotask(clearConnectTimeout);
          if (callback) {
            const cb = callback;
            callback = null;
            cb(err);
          }
        });
        return socket;
      };
    }

    // The `connect` keys the lookup route covers: a connect object with only
    // these keeps the pooled transport and its lookup hook. Any other key --
    // a socket or TLS option -- makes it a buildConnector connect function,
    // so the option is applied rather than dropped.
    const LOOKUP_ROUTE_KEYS = new Set([
      "lookup", "timeout", "keepAlive", "keepAliveInitialDelay", "maxCachedSessions", "allowH2",
    ]);

    // ---- dispatchers ------------------------------------------------------
    // All dispatchers delegate to request() -- oam's fetch owns the transport,
    // so pooling options are accepted and stored but NOT applied; the
    // connection policy in `connect` is honored by fetch() and request().
    // See the module-level notes.
    class Dispatcher extends EventEmitter {
      constructor(options) {
        super();
        this._options = options || {};
        this.destroyed = false;
        this.closed = false;
        const connect = this._options.connect;
        if (connect != null && typeof connect !== "function" && typeof connect !== "object") {
          throw new errors.InvalidArgumentError("connect must be a function or an object");
        }
        // Bridges for oam's globalThis.fetch, which looks them up on the
        // dispatcher a fetch rides (see policyOf):
        //  - `_oamConnect`, a connect FUNCTION, is asked for every connection
        //    the fetch makes and the request goes over the socket it hands
        //    back (connector mode);
        //  - `_oamConnectLookup`, a connect.lookup hook, is called for every
        //    host name the fetch connects to, and those connections are pinned
        //    to its addresses (Host + SNI preserved). This is how the
        //    DNS-rebind pin in e.g. @yawlabs/fetch-mcp becomes a REAL
        //    transport control instead of a no-op. The hook signature is
        //    Node's lookup(hostname, options, cb) with options
        //    { family, hints, all: true } and cb(null, [{address, family}]).
        this._oamConnect = null;
        this._oamConnectLookup = null;
        if (typeof connect === "function") {
          this._oamConnect = connect;
        } else if (connect && Object.keys(connect).some((key) => !LOOKUP_ROUTE_KEYS.has(key))) {
          this._oamConnect = buildConnector(connect);
        } else if (connect && typeof connect.lookup === "function") {
          this._oamConnectLookup = connect.lookup;
        }
        // Dispatch interceptors (undici's `interceptors` option) run inside
        // dispatch(), which oam does not use: a dispatcher that has them is
        // refused (policyOf) rather than run without them.
        const interceptors = this._options.interceptors;
        this._oamInterceptors = !!interceptors && typeof interceptors === "object" &&
          Object.keys(interceptors).some((key) => Array.isArray(interceptors[key]) && interceptors[key].length > 0);
      }
      // request(opts, handler?) -- callback form is rare; support the
      // promise form (returns the request() result) which is what fetch and
      // most callers use. THIS dispatcher is the one the request rides, so
      // its connect.lookup hook applies; dropping it here used to un-pin
      // every agent.request() call.
      request(opts, handler) {
        const p = request({ ...(opts || {}), dispatcher: this }, undefined);
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

    // An Agent's `factory(origin, options)` builds the dispatcher each origin's
    // requests go through, so that dispatcher's connection policy is the one
    // that applies (undici passes it the agent's options, `connect`
    // included). Honored as a connector: each connection asks the origin's
    // dispatcher (made once per origin) for its policy.
    class Agent extends Dispatcher {
      constructor(options) {
        super(options);
        const factory = this._options.factory;
        if (factory !== undefined && typeof factory !== "function") {
          throw new errors.InvalidArgumentError("factory must be a function.");
        }
        if (typeof factory === "function") {
          const { factory: _factory, maxRedirections: _maxRedirections, ...originOptions } = this._options;
          const byOrigin = new Map();
          const plain = buildConnector({});
          this._oamConnectLookup = null;
          this._oamConnect = function viaFactory(params, cb) {
            const origin = params.protocol + "//" + params.host;
            let dispatcher = byOrigin.get(origin);
            if (dispatcher === undefined) {
              dispatcher = factory(origin, originOptions);
              byOrigin.set(origin, dispatcher);
            }
            const policy = policyOf(dispatcher);
            if (policy.refuse) {
              cb(policy.refuse);
            } else if (policy.connector) {
              policy.connector.fn.call(policy.connector.self, params, cb);
            } else if (typeof dispatcher._oamConnectLookup === "function") {
              buildConnector({ lookup: dispatcher._oamConnectLookup })(params, cb);
            } else {
              plain(params, cb);
            }
          };
        }
      }
    }

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

    // The connection policy globalThis.fetch applies for a dispatcher (see
    // bootstrap.js dispatcherPolicy): `{ connector }` when its `connect` is a
    // function (or was built into one), `{}` for none, and `{ refuse }` for a
    // dispatcher oam cannot run as undici would -- its dispatch() overridden
    // (a subclass, a patched instance or prototype), interceptors, or an
    // object that is not one of these classes at all (a real undici's, a
    // hand-rolled `{ dispatch }`). undici would call that dispatch(); oam
    // sends requests itself, so it refuses them instead of skipping whatever
    // the dispatch() does.
    const shimDispatch = Dispatcher.prototype.dispatch;
    function notHonored(what) {
      return new errors.NotSupportedError(
        what + " is not supported on oam: oam sends the request itself and would skip it. " +
          "Put the connection policy in a `connect` function, which oam calls for every connection",
      );
    }
    function policyOf(dispatcher) {
      if (!(dispatcher instanceof Dispatcher)) {
        return { refuse: notHonored("A dispatcher that is not one of oam's undici classes") };
      }
      if (dispatcher.dispatch !== shimDispatch) {
        return { refuse: notHonored("A dispatcher that overrides dispatch()") };
      }
      if (dispatcher._oamInterceptors) {
        return { refuse: notHonored("A dispatcher with interceptors") };
      }
      if (typeof dispatcher._oamConnect === "function") {
        return { connector: { fn: dispatcher._oamConnect, self: dispatcher } };
      }
      return {};
    }

    // ---- global dispatcher ------------------------------------------------
    // The holder is a locked global so globalThis.fetch -- which is native and
    // knows nothing about this module -- can read it. node enforces a
    // dispatcher's connect.lookup hook for the GLOBAL dispatcher too: after
    // setGlobalDispatcher(agent), plain `fetch()` and `undici.fetch()` both
    // consult the hook (measured, node v22.22.2 + undici 6.24.1). Without this
    // bridge the hook was honoured only when the agent was passed as fetch's
    // `dispatcher` option, and the other four installation forms silently made
    // an UNPINNED request -- an SSRF guard that fails open.
    const holder = globalDispatcherHolder();
    if (holder.policy === undefined) {
      Object.defineProperty(holder, "policy", {
        value: policyOf,
        writable: false,
        enumerable: false,
        configurable: false,
      });
    }
    holder.current = new Agent();
    function setGlobalDispatcher(d) {
      if (!d || typeof d.request !== "function") {
        throw new errors.InvalidArgumentError("Argument agent must implement Agent");
      }
      holder.current = d;
    }
    function getGlobalDispatcher() {
      return holder.current;
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
      buildConnector,
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
