// Streams-port slices 3+5 (docs/design/streams-port.md sections 5.4, 5.6):
// install the vendored Node v26 port as THE stream factories. Runs at
// snapshot time AFTER node_compat.js (the registry exists; since slice 5 it
// no longer carries any stream factory of its own) and after seal.js -- no
// natives, no globals beyond the two registries, nothing executes until the
// first runtime require('stream'). The legacy hand-rolled streams and the
// OAM_LEGACY_STREAMS kill switch were deleted in slice 5; the env var is
// now silently ignored.
//
// stream/promises keeps Node byte-parity INCLUDING the promises-first wart
// (require('stream/promises') before require('stream') leaves
// stream.promises stale -- re-verified identical on v26.13.1; pinned by
// vendored_stream_promises_first_entry_matches_node).
"use strict";
(() => {
  const registry = globalThis.__oamNode;

  // The vendored statics route to/fromWeb through internal/webstreams/
  // adapters, which is a throwing stub until the real adapter over
  // js/streams.js lands (a filed follow-up). Until then, attach oam's
  // bridge implementations over the vendored classes: Readable.toWeb is a
  // port of node v26's adapter (below); the others are oam's proven
  // bridges (behavior-identical to the legacy ones at node_compat's
  // web-interop section).
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
    // Readable.toWeb is a port of node v26.13.1's
    // newReadableStreamFromStreamReadable (lib/internal/webstreams/
    // adapters.js), over oam's web streams and the vendored eos: the same
    // validation, queuing strategy (the Readable's own highWaterMark, in
    // bytes or objects), Buffer-off-the-pool copies, cancel -> destroy, and
    // completion through eos(writable: false) so a half-open Duplex's
    // readable side closes without waiting for its writable side.
    const kErrorSentinelAttached = Symbol("kErrorSentinelAttached");
    const noop = () => {};
    // node's handleKnownInternalErrors: a premature close reaches the web
    // side as an AbortError; zlib failures as a TypeError keeping the code.
    const handleKnownInternalErrors = (cause) => {
      const code = cause?.code;
      if (code === "ERR_STREAM_PREMATURE_CLOSE") {
        const { AbortError } = globalThis.__oamVendor.require("internal/errors");
        return new AbortError(undefined, { cause });
      }
      const zlibConstants = registry.get("zlib").constants;
      if (
        code != null &&
        ((typeof code === "string" && code.startsWith("ERR__ERROR_")) ||
          (Object.prototype.hasOwnProperty.call(zlibConstants, code) &&
            (code === "Z_NEED_DICT" || zlibConstants[code] < 0)))
      ) {
        const error = new TypeError(undefined, { cause });
        error.code = code;
        return error;
      }
      return cause;
    };
    Readable.toWeb = function toWeb(streamReadable, options = {}) {
      const vendor = globalThis.__oamVendor;
      const { codes } = vendor.require("internal/errors");
      if (typeof streamReadable?._readableState !== "object") {
        throw new codes.ERR_INVALID_ARG_TYPE("streamReadable", "stream.Readable", streamReadable);
      }
      vendor.require("internal/validators").validateObject(options, "options");
      if (options.type !== undefined && options.type !== "bytes") {
        throw new codes.ERR_INVALID_ARG_VALUE(
          "options.type",
          options.type,
          "must be one of: 'bytes', undefined",
        );
      }
      const { isReadable } = vendor.require("internal/streams/utils");
      const { eos, kEosNodeSynchronousCallback } = vendor.require(
        "internal/streams/end-of-stream",
      );

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
          S.destroy(streamReadable, reason);
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

      let cleanup = noop;
      cleanup = eos(
        streamReadable,
        { __proto__: null, writable: false, [kEosNodeSynchronousCallback]: true },
        (error) => {
          error = handleKnownInternalErrors(error);
          // If eos calls back synchronously, cleanup is still a no-op here.
          cleanup();
          if (!(kErrorSentinelAttached in streamReadable)) {
            // Protection against legacy streams that emit 'error' again
            // after finished() is called.
            streamReadable.on("error", noop);
            streamReadable[kErrorSentinelAttached] = true;
          }
          if (wasCanceled) return;
          wasCanceled = true;
          if (error) return controller.error(error);
          controller.close();
          if (isBYOB) controller.byobRequest?.respond(0);
        },
      );

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
    // node v26: options.readableType picks the readable side's type;
    // options.type is its deprecated alias (DEP0201, warned once).
    let warnedDEP0201 = false;
    Duplex.toWeb = function toWeb(duplex, options = {}) {
      globalThis.__oamVendor.require("internal/validators").validateObject(options, "options");
      let type = options.readableType;
      if (options.readableType == null && options.type != null) {
        if (!warnedDEP0201) {
          warnedDEP0201 = true;
          globalThis.process.emitWarning(
            "Passing 'options.type' to Duplex.toWeb() is deprecated. " +
              "To specify the ReadableStream type, use 'options.readableType'.",
            "DeprecationWarning",
            "DEP0201",
          );
        }
        type = options.type;
      }
      return {
        readable: Readable.toWeb(duplex, { type }),
        writable: Writable.toWeb(duplex),
      };
    };
  };

  registry.factories.stream = () => {
    const S = globalThis.__oamVendor.require("stream");
    attachWebBridges(S);
    return S;
  };
  registry.factories["stream/promises"] = () =>
    globalThis.__oamVendor.require("stream/promises");
  // stream/consumers derives via registry.get("stream") and rides the swap
  // automatically (node v26 has no _stream_* aliases); stream/web stays on
  // js/streams.js.
})();
