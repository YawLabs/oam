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
    // undici 6.29.0's (lib/core/errors.js): AbortError is `UND_ERR_ABORT`,
    // "The operation was aborted", and RequestAbortedError is one -- named
    // AbortError too, with its own code and message -- so a caller's
    // `err instanceof errors.AbortError` catches a refused proxy tunnel.
    class AbortError extends UndiciError {
      constructor(message) {
        super(message || "The operation was aborted", "UND_ERR_ABORT");
        this.name = "AbortError";
      }
    }
    class RequestAbortedError extends AbortError {
      constructor(message) {
        super(message || "Request aborted");
        this.code = "UND_ERR_ABORTED";
      }
    }
    const errors = {
      UndiciError,
      ConnectTimeoutError: mkError("ConnectTimeoutError", "UND_ERR_CONNECT_TIMEOUT"),
      HeadersTimeoutError: mkError("HeadersTimeoutError", "UND_ERR_HEADERS_TIMEOUT"),
      HeadersOverflowError: mkError("HeadersOverflowError", "UND_ERR_HEADERS_OVERFLOW"),
      BodyTimeoutError: mkError("BodyTimeoutError", "UND_ERR_BODY_TIMEOUT"),
      RequestContentLengthMismatchError: mkError("RequestContentLengthMismatchError", "UND_ERR_REQ_CONTENT_LENGTH_MISMATCH"),
      ResponseContentLengthMismatchError: mkError("ResponseContentLengthMismatchError", "UND_ERR_RES_CONTENT_LENGTH_MISMATCH"),
      RequestAbortedError,
      AbortError,
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
      SecureProxyConnectionError: class SecureProxyConnectionError extends UndiciError {
        constructor(cause, message) {
          super(message || "Secure Proxy Connection failed", "UND_ERR_PRX_TLS");
          this.name = "SecureProxyConnectionError";
          this.cause = cause;
        }
      },
    };

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
    // undici's `bodyTimeout` runs in the transport, as for fetch: each read
    // of the body waits at most that long for bytes, and one that lapses
    // closes the connection and fails the read -- which fetch's body reports
    // as `TypeError: terminated` with a BodyTimeoutError cause, and this
    // body as the BodyTimeoutError itself, as undici's request() does.
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
            (err) => {
              if (err instanceof TypeError && err.cause instanceof errors.BodyTimeoutError) err = err.cause;
              this.destroy(err instanceof Error ? err : new Error(String(err)));
            },
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
      // undici's two per-phase stall limits: `headersTimeout` bounds the wait
      // for the response head, `bodyTimeout` the gap between body bytes. The
      // request's own value wins, then the dispatcher's (the one passed, else
      // the global one -- `new Agent({ headersTimeout })`), then undici's
      // default of 300 s; 0 disables. The dispatcher's values are checked
      // first, as undici's Client checks them, then the request's, as its
      // Request does.
      const carrier = opts.dispatcher || holder.current;
      const dispatcherHeadersTimeout = dispatcherTimeout("headersTimeout", carrier);
      const dispatcherBodyTimeout = dispatcherTimeout("bodyTimeout", carrier);
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
      const method = opts.method || "GET";
      const init = {
        method,
        headers: opts.headers ? headerPairs(opts.headers) : undefined,
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
      // that connection, as undici destroys the socket. bodyTimeout runs
      // there too, on each read of the body. Both are given whatever they
      // are (0: no limit), so fetch does not apply its dispatcher's.
      init.__oamHeadersTimeout = headersTimeout;
      init.__oamBodyTimeout = bodyTimeout;
      let res;
      try {
        // The body, by undici's rules: checked before anything is sent,
        // framed as undici frames it, and a streamed one sent as it is
        // produced.
        res = await sendBody(String(url), init, opts.body, controller);
      } catch (err) {
        unlink();
        throw requestError(err);
      }
      const body = makeBodyReadable(res.body);
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

    // request()'s failure as undici's request() reports it. oam's fetch wraps
    // a refusal it makes before sending -- a hop-by-hop header, a
    // content-length that disagrees with the body -- and a late response
    // head in `TypeError: fetch failed`; undici's request() rejects with the
    // error itself (measured on node v22.22.2 + undici 6.29.0: `name`,
    // `code` and `message` of the undici class, no `cause`).
    const UNWRAPPED = {
      InvalidArgumentError: errors.InvalidArgumentError,
      NotSupportedError: errors.NotSupportedError,
      RequestContentLengthMismatchError: errors.RequestContentLengthMismatchError,
    };
    function requestError(err) {
      if (!(err instanceof TypeError) || err.message !== "fetch failed" || !err.cause) return err;
      const cause = err.cause;
      // fetch's cause is already undici's class (holder.undiciError).
      if (cause instanceof errors.HeadersTimeoutError) return cause;
      if (cause instanceof errors.UndiciError) return err;
      const Class = UNWRAPPED[cause.name];
      return Class ? new Class(cause.message) : err;
    }

    // ---- request bodies ---------------------------------------------------
    // undici's request() takes a string, a Buffer / typed array / ArrayBuffer,
    // a Readable (anything with pipe() and on()), an iterable or async
    // iterable (a web ReadableStream included), a Blob or a FormData, and
    // refuses anything else (lib/core/request.js). It frames them in
    // client-h1.js's writeH1: a known length goes out with content-length
    // (the caller's own `content-length` header is consumed, checked against
    // it, and never sent as given), a streamed body with the caller's
    // declared length if there is one and chunked otherwise -- and its head
    // goes out only with the first non-empty chunk, so a stream that ends
    // empty is sent as no body at all. A method that expects a payload (POST,
    // PUT, PATCH, ...) with no body still gets `content-length: 0`.
    const EXPECTS_PAYLOAD = new Set(["PUT", "POST", "PATCH", "QUERY", "PROPFIND", "PROPPATCH"]);
    const NO_CONTENT_LENGTH = new Set(["GET", "HEAD", "OPTIONS", "TRACE", "CONNECT"]);
    const BAD_BODY = "body must be a string, a Buffer, a Readable stream, an iterable, or an async iterable";
    // undici's util.isStream / isIterable / isBlobLike / isFormDataLike.
    const isStream = (b) => !!b && typeof b === "object" && typeof b.pipe === "function" && typeof b.on === "function";
    const isIterable = (b) =>
      b != null && (typeof b[Symbol.iterator] === "function" || typeof b[Symbol.asyncIterator] === "function");
    function isBlobLike(b) {
      if (b === null || typeof b !== "object") return false;
      if (typeof G.Blob === "function" && b instanceof G.Blob) return true;
      const tag = b[Symbol.toStringTag];
      return (tag === "Blob" || tag === "File") &&
        (typeof b.stream === "function" || typeof b.arrayBuffer === "function");
    }
    const isFormDataLike = (b) =>
      !!b && typeof b === "object" && b[Symbol.toStringTag] === "FormData" &&
      ["append", "delete", "get", "getAll", "has", "set"].every((m) => typeof b[m] === "function");
    function mismatch() {
      return new errors.RequestContentLengthMismatchError("Request body length does not match content-length header");
    }
    // undici's util.destroy: a stream is destroyed (with `err`), one without
    // destroy() is sent the error.
    function destroyStream(stream, err) {
      if (!isStream(stream) || stream.destroyed === true) return;
      if (typeof stream.destroy === "function") stream.destroy(err);
      else if (err) queueMicrotask(() => stream.emit("error", err));
    }

    // Send `init` (a request() fetch init without its body) with `body`:
    // resolves with the fetch's Response, or rejects as undici's request()
    // would. `controller` is the request's own: the caller's signal and the
    // shim's timeouts abort it.
    async function sendBody(url, init, body, controller) {
      const method = init.method;
      // The caller's content-length is undici's to frame with (processHeader).
      let declared = null;
      let typed = false;
      const headers = [];
      for (const pair of init.headers || []) {
        const name = String(pair[0]).toLowerCase();
        if (name === "content-length") {
          if (declared !== null) throw new errors.InvalidArgumentError("duplicate content-length header");
          declared = parseInt(pair[1], 10);
          if (!Number.isFinite(declared)) throw new errors.InvalidArgumentError("invalid content-length header");
          continue;
        }
        if (name === "content-type") typed = true;
        headers.push(pair);
      }
      const expectsPayload = EXPECTS_PAYLOAD.has(method);
      let bytes = null;
      if (body == null) {
        bytes = null;
      } else if (isStream(body)) {
        return sendStreamed(url, { ...init, headers }, body, true, declared, expectsPayload, controller);
      } else if (body instanceof Uint8Array || ArrayBuffer.isView(body)) {
        bytes = body.byteLength ? new Uint8Array(body.buffer, body.byteOffset, body.byteLength) : null;
      } else if (body instanceof ArrayBuffer) {
        bytes = body.byteLength ? new Uint8Array(body) : null;
      } else if (typeof body === "string") {
        // Sent as the string (the transport encodes it as UTF-8).
        bytes = body.length ? body : null;
      } else if (isFormDataLike(body)) {
        throw new errors.NotSupportedError(
          "a FormData body is not supported by oam's undici.request(): oam has no multipart/form-data encoder yet. " +
            "Send it with fetch() through a library that encodes it, or encode it yourself",
        );
      } else if (isIterable(body)) {
        return sendStreamed(url, { ...init, headers }, body, false, declared, expectsPayload, controller);
      } else if (isBlobLike(body)) {
        // undici sends a Blob with its size as content-length and its type
        // as content-type, unless the caller set one.
        if (!typed && body.type) headers.push(["content-type", String(body.type)]);
        const read = new Uint8Array(await body.arrayBuffer());
        bytes = read.byteLength ? read : null;
      } else {
        throw new errors.InvalidArgumentError(BAD_BODY);
      }
      // A body of known length: undici's writeH1 content-length rules.
      let length = bytes === null ? 0 : typeof bytes === "string" ? G.Buffer.byteLength(bytes) : bytes.byteLength;
      if (length === 0 && !expectsPayload) length = null;
      if (!NO_CONTENT_LENGTH.has(method) && length > 0 && declared !== null && declared !== length) throw mismatch();
      if (length === 0) headers.push(["content-length", "0"]);
      return G.fetch(url, { ...init, headers, body: bytes === null ? undefined : bytes });
    }

    // A Readable (`stream`) or an iterable body, written as undici's
    // AsyncWriter writes it: each chunk's length is Buffer.byteLength's (so
    // anything but a string or a buffer fails as there), a chunk that would
    // go past the content-length is refused before it is sent and a body
    // that ends short of it fails, and nothing is sent until the first
    // non-empty chunk. The request is dispatched with that chunk, its body
    // then following over an outbound channel (with backpressure: the next
    // chunk is taken once the transport has the last one); a body that ends
    // with none is sent as no body. Once the body has ended, the transport's
    // headers timer starts (it holds it off while a streamed body is still
    // going out, as undici does).
    //
    // A body that fails -- the stream errors or closes before its end, the
    // iterator throws, a chunk is refused -- fails the request with that
    // error, before the head or after it, and the stream is destroyed with
    // it. A request that fails (or is aborted) stops the body.
    function sendStreamed(url, init, body, stream, declared, expectsPayload, controller) {
      const signal = controller.signal;
      const ops = G.__oam.node;
      let length = declared;
      if (stream) {
        // undici's util.bodyLength: an ended byte stream's buffered length.
        if (typeof body.read === "function") body.read(0);
        const state = body._readableState;
        if (state && state.objectMode === false && state.ended === true && Number.isFinite(state.length)) {
          length = state.length;
        }
      }
      if (length === 0 && !expectsPayload) length = null;
      if (!NO_CONTENT_LENGTH.has(init.method) && length > 0 && declared !== null && declared !== length) {
        throw mismatch();
      }
      return new Promise((resolve, reject) => {
        let channel = null;
        let started = false;
        let over = false;
        let written = 0;
        let tail = null;
        const start = (withBody) => {
          started = true;
          const headers = init.headers.slice();
          if (withBody && length !== null) headers.push(["content-length", String(length)]);
          if (!withBody && expectsPayload) headers.push(["content-length", "0"]);
          const sent = G.fetch(
            url,
            withBody
              ? { ...init, headers, __oamBodyStream: channel, __oamChunked: length === null }
              : { ...init, headers },
          );
          // A request that fails takes its body with it.
          sent.catch(() => finish(null, true));
          resolve(sent);
        };
        // The next chunk: `null` when it was empty, else a promise that
        // resolves once the transport has it (`false`, or a rejection: the
        // request is gone).
        const write = (chunk) => {
          const len = G.Buffer.byteLength(chunk);
          if (!len) return null;
          if (length !== null && written + len > length) throw mismatch();
          const bytes = typeof chunk === "string"
            ? G.Buffer.from(chunk)
            : chunk instanceof ArrayBuffer
              ? new Uint8Array(chunk)
              : new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
          if (channel === null) {
            channel = ops.fetchBodyChannelNew();
            start(true);
          }
          written += len;
          const id = channel;
          const next = () => ops.fetchBodyChannelWrite(id, bytes);
          tail = tail === null ? next() : tail.then(next);
          return tail;
        };
        const end = () => {
          if (length !== null && written !== length) throw mismatch();
          if (channel === null) {
            start(false);
            return;
          }
          const id = channel;
          const close = () => ops.fetchBodyChannelEnd(id);
          if (tail === null) close();
          else tail.then(close, close);
        };
        // The body is over: ended (`err` null), failed with `err`, or
        // stopped because the request is over (`quiet`).
        let stop = () => {};
        const finish = (err, quiet) => {
          if (over) return;
          over = true;
          signal.removeEventListener("abort", onAbort);
          if (err === null && !quiet) {
            try {
              end();
            } catch (e) {
              err = e;
            }
          }
          stop(err ?? undefined);
          if (err === null && !quiet) return;
          if (channel !== null) ops.fetchBodyChannelCancel(channel);
          if (quiet) return;
          // Before the head the request rejects with it, after the head its
          // response body errors with it.
          if (!started) reject(err);
          else if (!signal.aborted) controller.abort(err);
        };
        // An abort (the caller's, or a timeout) stops the body; a request
        // already sent rejects with its reason by itself.
        const onAbort = () => {
          if (!started) reject(signal.reason);
          finish(null, true);
        };
        signal.addEventListener("abort", onAbort, { once: true });
        if (stream) stop = pumpStream(body, write, finish);
        else stop = pumpIterable(body, write, finish);
        if (signal.aborted) onAbort();
      });
    }

    // A Readable body, read as undici's writeStream reads it: 'data' events,
    // paused while the transport takes a chunk; 'end' ends the body, 'error'
    // fails it, and a 'close' before either fails it with undici's
    // RequestAbortedError. Returns the function that detaches it.
    function pumpStream(body, write, finish) {
      let done = false;
      // undici's Request keeps an 'error' listener on its body for good, so
      // an error after the request is over is not an uncaught one.
      body.on("error", () => {});
      const onData = function (chunk) {
        if (done) return;
        let pending;
        try {
          pending = write(chunk);
        } catch (err) {
          destroyStream(body, err);
          return;
        }
        if (pending === null) return;
        if (typeof body.pause === "function") body.pause();
        pending.then(
          (more) => {
            if (more === false) finish(null, true);
            else if (!done && typeof body.resume === "function") body.resume();
          },
          () => finish(null, true),
        );
      };
      const onEnd = () => over(null);
      const onError = (err) => over(err);
      const onClose = () => {
        if (!done) queueMicrotask(() => over(new errors.RequestAbortedError()));
      };
      function over(err) {
        if (done) return;
        finish(err, false);
      }
      body.on("data", onData).on("end", onEnd).on("error", onError).on("close", onClose);
      if (typeof body.resume === "function") body.resume();
      if (body.errorEmitted ?? body.errored) setImmediate(() => over(body.errored));
      else if (body.endEmitted ?? body.readableEnded) setImmediate(() => over(null));
      if (body.closeEmitted ?? body.closed) setImmediate(onClose);
      return function stop(err) {
        done = true;
        body.removeListener("data", onData).removeListener("end", onEnd).removeListener("close", onClose);
        queueMicrotask(() => body.removeListener("error", onError));
        destroyStream(body, err);
      };
    }

    // An iterable or async iterable body (a web ReadableStream too), read as
    // undici's writeIterable reads it: `for await`, the next value taken once
    // the transport has the last. Stopping returns the iterator, which ends
    // a generator and cancels a ReadableStream.
    function pumpIterable(iterable, write, finish) {
      let stopped = false;
      (async () => {
        for await (const chunk of iterable) {
          if (stopped) return;
          const pending = write(chunk);
          if (pending !== null && (await pending.then(null, () => false)) === false) return finish(null, true);
          if (stopped) return;
        }
        finish(null, false);
      })().catch((err) => finish(err, false));
      return function stop() {
        stopped = true;
      };
    }

    // request()'s `headers` option as [name, value] pairs, in any of the
    // shapes undici takes: an object, a flat [name, value, ...] array, or an
    // iterable of pairs. As in undici's processHeader, an array value is one
    // line per element, `null` is an empty value and `undefined` no header.
    function headerPairs(headers) {
      let entries;
      if (typeof headers !== "object") return [];
      if (Array.isArray(headers) && !(headers.length > 0 && Array.isArray(headers[0]))) {
        entries = [];
        for (let i = 0; i + 1 < headers.length; i += 2) entries.push([headers[i], headers[i + 1]]);
      } else if (typeof headers[Symbol.iterator] === "function") {
        entries = [...headers];
      } else {
        entries = Object.entries(headers);
      }
      const pairs = [];
      for (const [name, value] of entries) {
        if (value === undefined) continue;
        if (Array.isArray(value)) {
          for (const v of value) pairs.push([name, v === null ? "" : v]);
        } else {
          pairs.push([name, value === null ? "" : value]);
        }
      }
      return pairs;
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
          this._oamConnectLookup = null;
          this._oamConnect = function viaFactory(params, cb) {
            const origin = params.protocol + "//" + params.host;
            let dispatcher = byOrigin.get(origin);
            if (dispatcher === undefined) {
              dispatcher = factory(origin, originOptions);
              byOrigin.set(origin, dispatcher);
            }
            connectVia(dispatcher, params, cb);
          };
        }
      }
    }

    // One connection made the way `dispatcher` would make it, for a
    // dispatcher that hands its requests to another one (an Agent's factory,
    // EnvHttpProxyAgent): that one's connect function, its lookup hook, or
    // undici's plain connector -- or its refusal, if oam cannot run it.
    const plainConnect = buildConnector({});
    function connectVia(dispatcher, params, cb) {
      const policy = policyOf(dispatcher);
      if (policy.refuse) {
        cb(policy.refuse);
      } else if (policy.connector) {
        policy.connector.fn.call(policy.connector.self, params, cb);
      } else if (typeof dispatcher._oamConnectLookup === "function") {
        buildConnector({ lookup: dispatcher._oamConnectLookup })(params, cb);
      } else {
        plainConnect(params, cb);
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
      const policy = {};
      if (request) {
        // A fetch rides the dispatcher's own headersTimeout / bodyTimeout
        // (null: undici's 300 s), checked as its Client checks them -- a bad
        // one fails the fetch with that InvalidArgumentError as the cause.
        try {
          policy.headersTimeout = dispatcherTimeout("headersTimeout", dispatcher);
          policy.bodyTimeout = dispatcherTimeout("bodyTimeout", dispatcher);
        } catch (err) {
          return { refuse: err };
        }
      }
      if (typeof dispatcher._oamConnect === "function") {
        policy.connector = { fn: dispatcher._oamConnect, self: dispatcher };
      }
      return policy;
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
    // The undici class of a failure oam's transport raises with undici's
    // code (a headers or body timeout, an oversized response head), so the
    // `cause` of a fetch failure is an instance of `errors.*` as in node.
    if (holder.undiciError === undefined) {
      const byCode = {
        UND_ERR_HEADERS_TIMEOUT: errors.HeadersTimeoutError,
        UND_ERR_BODY_TIMEOUT: errors.BodyTimeoutError,
        UND_ERR_HEADERS_OVERFLOW: errors.HeadersOverflowError,
      };
      Object.defineProperty(holder, "undiciError", {
        value: (code, message) => (byCode[code] ? new byCode[code](message) : null),
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
    const mockErrors = {
      MockNotMatchedError: class MockNotMatchedError extends UndiciError {
        constructor(message) {
          super(message || "The request does not match any registered mock dispatches", "UND_MOCK_ERR_MOCK_NOT_MATCHED");
          this.name = "MockNotMatchedError";
        }
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
