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
    Readable.toWeb = function toWeb(nodeReadable) {
      return new globalThis.ReadableStream({
        start(controller) {
          nodeReadable.on("data", (chunk) => {
            controller.enqueue(chunk);
            if (controller.desiredSize !== null && controller.desiredSize <= 0) {
              nodeReadable.pause();
            }
          });
          nodeReadable.on("end", () => controller.close());
          nodeReadable.on("error", (e) => controller.error(e));
        },
        pull() {
          nodeReadable.resume();
        },
        cancel(reason) {
          nodeReadable.destroy(reason instanceof Error ? reason : undefined);
        },
      });
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
