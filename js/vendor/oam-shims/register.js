// Streams-port slices 3+5 (docs/design/streams-port.md sections 5.4, 5.6):
// install the vendored Node v22 port as THE stream factories. Runs at
// snapshot time AFTER node_compat.js (the registry exists; since slice 5 it
// no longer carries any stream factory of its own) and after seal.js -- no
// natives, no globals beyond the two registries, nothing executes until the
// first runtime require('stream'). The legacy hand-rolled streams and the
// OAM_LEGACY_STREAMS kill switch were deleted in slice 5; the env var is
// now silently ignored.
//
// stream/promises keeps Node byte-parity INCLUDING the promises-first wart
// (require('stream/promises') before require('stream') leaves
// stream.promises stale -- verified identical on real v22.22.2; pinned by
// vendored_stream_promises_first_entry_matches_node).
"use strict";
(() => {
  const registry = globalThis.__oamNode;

  // The vendored statics route to/fromWeb through internal/webstreams/
  // adapters, which is a throwing stub until the real adapter over
  // js/streams.js lands (a filed follow-up). Until then, attach oam's
  // proven bridge implementations (behavior-identical to the legacy ones at
  // node_compat's web-interop section) over the vendored classes.
  const attachWebBridges = (S) => {
    const { Readable, Writable, Duplex } = S;

    Readable.fromWeb = function fromWeb(webStream, options = {}) {
      const reader = webStream.getReader();
      return new Readable({
        ...options,
        objectMode: options.objectMode === true,
        async read() {
          try {
            const { value, done } = await reader.read();
            if (done) this.push(null);
            else this.push(value);
          } catch (e) {
            this.destroy(e);
          }
        },
        destroy(err, cb) {
          reader.cancel(err).then(
            () => cb(err),
            () => cb(err),
          );
        },
      });
    };
    Readable.toWeb = function toWeb(streamReadable, options) {
      return newReadableStreamFromStreamReadable(streamReadable, options);
    };

    Writable.fromWeb = function fromWeb(webStream) {
      const writer = webStream.getWriter();
      return new Writable({
        async write(chunk, _enc, cb) {
          try {
            await writer.write(chunk);
            cb();
          } catch (e) {
            cb(e);
          }
        },
        async final(cb) {
          try {
            await writer.close();
            cb();
          } catch (e) {
            cb(e);
          }
        },
        destroy(err, cb) {
          writer.abort(err).then(
            () => cb(err),
            () => cb(err),
          );
        },
      });
    };
    Writable.toWeb = function toWeb(nodeWritable) {
      return new globalThis.WritableStream({
        write(chunk) {
          return new Promise((resolve, reject) => {
            nodeWritable.write(chunk, (err) => (err ? reject(err) : resolve()));
          });
        },
        close() {
          return new Promise((resolve) => nodeWritable.end(resolve));
        },
        abort(reason) {
          nodeWritable.destroy(reason instanceof Error ? reason : undefined);
        },
      });
    };

    Duplex.fromWeb = function fromWeb(pair, options = {}) {
      const reader = pair.readable.getReader();
      const writer = pair.writable.getWriter();
      return new Duplex({
        // Options spread FIRST so the bridge's handlers can't be clobbered;
        // forwards objectMode/encoding/highWaterMark/signal like Node's
        // adapter (test-stream-duplex exercises encoding + objectMode).
        ...options,
        async read() {
          try {
            const r = await reader.read();
            if (r.done) this.push(null);
            else this.push(r.value);
          } catch (e) {
            this.destroy(e);
          }
        },
        write(chunk, _enc, cb) {
          writer.write(chunk).then(() => cb(), cb);
        },
        final(cb) {
          writer.close().then(() => cb(), cb);
        },
      });
    };
    Duplex.toWeb = function toWeb(duplex) {
      return {
        readable: Readable.toWeb(duplex),
        writable: Writable.toWeb(duplex),
      };
    };
  };

  // node v26.13.1's newReadableStreamFromStreamReadable
  // (lib/internal/webstreams/adapters.js), over js/streams.js: a default
  // stream (or, with `type: 'bytes'`, a byte stream a BYOB reader can read)
  // whose queuing strategy is the Readable's high-water mark -- in bytes,
  // or in objects for an object-mode Readable, unless options.strategy
  // names one. The Readable is paused and drained through 'data' until the
  // web stream's queue is full, resumed by each pull; its end closes the
  // web stream (answering a pending BYOB read), its error errors it, and a
  // cancel destroys it. Once cancelled or finished, a late 'data' is
  // dropped (nodejs/node#54205). A Readable that is no longer readable
  // gives a stream closed (or errored) by end-of-stream at once.
  const kErrorSentinelAttached = Symbol("kErrorSentinelAttached");
  function newReadableStreamFromStreamReadable(streamReadable, options = {}) {
    const vendorRequire = globalThis.__oamVendor.require;
    const { codes, AbortError } = registry.get("internal/errors");
    if (typeof streamReadable?._readableState !== "object") {
      throw new codes.ERR_INVALID_ARG_TYPE("streamReadable", "stream.Readable", streamReadable);
    }
    if (options === null || typeof options !== "object" || Array.isArray(options)) {
      throw new codes.ERR_INVALID_ARG_TYPE("options", "object", options);
    }
    if (options.type !== undefined && options.type !== "bytes") {
      throw new codes.ERR_INVALID_ARG_VALUE(
        "options.type",
        options.type,
        "must be one of: 'bytes', undefined",
      );
    }
    // (The v22 port exports eos itself; v26's an object holding it.)
    const eosModule = vendorRequire("internal/streams/end-of-stream");
    const eos = typeof eosModule === "function" ? eosModule : eosModule.eos;
    const { destroyer } = vendorRequire("internal/streams/destroy");
    const { isReadable } = vendorRequire("internal/streams/utils");
    const handleKnownInternalErrors = (cause) => {
      const causeCode = cause?.code;
      if (causeCode === "ERR_STREAM_PREMATURE_CLOSE") return new AbortError(undefined, { cause });
      if (causeCode != null) {
        const zlib = registry.get("zlib").constants;
        if (
          causeCode === "Z_NEED_DICT" ||
          (Object.hasOwn(zlib, causeCode) && zlib[causeCode] < 0) ||
          String(causeCode).startsWith("ERR__ERROR_")
        ) {
          const error = new TypeError(undefined, { cause });
          error.code = causeCode;
          return error;
        }
      }
      return cause;
    };

    const isBYOB = options.type === "bytes";
    let controller;
    let wasCanceled = false;
    let strategy;
    const underlyingSource = {
      __proto__: null,
      type: isBYOB ? "bytes" : undefined,
      start(c) {
        controller = c;
      },
      cancel(reason) {
        wasCanceled = true;
        destroyer(streamReadable, reason);
      },
    };
    const readable = isReadable(streamReadable);
    const objectMode = streamReadable.readableObjectMode;
    if (readable) {
      underlyingSource.pull = function pull() {
        streamReadable.resume();
      };
      const highWaterMark = streamReadable.readableHighWaterMark;
      strategy = isBYOB
        ? { highWaterMark }
        : options.strategy ??
          new (objectMode ? globalThis.CountQueuingStrategy : globalThis.ByteLengthQueuingStrategy)({
            highWaterMark,
          });
    }
    const readableStream = new globalThis.ReadableStream(underlyingSource, strategy);

    // A Duplex's readable completion does not wait on its writable side.
    // (eos settles synchronously where it can when the port's end-of-stream
    // offers node v26's kEosNodeSynchronousCallback; the cleanup is a no-op
    // until it returns. The v22 port does not offer it: over that one a
    // Readable already ended or destroyed gives a stream still 'readable'
    // when toWeb returns, closed or errored a process.nextTick later, where
    // node v26's is closed or errored at once -- measured. The streams
    // re-vendor to v26 brings the symbol and node's timing.)
    const eosOptions = { __proto__: null, writable: false };
    if (eosModule.kEosNodeSynchronousCallback !== undefined) {
      eosOptions[eosModule.kEosNodeSynchronousCallback] = true;
    }
    let cleanup = () => {};
    cleanup = eos(streamReadable, eosOptions, (error) => {
      error = handleKnownInternalErrors(error);
      cleanup();
      if (!(kErrorSentinelAttached in streamReadable)) {
        // Legacy streams that emit 'error' again after finished.
        streamReadable.on("error", () => {});
        streamReadable[kErrorSentinelAttached] = true;
      }
      if (wasCanceled) return;
      wasCanceled = true;
      if (error) {
        controller.error(error);
        return;
      }
      controller.close();
      if (isBYOB) controller.byobRequest?.respond(0);
    });

    if (wasCanceled) {
      // eos called back synchronously.
      cleanup();
    } else if (readable) {
      streamReadable.pause();
      streamReadable.on("data", function onData(chunk) {
        if (wasCanceled) return;
        // Copy the Buffer to detach it from the pool.
        if (globalThis.Buffer.isBuffer(chunk) && !objectMode) chunk = new Uint8Array(chunk);
        controller.enqueue(chunk);
        if (controller.desiredSize <= 0) streamReadable.pause();
      });
    }
    return readableStream;
  }

  registry.factories.stream = () => {
    const S = globalThis.__oamVendor.require("stream");
    attachWebBridges(S);
    return S;
  };
  registry.factories["stream/promises"] = () =>
    globalThis.__oamVendor.require("stream/promises");
  // _stream_* aliases and stream/consumers derive via registry.get("stream")
  // and ride the swap automatically; stream/web stays on js/streams.js.
})();
