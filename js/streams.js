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
// - `type: 'bytes'` streams get the byte controller: its queuing --
//   high-water mark 0 by default, desiredSize in bytes, chunks must be
//   ArrayBufferViews and are read as Uint8Arrays -- BYOB readers
//   (getReader({ mode: 'byob' })) with node's pull-into algorithms, and
//   controller.byobRequest. Not there: `autoAllocateChunkSize` (read and 0
//   refused, but a default read gets no byobRequest), and an enqueued
//   buffer is not transferred (node detaches it).
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
  // node's kPendingPromise: the forever-pending promise a pipe's ready
  // hook carries, so the erroring and release paths see a pending record;
  // marking it handled is skipped, so no reaction piles up on it.
  const kPendingPromise = new Promise(noop);
  const markHandled = (promise) => {
    if (promise === kPendingPromise) return;
    PromiseThen.call(promise, undefined, noop);
  };

  const PromiseCtor = Promise;
  const PromiseResolveFn = Promise.resolve;
  const PromiseRejectFn = Promise.reject;
  const PromiseAllFn = Promise.all;
  const PromiseResolve = (value) => PromiseResolveFn.call(PromiseCtor, value);
  const PromiseReject = (reason) => PromiseRejectFn.call(PromiseCtor, reason);
  const ReflectApply = Reflect.apply;

  // node's lib/internal/webstreams/util.js (v26.13.1). The start, pull,
  // write and transform callbacks are RAW: called as they are, their result
  // not wrapped in a promise. thenAlgorithmResult wires a result to its
  // continuations: a non-thenable one runs the fulfilled step a microtask
  // later off one shared resolved promise, a thenable one is adopted with
  // Promise.resolve (identity for a native promise, so a pull that returns
  // a promise is answered one microtask after it settles, not three). An
  // algorithm that settles its operation itself later returns kParked.
  // cancel, close, abort and flush stay async functions, as node's.
  const kResolvedPromise = PromiseResolve();
  const kParked = Symbol("kParkedAlgorithmResult");
  const nonOpCallback = () => {};
  const nonOpCancel = async () => {};
  function thenAlgorithmResult(result, onFulfilled, onRejected) {
    if (result === kParked) return;
    if (result === null || (typeof result !== "object" && typeof result !== "function")) {
      then(kResolvedPromise, onFulfilled);
    } else {
      then(PromiseResolve(result), onFulfilled, onRejected);
    }
  }
  const isThenableResult = (result) =>
    result !== null && (typeof result === "object" || typeof result === "function");

  // node's createRawCallback* / createPromiseCallback*: validateFunction,
  // then a wrapper passing exactly the arguments node passes (a callback's
  // arguments.length is observable).
  function validateCallback(fn, name) {
    if (typeof fn !== "function") throw new (codes().ERR_INVALID_ARG_TYPE)(name, "Function", fn);
  }
  function rawCallback1(name, fn, thisArg) {
    validateCallback(fn, name);
    return (a) => ReflectApply(fn, thisArg, [a]);
  }
  function rawCallback2(name, fn, thisArg) {
    validateCallback(fn, name);
    return (a, b) => ReflectApply(fn, thisArg, [a, b]);
  }
  function promiseCallback0(name, fn, thisArg) {
    validateCallback(fn, name);
    return async () => ReflectApply(fn, thisArg, []);
  }
  function promiseCallback1(name, fn, thisArg) {
    validateCallback(fn, name);
    return async (a) => ReflectApply(fn, thisArg, [a]);
  }
  // node's FunctionPrototypeBind(start, source, controller): a start that
  // is set but is not a function is V8's bind refusal.
  function bindStart(start, thisArg) {
    if (typeof start !== "function") throw new TypeError("Bind must be called on a function");
    return (controller) => ReflectApply(start, thisArg, [controller]);
  }

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

  // node's kIsClosedPromise (`Symbol.for('nodejs.webstream.isClosedPromise')`):
  // a promise every ReadableStream and WritableStream carries, settled when
  // the stream closes (resolved) or errors (rejected, and marked handled).
  // stream.finished() / eos() on a web stream waits on it -- undici's fetch
  // does, to finalize a response -- so a stream without it made finished()
  // throw. Made on first use rather than per stream, so the streams nobody
  // asks about (every fetch body) pay nothing: settled at once for a stream
  // already closed or errored, and settled by the stream otherwise.
  const kIsClosedPromise = Symbol.for("nodejs.webstream.isClosedPromise");
  function isClosedPromise(stream, closed, errored, error) {
    let d = stream._isClosed;
    if (d !== undefined) return d;
    if (closed) {
      d = settled(Promise.resolve());
    } else if (errored) {
      d = settled(Promise.reject(error));
      markHandled(d.promise);
    } else {
      d = deferred();
    }
    stream._isClosed = d;
    return d;
  }
  function settleIsClosed(stream, error, failed) {
    const d = stream._isClosed;
    if (d === undefined || !d.pending) return;
    if (failed) {
      markHandled(d.promise);
      d.reject(error);
    } else {
      d.resolve();
    }
  }

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
    // A BYOB read took `count` bytes off the head chunk: the rest of it
    // stays at the head (a byte stream's queue; its sizes are byte counts).
    consumeHead(rest, count) {
      this._items[this._head] = rest;
      this._sizes[this._head] -= count;
      this.totalSize = Math.max(0, this.totalSize - count);
    }
    clear() {
      this._items = [];
      this._sizes = [];
      this._head = 0;
      this.totalSize = 0;
    }
  }

  // node's extractHighWaterMark: `+value`, and NaN or negative is a
  // RangeError ERR_INVALID_ARG_VALUE naming the value as given (v26.13.1:
  // `Received 'x'`, where v22 named the coerced NaN; -0 is not negative).
  function extractHighWaterMark(value, defaultHWM) {
    if (value === undefined) return defaultHWM;
    const coerced = +value;
    if (Number.isNaN(coerced) || coerced < 0) {
      throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("strategy.highWaterMark", value);
    }
    return coerced;
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
  // Infinity is a RangeError (which errors the stream), naming the size as
  // size() returned it (v26.13.1).
  function validChunkSize(size) {
    const coerced = +size;
    if (Number.isNaN(coerced) || coerced < 0 || coerced === Infinity) {
      throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("size", size);
    }
    return coerced;
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
    if (typeof SharedArrayBuffer === "function" && chunk.buffer instanceof SharedArrayBuffer) {
      throw new (codes().ERR_INVALID_ARG_VALUE)("chunk", chunk, "must not be backed by a SharedArrayBuffer");
    }
    if (chunk.byteLength === 0 || chunk.buffer.byteLength === 0) {
      throw invalidState("chunk ArrayBuffer is zero-length or detached");
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
      if (stream._isBytes) {
        byteEnqueue(stream, byteEnqueueChunk(stream, chunk));
        return;
      }
      if (!readableCanCloseOrEnqueue(stream)) throw invalidState("Controller is already closed");
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

  // A byte stream's controller: the default one's queuing, plus the
  // byobRequest a source answers a BYOB read through (node's
  // ReadableByteStreamController getter: made on first ask while a read is
  // pending, the same object until it is answered).
  class ReadableByteStreamController extends ReadableStreamDefaultController {
    get byobRequest() {
      const stream = this._stream;
      if (stream._byobRequest === null && stream._pullIntos.length > 0) {
        const { buffer, byteOffset, bytesFilled, byteLength } = stream._pullIntos[0];
        const view = new Uint8Array(buffer, byteOffset + bytesFilled, byteLength - bytesFilled);
        stream._byobRequest = new ReadableStreamBYOBRequest(stream, view);
      }
      return stream._byobRequest;
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
    stream._pullAlgorithm = nonOpCallback;
    stream._cancelAlgorithm = nonOpCancel;
    // node's cached pull reactions, made on the first pull: a parked pull
    // (ReadableStream.from, TransformStream's readable side) completes by
    // calling _pullFulfilled itself.
    stream._pullFulfilled = undefined;
    stream._pullRejected = undefined;
    // Has anything been read from, or cancelled on, this stream? A fetch
    // body asks (js/bootstrap.js): a disturbed stream cannot be a body, and
    // a body whose stream was read is `bodyUsed`. Never reset -- releasing
    // the reader does not un-read the chunks.
    stream._disturbed = false;
    stream._resolveClosed = undefined;
    stream._rejectClosed = undefined;
    // A byte stream's pending BYOB reads: the standard's pull-into
    // descriptors (the controller's), the reads waiting on them (the BYOB
    // reader's readIntoRequests), and the byobRequest made for the first.
    stream._pullIntos = [];
    stream._readIntos = new Queue();
    stream._byobRequest = null;
    stream._controller = isBytes
      ? new ReadableByteStreamController(stream)
      : new ReadableStreamDefaultController(stream);
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
  // started and makes its first pull. node v26.13.1's
  // setupReadableStreamDefaultController: a non-thenable result is started
  // a microtask later (queueMicrotask, the position the promise reaction
  // had), and at once when there is no pull() to start; a thenable is
  // wrapped in `new Promise((r) => r(result))`, with that wrapper's extra
  // microtasks.
  function setupReadable(stream, startAlgorithm, pullAlgorithm, cancelAlgorithm) {
    stream._pullAlgorithm = pullAlgorithm;
    stream._cancelAlgorithm = cancelAlgorithm;
    const startResult = startAlgorithm(stream._controller);
    const startFulfilled = !isThenableResult(startResult);
    if (startFulfilled && pullAlgorithm === nonOpCallback) {
      stream._started = true;
      return;
    }
    const started = () => {
      stream._started = true;
      readableCallPullIfNeeded(stream);
    };
    if (startFulfilled) {
      // (node's queueMicrotask; a reaction on the shared resolved promise
      // runs at the same position.)
      then(kResolvedPromise, started);
      return;
    }
    then(
      new PromiseCtor((resolve) => resolve(startResult)),
      started,
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
    if (stream._reader !== null && (stream._waiters.length > 0 || stream._readIntos.length > 0)) {
      return true;
    }
    return readableDesiredSize(stream) > 0;
  }

  function readableCallPullIfNeeded(stream) {
    if (!readableShouldCallPull(stream)) return;
    readablePull(stream);
  }

  // node v26.13.1's readableStreamDefaultControllerPull (and the byte
  // controller's half of CallPullIfNeeded): a stream with no pull() never
  // pulls; the reactions are made once and reused; the pull is a raw call
  // whose result goes through thenAlgorithmResult, a synchronous throw as
  // a rejection.
  function readablePull(stream) {
    if (stream._pullAlgorithm === nonOpCallback) return;
    if (stream._pulling) {
      stream._pullAgain = true;
      return;
    }
    stream._pulling = true;
    if (stream._pullFulfilled === undefined) {
      stream._pullFulfilled = () => {
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
      };
      stream._pullRejected = (error) => readableError(stream, error);
    }
    let result;
    try {
      result = stream._pullAlgorithm(stream._controller);
    } catch (error) {
      result = PromiseReject(error);
    }
    thenAlgorithmResult(result, stream._pullFulfilled, stream._pullRejected);
  }

  function readableEnqueue(stream, chunk) {
    if (stream._waiters.length > 0) {
      stream._waiters.shift().resolve({ done: false, value: chunk });
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
    stream._pullAlgorithm = nonOpCallback;
    stream._cancelAlgorithm = nonOpCancel;
    stream._sizeFn = countSize;
  }

  // With chunks still queued the close waits for the last of them to be
  // read (closeRequested); otherwise the stream closes now. closeRequested
  // is only set in the first case, as node's byte controller sets it: an
  // enqueue then tells "Controller is already closed" from "ReadableStream
  // is already closed" (the default controller says the former for both).
  //
  // A byte stream counts what is left in bytes (the standard's
  // `queueTotalSize > 0`), not in entries: oam accepts a zero-length chunk
  // (docs/node-divergences.md), and a queue holding only such chunks has
  // nothing left to read. Counting it as pending would leave the stream
  // closeRequested with no bytes, and a BYOB read would then wait on a pull
  // closeRequested never makes. Those empty entries are dropped as it closes.
  function readableControllerClose(stream) {
    if (stream._isBytes ? stream._queue.totalSize > 0 : stream._queue.length > 0) {
      stream._closeRequested = true;
      return;
    }
    if (stream._isBytes) resetReadableQueue(stream);
    // node's ReadableByteStreamControllerClose: a BYOB read holding part of
    // an element can never be completed, so the stream errors. A pending
    // read otherwise stays pending until the source answers its
    // byobRequest with respond(0).
    if (stream._pullIntos.length > 0) {
      const first = stream._pullIntos[0];
      if (first.bytesFilled % first.elementSize !== 0) {
        const error = invalidState("Partial read");
        readableError(stream, error);
        throw error;
      }
    }
    readableClearAlgorithms(stream);
    readableClose(stream);
  }

  // ReadableStreamClose: the stream is closed, then reader.closed settles,
  // then the pending reads are done (node's order).
  function readableClose(stream) {
    stream._state = "closed";
    settleIsClosed(stream);
    stream._resolveClosed?.();
    while (stream._waiters.length > 0) {
      stream._waiters.shift().resolve({ done: true, value: undefined });
    }
  }

  function readableError(stream, reason) {
    if (stream._state !== "readable") return;
    clearPendingPullIntos(stream);
    resetReadableQueue(stream);
    readableClearAlgorithms(stream);
    stream._state = "errored";
    stream._error = reason;
    settleIsClosed(stream, reason, true);
    stream._rejectClosed?.(reason);
    while (stream._waiters.length > 0) {
      stream._waiters.shift().reject(reason);
    }
    while (stream._readIntos.length > 0) {
      stream._readIntos.shift().reject(reason);
    }
  }

  function readableCancel(stream, reason) {
    stream._disturbed = true;
    if (stream._state === "closed") return Promise.resolve();
    if (stream._state === "errored") return Promise.reject(stream._error);
    readableClose(stream);
    // A BYOB read pending at a cancel is done, with no view at all (node's
    // ReadableStreamCancel); the controller's cancel steps drop the
    // descriptors with the queue.
    while (stream._readIntos.length > 0) {
      stream._readIntos.shift().resolve({ done: true, value: undefined });
    }
    clearPendingPullIntos(stream);
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
  // resolve takes the {done, value} result, synchronously when a chunk is
  // queued, or when one is enqueued (a pipe acts on it right there). Key
  // order is node's: every read result here (and the async iterator's) is
  // built done-first, as v26.13.1 builds them (Object.keys -> done, value).
  function readableReaderRead(stream, request) {
    stream._disturbed = true;
    if (stream._state === "closed") {
      request.resolve({ done: true, value: undefined });
    } else if (stream._state === "errored") {
      request.reject(stream._error);
    } else if (stream._queue.length > 0) {
      const value = stream._queue.shift();
      // A byte stream drains on bytes, as readableControllerClose closes on
      // them (byteHandleQueueDrain's test): zero-length entries left behind
      // the last bytes are dropped rather than kept as "still queued", or a
      // BYOB read that follows would wait on a pull that never comes.
      if (
        stream._closeRequested &&
        (stream._isBytes ? stream._queue.totalSize === 0 : stream._queue.length === 0)
      ) {
        if (stream._isBytes) resetReadableQueue(stream);
        readableClearAlgorithms(stream);
        readableClose(stream);
      } else {
        readableCallPullIfNeeded(stream);
      }
      request.resolve({ done: false, value });
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
    // The byte controller's release steps: a BYOB read still pending stays
    // as a descriptor of type "none", so what the source writes into it is
    // not lost (it is queued when answered); any after it go.
    if (stream._pullIntos.length > 0) {
      const first = stream._pullIntos[0];
      first.type = "none";
      stream._pullIntos = [first];
    }
    stream._reader = null;
    stream._resolveClosed = undefined;
    stream._rejectClosed = undefined;
  }

  // ------------------------------------------------- BYOB reads (bytes)
  //
  // node's ReadableStreamBYOBReader and the byte controller's pull-into
  // algorithms (lib/internal/webstreams/readablestream.js, v26.13.1), step
  // for step. A read(view) transfers the view's buffer into a descriptor;
  // queued bytes are copied into it -- at most the view's length, across
  // chunks, the rest left queued -- and once it holds `min` elements it is
  // handed back as a view of the caller's type over the transferred buffer.
  // A read the queue cannot satisfy waits: an enqueue fills it, or the
  // source writes into controller.byobRequest.view and answers respond().
  const ArrayBufferPrototypeTransfer = ArrayBuffer.prototype.transfer;
  const transferBuffer = (buffer) => ArrayBufferPrototypeTransfer.call(buffer);
  const isDataView = (view) => view instanceof DataView;

  function invalidateBYOBRequest(stream) {
    const request = stream._byobRequest;
    if (request === null) return;
    request._stream = undefined;
    request._view = null;
    stream._byobRequest = null;
  }

  function clearPendingPullIntos(stream) {
    invalidateBYOBRequest(stream);
    stream._pullIntos = [];
  }

  // A view of the type a read asked for. A Buffer goes through Buffer.from,
  // not the deprecated `new Buffer(arrayBuffer, ...)` constructor.
  function viewOf(ctor, buffer, byteOffset, length) {
    if (ctor === globalThis.Buffer) return globalThis.Buffer.from(buffer, byteOffset, length);
    return new ctor(buffer, byteOffset, length);
  }

  // A descriptor's bytes as the view its read asked for.
  function convertPullInto(desc) {
    const { bytesFilled, byteLength, byteOffset, ctor, elementSize } = desc;
    if (bytesFilled > byteLength) {
      throw new (codes().ERR_INVALID_STATE.RangeError)("The buffer size is invalid");
    }
    return viewOf(ctor, transferBuffer(desc.buffer), byteOffset, bytesFilled / elementSize);
  }

  function commitPullInto(stream, desc) {
    const done = stream._state === "closed";
    const filled = convertPullInto(desc);
    const request = stream._readIntos.shift();
    request.resolve({ done, value: filled });
  }

  function commitPullIntos(stream, descs) {
    for (let i = 0; i < descs.length; i++) commitPullInto(stream, descs[i]);
  }

  // The standard's ReadableByteStreamControllerFillPullIntoDescriptorFromQueue:
  // true when the descriptor now holds its minimum (whole elements only).
  function fillPullIntoFromQueue(stream, desc) {
    const queue = stream._queue;
    const maxBytesToCopy = Math.min(queue.totalSize, desc.byteLength - desc.bytesFilled);
    const maxBytesFilled = desc.bytesFilled + maxBytesToCopy;
    const maxAlignedBytes = maxBytesFilled - (maxBytesFilled % desc.elementSize);
    let remaining = maxBytesToCopy;
    let ready = false;
    if (maxAlignedBytes >= desc.minimumFill) {
      remaining = maxAlignedBytes - desc.bytesFilled;
      ready = true;
    }
    const dest = new Uint8Array(desc.buffer);
    while (remaining > 0) {
      const head = queue.peek();
      const count = Math.min(remaining, head.byteLength);
      if (count === head.byteLength) {
        dest.set(head, desc.byteOffset + desc.bytesFilled);
        queue.shift();
      } else {
        dest.set(head.subarray(0, count), desc.byteOffset + desc.bytesFilled);
        queue.consumeHead(head.subarray(count), count);
      }
      desc.bytesFilled += count;
      remaining -= count;
    }
    return ready;
  }

  function processPullIntosUsingQueue(stream) {
    const filled = [];
    while (stream._pullIntos.length > 0) {
      if (stream._queue.totalSize === 0) break;
      const desc = stream._pullIntos[0];
      if (fillPullIntoFromQueue(stream, desc)) {
        stream._pullIntos.shift();
        filled.push(desc);
      }
    }
    return filled;
  }

  // ReadableByteStreamControllerHandleQueueDrain.
  function byteHandleQueueDrain(stream) {
    if (stream._queue.totalSize === 0 && stream._closeRequested) {
      resetReadableQueue(stream);
      readableClearAlgorithms(stream);
      readableClose(stream);
      return;
    }
    readableCallPullIfNeeded(stream);
  }

  // A released BYOB read's bytes (type "none") go to the queue as a copy.
  function enqueueDetachedPullIntoToQueue(stream, desc) {
    if (desc.bytesFilled > 0) {
      const copy = new Uint8Array(desc.buffer.slice(desc.byteOffset, desc.byteOffset + desc.bytesFilled));
      stream._queue.push(copy, copy.byteLength);
    }
    stream._pullIntos.shift();
  }

  // ReadableByteStreamControllerEnqueue, past the controller's checks.
  function byteEnqueue(stream, chunk) {
    if (stream._pullIntos.length > 0) {
      const first = stream._pullIntos[0];
      if (first.buffer.detached) throw invalidState("Destination ArrayBuffer is detached");
      invalidateBYOBRequest(stream);
      first.buffer = transferBuffer(first.buffer);
      if (first.type === "none") enqueueDetachedPullIntoToQueue(stream, first);
    }
    if (stream._reader !== null && byobReaders.has(stream._reader)) {
      stream._queue.push(chunk, chunk.byteLength);
      commitPullIntos(stream, processPullIntosUsingQueue(stream));
      readableCallPullIfNeeded(stream);
      return;
    }
    // A default reader, or none: what a released BYOB read left queued is
    // read first, then the chunk goes to a waiting read or the queue.
    while (stream._waiters.length > 0 && stream._queue.length > 0) {
      const value = stream._queue.shift();
      stream._waiters.shift().resolve({ done: false, value });
    }
    readableEnqueue(stream, chunk);
  }

  // ReadableByteStreamControllerPullInto.
  function bytePullInto(stream, view, min, request) {
    let elementSize = 1;
    let ctor = DataView;
    if (!isDataView(view)) {
      elementSize = view.constructor.BYTES_PER_ELEMENT;
      ctor = view.constructor;
    }
    const buffer = view.buffer;
    const byteOffset = view.byteOffset;
    const byteLength = view.byteLength;
    const bufferByteLength = buffer.byteLength;
    let transferred;
    try {
      transferred = transferBuffer(buffer);
    } catch (error) {
      request.reject(error);
      return;
    }
    const desc = {
      buffer: transferred,
      bufferByteLength,
      byteOffset,
      byteLength,
      bytesFilled: 0,
      minimumFill: min * elementSize,
      elementSize,
      ctor,
      type: "byob",
    };
    if (stream._pullIntos.length > 0) {
      stream._pullIntos.push(desc);
      stream._readIntos.push(request);
      return;
    }
    if (stream._state === "closed") {
      request.resolve({ done: true, value: viewOf(ctor, desc.buffer, byteOffset, 0) });
      return;
    }
    if (stream._queue.totalSize > 0) {
      if (fillPullIntoFromQueue(stream, desc)) {
        const filled = convertPullInto(desc);
        byteHandleQueueDrain(stream);
        request.resolve({ done: false, value: filled });
        return;
      }
      if (stream._closeRequested) {
        const error = invalidState("ReadableStream closed");
        readableError(stream, error);
        request.reject(error);
        return;
      }
    }
    stream._pullIntos.push(desc);
    stream._readIntos.push(request);
    readableCallPullIfNeeded(stream);
  }

  function respondInClosedState(stream, desc) {
    if (desc.type === "none") stream._pullIntos.shift();
    if (stream._reader !== null && byobReaders.has(stream._reader)) {
      const filled = [];
      const pending = stream._readIntos.length;
      for (let i = 0; i < pending; i++) filled.push(stream._pullIntos.shift());
      commitPullIntos(stream, filled);
    }
  }

  function respondInReadableState(stream, bytesWritten, desc) {
    if (desc.bytesFilled + bytesWritten > desc.byteLength) {
      throw new (codes().ERR_INVALID_STATE.RangeError)("The buffer size is invalid");
    }
    desc.bytesFilled += bytesWritten;
    if (desc.type === "none") {
      enqueueDetachedPullIntoToQueue(stream, desc);
      commitPullIntos(stream, processPullIntosUsingQueue(stream));
      return;
    }
    if (desc.bytesFilled < desc.minimumFill) return;
    stream._pullIntos.shift();
    const remainderSize = desc.bytesFilled % desc.elementSize;
    if (remainderSize) {
      const end = desc.byteOffset + desc.bytesFilled;
      const remainder = new Uint8Array(desc.buffer.slice(end - remainderSize, end));
      stream._queue.push(remainder, remainder.byteLength);
    }
    desc.bytesFilled -= remainderSize;
    const filled = processPullIntosUsingQueue(stream);
    commitPullInto(stream, desc);
    commitPullIntos(stream, filled);
  }

  function respondInternal(stream, bytesWritten) {
    const desc = stream._pullIntos[0];
    invalidateBYOBRequest(stream);
    if (stream._state === "closed") {
      if (bytesWritten) throw invalidState("Controller is closed but view is not zero-length");
      respondInClosedState(stream, desc);
    } else {
      if (!bytesWritten) throw invalidState("View cannot be zero-length");
      respondInReadableState(stream, bytesWritten, desc);
    }
    readableCallPullIfNeeded(stream);
  }

  // node's ReadableStreamBYOBRequest: the view the source writes into, and
  // its two answers. Invalidated (no stream, no view) once answered, or
  // when an enqueue, an error or a cancel makes it moot.
  class ReadableStreamBYOBRequest {
    constructor(stream, view) {
      this._stream = stream;
      this._view = view;
    }
    get view() {
      return this._view;
    }
    respond(bytesWritten) {
      const stream = this._stream;
      if (stream === undefined) throw invalidState("This BYOB request has been invalidated");
      if (this._view.buffer.detached) throw invalidState("Viewed ArrayBuffer is detached");
      byteRespond(stream, bytesWritten);
    }
    respondWithNewView(view) {
      const stream = this._stream;
      if (stream === undefined) throw invalidState("This BYOB request has been invalidated");
      if (!ArrayBuffer.isView(view)) {
        throw new (codes().ERR_INVALID_ARG_TYPE)("view", ["Buffer", "TypedArray", "DataView"], view);
      }
      if (view.buffer.detached) throw invalidState("Viewed ArrayBuffer is detached");
      byteRespondWithNewView(stream, view);
    }
  }

  // ReadableByteStreamControllerRespond.
  function byteRespond(stream, bytesWritten) {
    const desc = stream._pullIntos[0];
    if (stream._state === "closed") {
      if (bytesWritten !== 0) throw new (codes().ERR_INVALID_ARG_VALUE)("bytesWritten", bytesWritten);
    } else {
      if (!bytesWritten) throw new (codes().ERR_INVALID_ARG_VALUE)("bytesWritten", bytesWritten);
      if (desc.bytesFilled + bytesWritten > desc.byteLength) {
        throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("bytesWritten", bytesWritten);
      }
    }
    desc.buffer = transferBuffer(desc.buffer);
    respondInternal(stream, bytesWritten);
  }

  // ReadableByteStreamControllerRespondWithNewView.
  function byteRespondWithNewView(stream, view) {
    const desc = stream._pullIntos[0];
    const viewByteLength = view.byteLength;
    if (stream._state === "closed") {
      if (viewByteLength !== 0) throw invalidState("View is not zero-length");
    } else if (viewByteLength === 0) {
      throw invalidState("View is zero-length");
    }
    if (
      desc.byteOffset + desc.bytesFilled !== view.byteOffset ||
      desc.bytesFilled + viewByteLength > desc.byteLength ||
      desc.bufferByteLength !== view.buffer.byteLength
    ) {
      throw new (codes().ERR_INVALID_ARG_VALUE.RangeError)("view", view);
    }
    desc.buffer = transferBuffer(view.buffer);
    respondInternal(stream, viewByteLength);
  }
  Object.defineProperty(ReadableStreamBYOBRequest.prototype, Symbol.toStringTag, {
    value: "ReadableStreamBYOBRequest",
    configurable: true,
  });

  // The readers getReader({ mode: 'byob' }) has made, told apart from
  // default readers by the controller's algorithms.
  const byobReaders = new WeakSet();
  const BYOBReaderPrototype = Object.create(Object.prototype, {
    [Symbol.toStringTag]: { value: "ReadableStreamBYOBReader", configurable: true },
  });

  // node's setupReadableStreamBYOBReader and the reader's methods. Shaped as
  // the default reader is (acquireDefaultReader): own read / cancel /
  // releaseLock / closed.
  function acquireBYOBReader(stream) {
    if (stream._reader !== null) throw invalidState("ReadableStream is locked");
    if (!stream._isBytes) {
      throw new (codes().ERR_INVALID_ARG_VALUE)("stream", stream, "must be a byte stream");
    }
    let closedResolve;
    let closedReject;
    const closed = new Promise((resolve, reject) => {
      closedResolve = resolve;
      closedReject = reject;
    });
    markHandled(closed);
    if (stream._state === "closed") closedResolve();
    if (stream._state === "errored") closedReject(stream._error);
    stream._resolveClosed = closedResolve;
    stream._rejectClosed = closedReject;

    const reader = Object.create(BYOBReaderPrototype);
    reader.closed = closed;
    // node v26.13.1 hands back the read request's own promise (v22's read
    // was an async function, which settled two microtasks later); every
    // check is still a rejection, not a throw.
    reader.read = function read(view, options = {}) {
      try {
        return byobReadView(view, options);
      } catch (error) {
        return PromiseReject(error);
      }
    };
    const byobReadView = (view, options) => {
      if (!ArrayBuffer.isView(view)) {
        throw new (codes().ERR_INVALID_ARG_TYPE)("view", ["Buffer", "TypedArray", "DataView"], view);
      }
      validateObjectArg(options, "options", true);
      if (typeof SharedArrayBuffer === "function" && view.buffer instanceof SharedArrayBuffer) {
        throw new (codes().ERR_INVALID_ARG_VALUE)("view", view, "must not be backed by a SharedArrayBuffer");
      }
      if (view.byteLength === 0 || view.buffer.byteLength === 0) {
        throw invalidState("View or Viewed ArrayBuffer is zero-length or detached");
      }
      const min = options?.min ?? 1;
      if (typeof min !== "number") throw new (codes().ERR_INVALID_ARG_TYPE)("options.min", "number", min);
      if (!Number.isInteger(min)) {
        throw new (codes().ERR_INVALID_ARG_VALUE)("options.min", min, "must be an integer");
      }
      if (min <= 0) throw new (codes().ERR_INVALID_ARG_VALUE)("options.min", min, "must be greater than 0");
      if (!isDataView(view)) {
        if (min > view.length) throw new (codes().ERR_OUT_OF_RANGE)("options.min", "<= view.length", min);
      } else if (min > view.byteLength) {
        throw new (codes().ERR_OUT_OF_RANGE)("options.min", "<= view.byteLength", min);
      }
      if (stream._reader !== reader) throw invalidState("The reader is not attached to a stream");
      return new PromiseCtor((resolve, reject) => {
        stream._disturbed = true;
        if (stream._state === "errored") reject(stream._error);
        else bytePullInto(stream, view, min, { resolve, reject });
      });
    };
    reader.cancel = function cancel(reason) {
      if (stream._reader !== reader) {
        return Promise.reject(invalidState("The reader is not attached to a stream"));
      }
      return readableCancel(stream, reason);
    };
    reader.releaseLock = function releaseLock() {
      if (stream._reader !== reader) return;
      readableReaderGenericRelease(stream, reader);
      const releasing = readerReleasingError();
      while (stream._readIntos.length > 0) stream._readIntos.shift().reject(releasing);
    };
    byobReaders.add(reader);
    stream._reader = reader;
    return reader;
  }

  // node's internal ReadableByteStreamControllerEnqueue / Close: nothing on
  // a stream that is closing or no longer readable.
  function byteControllerEnqueue(stream, chunk) {
    if (stream._closeRequested || stream._state !== "readable") return;
    byteEnqueue(stream, chunk);
  }
  function byteControllerClose(stream) {
    if (stream._closeRequested || stream._state !== "readable") return;
    readableControllerClose(stream);
  }

  // node's readableByteStreamTee (the standard's ReadableByteStreamTee): two
  // byte streams over one reader of the source. A branch's default read
  // reads the source through a default reader and the second branch is
  // handed a copy of each chunk; a branch's BYOB read reads the source
  // through a BYOB reader into that read's own view, and the other branch
  // is handed a copy of what was read -- so both see the same bytes, in the
  // sizes the reading branch asked for, and neither sees the other's
  // writes.
  function byteStreamTee(stream) {
    let reader = acquireDefaultReader(stream);
    let reading = false;
    let readAgainForBranch1 = false;
    let readAgainForBranch2 = false;
    let canceled1 = false;
    let canceled2 = false;
    let reason1;
    let reason2;
    const cancelDeferred = deferred();
    const cloneAsUint8Array = (chunk) =>
      new Uint8Array(chunk.buffer.slice(chunk.byteOffset, chunk.byteOffset + chunk.byteLength));

    function forwardReaderError(thisReader) {
      then(thisReader.closed, undefined, (error) => {
        if (thisReader !== reader) return;
        readableError(branch1, error);
        readableError(branch2, error);
        if (!canceled1 || !canceled2) cancelDeferred.resolve();
      });
    }

    function cloneFailed(error) {
      readableError(branch1, error);
      readableError(branch2, error);
      cancelDeferred.resolve(readableCancel(stream, error));
    }

    function readAgain() {
      if (readAgainForBranch1) pull1Algorithm();
      else if (readAgainForBranch2) pull2Algorithm();
    }

    function pullWithDefaultReader() {
      if (byobReaders.has(reader)) {
        reader.releaseLock();
        reader = acquireDefaultReader(stream);
        forwardReaderError(reader);
      }
      readableReaderRead(stream, {
        resolve({ done, value: chunk }) {
          if (done) {
            reading = false;
            if (!canceled1) byteControllerClose(branch1);
            if (!canceled2) byteControllerClose(branch2);
            if (branch1._pullIntos.length > 0) byteRespond(branch1, 0);
            if (branch2._pullIntos.length > 0) byteRespond(branch2, 0);
            if (!canceled1 || !canceled2) cancelDeferred.resolve();
            return;
          }
          queueMicrotask(() => {
            readAgainForBranch1 = false;
            readAgainForBranch2 = false;
            const chunk1 = chunk;
            let chunk2 = chunk;
            if (!canceled1 && !canceled2) {
              try {
                chunk2 = cloneAsUint8Array(chunk);
              } catch (error) {
                cloneFailed(error);
                return;
              }
            }
            if (!canceled1) byteControllerEnqueue(branch1, chunk1);
            if (!canceled2) byteControllerEnqueue(branch2, chunk2);
            reading = false;
            readAgain();
          });
        },
        reject() {
          reading = false;
        },
      });
    }

    function pullWithBYOBReader(view, forBranch2) {
      if (!byobReaders.has(reader)) {
        reader.releaseLock();
        reader = acquireBYOBReader(stream);
        forwardReaderError(reader);
      }
      const byobBranch = forBranch2 ? branch2 : branch1;
      const otherBranch = forBranch2 ? branch1 : branch2;
      const request = {
        resolve({ done, value: chunk }) {
          const byobCanceled = forBranch2 ? canceled2 : canceled1;
          const otherCanceled = forBranch2 ? canceled1 : canceled2;
          if (done) {
            reading = false;
            if (!byobCanceled) byteControllerClose(byobBranch);
            if (!otherCanceled) byteControllerClose(otherBranch);
            if (chunk !== undefined) {
              if (!byobCanceled) byteRespondWithNewView(byobBranch, chunk);
              if (!otherCanceled && otherBranch._pullIntos.length > 0) byteRespond(otherBranch, 0);
            }
            if (!byobCanceled || !otherCanceled) cancelDeferred.resolve();
            return;
          }
          queueMicrotask(() => {
            readAgainForBranch1 = false;
            readAgainForBranch2 = false;
            const byobCanceledNow = forBranch2 ? canceled2 : canceled1;
            const otherCanceledNow = forBranch2 ? canceled1 : canceled2;
            if (!otherCanceledNow) {
              let cloned;
              try {
                cloned = cloneAsUint8Array(chunk);
              } catch (error) {
                cloneFailed(error);
                return;
              }
              if (!byobCanceledNow) byteRespondWithNewView(byobBranch, chunk);
              byteControllerEnqueue(otherBranch, cloned);
            } else if (!byobCanceledNow) {
              byteRespondWithNewView(byobBranch, chunk);
            }
            reading = false;
            readAgain();
          });
        },
        reject() {
          reading = false;
        },
      };
      stream._disturbed = true;
      if (stream._state === "errored") request.reject(stream._error);
      else bytePullInto(stream, view, 1, request);
    }

    function pull1Algorithm() {
      if (reading) {
        readAgainForBranch1 = true;
        return;
      }
      reading = true;
      const byobRequest = branch1._controller.byobRequest;
      if (byobRequest === null) pullWithDefaultReader();
      else pullWithBYOBReader(byobRequest._view, false);
    }

    function pull2Algorithm() {
      if (reading) {
        readAgainForBranch2 = true;
        return;
      }
      reading = true;
      const byobRequest = branch2._controller.byobRequest;
      if (byobRequest === null) pullWithDefaultReader();
      else pullWithBYOBReader(byobRequest._view, true);
    }

    function cancel1Algorithm(reason) {
      canceled1 = true;
      reason1 = reason;
      if (canceled2) cancelDeferred.resolve(readableCancel(stream, [reason1, reason2]));
      return cancelDeferred.promise;
    }

    function cancel2Algorithm(reason) {
      canceled2 = true;
      reason2 = reason;
      if (canceled1) cancelDeferred.resolve(readableCancel(stream, [reason1, reason2]));
      return cancelDeferred.promise;
    }

    const branch1 = createByteReadable(pull1Algorithm, cancel1Algorithm);
    const branch2 = createByteReadable(pull2Algorithm, cancel2Algorithm);
    forwardReaderError(reader);
    return [branch1, branch2];
  }

  // A byte stream built over internal algorithms, as node's
  // createReadableByteStream builds one (high-water mark 0).
  function createByteReadable(pullAlgorithm, cancelAlgorithm) {
    const stream = Object.create(ReadableStream.prototype);
    initReadable(stream, 0, byteChunkSize, true);
    setupReadable(stream, nonOpCallback, pullAlgorithm, cancelAlgorithm);
    return stream;
  }

  // node's validateAbortSignal: duck-typed on `aborted`.
  function validateAbortSignal(signal, name) {
    if (signal === null || typeof signal !== "object" || !("aborted" in signal)) {
      throw new (codes().ERR_INVALID_ARG_TYPE)(name, "AbortSignal", signal);
    }
  }

  // node v26.13.1's readableStreamPipeTo, the standard's ReadableStreamPipeTo
  // driven by callbacks: a pump that, while the destination has no
  // backpressure, writes every chunk already queued in a default source in
  // one batch (yielding a microtask between batches), or parks one read
  // whose chunk is written a microtask after it arrives; on backpressure it
  // parks on the writer's ready record, which the destination resolves --
  // re-entering the pump synchronously -- once the backpressure clears.
  // Every write shares one request (a counter of pending writes), so a
  // shutdown waits on them without a promise per write. Errors and closes
  // on either side shut the pipe down with node's actions: an errored
  // source aborts the destination, an errored destination cancels the
  // source, a closed source closes the destination (each unless
  // prevented), and an abort signal does both.
  function readableStreamPipeTo(source, dest, preventClose, preventAbort, preventCancel, signal) {
    let reader;
    let writer;
    try {
      reader = acquireDefaultReader(source);
      writer = new WritableStreamDefaultWriter(dest);
    } catch (error) {
      return PromiseReject(error);
    }
    source._disturbed = true;
    let shuttingDown = false;
    let removeAbortListener;
    const promise = deferred();

    const writeTracker = {
      pending: 0,
      failed: false,
      failure: undefined,
      stall: null,
      resolve() {
        if (--this.pending === 0 && this.stall !== null) {
          const stall = this.stall;
          this.stall = null;
          if (this.failed) stall.reject(this.failure);
          else stall.resolve();
        }
      },
      reject(error) {
        this.pending--;
        this.failed = true;
        this.failure = error;
        if (this.stall !== null) {
          const stall = this.stall;
          this.stall = null;
          stall.reject(error);
        }
      },
    };

    let readyHook;
    function finalize(rejected, error) {
      // The pipe was the only observer of the writer's ready record (the
      // hook parkOnReady installs); it goes before the release.
      if (writer._ready === readyHook) writer._ready = settled(PromiseResolve());
      writerRelease(writer);
      readableReaderGenericRelease(source, reader);
      // The pipe's own read may still be pending (an abort or a destination
      // error with preventCancel leaves the source readable). It goes with
      // the reader: left queued, the next enqueue would hand its chunk to
      // the released writer instead of the source's queue. (node keeps read
      // requests on the reader, which the release detaches.)
      if (source._waiters.length > 0) {
        const releasing = readerReleasingError();
        while (source._waiters.length > 0) source._waiters.shift().reject(releasing);
      }
      removeAbortListener?.();
      if (rejected) promise.reject(error);
      else promise.resolve();
    }

    function waitForPendingWrites() {
      if (writeTracker.pending === 0) {
        return writeTracker.failed ? PromiseReject(writeTracker.failure) : PromiseResolve();
      }
      writeTracker.stall = deferred();
      return writeTracker.stall.promise;
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
        then(waitForPendingWrites(), complete, (error) => finalize(true, error));
        return;
      }
      complete();
    }

    function shutdown(rejected, error) {
      if (shuttingDown) return;
      shuttingDown = true;
      if (dest._state === "writable" && !writableCloseQueuedOrInFlight(dest)) {
        then(
          waitForPendingWrites(),
          () => finalize(rejected, error),
          (err) => finalize(true, err),
        );
        return;
      }
      finalize(rejected, error);
    }

    function abortAlgorithm() {
      let error = signal.reason;
      // node's AbortError is not a DOMException; the pipe's must be one.
      const AbortError = globalThis.__oamAbortError;
      if (typeof AbortError === "function" && error instanceof AbortError) {
        error = new DOMException(error.message, "AbortError");
      }
      const actions = [];
      if (!preventAbort) {
        actions.push(() => (dest._state === "writable" ? writableAbort(dest, error) : PromiseResolve()));
      }
      if (!preventCancel) {
        actions.push(() => (source._state === "readable" ? readableCancel(source, error) : PromiseResolve()));
      }
      // node's SafePromiseAll(actions, (action) => action()): each result
      // wrapped in a promise of its own, the all() re-wrapped -- the
      // microtasks a plain Promise.all would not take.
      shutdownWithAnAction(
        () =>
          new PromiseCtor((resolve, reject) => {
            const wrapped = actions.map(
              (action) => new PromiseCtor((res, rej) => then(action(), res, rej)),
            );
            then(PromiseAllFn.call(PromiseCtor, wrapped), resolve, reject);
          }),
        true,
        error,
      );
    }

    let pendingChunk;
    let readRequest;
    function parkOnReady() {
      readyHook ??= { promise: kPendingPromise, resolve: pump, reject: noop, pending: true };
      writer._ready = readyHook;
    }

    function forwardChunk() {
      const chunk = pendingChunk;
      pendingChunk = undefined;
      writerWriteWithRequest(writer, chunk, writeTracker);
      pump();
    }

    function pump() {
      if (shuttingDown) return;
      if (dest._backpressure) {
        parkOnReady();
        return;
      }
      // A default source with chunks queued: write them in one batch.
      if (source._state === "readable" && !source._isBytes && source._queue.length > 0) {
        while (source._queue.length > 0) {
          if (shuttingDown) return;
          const chunk = source._queue.shift();
          if (source._closeRequested && source._queue.length === 0) {
            readableClearAlgorithms(source);
            readableClose(source);
          }
          writerWriteWithRequest(writer, chunk, writeTracker);
          if (dest._backpressure) break;
          if (dest._state !== "writable" || writableCloseQueuedOrInFlight(dest)) break;
        }
        readableCallPullIfNeeded(source);
        if (source._state === "closed") return;
        if (dest._backpressure) {
          parkOnReady();
          return;
        }
        then(kResolvedPromise, pump);
        return;
      }
      // Park one read; close and error are the watchers' to handle. The
      // standard writes the chunk a microtask after it arrives, never
      // inside the enqueue that delivered it.
      readRequest ??= {
        resolve(result) {
          if (result.done) return;
          pendingChunk = result.value;
          then(kResolvedPromise, forwardChunk);
        },
        reject: noop,
      };
      readableReaderRead(source, readRequest);
    }

    if (signal !== undefined) {
      if (signal.aborted) {
        abortAlgorithm();
        return promise.promise;
      }
      signal.addEventListener("abort", abortAlgorithm, { once: true });
      removeAbortListener = () => signal.removeEventListener("abort", abortAlgorithm);
    }

    pump();

    function onSourceErrored(error) {
      if (!preventAbort) {
        shutdownWithAnAction(() => writableAbort(dest, error), true, error);
        return;
      }
      shutdown(true, error);
    }
    function onSourceClosed() {
      if (!preventClose) {
        shutdownWithAnAction(() => writerCloseWithErrorPropagation(writer));
        return;
      }
      shutdown();
    }
    function onDestErrored(error) {
      if (!preventCancel) {
        shutdownWithAnAction(() => readableCancel(source, error), true, error);
        return;
      }
      shutdown(true, error);
    }

    // node's order: the source-errored watcher, then the destination's,
    // then an already closed source; a watcher on a side already in that
    // state acts at once.
    if (source._state === "errored") onSourceErrored(source._error);
    else if (source._state !== "closed") then(reader.closed, onSourceClosed, onSourceErrored);

    if (dest._state === "errored") onDestErrored(dest._storedError);
    else then(writer._closed.promise, undefined, onDestErrored);

    if (source._state === "closed") onSourceClosed();

    if (writableCloseQueuedOrInFlight(dest) || dest._state === "closed") {
      const error = invalidState("Destination WritableStream is closed");
      if (!preventCancel) shutdownWithAnAction(() => readableCancel(source, error), true, error);
      else shutdown(true, error);
    }

    return promise.promise;
  }

  // node's internal readableStreamDefaultControllerEnqueue / Close: nothing
  // on a stream that is closing or no longer readable (the controller's
  // own enqueue() and close() throw there instead).
  function defaultControllerEnqueue(stream, chunk) {
    if (!readableCanCloseOrEnqueue(stream)) return;
    readableEnqueue(stream, chunk);
  }
  function defaultControllerClose(stream) {
    if (!readableCanCloseOrEnqueue(stream)) return;
    readableControllerClose(stream);
  }

  // node v26.13.1's ReadableStreamAsyncIterator: next() and return() on a
  // shared prototype (an async iterator prototype, tagged
  // "ReadableStream AsyncIterator"), one read in flight at a time. The
  // first next() is delayed a microtask; a next() with a chunk already
  // queued and no read in flight answers at once; the lock is released
  // when the stream is done, a read fails, or return() runs (cancelling
  // the stream unless preventCancel). Results are keyed {done, value}.
  class ReadableStreamAsyncIterator {
    #state;
    constructor(state) {
      this.#state = state;
    }
    static isIterator(value) {
      return typeof value === "object" && value !== null && #state in value;
    }
    static stateOf(value) {
      return value.#state;
    }
  }
  const iteratorIsIterator = ReadableStreamAsyncIterator.isIterator;
  const iteratorStateOf = ReadableStreamAsyncIterator.stateOf;
  delete ReadableStreamAsyncIterator.isIterator;
  delete ReadableStreamAsyncIterator.stateOf;
  const iteratorNotBound = () => invalidState("The reader is not bound to a ReadableStream");

  function iteratorNextSteps(state) {
    if (state.done) return PromiseResolve({ done: true, value: undefined });
    if (state.stream._reader !== state.reader) return PromiseReject(iteratorNotBound());
    state.promise = deferred();
    readableReaderRead(state.stream, state.request);
    return state.promise.promise;
  }

  function iteratorReturnSteps(state, value) {
    const iterResult = { done: true, value };
    if (state.done) return PromiseResolve(iterResult);
    state.done = true;
    try {
      const { stream, reader } = state;
      if (stream._reader !== reader) throw iteratorNotBound();
      if (!state.preventCancel) {
        const result = readableCancel(stream, value);
        readableReaderGenericRelease(stream, reader);
        return then(result, () => iterResult);
      }
      readableReaderGenericRelease(stream, reader);
      return PromiseResolve(iterResult);
    } catch (error) {
      return PromiseReject(error);
    }
  }

  const iteratorMethods = {
    next() {
      if (!iteratorIsIterator(this)) {
        return PromiseReject(new (codes().ERR_INVALID_THIS)("ReadableStreamAsyncIterator"));
      }
      const state = iteratorStateOf(this);
      if (!state.started) {
        state.current = kResolvedPromise;
        state.started = true;
      }
      if (state.current !== undefined) {
        state.chainedNextSteps ??= () => iteratorNextSteps(state);
        state.current = then(state.current, state.chainedNextSteps, state.chainedNextSteps);
        return state.current;
      }
      // No read in flight: a chunk already queued is answered at once, as
      // node's read() fast path does.
      const stream = state.stream;
      if (
        !state.done &&
        stream._reader === state.reader &&
        stream._state === "readable" &&
        (stream._isBytes ? stream._queue.totalSize > 0 : stream._queue.length > 0)
      ) {
        let result;
        readableReaderRead(stream, {
          resolve(r) {
            result = r;
          },
          reject: noop,
        });
        return PromiseResolve({ done: false, value: result.value });
      }
      state.current = iteratorNextSteps(state);
      return state.current;
    },
    return(value) {
      if (!iteratorIsIterator(this)) {
        return PromiseReject(new (codes().ERR_INVALID_THIS)("ReadableStreamAsyncIterator"));
      }
      const state = iteratorStateOf(this);
      state.started = true;
      state.current =
        state.current !== undefined
          ? then(
              state.current,
              () => iteratorReturnSteps(state, value),
              () => iteratorReturnSteps(state, value),
            )
          : iteratorReturnSteps(state, value);
      return state.current;
    },
  };
  delete ReadableStreamAsyncIterator.prototype.constructor;
  Object.setPrototypeOf(
    ReadableStreamAsyncIterator.prototype,
    Object.getPrototypeOf(Object.getPrototypeOf(async function* () {}).prototype),
  );
  Object.defineProperties(ReadableStreamAsyncIterator.prototype, {
    next: { value: iteratorMethods.next, writable: true, enumerable: true, configurable: true },
    return: { value: iteratorMethods.return, writable: true, enumerable: true, configurable: true },
    [Symbol.toStringTag]: { value: "ReadableStream AsyncIterator", configurable: true },
  });

  function createReadableStreamAsyncIterator(stream, reader, preventCancel) {
    const state = {
      stream,
      reader,
      preventCancel,
      done: false,
      started: false,
      current: undefined,
      promise: undefined,
      chainedNextSteps: undefined,
      request: undefined,
    };
    // The iterator's read request (one read is in flight at a time, so it
    // is reused): a chunk, the end (which releases the lock), an error
    // (which releases it too).
    state.request = {
      resolve(result) {
        state.current = undefined;
        if (result.done) {
          state.done = true;
          readableReaderGenericRelease(stream, reader);
          state.promise.resolve({ done: true, value: undefined });
          return;
        }
        state.promise.resolve({ done: false, value: result.value });
      },
      reject(error) {
        state.current = undefined;
        state.done = true;
        readableReaderGenericRelease(stream, reader);
        state.promise.reject(error);
      },
    };
    return new ReadableStreamAsyncIterator(state);
  }

  function readableStreamTee(stream, cloneForBranch2) {
    if (stream._isBytes) return byteStreamTee(stream);
    return readableStreamDefaultTee(stream, cloneForBranch2);
  }

  // node v26.13.1's readableStreamDefaultTee: one read of the source at a
  // time, each chunk handed to both branches a microtask after it is read
  // (structuredClone'd for the second when cloneForBranch2), the end a
  // process.nextTick after it is read; a cancelled branch takes no more
  // chunks, and once both are cancelled the source is cancelled with the
  // two reasons. A source error errors both branches.
  function readableStreamDefaultTee(stream, cloneForBranch2) {
    const reader = acquireDefaultReader(stream);
    let reading = false;
    let canceled1 = false;
    let canceled2 = false;
    let reason1;
    let reason2;
    let branch1;
    let branch2;
    let cancelPromise;
    let cancelSettled = false;
    const settleCancelPromise = () => {
      if (cancelPromise !== undefined) cancelPromise.resolve();
      else cancelSettled = true;
    };
    const cancelPromiseRecord = () => {
      if (cancelPromise === undefined) {
        cancelPromise = deferred();
        if (cancelSettled) cancelPromise.resolve();
      }
      return cancelPromise;
    };
    let pendingChunk;
    let readRequest;
    const forwardChunk = () => {
      reading = false;
      const value1 = pendingChunk;
      let value2 = pendingChunk;
      pendingChunk = undefined;
      if (!canceled2 && cloneForBranch2) value2 = globalThis.structuredClone(value2);
      if (!canceled1) defaultControllerEnqueue(branch1, value1);
      if (!canceled2) defaultControllerEnqueue(branch2, value2);
    };
    const pullAlgorithm = () => {
      if (reading) return;
      reading = true;
      readRequest ??= {
        resolve(result) {
          if (!result.done) {
            pendingChunk = result.value;
            then(kResolvedPromise, forwardChunk);
            return;
          }
          // node's process.nextTick (not the standard's; nodejs/node#39758).
          globalThis.process.nextTick(() => {
            reading = false;
            if (!canceled1) defaultControllerClose(branch1);
            if (!canceled2) defaultControllerClose(branch2);
            if (!canceled1 || !canceled2) settleCancelPromise();
          });
        },
        reject() {
          reading = false;
        },
      };
      readableReaderRead(stream, readRequest);
    };
    const cancel1Algorithm = (reason) => {
      canceled1 = true;
      reason1 = reason;
      const record = cancelPromiseRecord();
      if (canceled2) record.resolve(readableCancel(stream, [reason1, reason2]));
      return record.promise;
    };
    const cancel2Algorithm = (reason) => {
      canceled2 = true;
      reason2 = reason;
      const record = cancelPromiseRecord();
      if (canceled1) record.resolve(readableCancel(stream, [reason1, reason2]));
      return record.promise;
    };
    branch1 = createReadable(nonOpCallback, pullAlgorithm, cancel1Algorithm, 1, countSize);
    branch2 = createReadable(nonOpCallback, pullAlgorithm, cancel2Algorithm, 1, countSize);
    then(reader.closed, undefined, (error) => {
      readableError(branch1, error);
      readableError(branch2, error);
      if (!canceled1 || !canceled2) settleCancelPromise();
    });
    return [branch1, branch2];
  }

  // node v26.13.1's stream/web ReadableStreamTee(stream, cloneForBranch2):
  // what undici 8's Response/Request clone() tees a body with.
  function ReadableStreamTee(stream, cloneForBranch2 = false) {
    if (!(stream instanceof ReadableStream)) {
      throw new (codes().ERR_INVALID_ARG_TYPE)("stream", "ReadableStream", stream);
    }
    if (typeof cloneForBranch2 !== "boolean") {
      throw new (codes().ERR_INVALID_ARG_TYPE)("cloneForBranch2", "boolean", cloneForBranch2);
    }
    return readableStreamTee(stream, cloneForBranch2);
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
      // node's setup*ControllerFromSource: start bound, pull a raw callback,
      // cancel an async one, each validated when it is set (truthy) at all.
      const start = source.start;
      const pull = source.pull;
      const cancel = source.cancel;
      const autoAllocateChunkSize = isBytes ? source.autoAllocateChunkSize : undefined;
      const startAlgorithm = start ? bindStart(start, source) : nonOpCallback;
      const pullAlgorithm = pull ? rawCallback1("source.pull", pull, source) : nonOpCallback;
      const cancelAlgorithm = cancel ? promiseCallback1("source.cancel", cancel, source) : nonOpCancel;
      // Read, and 0 refused, as node does; a default read gets no
      // byobRequest for it to size (docs/node-divergences.md).
      if (isBytes && autoAllocateChunkSize === 0) {
        throw new (codes().ERR_INVALID_ARG_VALUE)("source.autoAllocateChunkSize", autoAllocateChunkSize);
      }
      setupReadable(this, startAlgorithm, pullAlgorithm, cancelAlgorithm);
    }

    get locked() {
      return this._reader !== null;
    }

    // node's [kInspect] through its customInspect: the three-field summary
    // in place of the internal fields. Past the depth limit node's helper
    // returns its own `this`, which is undefined there (it is called as a
    // plain function), so util.inspect prints `undefined`; this does too.
    [Symbol.for("nodejs.util.inspect.custom")](depth, options, inspect) {
      if (depth < 0) return undefined;
      const opts = { ...options, depth: options.depth == null ? null : options.depth - 1 };
      inspect ??= globalThis.__oamNode.get("util").inspect;
      return `ReadableStream ${inspect({ locked: this.locked, state: this._state, supportsBYOB: this._isBytes }, opts)}`;
    }

    // node's checks: options an object (or null), then the mode -- absent
    // for a default reader, else it must stringify to "byob".
    getReader(options = {}) {
      validateObjectArg(options, "options", true);
      const mode = options?.mode;
      if (mode === undefined) return acquireDefaultReader(this);
      if (`${mode}` !== "byob") throw new (codes().ERR_INVALID_ARG_VALUE)("options.mode", mode);
      return acquireBYOBReader(this);
    }

    cancel(reason) {
      if (this.locked) return Promise.reject(invalidState("ReadableStream is locked"));
      return readableCancel(this, reason);
    }

    // node v26.13.1's values(): options an object (or null), then a default
    // reader and the shared-prototype async iterator over it.
    // [Symbol.asyncIterator] is this same function (defined below).
    values(options = {}) {
      validateObjectArg(options, "options", true);
      const preventCancel = !!options?.preventCancel;
      const reader = acquireDefaultReader(this);
      return createReadableStreamAsyncIterator(this, reader, preventCancel);
    }

    // A byte stream tees as the standard's ReadableByteStreamTee (below); a
    // default stream as node's readableStreamDefaultTee, its branches
    // sharing each chunk.
    tee() {
      return readableStreamTee(this, false);
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

    // node v26.13.1's readableStreamFromIterable: the async iterator method
    // (else the sync one) is called directly -- a sync iterator is not
    // wrapped -- and its `next` looked up once. A high-water mark of 0, so
    // the iterator is not advanced until a read asks. Each pull is parked:
    // the next() result is adopted as `await` would (a value that is an
    // object is adopted too), and completion goes straight to the
    // controller's pull reactions.
    static from(iterable) {
      let stream;
      const iteratorGetter = iterable[Symbol.asyncIterator] ?? iterable[Symbol.iterator];
      if (iteratorGetter == null || typeof iteratorGetter !== "function") {
        throw new (codes().ERR_ARG_NOT_ITERABLE)(iterable);
      }
      const iterator = ReflectApply(iteratorGetter, iterable, []);
      if (iterator === null || (typeof iterator !== "object" && typeof iterator !== "function")) {
        throw invalidState("The iterator method must return an object");
      }
      const nextMethod = iterator.next;
      let pendingIterResult;
      const rejectPull = (error) => readableError(stream, error);
      const enqueueValue = (value) => {
        try {
          defaultControllerEnqueue(stream, value);
        } catch (error) {
          rejectPull(error);
          return;
        }
        stream._pullFulfilled();
      };
      const processIterResult = (iterResult) => {
        if (typeof iterResult !== "object" || iterResult === null) {
          rejectPull(
            invalidState("The promise returned by the iterator.next() method must fulfill with an object"),
          );
          return;
        }
        try {
          if (iterResult.done) {
            defaultControllerClose(stream);
          } else {
            const value = iterResult.value;
            if (value !== null && (typeof value === "object" || typeof value === "function")) {
              then(PromiseResolve(value), enqueueValue, rejectPull);
              return;
            }
            defaultControllerEnqueue(stream, value);
          }
        } catch (error) {
          rejectPull(error);
          return;
        }
        stream._pullFulfilled();
      };
      const processPendingIterResult = () => {
        const iterResult = pendingIterResult;
        pendingIterResult = undefined;
        processIterResult(iterResult);
      };
      const pullAlgorithm = () => {
        let nextResult;
        try {
          nextResult = ReflectApply(nextMethod, iterator, []);
        } catch (error) {
          return PromiseReject(error);
        }
        if (isThenableResult(nextResult)) {
          then(PromiseResolve(nextResult), processIterResult, rejectPull);
          return kParked;
        }
        // A non-thenable next() result fails validation a microtask later.
        pendingIterResult = nextResult;
        then(kResolvedPromise, processPendingIterResult);
        return kParked;
      };
      const cancelAlgorithm = async (reason) => {
        const returnMethod = iterator.return;
        if (returnMethod === undefined) return;
        const iterResult = await ReflectApply(returnMethod, iterator, [reason]);
        if (typeof iterResult !== "object" || iterResult === null) {
          throw invalidState("The promise returned by the iterator.return() method must fulfill with an object");
        }
      };
      stream = createReadable(nonOpCallback, pullAlgorithm, cancelAlgorithm, 0, countSize);
      return stream;
    }
  }

  Object.defineProperty(ReadableStream.prototype, Symbol.asyncIterator, {
    value: ReadableStream.prototype.values,
    writable: true,
    enumerable: false,
    configurable: true,
  });

  // ------------------------------------------------------- WritableStream
  const CLOSE_SENTINEL = {};

  class WritableStreamDefaultController {
    constructor(stream, highWaterMark, sizeFn) {
      this._stream = stream;
      this._queue = new Queue();
      this._highWaterMark = highWaterMark;
      this._sizeFn = sizeFn;
      this._started = false;
      this._writeAlgorithm = nonOpCallback;
      this._closeAlgorithm = nonOpCancel;
      this._abortAlgorithm = nonOpCancel;
      // node's cached write reactions, made on the first write (a parked
      // TransformStream write completes through them).
      this._writeFulfilled = undefined;
      this._writeRejected = undefined;
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
    const started = () => {
      controller._started = true;
      writableAdvanceQueueIfNeeded(controller);
    };
    // node v26.13.1: a non-thenable start result is started a microtask
    // later (its queueMicrotask); a thenable one through the wrapper
    // promise, with its extra microtasks.
    if (!isThenableResult(startResult)) {
      then(kResolvedPromise, started);
      return;
    }
    then(
      new PromiseCtor((resolve) => resolve(startResult)),
      started,
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
    let state = stream._state;
    if (state === "closed" || state === "errored") return Promise.resolve();
    writableSignalAbort(stream._controller, reason);
    // node v26.13.1 looks again: an abort listener on the controller's
    // signal may have closed or errored the stream.
    state = stream._state;
    if (state === "closed" || state === "errored") return PromiseResolve();
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

  // The new backpressure is published BEFORE the ready record is resolved:
  // a pipe's ready hook re-enters its pump from that resolve and reads it
  // (node v26.13.1).
  function writableUpdateBackpressure(stream, backpressure) {
    const writer = stream._writer;
    const changed = stream._backpressure !== backpressure;
    stream._backpressure = backpressure;
    if (writer !== undefined && changed) {
      if (backpressure) writer._ready = deferred();
      else writer._ready.resolve?.();
    }
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
    settleIsClosed(stream, stream._storedError, true);
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
    settleIsClosed(stream);
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

  // node v26.13.1: the write reactions are made once and reused; the write
  // is a raw call whose result goes through thenAlgorithmResult (a
  // synchronous throw as a rejection; kParked from a TransformStream whose
  // write waits on backpressure).
  function writableProcessWrite(controller, chunk) {
    const stream = controller._stream;
    stream._inFlightWriteRequest = stream._writeRequests.shift();
    if (controller._writeFulfilled === undefined) {
      controller._writeFulfilled = () => {
        writableFinishInFlightWrite(stream);
        const state = stream._state;
        writableDequeue(controller);
        if (!writableCloseQueuedOrInFlight(stream) && state === "writable") {
          writableUpdateBackpressure(stream, writableGetBackpressure(controller));
        }
        writableAdvanceQueueIfNeeded(controller);
      };
      controller._writeRejected = (error) => {
        if (stream._state === "writable") writableClearAlgorithms(controller);
        writableFinishInFlightWriteWithError(stream, error);
      };
    }
    let result;
    try {
      result = controller._writeAlgorithm(chunk, controller);
    } catch (error) {
      result = PromiseReject(error);
    }
    thenAlgorithmResult(result, controller._writeFulfilled, controller._writeRejected);
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

  // node v26.13.1's writableStreamDefaultWriterWriteWithRequest, for
  // pipeTo: one shared request (a counter of pending writes) instead of a
  // promise per write; a write that cannot proceed is latched on it.
  function writerWriteWithRequest(writer, chunk, request) {
    const stream = writer._stream;
    const controller = stream._controller;
    const chunkSize = writableGetChunkSize(controller, chunk);
    const state = stream._state;
    if (state === "errored") {
      request.failed = true;
      request.failure = stream._storedError;
      return;
    }
    if (writableCloseQueuedOrInFlight(stream) || state === "closed") {
      request.failed = true;
      request.failure = invalidState("WritableStream is closed");
      return;
    }
    if (state === "erroring") {
      request.failed = true;
      request.failure = stream._storedError;
      return;
    }
    stream._writeRequests.push(request);
    request.pending++;
    writableControllerWrite(controller, chunk, chunkSize);
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
      // node's setupWritableStreamDefaultControllerFromSink: start bound,
      // write a raw callback, close and abort async ones (close called with
      // no arguments), each validated when it is set at all.
      const start = sink.start;
      const write = sink.write;
      const close = sink.close;
      const abort = sink.abort;
      setupWritable(
        this,
        start ? bindStart(start, sink) : nonOpCallback,
        write ? rawCallback2("sink.write", write, sink) : nonOpCallback,
        close ? promiseCallback0("sink.close", close, sink) : nonOpCancel,
        abort ? promiseCallback1("sink.abort", abort, sink) : nonOpCancel,
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
  //
  // node v26.13.1's lib/internal/webstreams/transformstream.js: the
  // standard's [[backpressureChangePromise]] is replaced by two parked
  // continuations, each delivered off the shared resolved promise at the
  // microtask position the promise's reaction had -- a readable-side pull
  // parked until backpressure turns on again, and a write parked until it
  // turns off (which then runs the transform and wires its result straight
  // to the writable's write reactions, a promise per chunk fewer than v22).
  class TransformStreamDefaultController {
    constructor(stream, transformAlgorithm, flushAlgorithm, cancelAlgorithm) {
      this._stream = stream;
      this._transformAlgorithm = transformAlgorithm;
      this._flushAlgorithm = flushAlgorithm;
      this._cancelAlgorithm = cancelAlgorithm;
      this._performTransformRejected = undefined;
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
      defaultControllerClose(stream._readable);
      transformErrorWritableAndUnblockWrite(
        stream,
        invalidState("TransformStream has been terminated"),
      );
    }
  }

  // No `transform` means the IDENTITY transform, which enqueues the chunk
  // unchanged -- per spec, and it is the whole point of the bare `new
  // TransformStream()` used as a plumbing pipe. A raw callback, as node's.
  function identityTransform(chunk, controller) {
    transformEnqueue(controller, chunk);
  }

  function transformSetBackpressure(stream, backpressure) {
    stream._backpressure = backpressure;
    if (backpressure) {
      if (stream._pullPending) {
        stream._pullPending = false;
        then(kResolvedPromise, stream._readable._pullFulfilled);
      }
    } else if (stream._pendingWriteParked) {
      then(kResolvedPromise, stream._writeContinuation);
    }
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

  // node's transformStreamDefaultControllerPerformTransform: the reference
  // implementation's `promiseCall(transform).then(undefined, onRejected)`
  // -- settled a microtask after the (coerced) result, a rejection
  // erroring the stream before it propagates.
  function transformPerformTransform(controller, chunk) {
    const transformAlgorithm = controller._transformAlgorithm;
    if (transformAlgorithm === undefined) return kResolvedPromise;
    let result;
    try {
      result = transformAlgorithm(chunk, controller);
    } catch (error) {
      result = PromiseReject(error);
    }
    result = isThenableResult(result) ? PromiseResolve(result) : kResolvedPromise;
    controller._performTransformRejected ??= (error) => {
      transformError(controller._stream, error);
      throw error;
    };
    return then(result, undefined, controller._performTransformRejected);
  }

  function transformSinkWrite(stream, chunk) {
    const controller = stream._controller;
    if (stream._backpressure) {
      // Parked: the flip of backpressure to false delivers the
      // continuation, which completes the write through the writable
      // controller's write reactions (made before this ran). One write is
      // in flight at a time, so one slot holds the chunk.
      stream._pendingWriteParked = true;
      stream._pendingWriteChunk = chunk;
      stream._writeContinuation ??= () => {
        const pendingChunk = stream._pendingWriteChunk;
        stream._pendingWriteParked = false;
        stream._pendingWriteChunk = undefined;
        const writable = stream._writable;
        const writableController = writable._controller;
        if (writable._state === "erroring") {
          const error = writable._storedError;
          then(kResolvedPromise, () => writableController._writeRejected(error));
          return;
        }
        then(
          transformPerformTransform(stream._controller, pendingChunk),
          writableController._writeFulfilled,
          writableController._writeRejected,
        );
      };
      return kParked;
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
          defaultControllerClose(readable);
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

  // Parked: the next flip of backpressure to true delivers the readable
  // controller's pull-fulfilled step.
  function transformSourcePull(stream) {
    transformSetBackpressure(stream, false);
    stream._pullPending = true;
    return kParked;
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

  // node's no-start start algorithm: the shared resolved promise (a
  // thenable, so both sides start through the wrapper promise).
  const resolvedStartAlgorithm = () => kResolvedPromise;

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

      // Both sides start from ONE promise, resolved with transformer.start's
      // result below -- so a throw from start() is this constructor's throw.
      // Without a start() it is the shared resolved promise.
      const startPromise = start !== undefined ? deferred() : undefined;
      const startAlgorithm =
        startPromise === undefined ? resolvedStartAlgorithm : () => startPromise.promise;

      this._backpressure = undefined;
      this._pullPending = false;
      this._pendingWriteParked = false;
      this._pendingWriteChunk = undefined;
      this._writeContinuation = undefined;
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

      // node's setupTransformStreamDefaultControllerFromTransformer:
      // transform a raw callback, flush and cancel async ones, each
      // validated when it is set at all.
      const transform = transformer.transform;
      const flush = transformer.flush;
      const cancel = transformer.cancel;
      const controller = new TransformStreamDefaultController(
        this,
        transform ? rawCallback2("transformer.transform", transform, transformer) : identityTransform,
        flush ? promiseCallback1("transformer.flush", flush, transformer) : nonOpCancel,
        cancel ? promiseCallback1("transformer.cancel", cancel, transformer) : nonOpCancel,
      );
      this._controller = controller;

      if (start !== undefined) startPromise.resolve(ReflectApply(start, transformer, [controller]));
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
      // A chunk ending in a high surrogate holds it back for the next
      // chunk, which may open with its low half (the Encoding standard's
      // pending high surrogate, as node carries it); an unpaired one is
      // U+FFFD, at the next chunk through encode() or at the end in flush.
      let pendingHighSurrogate = "";
      const transform = new TransformStream({
        transform(chunk, controller) {
          let text = String(chunk);
          if (pendingHighSurrogate !== "") {
            text = pendingHighSurrogate + text;
            pendingHighSurrogate = "";
          }
          if (text.length > 0) {
            const last = text.charCodeAt(text.length - 1);
            if (last >= 0xd800 && last <= 0xdbff) {
              pendingHighSurrogate = text.slice(-1);
              text = text.slice(0, -1);
            }
          }
          const bytes = encoder.encode(text);
          if (bytes.length > 0) controller.enqueue(bytes);
        },
        flush(controller) {
          if (pendingHighSurrogate !== "") {
            pendingHighSurrogate = "";
            controller.enqueue(new Uint8Array([0xef, 0xbf, 0xbd]));
          }
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
  // node's kIsClosedPromise (see isClosedPromise).
  Object.defineProperty(ReadableStream.prototype, kIsClosedPromise, {
    get() {
      return isClosedPromise(this, this._state === "closed", this._state === "errored", this._error);
    },
    configurable: true,
  });
  Object.defineProperty(WritableStream.prototype, kIsClosedPromise, {
    get() {
      return isClosedPromise(this, this._state === "closed", this._state === "errored", this._storedError);
    },
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
  // stream/web's ReadableStreamTee, for node_compat's module factory: a
  // hidden global, as the other cross-file hooks are.
  Object.defineProperty(globalThis, "__oamReadableStreamTee", {
    value: ReadableStreamTee,
    writable: false,
    enumerable: false,
    configurable: true,
  });
  globalThis.ReadableStream = ReadableStream;
  globalThis.WritableStream = WritableStream;
  globalThis.TransformStream = TransformStream;
  globalThis.TextDecoderStream = TextDecoderStream;
  globalThis.TextEncoderStream = TextEncoderStream;
  globalThis.CountQueuingStrategy = CountQueuingStrategy;
  globalThis.ByteLengthQueuingStrategy = ByteLengthQueuingStrategy;
})();
