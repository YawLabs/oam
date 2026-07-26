// Streams-port slice 3 (docs/design/streams-port.md section 5.4): swap the
// builtin registry's stream factories to the vendored Node v22 port. Runs at
// snapshot time AFTER node_compat.js (the registry + legacy factories exist)
// and after seal.js; it only REASSIGNS registry.factories entries -- no
// natives, no globals beyond the two registries, nothing executes until the
// first runtime require('stream').
//
// Kill switch: OAM_LEGACY_STREAMS=1 (read from natives.env() at first
// require, runtime-only) routes back to the legacy hand-rolled factory,
// UNTOUCHED through slice 4; both it and this switch are deleted in slice 5.
//
// stream/promises keeps Node byte-parity INCLUDING the promises-first wart
// (require('stream/promises') before require('stream') leaves
// stream.promises stale -- verified identical on real v22.22.2; pinned by
// vendored_stream_promises_first_entry_matches_node).
"use strict";
(() => {
  const registry = globalThis.__oamNode;
  const legacyStream = registry.factories.stream;
  const legacyPromises = registry.factories["stream/promises"];

  const useLegacy = (natives) => {
    try {
      const v = natives.env().OAM_LEGACY_STREAMS;
      if (v === undefined || v === "" || v === "0") return false;
      if (v !== "1") {
        // A rollback lever must never near-miss silently: any other
        // non-empty value still routes to legacy, with a note that the
        // documented spelling is =1.
        try {
          natives.stderrWrite(
            `oam: OAM_LEGACY_STREAMS=${v} treated as enabled (documented value: 1)\n`,
          );
        } catch (_e) { /* stderr unavailable -- still honor the intent */ }
      }
      return true;
    } catch (_e) {
      return false;
    }
  };

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

    Duplex.fromWeb = function fromWeb(pair) {
      const reader = pair.readable.getReader();
      const writer = pair.writable.getWriter();
      return new Duplex({
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

  registry.factories.stream = (natives) => {
    if (useLegacy(natives)) return legacyStream(natives);
    const S = globalThis.__oamVendor.require("stream");
    attachWebBridges(S);
    return S;
  };
  registry.factories["stream/promises"] = (natives) => {
    if (useLegacy(natives)) return legacyPromises(natives);
    return globalThis.__oamVendor.require("stream/promises");
  };
  // _stream_* aliases and stream/consumers derive via registry.get("stream")
  // and ride the swap automatically; stream/web stays on js/streams.js.
})();
