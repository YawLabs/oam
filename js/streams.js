// oam web streams: ReadableStream / WritableStream / TransformStream +
// TextEncoderStream / TextDecoderStream (WHATWG Streams, ECMA-429 surface).
//
// The controllers follow the standard's algorithms step for step, in the
// shape node's lib/internal/webstreams uses, so start / pull / write /
// ready / backpressure settle in node's order, microtask for microtask:
//
// - start() runs synchronously in the constructor; once the promise for
//   its result settles the stream is "started", and a ReadableStream then
//   makes its INITIAL pull (CallPullIfNeeded) -- with no read pending, it
//   pulls until the queue reaches the high-water mark.
// - Every enqueue and every read re-checks "should pull" (a reader is
//   waiting, or desiredSize > 0); a pull requested while one is in flight
//   runs once that one settles (pullAgain).
// - A WritableStream queues writes against its strategy's high-water mark,
//   reports desiredSize and backpressure through writer.ready, and hands the
//   sink one chunk at a time once start() has settled.
// - A TransformStream runs transform() only while its readable side wants
//   data: the readable's high-water mark defaults to 0, so a write waits for
//   a read (backpressure), exactly as node's does.
// - pipeTo() waits for the destination's `ready` before each read and does
//   not wait for the write, so the destination fills to its high-water mark;
//   it watches both sides for errors and closes, as node's does.
//
// Scope (documented in docs/node-divergences.md):
// - Default readers only. `type: 'bytes'` streams get the byte controller's
//   queuing -- high-water mark 0 by default, desiredSize in bytes, chunks
//   must be ArrayBufferViews and are read as Uint8Arrays -- but there is no
//   byobRequest, no BYOB reader, and an enqueued buffer is not transferred
//   (node detaches it).
// - The reader, writer and controller classes are not exposed as globals.
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
  // A user may replace Promise.prototype.then; the stream machinery must
  // not route through theirs (node uses primordials for the same reason).
  const PromiseThen = Promise.prototype.then;
  const then = (promise, onFulfilled, onRejected) =>
    PromiseThen.call(promise, onFulfilled, onRejected);
  const noop = () => {};
  const markHandled = (promise) => {
    PromiseThen.call(promise, undefined, noop);
  };

  // node's nonOp* algorithms: async functions, so each returns a promise.
  const nonOpPromise = async () => {};
  const nonOpStart = () => {};

  // node's createPromiseCallback / invokePromiseCallback: the user callback
  // runs inside an async function, so a sync throw becomes a rejection and
  // a returned promise is adopted with the same microtask count as node's.
  const promiseCallback = (fn, thisArg) =>
    async function (a, b) {
      return fn.call(thisArg, a, b);
    };

  // A promise with its resolvers, and whether it has settled (node's
  // isPromisePending, without the V8 internals call).
  function deferred() {
    const d = { promise: undefined, resolve: undefined, reject: undefined, pending: true };
    d.promise = new Promise((resolve, reject) => {
      d.resolve = (value) => {
        d.pending = false;
        resolve(value);
      };
      d.reject = (reason) => {
        d.pending = false;
        reject(reason);
      };
    });
    return d;
  }
  const settled = (promise) => ({ promise, resolve: undefined, reject: undefined, pending: false });

  // node's coded errors come from node_compat.js's registry
  // (lib/internal/errors.js), looked up on first use: the registry is not
  // there yet while this file is evaluated.
  let errorCodes;
  const codes = () => (errorCodes ??= globalThis.__oamNode.get("internal/errors").codes);

  function invalidState(message) {
    return new (codes().ERR_INVALID_STATE.TypeError)(message);
  }

  // node makes each of a released reader's two errors, and a released
  // writer's, once, on first use, and rejects with that one error from
  // then on.
  let releasedError;
  let releasingError;
  let writerReleased;
  const readerReleasedError = () => (releasedError ??= invalidState("Reader released"));
  const readerReleasingError = () => (releasingError ??= invalidState("Releasing reader"));
  const writerReleasedError = () => (writerReleased ??= invalidState("Writer has been released"));

  // The streams' FIFOs -- a stream's chunk queue, its pending reads, a
  // writable's write requests -- with an O(1) shift. Array#shift is O(n) in
  // this engine (it moves every element left), so a queue drained by
  // shift() went quadratic as soon as a producer ran ahead of its consumer:
  // 160k unawaited writer.write() calls took 45 s. A head index walks the
  // backing array instead, and the consumed prefix is dropped when the
  // queue empties or once it is at least half the array (amortized O(1)).
  // Each item's size sits alongside it, and the queue keeps the total (the
  // standard's [[queueTotalSize]]), so size() is never called again at
  // dequeue.
  class Queue {
    constructor() {
      this._items = [];
      this._sizes = [];
      this._head = 0;
      this.totalSize = 0;
    }
    get length() {
      return this._items.length - this._head;
    }
    peek() {
      return this._items[this._head];
    }
    push(item, size = 0) {
      this._items.push(item);
      this._sizes.push(size);
      this.totalSize += size;
    }
    // node's DequeueValue: the total drops by the item's size, floored at 0
    // (and not reset when the queue empties, so rounding shows as node's).
    shift() {
      const items = this._items;
      const sizes = this._sizes;
      const head = this._head;
      if (head === items.length) return undefined;
      const item = items[head];
      this.totalSize = Math.max(0, this.totalSize - sizes[head]);
      if (head + 1 === items.length) {
        items.length = 0;
        sizes.length = 0;
        this._head = 0;
      } else if (head >= 1024 && head * 2 >= items.length) {
        this._items = items.slice(head + 1);
        this._sizes = sizes.slice(head + 1);
        this._head = 0;
      } else {
        items[head] = undefined;
        this._head = head + 1;
      }
      return item;
    }
    clear() {
      this._items = [];
      this._sizes = [];
      this._head = 0;
      this.totalSize = 0;
    }
  }

  // node's extractHighWaterMark: `+value`, and NaN or negative is a
  // RangeError ERR_INVALID_ARG_VALUE (a number inspects as String() does;
  // -0 is not negative).
  function extractHighWaterMark(value, defaultHWM) {
    if (value === undefined) return defaultHWM;
    value = +value;
    if (Number.isNaN(value) || value < 0) {
      throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("strategy.highWaterMark", value);
    }
    return value;
  }

  // Queue accounting goes through strategy.size (ByteLengthQueuingStrategy
  // budgets BYTES); a parallel size ledger avoids re-invoking a possibly
  // impure size() at dequeue. No strategy.size = count semantics (1/chunk).
  const countSize = () => 1;
  function makeSizeFn(sizeFn) {
    if (sizeFn === undefined) return countSize;
    if (typeof sizeFn !== "function") {
      throw new (codes().ERR_INVALID_ARG_TYPE)("strategy.size", "Function", sizeFn);
    }
    // Called as a plain function, `this` undefined, as node calls it.
    return (chunk) => sizeFn(chunk);
  }
  // node's enqueueValueWithSize: the size is `+size`, and NaN, negative or
  // Infinity is a RangeError (which errors the stream).
  function validChunkSize(size) {
    size = +size;
    if (Number.isNaN(size) || size < 0 || size === Infinity) {
      throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("size", size);
    }
    return size;
  }
  // The byte controller queues bytes: a chunk counts its byteLength. Every
  // chunk in a byte stream's queue is a Uint8Array (byteEnqueueChunk), so
  // this never throws -- a size error would error the stream.
  const byteChunkSize = (chunk) => chunk.byteLength;

  // node's ReadableByteStreamController.enqueue checks, in its order, before
  // anything is queued: the chunk must be an ArrayBufferView (a coded
  // TypeError that leaves the stream as it was), then the controller must
  // not be closing and the stream must be readable. A reader is handed a
  // Uint8Array over the chunk's bytes whatever view was enqueued, on the
  // queued path and the waiting-read path alike; a Uint8Array goes through
  // as it is (node hands back a new view over the transferred buffer --
  // oam does not transfer, docs/node-divergences.md).
  function byteEnqueueChunk(stream, chunk) {
    if (!ArrayBuffer.isView(chunk)) {
      throw new (codes().ERR_INVALID_ARG_TYPE)("buffer", ["Buffer", "TypedArray", "DataView"], chunk);
    }
    if (stream._closeRequested) throw invalidState("Controller is already closed");
    if (stream._state !== "readable") throw invalidState("ReadableStream is already closed");
    return Object.getPrototypeOf(chunk) === Uint8Array.prototype
      ? chunk
      : new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  }

  // node's validateObject for a constructor's source / strategy argument:
  // an object, a function or an array, and (for a strategy) null.
  function validateObjectArg(value, name, nullable) {
    if (value === null ? !nullable : typeof value !== "object" && typeof value !== "function") {
      throw new (codes().ERR_INVALID_ARG_TYPE)(name, "Object", value);
    }
  }

  // ------------------------------------------------------- ReadableStream
  class ReadableStreamDefaultController {
    constructor(stream) {
      this._stream = stream;
    }
    get desiredSize() {
      return readableDesiredSize(this._stream);
    }
    enqueue(chunk) {
      const stream = this._stream;
      if (stream._isBytes) chunk = byteEnqueueChunk(stream, chunk);
      else if (!readableCanCloseOrEnqueue(stream)) throw invalidState("Controller is already closed");
      readableEnqueue(stream, chunk);
    }
    close() {
      const stream = this._stream;
      // The byte controller tells a closing controller from a stream that is
      // no longer readable; the default one says the same for both.
      if (stream._closeRequested || (stream._state !== "readable" && !stream._isBytes)) {
        throw invalidState("Controller is already closed");
      }
      if (stream._state !== "readable") throw invalidState("ReadableStream is already closed");
      readableControllerClose(stream);
    }
    error(reason) {
      readableError(this._stream, reason);
    }
  }

  function initReadable(stream, highWaterMark, sizeFn, isBytes = false) {
    stream._queue = new Queue();
    stream._state = "readable"; // readable | closed | errored
    stream._error = undefined;
    stream._reader = null; // the active default reader (lock)
    stream._waiters = new Queue(); // pending read() resolvers: {resolve, reject}
    stream._highWaterMark = highWaterMark;
    stream._sizeFn = sizeFn;
    // A `type: 'bytes'` stream: its controller enqueues only
    // ArrayBufferViews, handed on as Uint8Arrays.
    stream._isBytes = isBytes;
    stream._started = false;
    stream._pulling = false;
    stream._pullAgain = false;
    // close() was called with chunks still queued: the stream closes when
    // the last of them is read.
    stream._closeRequested = false;
    stream._pullAlgorithm = nonOpPromise;
    stream._cancelAlgorithm = nonOpPromise;
    // Has anything been read from, or cancelled on, this stream? A fetch
    // body asks (js/bootstrap.js): a disturbed stream cannot be a body, and
    // a body whose stream was read is `bodyUsed`. Never reset -- releasing
    // the reader does not un-read the chunks.
    stream._disturbed = false;
    stream._resolveClosed = undefined;
    stream._rejectClosed = undefined;
    stream._controller = new ReadableStreamDefaultController(stream);
  }

  // start() runs SYNCHRONOUSLY, inside the constructor (WHATWG "set up
  // readable stream default controller": the start algorithm is performed,
  // then its result is wrapped in a promise). Code relies on it: it
  // captures the controller in start() and enqueues as soon as `new
  // ReadableStream(...)` returns -- the MCP SDK's streamable-HTTP server
  // does, and with start() deferred to a microtask its controller was still
  // undefined, so every response body it wrote was dropped. A throw from
  // start() is the constructor's throw; only a REJECTED start promise
  // errors the stream. Once the start promise settles the stream is
  // started and makes its first pull -- node wraps the result in `new
  // Promise((r) => r(result))`, so a thenable or a promise result costs
  // the same extra microtasks it costs there.
  function setupReadable(stream, startAlgorithm, pullAlgorithm, cancelAlgorithm) {
    stream._pullAlgorithm = pullAlgorithm;
    stream._cancelAlgorithm = cancelAlgorithm;
    const startResult = startAlgorithm(stream._controller);
    then(
      new Promise((resolve) => resolve(startResult)),
      () => {
        stream._started = true;
        readableCallPullIfNeeded(stream);
      },
      (error) => readableError(stream, error),
    );
  }

  function readableCanCloseOrEnqueue(stream) {
    return stream._state === "readable" && !stream._closeRequested;
  }

  function readableDesiredSize(stream) {
    if (stream._state === "errored") return null;
    if (stream._state === "closed") return 0;
    return stream._highWaterMark - stream._queue.totalSize;
  }

  function readableShouldCallPull(stream) {
    if (!readableCanCloseOrEnqueue(stream) || !stream._started) return false;
    if (stream._reader !== null && stream._waiters.length > 0) return true;
    return readableDesiredSize(stream) > 0;
  }

  function readableCallPullIfNeeded(stream) {
    if (!readableShouldCallPull(stream)) return;
    if (stream._pulling) {
      stream._pullAgain = true;
      return;
    }
    stream._pulling = true;
    then(
      stream._pullAlgorithm(stream._controller),
      () => {
        stream._pulling = false;
        // Re-pull ONLY when something asked during the pull (pullAgain). A
        // pull that settles without enqueueing, while a read waits, does
        // not pull again on its own: a source that defers its enqueue (to
        // process.nextTick, a socket) resolves the waiter when it lands,
        // and that enqueue asks again.
        if (stream._pullAgain) {
          stream._pullAgain = false;
          readableCallPullIfNeeded(stream);
        }
      },
      (error) => readableError(stream, error),
    );
  }

  function readableEnqueue(stream, chunk) {
    if (stream._waiters.length > 0) {
      stream._waiters.shift().resolve({ value: chunk, done: false });
    } else {
      let size;
      try {
        size = validChunkSize(stream._sizeFn(chunk));
      } catch (error) {
        readableError(stream, error);
        throw error;
      }
      stream._queue.push(chunk, size);
    }
    readableCallPullIfNeeded(stream);
  }

  function resetReadableQueue(stream) {
    stream._queue.clear();
  }

  function readableClearAlgorithms(stream) {
    stream._pullAlgorithm = nonOpPromise;
    stream._cancelAlgorithm = nonOpPromise;
    stream._sizeFn = countSize;
  }

  // With chunks still queued the close waits for the last of them to be
  // read (closeRequested); otherwise the stream closes now. closeRequested
  // is only set in the first case, as node's byte controller sets it: an
  // enqueue then tells "Controller is already closed" from "ReadableStream
  // is already closed" (the default controller says the former for both).
  function readableControllerClose(stream) {
    if (stream._queue.length > 0) {
      stream._closeRequested = true;
      return;
    }
    readableClearAlgorithms(stream);
    readableClose(stream);
  }

  // ReadableStreamClose: the stream is closed, then reader.closed settles,
  // then the pending reads are done (node's order).
  function readableClose(stream) {
    stream._state = "closed";
    stream._resolveClosed?.();
    while (stream._waiters.length > 0) {
      stream._waiters.shift().resolve({ value: undefined, done: true });
    }
  }

  function readableError(stream, reason) {
    if (stream._state !== "readable") return;
    resetReadableQueue(stream);
    readableClearAlgorithms(stream);
    stream._state = "errored";
    stream._error = reason;
    stream._rejectClosed?.(reason);
    while (stream._waiters.length > 0) {
      stream._waiters.shift().reject(reason);
    }
  }

  function readableCancel(stream, reason) {
    stream._disturbed = true;
    if (stream._state === "closed") return Promise.resolve();
    if (stream._state === "errored") return Promise.reject(stream._error);
    readableClose(stream);
    resetReadableQueue(stream);
    const result = stream._cancelAlgorithm(reason);
    readableClearAlgorithms(stream);
    return then(result, noop);
  }

  // A default reader: `closed` follows the stream, `read()` takes the next
  // chunk. Shared by getReader() and pipeTo().
  function acquireDefaultReader(stream) {
    // node tags this ERR_INVALID_STATE, and callers key on the code --
    // stream/consumers' rejection is asserted by code, not by message.
    if (stream._reader !== null) throw invalidState("ReadableStream is locked");
    let closedResolve;
    let closedReject;
    const closed = new Promise((resolve, reject) => {
      closedResolve = resolve;
      closedReject = reject;
    });
    markHandled(closed); // observable via reader.closed; never unhandled
    if (stream._state === "closed") closedResolve();
    if (stream._state === "errored") closedReject(stream._error);
    stream._resolveClosed = closedResolve;
    stream._rejectClosed = closedReject;

    const reader = {
      closed,
      read() {
        if (stream._reader !== reader) {
          return Promise.reject(invalidState("The reader is not attached to a stream"));
        }
        return new Promise((resolve, reject) => readableReaderRead(stream, { resolve, reject }));
      },
      cancel(reason) {
        if (stream._reader !== reader) {
          return Promise.reject(invalidState("The reader is not attached to a stream"));
        }
        return readableCancel(stream, reason);
      },
      // node's ReadableStreamDefaultReaderRelease: the generic release,
      // then every pending read rejects with "Releasing reader".
      releaseLock() {
        if (stream._reader !== reader) return;
        readableReaderGenericRelease(stream, reader);
        const releasing = readerReleasingError();
        while (stream._waiters.length > 0) stream._waiters.shift().reject(releasing);
      },
    };
    stream._reader = reader;
    return reader;
  }

  // node's ReadableStreamDefaultReaderRead: a request is {resolve, reject};
  // resolve takes the {value, done} result, synchronously when a chunk is
  // queued, or when one is enqueued (a pipe acts on it right there).
  function readableReaderRead(stream, request) {
    stream._disturbed = true;
    if (stream._state === "closed") {
      request.resolve({ value: undefined, done: true });
    } else if (stream._state === "errored") {
      request.reject(stream._error);
    } else if (stream._queue.length > 0) {
      const value = stream._queue.shift();
      if (stream._closeRequested && stream._queue.length === 0) {
        readableClearAlgorithms(stream);
        readableClose(stream);
      } else {
        readableCallPullIfNeeded(stream);
      }
      request.resolve({ value, done: false });
    } else {
      stream._waiters.push(request);
      readableCallPullIfNeeded(stream);
    }
  }

  // node's ReadableStreamReaderGenericRelease: `closed` rejects with
  // "Reader released" (a stream that has already closed or errored gives
  // the reader a new, rejected `closed`), and the lock is dropped.
  function readableReaderGenericRelease(stream, reader) {
    const released = readerReleasedError();
    if (stream._state === "readable") stream._rejectClosed(released);
    else reader.closed = Promise.reject(released);
    markHandled(reader.closed);
    stream._reader = null;
    stream._resolveClosed = undefined;
    stream._rejectClosed = undefined;
  }

  // node's validateAbortSignal: duck-typed on `aborted`.
  function validateAbortSignal(signal, name) {
    if (signal === null || typeof signal !== "object" || !("aborted" in signal)) {
      throw new (codes().ERR_INVALID_ARG_TYPE)(name, "AbortSignal", signal);
    }
  }

  // node's readableStreamPipeTo, the standard's ReadableStreamPipeTo: each
  // step waits for the destination's `ready`, then reads, then starts the
  // write without waiting for it -- so the destination's queue fills to its
  // high-water mark, and the source is pulled as far ahead as node pulls
  // it. Errors and closes on either side are watched while a read or a
  // write is pending, and shut the pipe down with node's actions: an
  // errored source aborts the destination, an errored destination cancels
  // the source, a closed source closes the destination (each unless
  // prevented), and an abort signal does both.
  function readableStreamPipeTo(source, dest, preventClose, preventAbort, preventCancel, signal) {
    let reader;
    let writer;
    try {
      reader = acquireDefaultReader(source);
      writer = new WritableStreamDefaultWriter(dest);
    } catch (error) {
      return Promise.reject(error);
    }
    source._disturbed = true;
    let shuttingDown = false;
    let removeAbortListener;
    const promise = deferred();
    let currentWrite = Promise.resolve();

    function finalize(rejected, error) {
      writerRelease(writer);
      readableReaderGenericRelease(source, reader);
      removeAbortListener?.();
      if (rejected) promise.reject(error);
      else promise.resolve();
    }

    async function waitForCurrentWrite() {
      const write = currentWrite;
      await write;
      if (write !== currentWrite) await waitForCurrentWrite();
    }

    function shutdownWithAnAction(action, rejected, originalError) {
      if (shuttingDown) return;
      shuttingDown = true;
      const complete = () =>
        then(
          action(),
          () => finalize(rejected, originalError),
          (error) => finalize(true, error),
        );
      if (dest._state === "writable" && !writableCloseQueuedOrInFlight(dest)) {
        then(waitForCurrentWrite(), complete, (error) => finalize(true, error));
        return;
      }
      complete();
    }

    function shutdown(rejected, error) {
      if (shuttingDown) return;
      shuttingDown = true;
      if (dest._state === "writable" && !writableCloseQueuedOrInFlight(dest)) {
        then(
          waitForCurrentWrite(),
          () => finalize(rejected, error),
          (err) => finalize(true, err),
        );
        return;
      }
      finalize(rejected, error);
    }

    function abortAlgorithm() {
      const error = signal.reason;
      const actions = [];
      if (!preventAbort) {
        actions.push(() =>
          dest._state === "writable" ? writableAbort(dest, error) : Promise.resolve(),
        );
      }
      if (!preventCancel) {
        actions.push(() =>
          source._state === "readable" ? readableCancel(source, error) : Promise.resolve(),
        );
      }
      shutdownWithAnAction(() => Promise.all(actions.map((action) => action())), true, error);
    }

    function watchErrored(stream, closedPromise, storedError, action) {
      if (stream._state === "errored") action(storedError());
      else then(closedPromise, undefined, action);
    }

    function watchClosed(stream, closedPromise, action) {
      if (stream._state === "closed") action();
      else then(closedPromise, action, noop);
    }

    // One read: true once the source is done (or the pipe shutting down).
    async function step() {
      if (shuttingDown) return true;
      await writer._ready.promise;
      return new Promise((resolve, reject) => {
        readableReaderRead(source, {
          resolve(result) {
            if (result.done) {
              resolve(true);
              return;
            }
            currentWrite = writerWrite(writer, result.value);
            markHandled(currentWrite);
            resolve(false);
          },
          reject,
        });
      });
    }

    async function run() {
      while (!(await step()));
    }

    if (signal !== undefined) {
      if (signal.aborted) {
        abortAlgorithm();
        return promise.promise;
      }
      signal.addEventListener("abort", abortAlgorithm, { once: true });
      removeAbortListener = () => signal.removeEventListener("abort", abortAlgorithm);
    }

    markHandled(run());

    watchErrored(source, reader.closed, () => source._error, (error) => {
      if (!preventAbort) {
        shutdownWithAnAction(() => writableAbort(dest, error), true, error);
        return;
      }
      shutdown(true, error);
    });

    watchErrored(dest, writer._closed.promise, () => dest._storedError, (error) => {
      if (!preventCancel) {
        shutdownWithAnAction(() => readableCancel(source, error), true, error);
        return;
      }
      shutdown(true, error);
    });

    watchClosed(source, reader.closed, () => {
      if (!preventClose) {
        shutdownWithAnAction(() => writerCloseWithErrorPropagation(writer));
        return;
      }
      shutdown();
    });

    if (writableCloseQueuedOrInFlight(dest) || dest._state === "closed") {
      const error = invalidState("Destination WritableStream is closed");
      if (!preventCancel) shutdownWithAnAction(() => readableCancel(source, error), true, error);
      else shutdown(true, error);
    }

    return promise.promise;
  }

  // A stream built over internal algorithms (TransformStream's readable
  // side), as node's createReadableStream builds one.
  function createReadable(startAlgorithm, pullAlgorithm, cancelAlgorithm, highWaterMark, sizeFn) {
    const stream = Object.create(ReadableStream.prototype);
    initReadable(stream, highWaterMark, sizeFn);
    setupReadable(stream, startAlgorithm, pullAlgorithm, cancelAlgorithm);
    return stream;
  }

  class ReadableStream {
    constructor(source = {}, strategy = {}) {
      validateObjectArg(source, "source", false);
      validateObjectArg(strategy, "strategy", true);
      // node's order, which is the standard's: the strategy's size and
      // highWaterMark are read before anything of the source, then
      // source.type, which is "bytes" when it stringifies so. A byte stream
      // queues bytes, so it takes no size(); its high-water mark defaults to
      // 0: nothing is pulled until a read asks. Any other type is refused.
      const size = strategy?.size;
      const highWaterMark = strategy?.highWaterMark;
      const type = source.type;
      let isBytes = false;
      if (`${type}` === "bytes") {
        if (size !== undefined) {
          throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("strategy.size", size);
        }
        isBytes = true;
      } else if (type !== undefined) {
        throw new (codes().ERR_INVALID_ARG_VALUE)("source.type", type);
      }
      initReadable(
        this,
        extractHighWaterMark(highWaterMark, isBytes ? 0 : 1),
        isBytes ? byteChunkSize : makeSizeFn(size),
        isBytes,
      );
      const start = source.start;
      const pull = source.pull;
      const cancel = source.cancel;
      if (isBytes) {
        // Read, and 0 refused, as node does; there is no byobRequest for
        // it to size (docs/node-divergences.md).
        const autoAllocateChunkSize = source.autoAllocateChunkSize;
        if (autoAllocateChunkSize === 0) {
          throw new (codes().ERR_INVALID_ARG_VALUE)(
            "source.autoAllocateChunkSize",
            autoAllocateChunkSize,
          );
        }
      }
      setupReadable(
        this,
        typeof start === "function" ? (controller) => start.call(source, controller) : nonOpStart,
        typeof pull === "function" ? promiseCallback(pull, source) : nonOpPromise,
        typeof cancel === "function" ? promiseCallback(cancel, source) : nonOpPromise,
      );
    }

    get locked() {
      return this._reader !== null;
    }

    getReader() {
      return acquireDefaultReader(this);
    }

    cancel(reason) {
      if (this.locked) return Promise.reject(invalidState("ReadableStream is locked"));
      return readableCancel(this, reason);
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
      const queues = [new Queue(), new Queue()];
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

    // node's argument checks, in node's order; a failed check is a
    // rejected promise from pipeTo and a throw from pipeThrough.
    pipeTo(destination, options = {}) {
      try {
        if (!(destination instanceof WritableStream)) {
          throw new (codes().ERR_INVALID_ARG_TYPE)("transform.writable", "WritableStream", destination);
        }
        validateObjectArg(options, "options", true);
        const preventAbort = options?.preventAbort;
        const preventCancel = options?.preventCancel;
        const preventClose = options?.preventClose;
        const signal = options?.signal;
        if (signal !== undefined) validateAbortSignal(signal, "options.signal");
        if (this.locked) throw invalidState("The ReadableStream is locked");
        if (destination.locked) throw invalidState("The WritableStream is locked");
        return readableStreamPipeTo(
          this,
          destination,
          !!preventClose,
          !!preventAbort,
          !!preventCancel,
          signal,
        );
      } catch (error) {
        return Promise.reject(error);
      }
    }

    pipeThrough(transform, options = {}) {
      const readable = transform?.readable;
      if (!(readable instanceof ReadableStream)) {
        throw new (codes().ERR_INVALID_ARG_TYPE)("transform.readable", "ReadableStream", readable);
      }
      const writable = transform?.writable;
      if (!(writable instanceof WritableStream)) {
        throw new (codes().ERR_INVALID_ARG_TYPE)("transform.writable", "WritableStream", writable);
      }
      validateObjectArg(options, "options", true);
      const preventAbort = options?.preventAbort;
      const preventCancel = options?.preventCancel;
      const preventClose = options?.preventClose;
      const signal = options?.signal;
      if (signal !== undefined) validateAbortSignal(signal, "options.signal");
      if (this.locked) throw invalidState("The ReadableStream is locked");
      if (writable.locked) throw invalidState("The WritableStream is locked");
      // The pipe runs detached; its errors surface through the readable.
      markHandled(
        readableStreamPipeTo(this, writable, !!preventClose, !!preventAbort, !!preventCancel, signal),
      );
      return readable;
    }

    static from(source) {
      const iterator =
        source[Symbol.asyncIterator]?.() ?? source[Symbol.iterator]?.();
      if (!iterator) {
        throw new TypeError("ReadableStream.from requires an iterable");
      }
      // node builds it with a high-water mark of 0: the iterator is not
      // advanced until a read asks for a value.
      return new ReadableStream(
        {
          async pull(controller) {
            const { value, done } = await iterator.next();
            if (done) controller.close();
            else controller.enqueue(value);
          },
          async cancel(reason) {
            await iterator.return?.(reason);
          },
        },
        { highWaterMark: 0 },
      );
    }
  }

  // ------------------------------------------------------- WritableStream
  const CLOSE_SENTINEL = {};

  class WritableStreamDefaultController {
    constructor(stream, highWaterMark, sizeFn) {
      this._stream = stream;
      this._queue = new Queue();
      this._highWaterMark = highWaterMark;
      this._sizeFn = sizeFn;
      this._started = false;
      this._writeAlgorithm = nonOpPromise;
      this._closeAlgorithm = nonOpPromise;
      this._abortAlgorithm = nonOpPromise;
      // The AbortController behind `signal` is made on first ask:
      // AbortController is defined by bootstrap.js, and most sinks never
      // look at the signal. An abort that lands first is remembered.
      this._abortController = undefined;
      this._aborted = false;
      this._abortReason = undefined;
    }
    get signal() {
      if (this._abortController === undefined) {
        this._abortController = new globalThis.AbortController();
        if (this._aborted) this._abortController.abort(this._abortReason);
      }
      return this._abortController.signal;
    }
    error(error) {
      if (this._stream._state !== "writable") return;
      writableControllerError(this, error);
    }
  }

  function writableSignalAbort(controller, reason) {
    if (controller._abortController !== undefined) {
      controller._abortController.abort(reason);
    } else if (!controller._aborted) {
      controller._aborted = true;
      controller._abortReason = reason;
    }
  }

  function initWritable(stream) {
    stream._state = "writable"; // writable | erroring | errored | closed
    stream._storedError = undefined;
    stream._writer = undefined;
    stream._controller = undefined;
    stream._writeRequests = new Queue();
    stream._inFlightWriteRequest = undefined;
    stream._closeRequest = undefined;
    stream._inFlightCloseRequest = undefined;
    stream._pendingAbortRequest = undefined;
    stream._backpressure = false;
  }

  // start() runs synchronously, in the constructor, as the ReadableStream
  // one does and for the same reason; writes queue until the promise for
  // its result settles. (TransformStream's transformer.start rides on its
  // own start promise.)
  function setupWritable(
    stream,
    startAlgorithm,
    writeAlgorithm,
    closeAlgorithm,
    abortAlgorithm,
    highWaterMark,
    sizeFn,
  ) {
    const controller = new WritableStreamDefaultController(stream, highWaterMark, sizeFn);
    controller._writeAlgorithm = writeAlgorithm;
    controller._closeAlgorithm = closeAlgorithm;
    controller._abortAlgorithm = abortAlgorithm;
    stream._controller = controller;
    writableUpdateBackpressure(stream, writableGetBackpressure(controller));
    const startResult = startAlgorithm(controller);
    then(
      new Promise((resolve) => resolve(startResult)),
      () => {
        controller._started = true;
        writableAdvanceQueueIfNeeded(controller);
      },
      (error) => {
        controller._started = true;
        writableDealWithRejection(stream, error);
      },
    );
  }

  function createWritable(
    startAlgorithm,
    writeAlgorithm,
    closeAlgorithm,
    abortAlgorithm,
    highWaterMark,
    sizeFn,
  ) {
    const stream = Object.create(WritableStream.prototype);
    initWritable(stream);
    setupWritable(
      stream,
      startAlgorithm,
      writeAlgorithm,
      closeAlgorithm,
      abortAlgorithm,
      highWaterMark,
      sizeFn,
    );
    return stream;
  }

  function writableCloseQueuedOrInFlight(stream) {
    return stream._closeRequest !== undefined || stream._inFlightCloseRequest !== undefined;
  }

  function writableHasOperationMarkedInFlight(stream) {
    return stream._inFlightWriteRequest !== undefined || stream._inFlightCloseRequest !== undefined;
  }

  function writableAbort(stream, reason) {
    const state = stream._state;
    if (state === "closed" || state === "errored") return Promise.resolve();
    writableSignalAbort(stream._controller, reason);
    if (stream._pendingAbortRequest !== undefined) {
      return stream._pendingAbortRequest.promise;
    }
    let wasAlreadyErroring = false;
    if (state === "erroring") {
      wasAlreadyErroring = true;
      reason = undefined;
    }
    const request = deferred();
    request.reason = reason;
    request.wasAlreadyErroring = wasAlreadyErroring;
    stream._pendingAbortRequest = request;
    if (!wasAlreadyErroring) writableStartErroring(stream, reason);
    return request.promise;
  }

  function writableClose(stream) {
    const state = stream._state;
    if (state === "closed" || state === "errored") {
      return Promise.reject(invalidState("WritableStream is closed"));
    }
    const request = deferred();
    stream._closeRequest = request;
    const writer = stream._writer;
    if (writer !== undefined && stream._backpressure && state === "writable") {
      writer._ready.resolve?.();
    }
    writableControllerClose(stream._controller);
    return request.promise;
  }

  function writableUpdateBackpressure(stream, backpressure) {
    const writer = stream._writer;
    if (writer !== undefined && stream._backpressure !== backpressure) {
      if (backpressure) writer._ready = deferred();
      else writer._ready.resolve?.();
    }
    stream._backpressure = backpressure;
  }

  function writableStartErroring(stream, reason) {
    const controller = stream._controller;
    stream._state = "erroring";
    stream._storedError = reason;
    const writer = stream._writer;
    if (writer !== undefined) writerEnsureReadyPromiseRejected(writer, reason);
    if (!writableHasOperationMarkedInFlight(stream) && controller._started) {
      writableFinishErroring(stream);
    }
  }

  function writableRejectCloseAndClosedPromiseIfNeeded(stream) {
    if (stream._closeRequest !== undefined) {
      stream._closeRequest.reject(stream._storedError);
      stream._closeRequest = undefined;
    }
    const writer = stream._writer;
    if (writer !== undefined) {
      markHandled(writer._closed.promise);
      writer._closed.reject?.(stream._storedError);
    }
  }

  function writableFinishErroring(stream) {
    stream._state = "errored";
    const controller = stream._controller;
    controller._queue.clear();
    const storedError = stream._storedError;
    const writeRequests = stream._writeRequests;
    while (writeRequests.length > 0) writeRequests.shift().reject(storedError);
    const abortRequest = stream._pendingAbortRequest;
    if (abortRequest === undefined) {
      writableRejectCloseAndClosedPromiseIfNeeded(stream);
      return;
    }
    stream._pendingAbortRequest = undefined;
    if (abortRequest.wasAlreadyErroring) {
      abortRequest.reject(storedError);
      writableRejectCloseAndClosedPromiseIfNeeded(stream);
      return;
    }
    const result = controller._abortAlgorithm(abortRequest.reason);
    writableClearAlgorithms(controller);
    then(
      result,
      () => {
        abortRequest.resolve();
        writableRejectCloseAndClosedPromiseIfNeeded(stream);
      },
      (error) => {
        abortRequest.reject(error);
        writableRejectCloseAndClosedPromiseIfNeeded(stream);
      },
    );
  }

  function writableDealWithRejection(stream, error) {
    if (stream._state === "writable") {
      writableStartErroring(stream, error);
      return;
    }
    writableFinishErroring(stream);
  }

  function writableFinishInFlightWrite(stream) {
    stream._inFlightWriteRequest.resolve();
    stream._inFlightWriteRequest = undefined;
  }

  function writableFinishInFlightWriteWithError(stream, error) {
    stream._inFlightWriteRequest.reject(error);
    stream._inFlightWriteRequest = undefined;
    writableDealWithRejection(stream, error);
  }

  function writableFinishInFlightClose(stream) {
    stream._inFlightCloseRequest.resolve();
    stream._inFlightCloseRequest = undefined;
    if (stream._state === "erroring") {
      stream._storedError = undefined;
      if (stream._pendingAbortRequest !== undefined) {
        stream._pendingAbortRequest.resolve();
        stream._pendingAbortRequest = undefined;
      }
    }
    stream._state = "closed";
    stream._writer?._closed.resolve?.();
  }

  function writableFinishInFlightCloseWithError(stream, error) {
    stream._inFlightCloseRequest.reject(error);
    stream._inFlightCloseRequest = undefined;
    if (stream._pendingAbortRequest !== undefined) {
      stream._pendingAbortRequest.reject(error);
      stream._pendingAbortRequest = undefined;
    }
    writableDealWithRejection(stream, error);
  }

  function writableControllerError(controller, error) {
    writableClearAlgorithms(controller);
    writableStartErroring(controller._stream, error);
  }

  function writableControllerErrorIfNeeded(controller, error) {
    if (controller._stream._state === "writable") writableControllerError(controller, error);
  }

  function writableClearAlgorithms(controller) {
    controller._writeAlgorithm = undefined;
    controller._closeAlgorithm = undefined;
    controller._abortAlgorithm = undefined;
    controller._sizeFn = undefined;
  }

  function writableDesiredSize(controller) {
    return controller._highWaterMark - controller._queue.totalSize;
  }

  function writableGetBackpressure(controller) {
    return writableDesiredSize(controller) <= 0;
  }

  function writableGetChunkSize(controller, chunk) {
    if (controller._sizeFn === undefined) return 1;
    try {
      return controller._sizeFn(chunk);
    } catch (error) {
      writableControllerErrorIfNeeded(controller, error);
      return 1;
    }
  }

  function writableControllerWrite(controller, chunk, chunkSize) {
    try {
      chunkSize = validChunkSize(chunkSize);
    } catch (error) {
      writableControllerErrorIfNeeded(controller, error);
      return;
    }
    controller._queue.push(chunk, chunkSize);
    const stream = controller._stream;
    if (!writableCloseQueuedOrInFlight(stream) && stream._state === "writable") {
      writableUpdateBackpressure(stream, writableGetBackpressure(controller));
    }
    writableAdvanceQueueIfNeeded(controller);
  }

  function writableControllerClose(controller) {
    controller._queue.push(CLOSE_SENTINEL, 0);
    writableAdvanceQueueIfNeeded(controller);
  }

  function writableDequeue(controller) {
    controller._queue.shift();
  }

  function writableAdvanceQueueIfNeeded(controller) {
    const stream = controller._stream;
    if (!controller._started || stream._inFlightWriteRequest !== undefined) return;
    if (stream._state === "erroring") {
      writableFinishErroring(stream);
      return;
    }
    if (controller._queue.length === 0) return;
    const value = controller._queue.peek();
    if (value === CLOSE_SENTINEL) writableProcessClose(controller);
    else writableProcessWrite(controller, value);
  }

  function writableProcessClose(controller) {
    const stream = controller._stream;
    stream._inFlightCloseRequest = stream._closeRequest;
    stream._closeRequest = undefined;
    writableDequeue(controller);
    const sinkClosePromise = controller._closeAlgorithm();
    writableClearAlgorithms(controller);
    then(
      sinkClosePromise,
      () => writableFinishInFlightClose(stream),
      (error) => writableFinishInFlightCloseWithError(stream, error),
    );
  }

  function writableProcessWrite(controller, chunk) {
    const stream = controller._stream;
    stream._inFlightWriteRequest = stream._writeRequests.shift();
    then(
      controller._writeAlgorithm(chunk, controller),
      () => {
        writableFinishInFlightWrite(stream);
        const state = stream._state;
        writableDequeue(controller);
        if (!writableCloseQueuedOrInFlight(stream) && state === "writable") {
          writableUpdateBackpressure(stream, writableGetBackpressure(controller));
        }
        writableAdvanceQueueIfNeeded(controller);
      },
      (error) => {
        if (stream._state === "writable") writableClearAlgorithms(controller);
        writableFinishInFlightWriteWithError(stream, error);
      },
    );
  }

  function writerEnsureReadyPromiseRejected(writer, error) {
    if (writer._ready.pending) writer._ready.reject(error);
    else writer._ready = settled(Promise.reject(error));
    markHandled(writer._ready.promise);
  }

  function writerEnsureClosedPromiseRejected(writer, error) {
    if (writer._closed.pending) writer._closed.reject(error);
    else writer._closed = settled(Promise.reject(error));
    markHandled(writer._closed.promise);
  }

  // node's WritableStreamDefaultWriterWrite. Shared by writer.write() and
  // pipeTo().
  function writerWrite(writer, chunk) {
    const stream = writer._stream;
    const controller = stream._controller;
    const chunkSize = writableGetChunkSize(controller, chunk);
    const state = stream._state;
    if (state === "errored") return Promise.reject(stream._storedError);
    if (writableCloseQueuedOrInFlight(stream) || state === "closed") {
      return Promise.reject(invalidState("WritableStream is closed"));
    }
    if (state === "erroring") return Promise.reject(stream._storedError);
    const request = deferred();
    stream._writeRequests.push(request);
    writableControllerWrite(controller, chunk, chunkSize);
    return request.promise;
  }

  // node's WritableStreamDefaultWriterRelease.
  function writerRelease(writer) {
    const released = writerReleasedError();
    writerEnsureReadyPromiseRejected(writer, released);
    writerEnsureClosedPromiseRejected(writer, released);
    writer._stream._writer = undefined;
    writer._stream = undefined;
  }

  // node's WritableStreamDefaultWriterCloseWithErrorPropagation: what a
  // pipe does when its source closes.
  function writerCloseWithErrorPropagation(writer) {
    const stream = writer._stream;
    const state = stream._state;
    if (writableCloseQueuedOrInFlight(stream) || state === "closed") return Promise.resolve();
    if (state === "errored") return Promise.reject(stream._storedError);
    return writableClose(stream);
  }

  class WritableStreamDefaultWriter {
    constructor(stream) {
      if (stream._writer !== undefined) throw invalidState("WritableStream is locked");
      this._stream = stream;
      stream._writer = this;
      switch (stream._state) {
        case "writable":
          this._ready =
            !writableCloseQueuedOrInFlight(stream) && stream._backpressure
              ? deferred()
              : settled(Promise.resolve());
          this._closed = deferred();
          break;
        case "erroring":
          this._ready = settled(Promise.reject(stream._storedError));
          markHandled(this._ready.promise);
          this._closed = deferred();
          break;
        case "closed":
          this._ready = settled(Promise.resolve());
          this._closed = settled(Promise.resolve());
          break;
        default:
          this._ready = settled(Promise.reject(stream._storedError));
          this._closed = settled(Promise.reject(stream._storedError));
          markHandled(this._ready.promise);
          markHandled(this._closed.promise);
      }
    }

    get closed() {
      return this._closed.promise;
    }

    get ready() {
      return this._ready.promise;
    }

    get desiredSize() {
      const stream = this._stream;
      if (stream === undefined) throw invalidState("Writer is not bound to a WritableStream");
      switch (stream._state) {
        case "errored":
        case "erroring":
          return null;
        case "closed":
          return 0;
      }
      return writableDesiredSize(stream._controller);
    }

    write(chunk) {
      const stream = this._stream;
      if (stream === undefined) {
        return Promise.reject(invalidState("Writer is not bound to a WritableStream"));
      }
      return writerWrite(this, chunk);
    }

    close() {
      const stream = this._stream;
      if (stream === undefined) {
        return Promise.reject(invalidState("Writer is not bound to a WritableStream"));
      }
      if (writableCloseQueuedOrInFlight(stream)) {
        return Promise.reject(invalidState("Failure to close WritableStream"));
      }
      return writableClose(stream);
    }

    abort(reason) {
      const stream = this._stream;
      if (stream === undefined) {
        return Promise.reject(invalidState("Writer is not bound to a WritableStream"));
      }
      return writableAbort(stream, reason);
    }

    releaseLock() {
      if (this._stream === undefined) return;
      writerRelease(this);
    }
  }

  class WritableStream {
    constructor(sink = {}, strategy = {}) {
      validateObjectArg(sink, "sink", false);
      validateObjectArg(strategy, "strategy", true);
      const type = sink?.type;
      if (type !== undefined) throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("type", type);
      // node's order: size, then highWaterMark.
      const size = makeSizeFn(strategy?.size);
      const highWaterMark = extractHighWaterMark(strategy?.highWaterMark, 1);
      initWritable(this);
      const start = sink.start;
      const write = sink.write;
      const close = sink.close;
      const abort = sink.abort;
      setupWritable(
        this,
        typeof start === "function" ? (controller) => start.call(sink, controller) : nonOpStart,
        typeof write === "function" ? promiseCallback(write, sink) : nonOpPromise,
        typeof close === "function" ? promiseCallback(close, sink) : nonOpPromise,
        typeof abort === "function" ? promiseCallback(abort, sink) : nonOpPromise,
        highWaterMark,
        size,
      );
    }

    get locked() {
      return this._writer !== undefined;
    }

    getWriter() {
      return new WritableStreamDefaultWriter(this);
    }

    abort(reason) {
      if (this._writer !== undefined) {
        return Promise.reject(invalidState("WritableStream is locked"));
      }
      return writableAbort(this, reason);
    }

    close() {
      if (this._writer !== undefined) {
        return Promise.reject(invalidState("WritableStream is locked"));
      }
      if (writableCloseQueuedOrInFlight(this)) {
        return Promise.reject(invalidState("Failure closing WritableStream"));
      }
      return writableClose(this);
    }
  }

  // ------------------------------------------------------ TransformStream
  class TransformStreamDefaultController {
    constructor(stream, transformAlgorithm, flushAlgorithm, cancelAlgorithm) {
      this._stream = stream;
      this._transformAlgorithm = transformAlgorithm;
      this._flushAlgorithm = flushAlgorithm;
      this._cancelAlgorithm = cancelAlgorithm;
      this._finishPromise = undefined;
    }
    get desiredSize() {
      return readableDesiredSize(this._stream._readable);
    }
    enqueue(chunk) {
      transformEnqueue(this, chunk);
    }
    error(reason) {
      transformError(this._stream, reason);
    }
    terminate() {
      const stream = this._stream;
      const readable = stream._readable;
      if (readableCanCloseOrEnqueue(readable)) readableControllerClose(readable);
      transformErrorWritableAndUnblockWrite(
        stream,
        invalidState("TransformStream has been terminated"),
      );
    }
  }

  // No `transform` means the IDENTITY transform, which enqueues the chunk
  // unchanged -- per spec, and it is the whole point of the bare `new
  // TransformStream()` used as a plumbing pipe.
  async function identityTransform(chunk, controller) {
    transformEnqueue(controller, chunk);
  }

  function transformSetBackpressure(stream, backpressure) {
    stream._backpressureChange?.resolve();
    stream._backpressureChange = deferred();
    stream._backpressure = backpressure;
  }

  function transformUnblockWrite(stream) {
    if (stream._backpressure) transformSetBackpressure(stream, false);
  }

  function transformClearAlgorithms(controller) {
    controller._transformAlgorithm = undefined;
    controller._flushAlgorithm = undefined;
    controller._cancelAlgorithm = undefined;
  }

  function transformErrorWritableAndUnblockWrite(stream, error) {
    transformClearAlgorithms(stream._controller);
    writableControllerErrorIfNeeded(stream._writable._controller, error);
    transformUnblockWrite(stream);
  }

  function transformError(stream, error) {
    readableError(stream._readable, error);
    transformErrorWritableAndUnblockWrite(stream, error);
  }

  function transformEnqueue(controller, chunk) {
    const stream = controller._stream;
    const readable = stream._readable;
    if (!readableCanCloseOrEnqueue(readable)) throw invalidState("Unable to enqueue");
    try {
      readableEnqueue(readable, chunk);
    } catch (error) {
      transformErrorWritableAndUnblockWrite(stream, error);
      throw readable._error;
    }
    const backpressure = !readableShouldCallPull(readable);
    if (backpressure !== stream._backpressure) transformSetBackpressure(stream, true);
  }

  async function transformPerformTransform(controller, chunk) {
    try {
      return await controller._transformAlgorithm(chunk, controller);
    } catch (error) {
      transformError(controller._stream, error);
      throw error;
    }
  }

  function transformSinkWrite(stream, chunk) {
    const controller = stream._controller;
    if (stream._backpressure) {
      return then(stream._backpressureChange.promise, () => {
        const writable = stream._writable;
        if (writable._state === "erroring") throw writable._storedError;
        return transformPerformTransform(controller, chunk);
      });
    }
    return transformPerformTransform(controller, chunk);
  }

  async function transformSinkAbort(stream, reason) {
    const controller = stream._controller;
    const readable = stream._readable;
    if (controller._finishPromise !== undefined) return controller._finishPromise;
    const finish = deferred();
    controller._finishPromise = finish.promise;
    const cancelPromise = controller._cancelAlgorithm(reason);
    transformClearAlgorithms(controller);
    then(
      cancelPromise,
      () => {
        if (readable._state === "errored") finish.reject(readable._error);
        else {
          readableError(readable, reason);
          finish.resolve();
        }
      },
      (error) => {
        readableError(readable, error);
        finish.reject(error);
      },
    );
    return controller._finishPromise;
  }

  function transformSinkClose(stream) {
    const controller = stream._controller;
    const readable = stream._readable;
    if (controller._finishPromise !== undefined) return controller._finishPromise;
    const finish = deferred();
    controller._finishPromise = finish.promise;
    const flushPromise = controller._flushAlgorithm(controller);
    transformClearAlgorithms(controller);
    then(
      flushPromise,
      () => {
        if (readable._state === "errored") finish.reject(readable._error);
        else {
          if (readableCanCloseOrEnqueue(readable)) readableControllerClose(readable);
          finish.resolve();
        }
      },
      (error) => {
        readableError(readable, error);
        finish.reject(error);
      },
    );
    return controller._finishPromise;
  }

  function transformSourcePull(stream) {
    transformSetBackpressure(stream, false);
    return stream._backpressureChange.promise;
  }

  function transformSourceCancel(stream, reason) {
    const controller = stream._controller;
    const writable = stream._writable;
    if (controller._finishPromise !== undefined) return controller._finishPromise;
    const finish = deferred();
    controller._finishPromise = finish.promise;
    const cancelPromise = controller._cancelAlgorithm(reason);
    transformClearAlgorithms(controller);
    then(
      cancelPromise,
      () => {
        if (writable._state === "errored") finish.reject(writable._storedError);
        else {
          writableControllerErrorIfNeeded(writable._controller, reason);
          transformUnblockWrite(stream);
          finish.resolve();
        }
      },
      (error) => {
        writableControllerErrorIfNeeded(writable._controller, error);
        transformUnblockWrite(stream);
        finish.reject(error);
      },
    );
    return controller._finishPromise;
  }

  class TransformStream {
    constructor(transformer = {}, writableStrategy = {}, readableStrategy = {}) {
      validateObjectArg(transformer, "transformer", false);
      validateObjectArg(writableStrategy, "writableStrategy", true);
      validateObjectArg(readableStrategy, "readableStrategy", true);
      const readableType = transformer.readableType;
      const writableType = transformer.writableType;
      const start = transformer.start;
      if (readableType !== undefined) {
        throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("transformer.readableType", readableType);
      }
      if (writableType !== undefined) {
        throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("transformer.writableType", writableType);
      }
      // node reads the four strategy fields, then validates them. Its
      // defaults: the readable side holds 0 chunks (a transform runs when a
      // read wants its output), the writable side 1.
      const readableHighWaterMark = readableStrategy?.highWaterMark;
      const readableSizeFn = readableStrategy?.size;
      const writableHighWaterMark = writableStrategy?.highWaterMark;
      const writableSizeFn = writableStrategy?.size;
      const readableHWM = extractHighWaterMark(readableHighWaterMark, 0);
      const readableSize = makeSizeFn(readableSizeFn);
      const writableHWM = extractHighWaterMark(writableHighWaterMark, 1);
      const writableSize = makeSizeFn(writableSizeFn);
      const transform = transformer.transform;
      const flush = transformer.flush;
      const cancel = transformer.cancel;

      // Both sides start from ONE promise, resolved with transformer.start's
      // result below -- so a throw from start() is this constructor's throw.
      let resolveStart;
      const startPromise = new Promise((resolve) => {
        resolveStart = resolve;
      });
      const startAlgorithm = () => startPromise;

      this._backpressure = undefined;
      this._backpressureChange = undefined;
      this._controller = undefined;
      this._writable = createWritable(
        startAlgorithm,
        (chunk) => transformSinkWrite(this, chunk),
        () => transformSinkClose(this),
        (reason) => transformSinkAbort(this, reason),
        writableHWM,
        writableSize,
      );
      this._readable = createReadable(
        startAlgorithm,
        () => transformSourcePull(this),
        (reason) => transformSourceCancel(this, reason),
        readableHWM,
        readableSize,
      );
      transformSetBackpressure(this, true);

      const controller = new TransformStreamDefaultController(
        this,
        typeof transform === "function" ? promiseCallback(transform, transformer) : identityTransform,
        typeof flush === "function" ? promiseCallback(flush, transformer) : nonOpPromise,
        typeof cancel === "function" ? promiseCallback(cancel, transformer) : nonOpPromise,
      );
      this._controller = controller;

      if (start !== undefined) resolveStart(start.call(transformer, controller));
      else resolveStart();
    }

    get readable() {
      return this._readable;
    }

    get writable() {
      return this._writable;
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

  // WHATWG queuing strategies (Node globals since v18). The streams above
  // call size() through makeSizeFn; these carry the documented shape --
  // vendored stream tests construct them directly.
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

  // node brands each of these with Symbol.toStringTag, so
  // Object.prototype.toString.call(x) names the class and a brand
  // check (@sindresorhus/is, is-stream, type-detect) recognises it.
  // Web IDL's descriptor: a data property, not writable, not
  // enumerable, configurable.
  Object.defineProperty(ReadableStream.prototype, Symbol.toStringTag, {
    value: "ReadableStream",
    configurable: true,
  });
  Object.defineProperty(WritableStream.prototype, Symbol.toStringTag, {
    value: "WritableStream",
    configurable: true,
  });
  Object.defineProperty(TransformStream.prototype, Symbol.toStringTag, {
    value: "TransformStream",
    configurable: true,
  });
  Object.defineProperty(CountQueuingStrategy.prototype, Symbol.toStringTag, {
    value: "CountQueuingStrategy",
    configurable: true,
  });
  Object.defineProperty(ByteLengthQueuingStrategy.prototype, Symbol.toStringTag, {
    value: "ByteLengthQueuingStrategy",
    configurable: true,
  });
  // node's TextEncoderStream / TextDecoderStream carry no tag.
  globalThis.ReadableStream = ReadableStream;
  globalThis.WritableStream = WritableStream;
  globalThis.TransformStream = TransformStream;
  globalThis.TextDecoderStream = TextDecoderStream;
  globalThis.TextEncoderStream = TextEncoderStream;
  globalThis.CountQueuingStrategy = CountQueuingStrategy;
  globalThis.ByteLengthQueuingStrategy = ByteLengthQueuingStrategy;
})();
