// oam web streams: ReadableStream / WritableStream / TransformStream +
// TextEncoderStream / TextDecoderStream (WHATWG Streams, ECMA-429 surface).
//
// Scope (documented subset, grows toward full WPT conformance):
// - Default readers only — no BYOB/byte streams yet.
// - Count-based queuing: highWaterMark counts CHUNKS; size() functions in
//   queuing strategies are not consulted.
// - pipeTo/pipeThrough support preventClose/preventCancel/preventAbort.
//
// The async-iteration path is the load-bearing one: `for await (const
// chunk of response.body)` and TextDecoderStream pipelines are how SSE /
// token-streaming AI clients consume models.
//
// SNAPSHOT CONSTRAINT: evaluated at build time — pure JS only, no natives,
// nothing async at top level. TextDecoderStream instantiates TextDecoder
// lazily (node_compat.js defines it earlier in the snapshot order).
"use strict";
(() => {
  // ------------------------------------------------------- ReadableStream
  class ReadableStream {
    constructor(source = {}, strategy = {}) {
      this._queue = [];
      this._state = "readable"; // readable | closed | errored
      this._error = undefined;
      this._reader = null; // the active default reader (lock)
      this._waiters = []; // pending read() resolvers: {resolve, reject}
      this._pulling = false;
      this._pullAgain = false;
      this._source = source;
      this._highWaterMark = strategy.highWaterMark ?? 1;
      // Queue accounting goes through strategy.size (ByteLengthQueuingStrategy
      // budgets BYTES); a parallel size ledger avoids re-invoking a possibly
      // impure size() at dequeue. No strategy.size = count semantics (1/chunk).
      const sizeFn = strategy.size;
      this._sizeFn = typeof sizeFn === "function"
        ? (chunk) => {
            const n = Number(sizeFn(chunk));
            return Number.isFinite(n) && n >= 0 ? n : 1;
          }
        : () => 1;
      this._queueSizes = [];
      this._queueTotalSize = 0;
      this._cancelled = false;

      const stream = this;
      this._controller = {
        enqueue(chunk) {
          if (stream._state !== "readable") {
            throw new TypeError("Cannot enqueue on a non-readable stream");
          }
          if (stream._waiters.length > 0) {
            stream._waiters.shift().resolve({ value: chunk, done: false });
          } else {
            const size = stream._sizeFn(chunk);
            stream._queue.push(chunk);
            stream._queueSizes.push(size);
            stream._queueTotalSize += size;
          }
        },
        close() {
          if (stream._state !== "readable") return;
          stream._state = "closed";
          while (stream._waiters.length > 0) {
            stream._waiters.shift().resolve({ value: undefined, done: true });
          }
          stream._resolveClosed?.();
        },
        error(reason) {
          if (stream._state !== "readable") return;
          stream._state = "errored";
          stream._error = reason;
          stream._queue = [];
          stream._queueSizes = [];
          stream._queueTotalSize = 0;
          while (stream._waiters.length > 0) {
            stream._waiters.shift().reject(reason);
          }
          stream._rejectClosed?.(reason);
        },
        get desiredSize() {
          if (stream._state === "errored") return null;
          if (stream._state === "closed") return 0;
          return stream._highWaterMark - stream._queueTotalSize;
        },
      };

      this._started = Promise.resolve()
        .then(() => source.start?.(this._controller))
        .catch((e) => this._controller.error(e));
    }

    get locked() {
      return this._reader !== null;
    }

    _maybePull() {
      if (this._state !== "readable" || !this._source.pull) return;
      if (this._pulling) {
        this._pullAgain = true;
        return;
      }
      // Pull when a reader is waiting or the queued total is under the HWM.
      if (this._waiters.length === 0 && this._queueTotalSize >= this._highWaterMark) {
        return;
      }
      this._pulling = true;
      this._started
        .then(() => this._source.pull(this._controller))
        .then(
          () => {
            this._pulling = false;
            if (
              this._pullAgain ||
              (this._state === "readable" && this._waiters.length > 0)
            ) {
              this._pullAgain = false;
              this._maybePull();
            }
          },
          (e) => {
            this._pulling = false;
            this._controller.error(e);
          },
        );
    }

    getReader() {
      if (this._reader !== null) {
        throw new TypeError("ReadableStream is locked to a reader");
      }
      const stream = this;
      let closedResolve;
      let closedReject;
      const closed = new Promise((resolve, reject) => {
        closedResolve = resolve;
        closedReject = reject;
      });
      closed.catch(() => {}); // observable via reader.closed; never unhandled
      if (stream._state === "closed") closedResolve();
      if (stream._state === "errored") closedReject(stream._error);
      stream._resolveClosed = closedResolve;
      stream._rejectClosed = closedReject;

      const reader = {
        closed,
        read() {
          if (stream._reader !== reader) {
            return Promise.reject(new TypeError("reader has been released"));
          }
          if (stream._queue.length > 0) {
            const value = stream._queue.shift();
            stream._queueTotalSize -= stream._queueSizes.shift();
            stream._maybePull();
            return Promise.resolve({ value, done: false });
          }
          if (stream._state === "closed") {
            return Promise.resolve({ value: undefined, done: true });
          }
          if (stream._state === "errored") {
            return Promise.reject(stream._error);
          }
          return new Promise((resolve, reject) => {
            stream._waiters.push({ resolve, reject });
            stream._maybePull();
          });
        },
        cancel(reason) {
          return stream._cancelInternal(reason);
        },
        releaseLock() {
          if (stream._reader === reader) {
            stream._reader = null;
            while (stream._waiters.length > 0) {
              stream._waiters.shift().reject(new TypeError("reader was released"));
            }
          }
        },
      };
      stream._reader = reader;
      return reader;
    }

    _cancelInternal(reason) {
      if (this._state === "errored") return Promise.reject(this._error);
      if (this._cancelled || this._state === "closed") return Promise.resolve();
      this._cancelled = true;
      this._queue = [];
      this._queueSizes = [];
      this._queueTotalSize = 0;
      this._controller.close();
      return Promise.resolve()
        .then(() => this._source.cancel?.(reason))
        .then(() => undefined);
    }

    cancel(reason) {
      if (this.locked) {
        return Promise.reject(new TypeError("Cannot cancel a locked stream"));
      }
      return this._cancelInternal(reason);
    }

    [Symbol.asyncIterator](options = {}) {
      const reader = this.getReader();
      const preventCancel = options.preventCancel === true;
      return {
        next: () => reader.read(),
        return: async (value) => {
          // Early loop exit cancels the stream (WHATWG default).
          if (!preventCancel) await reader.cancel();
          reader.releaseLock();
          return { value, done: true };
        },
        [Symbol.asyncIterator]() {
          return this;
        },
      };
    }

    values(options) {
      return this[Symbol.asyncIterator](options);
    }

    tee() {
      const reader = this.getReader();
      const queues = [[], []];
      let pulling = null;
      const makeBranch = (index) =>
        new ReadableStream({
          async pull(controller) {
            if (queues[index].length > 0) {
              const item = queues[index].shift();
              if (item.done) controller.close();
              else controller.enqueue(item.value);
              return;
            }
            pulling ??= reader.read().then((result) => {
              pulling = null;
              for (const q of queues) {
                q.push(result.done ? { done: true } : { done: false, value: result.value });
              }
            }).catch((err) => {
              pulling = null;
              throw err;
            });
            await pulling;
            const item = queues[index].shift();
            if (item.done) controller.close();
            else controller.enqueue(item.value);
          },
        });
      return [makeBranch(0), makeBranch(1)];
    }

    async pipeTo(destination, options = {}) {
      const reader = this.getReader();
      const writer = destination.getWriter();
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          await writer.write(value);
        }
        if (options.preventClose !== true) await writer.close();
      } catch (e) {
        if (options.preventAbort !== true) await writer.abort(e).catch(() => {});
        if (options.preventCancel !== true) await reader.cancel(e).catch(() => {});
        throw e;
      } finally {
        reader.releaseLock();
        writer.releaseLock();
      }
    }

    pipeThrough(pair, options) {
      // The pump runs detached; errors surface through the readable side.
      this.pipeTo(pair.writable, options).catch(() => {});
      return pair.readable;
    }

    static from(source) {
      const iterator =
        source[Symbol.asyncIterator]?.() ?? source[Symbol.iterator]?.();
      if (!iterator) {
        throw new TypeError("ReadableStream.from requires an iterable");
      }
      return new ReadableStream({
        async pull(controller) {
          const { value, done } = await iterator.next();
          if (done) controller.close();
          else controller.enqueue(value);
        },
        async cancel(reason) {
          await iterator.return?.(reason);
        },
      });
    }
  }

  // ------------------------------------------------------- WritableStream
  class WritableStream {
    constructor(sink = {}, _strategy = {}) {
      this._sink = sink;
      this._state = "writable"; // writable | closed | errored
      this._error = undefined;
      this._writer = null;
      // Writes are SERIALIZED: each chains on the previous sink call.
      this._chain = Promise.resolve().then(() => sink.start?.(this._controllerFor()));
      this._chain = this._chain.catch((e) => {
        this._state = "errored";
        this._error = e;
      });
    }

    _controllerFor() {
      const stream = this;
      return {
        error(reason) {
          stream._state = "errored";
          stream._error = reason;
        },
      };
    }

    get locked() {
      return this._writer !== null;
    }

    getWriter() {
      if (this._writer !== null) {
        throw new TypeError("WritableStream is locked to a writer");
      }
      const stream = this;
      const writer = {
        get ready() {
          return stream._chain.then(() => undefined);
        },
        get closed() {
          return stream._chain.then(() => {
            if (stream._state === "errored") throw stream._error;
          });
        },
        get desiredSize() {
          return stream._state === "writable" ? 1 : stream._state === "closed" ? 0 : null;
        },
        write(chunk) {
          if (stream._writer !== writer) {
            return Promise.reject(new TypeError("writer has been released"));
          }
          const next = stream._chain.then(() => {
            if (stream._state === "errored") throw stream._error;
            if (stream._state !== "writable") {
              throw new TypeError("Cannot write to a closed stream");
            }
            return stream._sink.write?.(chunk, stream._controllerFor());
          });
          stream._chain = next.catch((e) => {
            stream._state = "errored";
            stream._error = e;
          });
          return next.then(() => undefined);
        },
        close() {
          const next = stream._chain.then(() => {
            if (stream._state === "errored") throw stream._error;
            if (stream._state !== "writable") {
              throw new TypeError("Cannot close a non-writable stream");
            }
            stream._state = "closed";
            return stream._sink.close?.();
          });
          stream._chain = next.catch((e) => {
            stream._state = "errored";
            stream._error = e;
          });
          return next.then(() => undefined);
        },
        abort(reason) {
          stream._state = "errored";
          stream._error = reason ?? new TypeError("aborted");
          return Promise.resolve()
            .then(() => stream._sink.abort?.(reason))
            .then(() => undefined);
        },
        releaseLock() {
          if (stream._writer === writer) stream._writer = null;
        },
      };
      stream._writer = writer;
      return writer;
    }

    abort(reason) {
      if (this.locked) {
        return Promise.reject(new TypeError("Cannot abort a locked stream"));
      }
      this._state = "errored";
      this._error = reason ?? new TypeError("aborted");
      return Promise.resolve()
        .then(() => this._sink.abort?.(reason))
        .then(() => undefined);
    }

    close() {
      if (this.locked) {
        return Promise.reject(new TypeError("Cannot close a locked stream"));
      }
      const writer = this.getWriter();
      const done = writer.close();
      writer.releaseLock();
      return done;
    }
  }

  // ------------------------------------------------------ TransformStream
  class TransformStream {
    constructor(transformer = {}, _writableStrategy, _readableStrategy) {
      let readableController;
      this.readable = new ReadableStream({
        start(controller) {
          readableController = controller;
        },
      });
      const controller = {
        enqueue: (chunk) => readableController.enqueue(chunk),
        terminate: () => readableController.close(),
        error: (reason) => readableController.error(reason),
        get desiredSize() {
          return readableController.desiredSize;
        },
      };
      this.writable = new WritableStream({
        start: () => transformer.start?.(controller),
        write: (chunk) => transformer.transform?.(chunk, controller),
        close: async () => {
          await transformer.flush?.(controller);
          readableController.close();
        },
        abort: (reason) => readableController.error(reason),
      });
    }
  }

  // -------------------------------------------- text en/decoding streams
  class TextDecoderStream {
    constructor(label = "utf-8", options = {}) {
      const decoder = new TextDecoder(label, options);
      const transform = new TransformStream({
        transform(chunk, controller) {
          const text = decoder.decode(chunk, { stream: true });
          if (text.length > 0) controller.enqueue(text);
        },
        flush(controller) {
          const tail = decoder.decode(); // flush any buffered partial char
          if (tail.length > 0) controller.enqueue(tail);
        },
      });
      this.readable = transform.readable;
      this.writable = transform.writable;
      this.encoding = decoder.encoding;
      this.fatal = decoder.fatal;
      this.ignoreBOM = decoder.ignoreBOM;
    }
  }

  class TextEncoderStream {
    constructor() {
      const encoder = new TextEncoder();
      const transform = new TransformStream({
        transform(chunk, controller) {
          const bytes = encoder.encode(String(chunk));
          if (bytes.length > 0) controller.enqueue(bytes);
        },
      });
      this.readable = transform.readable;
      this.writable = transform.writable;
      this.encoding = "utf-8";
    }
  }

  // WHATWG queuing strategies (Node globals since v18). Spec-minimal: the
  // stream machinery here is count-based and does not consult size() (see
  // the header note), but the classes must exist and carry the documented
  // shape -- vendored stream tests construct them directly.
  class CountQueuingStrategy {
    constructor(init) {
      if (init === null || typeof init !== "object") {
        throw new TypeError("init must be an object");
      }
      this.highWaterMark = Number(init.highWaterMark);
    }
    size() {
      return 1;
    }
  }
  class ByteLengthQueuingStrategy {
    constructor(init) {
      if (init === null || typeof init !== "object") {
        throw new TypeError("init must be an object");
      }
      this.highWaterMark = Number(init.highWaterMark);
    }
    size(chunk) {
      return chunk.byteLength;
    }
  }

  globalThis.ReadableStream = ReadableStream;
  globalThis.WritableStream = WritableStream;
  globalThis.TransformStream = TransformStream;
  globalThis.TextDecoderStream = TextDecoderStream;
  globalThis.TextEncoderStream = TextEncoderStream;
  globalThis.CountQueuingStrategy = CountQueuingStrategy;
  globalThis.ByteLengthQueuingStrategy = ByteLengthQueuingStrategy;
})();
