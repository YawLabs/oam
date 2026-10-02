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
// ProxyAgent/EnvHttpProxyAgent, buildConnector, get/setGlobalDispatcher,
// errors, interceptors (no-op), and the web globals undici re-exports
// (Headers/Response/Request/FormData/fetch/...).
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
// subclass or a patched instance), one built with `interceptors`, an object
// that is not one of this shim's dispatchers, and MockAgent / MockPool /
// MockClient fail with NotSupportedError. oam runs a request itself rather
// than through dispatch(), so honouring anything that lives there is
// impossible, and dropping it could skip a policy the application put there
// -- or, for the Mock* classes, send a request the test meant to stay in
// memory (they refuse at construction; see below).
//
// Documented divergences (a shim over fetch cannot honor everything):
//  - Other connection-level dispatcher options (connection pooling,
//    keep-alive tuning) are accepted but NOT applied -- oam's fetch owns the
//    transport beyond the connect hooks.

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
    // The classes globalThis.fetch raises (bootstrap.js undiciErrors, which
    // has their shape), so `e.cause instanceof errors.InvalidArgumentError`
    // holds for a fetch refusal as it does in node. A fresh object: undici's
    // `errors` export is an ordinary one, and adding to it must not reach
    // the locked table.
    const errors = { ...G.__oamUndiciErrors };

    // One of undici's timeouts, as an unref'd timer: whatever it guards
    // keeps the loop alive, the timer need not. undici runs these on its
    // FastTimer, which compares timestamps, so a delay past setTimeout's
    // 2^31-1 ms ceiling -- `headersTimeout: 2 ** 31`, a common spelling of
    // "no limit" -- simply never comes due there. A plain setTimeout would
    // fire it after 1 ms with a TimeoutOverflowWarning, so the delay is
    // clamped to the ceiling (about 24.8 days).
    const MAX_TIMER_DELAY = 2147483647;
    function undiciTimer(fn, ms) {
      const timer = setTimeout(fn, Math.min(ms, MAX_TIMER_DELAY));
      if (typeof timer.unref === "function") timer.unref();
      return timer;
    }

    // ---- undici-shaped response body -------------------------------------
    // request().body is a Readable streaming the response bytes, plus the
    // undici body-mixin helpers, all consuming the same stream.
    //
    // `bodyTimeout` (ms, 0 = none) is undici's: the longest the body may go
    // without a byte while something is reading it. The timer runs for as
    // long as a read is outstanding and is cleared by the chunk that answers
    // it, so a slow consumer never trips it -- undici does not count the time
    // its parser is paused by backpressure either. When it lapses the body is
    // destroyed with BodyTimeoutError and `abort` lets go of the connection.
    function makeBodyReadable(webStream, bodyTimeout, abort) {
      const reader = webStream && typeof webStream.getReader === "function" ? webStream.getReader() : null;
      let timer = null;
      const disarm = () => {
        if (timer !== null) {
          clearTimeout(timer);
          timer = null;
        }
      };
      const r = new Readable({
        read() {
          if (!reader) {
            this.push(null);
            return;
          }
          if (bodyTimeout && timer === null) {
            timer = undiciTimer(() => {
              timer = null;
              const err = new errors.BodyTimeoutError("Body Timeout Error");
              abort(err);
              r.destroy(err);
            }, bodyTimeout);
          }
          reader.read().then(
            ({ done, value }) => {
              disarm();
              if (done) this.push(null);
              else this.push(G.Buffer.from(value));
            },
            (err) => {
              disarm();
              this.destroy(err instanceof Error ? err : new Error(String(err)));
            },
          );
        },
        destroy(err, cb) {
          disarm();
          cb(err);
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

    // In the order the response carried them: the fetch Headers' stored list,
    // not its iteration, which sorts by name as the Fetch Standard says.
    function headersToObject(headers) {
      const out = { __proto__: null };
      const add = (key, value) => {
        out[key] = key in out ? out[key] + ", " + value : value;
      };
      if (headers && Array.isArray(headers._list)) {
        for (const [key, value] of headers._list) add(key, value);
      } else if (headers && typeof headers.forEach === "function") {
        headers.forEach((value, key) => add(key, value));
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
      // undici's two per-phase stall limits: `headersTimeout` bounds the wait
      // for the response head, `bodyTimeout` the gap between body bytes. The
      // request's own value wins, then the dispatcher's (the one passed, else
      // the global one -- `new Agent({ headersTimeout })`), then undici's
      // default of 300 s; 0 disables. The dispatcher's values are checked
      // first, as undici's Client checks them, then the request's, as its
      // Request does.
      //
      // The dispatcher is the one that serves the request: for an Agent with
      // a `factory`, the factory's dispatcher for the request's origin, as
      // undici's Agent dispatches to it -- the Agent's own options reach it
      // only if the factory passes them on.
      const carrier = opts.dispatcher || holder.current;
      let serving = carrier;
      if (carrier && typeof carrier._oamForOrigin === "function") {
        let origin = null;
        try {
          origin = new G.URL(String(url)).origin;
        } catch {
          // fetch reports a URL that does not parse.
        }
        if (origin !== null && origin !== "null") serving = carrier._oamForOrigin(origin);
      }
      const dispatcherHeadersTimeout = dispatcherTimeout("headersTimeout", serving);
      const dispatcherBodyTimeout = dispatcherTimeout("bodyTimeout", serving);
      const headersTimeout = phaseTimeout("headersTimeout", opts, dispatcherHeadersTimeout);
      const bodyTimeout = phaseTimeout("bodyTimeout", opts, dispatcherBodyTimeout);
      // Both limits end the request the way an abort does, so the fetch runs
      // under a signal of the shim's own: aborting it with the timeout error
      // rejects a fetch still waiting for its head with that error, and after
      // the head it errors the body and drops the connection. The caller's
      // signal is forwarded into it with its reason, so an abort of theirs
      // still rejects with what they gave.
      const controller = new G.AbortController();
      const outer = opts.signal || null;
      let unlink = () => {};
      if (outer) {
        if (outer.aborted) {
          controller.abort(outer.reason);
        } else {
          const forward = () => controller.abort(outer.reason);
          outer.addEventListener("abort", forward, { once: true });
          unlink = () => outer.removeEventListener("abort", forward);
        }
      }
      const init = {
        method: opts.method || "GET",
        headers: opts.headers || undefined,
        body: opts.body != null ? opts.body : undefined,
        signal: controller.signal,
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
      // A dispatcher's refusal of this one request (a ProxyAgent's of a
      // caller Proxy-Authorization) is thrown as undici's request() throws
      // it; fetch asks the same question and wraps the answer.
      const riding = opts.dispatcher || holder.current;
      if (riding && typeof riding._oamVet === "function") {
        const refusal = riding._oamVet({ url: String(url), headerNames: headerNamesOf(opts.headers) });
        if (refusal) {
          unlink();
          throw refusal;
        }
      }
      // undici's headersTimeout runs while the request is on a connected
      // socket: DNS, the connect, a TLS handshake and a connect function
      // (a ProxyAgent's tunnel included) count for nothing, and each
      // redirect hop gets its own. The transport runs it, from the moment it
      // has a connection for the request -- the pool's, or the socket a
      // connect function handed back -- so it costs no op of its own, and
      // a late head fails the fetch with UND_ERR_HEADERS_TIMEOUT and closes
      // that connection, as undici destroys the socket.
      if (headersTimeout) init.__oamHeadersTimeout = headersTimeout;
      let res;
      try {
        res = await G.fetch(String(url), init);
      } catch (err) {
        unlink();
        // undici's headersTimeout, run by the transport, fails the fetch with
        // UND_ERR_HEADERS_TIMEOUT: request() rejects with undici's error.
        if (err instanceof TypeError && err.cause && err.cause.code === "UND_ERR_HEADERS_TIMEOUT") {
          throw new errors.HeadersTimeoutError("Headers Timeout Error");
        }
        // Any other failure rejects with what failed -- undici's own error
        // (its connect timeout, a response head over the limit), a transport
        // one (`connect ECONNREFUSED`, `getaddrinfo ENOTFOUND`, a proxy that
        // refuses the connection) or a connect.lookup hook's -- not with
        // fetch's `TypeError: fetch failed` around it, as node + undici do.
        // An abort rejects with its reason, which this leaves alone.
        if (err instanceof TypeError && err.message === "fetch failed" && err.cause instanceof Error) {
          throw err.cause;
        }
        throw err;
      }
      const body = makeBodyReadable(res.body, bodyTimeout, (err) => controller.abort(err));
      // A signal shared by many requests must not keep one listener per
      // finished body.
      body.once("close", unlink);
      return {
        statusCode: res.status,
        headers: headersToObject(res.headers),
        trailers: { __proto__: null },
        opaque: opts.opaque ?? null,
        context: {},
        body,
      };
    }

    // The header names of request()'s `headers` option, in any of the shapes
    // undici takes: an object, a flat [name, value, ...] array, or an
    // iterable of pairs.
    function headerNamesOf(headers) {
      if (!headers || typeof headers !== "object") return [];
      if (Array.isArray(headers)) {
        if (headers.length > 0 && Array.isArray(headers[0])) return headers.map((pair) => pair[0]);
        return headers.filter((_, i) => i % 2 === 0);
      }
      if (typeof headers[Symbol.iterator] === "function") return [...headers].map((pair) => pair[0]);
      return Object.keys(headers);
    }

    // One of request()'s phase timeouts, in ms: the request's own, else the
    // dispatcher's (already checked by dispatcherTimeout), else undici's
    // 300 s. The request's own is checked as undici's Request checks it
    // (lib/core/request.js): anything but a finite number >= 0 is refused.
    function phaseTimeout(name, opts, fromDispatcher) {
      const value = opts[name];
      if (value == null) return fromDispatcher == null ? 300e3 : fromDispatcher;
      if (!Number.isFinite(value) || value < 0) {
        throw new errors.InvalidArgumentError("invalid " + name);
      }
      return value;
    }

    // undici's Client check of its own headersTimeout / bodyTimeout
    // (lib/dispatcher/client.js), stricter than the request's: an integer
    // >= 0, with its own message.
    function checkClientTimeout(name, value) {
      if (value != null && (!Number.isInteger(value) || value < 0)) {
        throw new errors.InvalidArgumentError(name + " must be a positive integer or zero");
      }
    }

    // A dispatcher's own headersTimeout / bodyTimeout, or null. A Client
    // checked it when it was built. An Agent, Pool, BalancedPool, ProxyAgent
    // or EnvHttpProxyAgent builds its Clients when a request needs one, so
    // undici refuses a bad value there, on the request; and those pass their
    // options through JSON first (util.deepClone), so NaN or an Infinity
    // reaches the Client as null -- the default -- rather than an error.
    function dispatcherTimeout(name, dispatcher) {
      if (!dispatcher || !dispatcher._options) return null;
      let value = dispatcher._options[name];
      if (!(dispatcher instanceof Client) || dispatcher instanceof Pool) {
        if (typeof value === "number" && !Number.isFinite(value)) value = null;
      }
      checkClientTimeout(name, value);
      return value == null ? null : value;
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
          timer = undiciTimer(() => {
            timer = null;
            socket.destroy(new errors.ConnectTimeoutError(
              `Connect Timeout Error (attempted address: ${hostname}:${port}, timeout: ${connectTimeout}ms)`,
            ));
          }, connectTimeout);
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
        //  - `_oamConnectTimeout`, the connect timeout for the connections
        //    oam's transport opens for it (ms; null is undici's 10 s default,
        //    0 none). undici builds its connector from
        //    `{ timeout: connectTimeout, ...connect }`, so `connect.timeout`
        //    wins over `connectTimeout` (measured on undici 6.24.1).
        this._oamConnect = null;
        this._oamConnectLookup = null;
        this._oamConnectTimeout = null;
        const connectOptions = {
          timeout: this._options.connectTimeout,
          ...(typeof connect === "object" ? connect : null),
        };
        if (typeof connect === "function") {
          this._oamConnect = connect;
        } else if (connect && Object.keys(connect).some((key) => !LOOKUP_ROUTE_KEYS.has(key))) {
          this._oamConnect = buildConnector(connectOptions);
        } else {
          if (connect && typeof connect.lookup === "function") this._oamConnectLookup = connect.lookup;
          this._oamConnectTimeout = connectOptions.timeout ?? null;
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
          // origin -> the factory's dispatcher for it, whose connections
          // connectVia makes (with its connect timeout).
          const byOrigin = new Map();
          // The dispatcher that serves `origin` (undici's Agent hands each
          // request to it): its connections, and its own headersTimeout /
          // bodyTimeout (request() reads them through this).
          this._oamForOrigin = function (origin) {
            let dispatcher = byOrigin.get(origin);
            if (dispatcher === undefined) {
              dispatcher = factory(origin, originOptions);
              byOrigin.set(origin, dispatcher);
            }
            return dispatcher;
          };
          const forOrigin = this._oamForOrigin;
          this._oamConnectLookup = null;
          this._oamConnect = function viaFactory(params, cb) {
            connectVia(forOrigin(params.protocol + "//" + params.host), params, cb);
          };
        }
      }
    }

    // One connection made the way `dispatcher` would make it, for a
    // dispatcher that hands its requests to another one (an Agent's factory,
    // EnvHttpProxyAgent): that one's connect function, its lookup hook, or
    // undici's plain connector -- or its refusal, if oam cannot run it.
    // The plain connector is built once per dispatcher, with its lookup hook
    // and its connect timeout (its `connectTimeout` -- which undici hands an
    // Agent's factory -- or `connect.timeout`), which bounds the connection
    // as undici's Pool does.
    const builtConnectors = new WeakMap();
    function connectVia(dispatcher, params, cb) {
      const policy = policyOf(dispatcher);
      if (policy.refuse) {
        cb(policy.refuse);
      } else if (policy.connector) {
        policy.connector.fn.call(policy.connector.self, params, cb);
      } else {
        let connect = builtConnectors.get(dispatcher);
        if (connect === undefined) {
          const lookup = typeof dispatcher._oamConnectLookup === "function"
            ? dispatcher._oamConnectLookup
            : undefined;
          const timeout = dispatcher._oamConnectTimeout ?? undefined;
          connect = buildConnector(lookup ? { lookup, timeout } : { timeout });
          builtConnectors.set(dispatcher, connect);
        }
        connect(params, cb);
      }
    }

    // ---- ProxyAgent / EnvHttpProxyAgent -----------------------------------
    // undici's ProxyAgent (lib/dispatcher/proxy-agent.js, 6.29.0) sends every
    // request -- to an http origin as much as to an https one -- through a
    // CONNECT tunnel: it connects to the proxy (TLS first for an https proxy,
    // under `proxyTls`), asks it for `CONNECT host:port`, and on a 200 uses
    // that socket as the connection to the origin, with TLS to the origin
    // inside it (under `requestTls`) for https. That is a connect function,
    // and a dispatcher's connect function is the seam oam's transport already
    // asks for every connection, redirect hops included -- so a ProxyAgent
    // here is a Dispatcher whose `_oamConnect` opens the tunnel, on every
    // entry point a dispatcher has.
    //
    // The CONNECT request is undici's, byte for byte: `host` (the origin's
    // authority), `connection: close`, then the proxy headers -- the
    // `headers` option and `proxy-authorization` from `token`, `auth`
    // (Basic), or the proxy URL's userinfo. The credentials go to the proxy
    // and never to the origin.
    //
    // Refused at construction, never ignored: `proxyTunnel: false` (an http
    // origin sent to an http proxy in absolute form -- oam's transport writes
    // the request line, and would write it origin-form), and a `clientFactory`
    // or `factory`, whose dispatchers' dispatch() oam does not run.

    // The CONNECT exchange on a socket to the proxy: write the request, read
    // the response head off the socket (and only the head -- bytes after it
    // belong to the tunnel and are put back), and call back once: with
    // nothing on a 200, with undici's error otherwise. The socket is
    // destroyed on any failure.
    function openTunnel(socket, authority, host, proxyHeaders, timeout, done) {
      let head = "CONNECT " + authority + " HTTP/1.1\r\nhost: " + host + "\r\nconnection: close\r\n";
      for (const name of Object.keys(proxyHeaders)) {
        if (name.toLowerCase() === "host") continue;
        const value = String(proxyHeaders[name]);
        // A header that could end the head early is refused, not written.
        if (/[^\t\x20-\x7e\x80-\xff]/.test(value) || !/^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(name)) {
          socket.destroy();
          done(new errors.InvalidArgumentError("invalid " + name + " header"));
          return;
        }
        head += name + ": " + value + "\r\n";
      }
      let received = G.Buffer.alloc(0);
      let timer = null;
      const finish = (err) => {
        socket.removeListener("readable", onReadable);
        socket.removeListener("end", onEnd);
        socket.removeListener("close", onEnd);
        socket.removeListener("error", onError);
        if (timer !== null) clearTimeout(timer);
        if (err) socket.destroy();
        done(err);
      };
      const onEnd = () => finish(new errors.SocketError("other side closed"));
      // buildConnector's own listener stays on the socket, so removing this
      // one never leaves an 'error' unhandled.
      const onError = (err) => finish(err);
      const onReadable = () => {
        let chunk;
        while ((chunk = socket.read()) !== null) {
          received = G.Buffer.concat([received, chunk]);
          const end = received.indexOf("\r\n\r\n");
          if (end === -1) {
            // undici's default maxHeaderSize.
            if (received.length > 16384) {
              finish(new errors.HeadersOverflowError("Headers Overflow Error"));
              return;
            }
            continue;
          }
          const rest = received.subarray(end + 4);
          if (rest.length > 0) socket.unshift(rest);
          const statusLine = received.subarray(0, end).toString("latin1").split("\r\n")[0];
          const status = /^HTTP\/1\.[01] (\d{3})(?: |$)/.exec(statusLine);
          if (status === null) {
            finish(new errors.SocketError("the proxy answered CONNECT with something that is not an HTTP response"));
          } else if (status[1] !== "200") {
            finish(new errors.RequestAbortedError(`Proxy response (${Number(status[1])}) !== 200 when HTTP Tunneling`));
          } else {
            finish(null);
          }
          return;
        }
      };
      socket.on("readable", onReadable);
      socket.on("end", onEnd);
      socket.on("close", onEnd);
      socket.on("error", onError);
      if (timeout) {
        timer = undiciTimer(() => {
          timer = null;
          finish(new errors.HeadersTimeoutError("Headers Timeout Error"));
        }, timeout);
      }
      socket.write(head + "\r\n");
    }

    function proxyAuthorizationSent(headerNames) {
      return headerNames.some((name) => String(name).toLowerCase() === "proxy-authorization");
    }

    class ProxyAgent extends Dispatcher {
      constructor(opts) {
        // undici's checks, in undici's order.
        if (!opts || (typeof opts === "object" && !(opts instanceof G.URL) && !opts.uri)) {
          throw new errors.InvalidArgumentError("Proxy uri is mandatory");
        }
        const given = typeof opts === "object" && !(opts instanceof G.URL) ? opts : {};
        const { clientFactory, proxyTunnel = true } = given;
        if (clientFactory !== undefined && typeof clientFactory !== "function") {
          throw new errors.InvalidArgumentError("Proxy opts.clientFactory must be a function.");
        }
        const url = typeof opts === "string" ? new G.URL(opts) : opts instanceof G.URL ? opts : new G.URL(opts.uri);
        const proxyHeaders = { ...(given.headers || {}) };
        if (given.auth && given.token) {
          throw new errors.InvalidArgumentError("opts.auth cannot be used in combination with opts.token");
        } else if (given.auth) {
          proxyHeaders["proxy-authorization"] = `Basic ${given.auth}`;
        } else if (given.token) {
          proxyHeaders["proxy-authorization"] = given.token;
        } else if (url.username && url.password) {
          proxyHeaders["proxy-authorization"] = "Basic " + G.Buffer.from(
            `${decodeURIComponent(url.username)}:${decodeURIComponent(url.password)}`,
          ).toString("base64");
        }
        if (!proxyTunnel) {
          throw new errors.NotSupportedError(
            "undici's ProxyAgent with proxyTunnel: false is not supported on oam: every request " +
              "goes through a CONNECT tunnel, the ProxyAgent default",
          );
        }
        if (typeof clientFactory === "function" || given.factory !== undefined) {
          throw notHonored("A ProxyAgent with a clientFactory or factory");
        }
        // `connect` is the tunnel below, as in undici (which overrides it).
        super({ ...given, connect: undefined, factory: undefined });

        const toProxy = buildConnector({ ...given.proxyTls });
        const toOrigin = buildConnector({ ...given.requestTls });
        // undici's Client: the address without an IPv6 literal's brackets.
        const proxyHostname = url.hostname[0] === "[" ? url.hostname.slice(1, url.hostname.indexOf("]")) : url.hostname;
        const proxyServername = (given.proxyTls && given.proxyTls.servername) || url.hostname;
        // The CONNECT's answer is a response head, timed as undici times it:
        // undici sends the CONNECT through its proxy client, built with
        // `clientFactory(url, { connect })` and nothing else, so it runs on
        // that Client's default headersTimeout of 300 s. The ProxyAgent's
        // own headersTimeout goes to its origin requests only.
        const tunnelTimeout = 300e3;
        this._oamConnect = (params, callback) => {
          let authority = params.host;
          if (!params.port) authority += params.protocol === "https:" ? ":443" : ":80";
          toProxy(
            {
              hostname: proxyHostname,
              host: url.host,
              protocol: url.protocol,
              port: url.port,
              servername: proxyServername,
              localAddress: null,
            },
            (err, socket) => {
              if (err) {
                callback(err.code === "ERR_TLS_CERT_ALTNAME_INVALID" ? new errors.SecureProxyConnectionError(err) : err);
                return;
              }
              openTunnel(socket, authority, params.host, proxyHeaders, tunnelTimeout, (refused) => {
                if (refused) {
                  callback(refused);
                } else if (params.protocol !== "https:") {
                  callback(null, socket);
                } else {
                  const servername = given.requestTls ? given.requestTls.servername : params.servername;
                  toOrigin({ ...params, servername, httpSocket: socket }, callback);
                }
              });
            },
          );
        };
        // undici's ProxyAgent.dispatch refuses a request that carries its
        // own Proxy-Authorization: inside the tunnel it would reach the
        // origin. Asked by request() and by fetch (see policyOf).
        this._oamVet = ({ headerNames }) =>
          proxyAuthorizationSent(headerNames)
            ? new errors.InvalidArgumentError("Proxy-Authorization should be sent in ProxyAgent constructor")
            : null;
      }
    }

    // undici's EnvHttpProxyAgent (lib/dispatcher/env-http-proxy-agent.js,
    // 6.29.0): a ProxyAgent per scheme from `httpProxy` / `httpsProxy`, else
    // `http_proxy` / `HTTP_PROXY` and `https_proxy` / `HTTPS_PROXY` (https
    // falls back to the http proxy), and a plain Agent for the origins
    // `noProxy` / `no_proxy` / `NO_PROXY` exempts -- undici's matching: `*`,
    // an exact host, a `.suffix` or `*.suffix`, each optionally `:port`; the
    // variable is re-read when it changes. Each connection is made by
    // whichever of the three the origin selects.
    const DEFAULT_PORTS = { "http:": 80, "https:": 443 };
    let envProxyWarned = false;
    class EnvHttpProxyAgent extends Dispatcher {
      constructor(opts = {}) {
        const { httpProxy, httpsProxy, noProxy, ...agentOpts } = opts;
        super({ ...agentOpts, connect: undefined, factory: undefined });
        if (!envProxyWarned) {
          envProxyWarned = true;
          G.process.emitWarning("EnvHttpProxyAgent is experimental, expect them to change at any time.", {
            code: "UNDICI-EHPA",
          });
        }
        const env = G.process.env;
        const direct = new Agent(agentOpts);
        const HTTP_PROXY = httpProxy ?? env.http_proxy ?? env.HTTP_PROXY;
        const viaHttp = HTTP_PROXY ? new ProxyAgent({ ...agentOpts, uri: HTTP_PROXY }) : direct;
        const HTTPS_PROXY = httpsProxy ?? env.https_proxy ?? env.HTTPS_PROXY;
        const viaHttps = HTTPS_PROXY ? new ProxyAgent({ ...agentOpts, uri: HTTPS_PROXY }) : viaHttp;

        let noProxyValue = null;
        let noProxyEntries = [];
        const noProxyNow = () => noProxy ?? env.no_proxy ?? env.NO_PROXY ?? "";
        const parseNoProxy = () => {
          noProxyValue = noProxyNow();
          noProxyEntries = [];
          for (const entry of noProxyValue.split(/[,\s]/)) {
            if (!entry) continue;
            const parsed = entry.match(/^(.+):(\d+)$/);
            noProxyEntries.push({
              hostname: (parsed ? parsed[1] : entry).toLowerCase(),
              port: parsed ? Number.parseInt(parsed[2], 10) : 0,
            });
          }
        };
        parseNoProxy();
        const shouldProxy = (hostname, port) => {
          if (noProxy === undefined && noProxyValue !== noProxyNow()) parseNoProxy();
          if (noProxyEntries.length === 0) return true;
          if (noProxyValue === "*") return false;
          for (const entry of noProxyEntries) {
            if (entry.port && entry.port !== port) continue;
            if (!/^[.*]/.test(entry.hostname)) {
              if (hostname === entry.hostname) return false;
            } else if (hostname.endsWith(entry.hostname.replace(/^\*/, ""))) {
              return false;
            }
          }
          return true;
        };
        // `host` keeps an IPv6 literal's brackets, as undici's match does.
        const agentFor = (protocol, host, port) => {
          const hostname = host.replace(/:\d*$/, "").toLowerCase();
          const portNumber = Number.parseInt(port, 10) || DEFAULT_PORTS[protocol] || 0;
          if (!shouldProxy(hostname, portNumber)) return direct;
          return protocol === "https:" ? viaHttps : viaHttp;
        };
        this._oamConnectLookup = null;
        this._oamConnect = (params, cb) => connectVia(agentFor(params.protocol, params.host, params.port), params, cb);
        this._oamVet = (request) => {
          let url;
          try {
            url = new G.URL(request.url);
          } catch {
            return null;
          }
          const agent = agentFor(url.protocol, url.host, url.port);
          return typeof agent._oamVet === "function" ? agent._oamVet(request) : null;
        };
      }
    }

    // Origin-bound dispatchers: resolve opts.path against the origin.
    class Client extends Dispatcher {
      constructor(origin, options) {
        // undici's Client refuses a bad headersTimeout / bodyTimeout when it
        // is built; a Pool (a Client here, a pool of them in undici) builds
        // its Clients on demand, so its values are checked per request.
        if (!(new.target === Pool || new.target.prototype instanceof Pool)) {
          checkClientTimeout("headersTimeout", options && options.headersTimeout);
          checkClientTimeout("bodyTimeout", options && options.bodyTimeout);
        }
        super(options);
        // undici's Client checks the option; a Pool (and so an Agent) takes
        // it out of the options before its Clients see them, and checks
        // nothing (measured on undici 6.24.1).
        const connectTimeout = this._options.connectTimeout;
        if (!(this instanceof Pool) && connectTimeout != null &&
            (!Number.isFinite(connectTimeout) || connectTimeout < 0)) {
          throw new errors.InvalidArgumentError("invalid connectTimeout");
        }
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
    //
    // `request` -- `{ url, headerNames }`, given by fetch for the request it
    // is about to send -- lets a dispatcher refuse that one request the way
    // its undici dispatch() would (`_oamVet`: a ProxyAgent's refusal of a
    // caller Proxy-Authorization).
    function policyOf(dispatcher, request) {
      if (!(dispatcher instanceof Dispatcher)) {
        return { refuse: notHonored("A dispatcher that is not one of oam's undici classes") };
      }
      if (dispatcher.dispatch !== shimDispatch) {
        return { refuse: notHonored("A dispatcher that overrides dispatch()") };
      }
      if (dispatcher._oamInterceptors) {
        return { refuse: notHonored("A dispatcher with interceptors") };
      }
      if (request && typeof dispatcher._oamVet === "function") {
        const refusal = dispatcher._oamVet(request);
        if (refusal) return { refuse: refusal };
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

    // ---- Mock*: refused, never a silent no-op ----------------------------
    // undici's MockAgent / MockPool / MockClient INTERCEPT: a request that
    // matches an interceptor is answered from memory and never dialled, and
    // disableNetConnect() turns an unmatched one into a MockNotMatchedError
    // instead of a real connection. oam's fetch owns its transport and
    // cannot be intercepted from JS, so these were constructible stubs that
    // intercepted nothing: a suite that installed a MockAgent, called
    // disableNetConnect() and expected canned answers made REAL requests to
    // whatever host it named, and read the real answers as its mocks. A test
    // double that fails open is worse than one that is missing, so -- as
    // with a dispatcher whose dispatch() oam cannot run (policyOf) -- they
    // refuse instead. The classes stay exported so the import still
    // resolves and the failure names itself.
    function mockNotSupported(what) {
      return new errors.NotSupportedError(
        what + " is not supported on oam: oam sends the request itself, so the interceptors " +
          "would not run and disableNetConnect() would not hold -- every request meant for the " +
          "mock would go to the real host and its answer would be read as the mock's. Point the " +
          "code under test at a local server instead",
      );
    }
    class MockAgent extends Dispatcher {
      constructor() {
        throw mockNotSupported("undici's MockAgent");
      }
    }
    class MockPool extends Dispatcher {
      constructor() {
        throw mockNotSupported("undici's MockPool");
      }
    }
    class MockClient extends Dispatcher {
      constructor() {
        throw mockNotSupported("undici's MockClient");
      }
    }

    // ---- dispatch-level API: exported, refused at use ----------------------
    // RetryAgent, the Retry / Redirect / Decorator handlers,
    // createRedirectInterceptor, and connect() / upgrade() / pipeline() all
    // work through dispatch(), which oam does not run. They are exported for
    // the same reason the Mock* classes are: a name missing from an ES
    // module is a link-time SyntaxError that stops the whole program at
    // import, whether or not the importer ever uses it -- so each links, and
    // refuses when used, naming itself.
    function dispatchOnly(what) {
      return new errors.NotSupportedError(
        "undici's " + what + " is not supported on oam: it works through a dispatcher's " +
          "dispatch(), and oam sends requests itself -- use fetch() or request()",
      );
    }
    class RetryAgent extends Dispatcher {
      constructor() {
        throw dispatchOnly("RetryAgent");
      }
    }
    class RetryHandler {
      constructor() {
        throw dispatchOnly("RetryHandler");
      }
    }
    class RedirectHandler {
      constructor() {
        throw dispatchOnly("RedirectHandler");
      }
    }
    class DecoratorHandler {
      constructor() {
        throw dispatchOnly("DecoratorHandler");
      }
    }
    function createRedirectInterceptor() {
      throw dispatchOnly("createRedirectInterceptor()");
    }
    // connect() and upgrade() report through their callback or their
    // promise, as undici's do; pipeline() returns a stream, so it throws.
    function refusedCall(what) {
      return function (opts, callback) {
        const err = dispatchOnly(what);
        if (typeof callback === "function") {
          queueMicrotask(() => callback(err, null));
          return undefined;
        }
        return Promise.reject(err);
      };
    }
    const connect = refusedCall("connect()");
    const upgrade = refusedCall("upgrade()");
    function pipeline() {
      throw dispatchOnly("pipeline()");
    }
    // undici's mockErrors, for code that names the class (an `instanceof` in
    // a catch); nothing on oam raises it, since the Mock* classes refuse.
    // Built on the shared UndiciError (bootstrap.js undiciErrors) in the shape
    // of the others there: name, message and code set after super(), and
    // undici's registered brand checked by its own Symbol.hasInstance -- the
    // inherited one would take any UndiciError for this class.
    const kMockNotMatchedError = Symbol.for("undici.error.UND_MOCK_ERR_MOCK_NOT_MATCHED");
    const mockErrors = {
      MockNotMatchedError: class MockNotMatchedError extends errors.UndiciError {
        constructor(message) {
          super(message);
          this.name = "MockNotMatchedError";
          this.message = message || "The request does not match any registered mock dispatches";
          this.code = "UND_MOCK_ERR_MOCK_NOT_MATCHED";
        }
        static [Symbol.hasInstance](instance) {
          return instance && instance[kMockNotMatchedError] === true;
        }
        [kMockNotMatchedError] = true;
      },
    };

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
      ProxyAgent,
      EnvHttpProxyAgent,
      RetryAgent,
      RetryHandler,
      RedirectHandler,
      DecoratorHandler,
      createRedirectInterceptor,
      connect,
      upgrade,
      pipeline,
      mockErrors,
      // Web-standard re-exports (oam ships these as globals).
      CloseEvent: G.CloseEvent,
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
