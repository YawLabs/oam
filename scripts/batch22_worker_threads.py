#!/usr/bin/env python3
"""Enhance worker_threads: SHARE_ENV, online event, name option,
Symbol.dispose, getEnvironmentData/setEnvironmentData, getHeapSnapshot stub,
performance ref, markAsUntransferable with WeakSet."""
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

OLD = r'''  // --------------------------------------------------------- worker_threads
  // MessageChannel / MessagePort are fully functional (in-process). Worker
  // class throws on construction (thread spawn needs native ops).
  registry.factories.worker_threads = () => {
    const EventEmitter = registry.get("events");
    class MessagePort extends EventEmitter {
      constructor() { super(); this._twin = null; this._active = true; }
      postMessage(data) {
        const twin = this._twin;
        if (twin && twin._active) queueMicrotask(() => twin.emit("message", data));
      }
      start() { return this; }
      close() { this._active = false; this.emit("close"); }
      ref() { return this; }
      unref() { return this; }
      addEventListener(type, fn) { this.on(type, fn); }
      removeEventListener(type, fn) { this.off(type, fn); }
    }
    class MessageChannel {
      constructor() {
        this.port1 = new MessagePort();
        this.port2 = new MessagePort();
        this.port1._twin = this.port2;
        this.port2._twin = this.port1;
      }
    }
    const natives = globalThis.__oam.node;
    const pathMod = registry.get("path");
    class Worker extends EventEmitter {
      constructor(filename, opts = {}) {
        super();
        if (typeof filename !== "string") throw new TypeError("Worker requires a filename");
        const resolved = pathMod.resolve(filename);
        const wd = opts.workerData !== undefined ? JSON.stringify(opts.workerData) : null;
        const result = natives.workerNew(resolved, wd);
        this._workerId = result.workerId;
        this.threadId = result.threadId;
        this._recvLoop();
      }
      async _recvLoop() {
        while (true) {
          let raw;
          try { raw = await natives.workerRecvMessage(this._workerId); } catch { break; }
          if (raw === undefined) { this.emit("exit", 0); break; }
          if (raw instanceof Uint8Array) {
            const text = new TextDecoder().decode(raw);
            try {
              const data = JSON.parse(text);
              this.emit("message", data);
            } catch {
              this.emit("message", text);
            }
          } else if (typeof raw === "object" && raw !== null) {
            if (raw.type === "error") { this.emit("error", new Error(raw.message)); }
            if (raw.type === "exit") { this.emit("exit", raw.code); break; }
          }
        }
      }
      postMessage(data) {
        natives.workerPostMessage(this._workerId, JSON.stringify(data));
      }
      terminate() {
        natives.workerTerminate(this._workerId);
        return Promise.resolve(0);
      }
      ref() { return this; }
      unref() { return this; }
    }

    const isMain = natives.workerIsMainThread();
    const threadId = natives.workerThreadId();
    const workerData = natives.workerGetData();

    let parentPort = null;
    if (!isMain) {
      parentPort = new MessagePort();
      parentPort.postMessage = function postMessage(data) {
        natives.parentPortPostMessage(JSON.stringify(data));
      };
      (async () => {
        while (true) {
          let raw;
          try { raw = await natives.parentPortRecvMessage(); } catch { break; }
          if (raw === undefined) break;
          if (raw instanceof Uint8Array) {
            const text = new TextDecoder().decode(raw);
            try { parentPort.emit("message", JSON.parse(text)); }
            catch { parentPort.emit("message", text); }
          }
        }
      })();
    }

    const _bc = typeof globalThis.BroadcastChannel !== "undefined" ? globalThis.BroadcastChannel : null;
    return {
      isMainThread: isMain,
      parentPort,
      workerData,
      threadId,
      resourceLimits: {},
      MessageChannel,
      MessagePort,
      Worker,
      BroadcastChannel: _bc || class BroadcastChannel { constructor() { throw new Error("BroadcastChannel not available"); } },
      receiveMessageOnPort: () => undefined,
      markAsUntransferable: () => {},
      moveMessagePortToContext: () => { throw new Error("moveMessagePortToContext is not supported in oam"); },
      getEnvironmentData: () => undefined,
      setEnvironmentData: () => {},
    };
  };'''

NEW = r'''  // --------------------------------------------------------- worker_threads
  // MessageChannel / MessagePort are fully functional (in-process). Worker
  // class has native-backed thread spawning via workerNew/workerRecvMessage/
  // workerPostMessage/workerTerminate ops.
  registry.factories.worker_threads = () => {
    const EventEmitter = registry.get("events");

    // SHARE_ENV sentinel -- when passed as opts.env, workers share the
    // process environment (which they do by default in oam anyway).
    const SHARE_ENV = Symbol("nodejs.worker_threads.SHARE_ENV");

    // Module-level environment data store shared across threads in-process.
    const _envData = new Map();

    // WeakSet for markAsUntransferable -- marks objects so transfer attempts
    // can check (actual enforcement is future; the mark is the contract).
    const _untransferableSet = new WeakSet();

    class MessagePort extends EventEmitter {
      constructor() { super(); this._twin = null; this._active = true; }
      postMessage(data) {
        const twin = this._twin;
        if (twin && twin._active) queueMicrotask(() => twin.emit("message", data));
      }
      start() { return this; }
      close() { this._active = false; this.emit("close"); }
      ref() { return this; }
      unref() { return this; }
      addEventListener(type, fn) { this.on(type, fn); }
      removeEventListener(type, fn) { this.off(type, fn); }
    }
    class MessageChannel {
      constructor() {
        this.port1 = new MessagePort();
        this.port2 = new MessagePort();
        this.port1._twin = this.port2;
        this.port2._twin = this.port1;
      }
    }
    const natives = globalThis.__oam.node;
    const pathMod = registry.get("path");
    class Worker extends EventEmitter {
      constructor(filename, opts = {}) {
        super();
        if (typeof filename !== "string") throw new TypeError("Worker requires a filename");
        const resolved = pathMod.resolve(filename);
        const wd = opts.workerData !== undefined ? JSON.stringify(opts.workerData) : null;

        // Store worker name (Node >=12.11 option)
        this.name = opts.name || "";

        // Note SHARE_ENV usage (workers share process env by default in oam)
        this._shareEnv = opts.env === SHARE_ENV;

        const result = natives.workerNew(resolved, wd);
        this._workerId = result.workerId;
        this.threadId = result.threadId;

        // Expose performance reference (mirrors Node behavior)
        this.performance = globalThis.performance;

        this._recvLoop();

        // Emit 'online' asynchronously after construction, matching Node
        queueMicrotask(() => this.emit("online"));
      }
      async _recvLoop() {
        while (true) {
          let raw;
          try { raw = await natives.workerRecvMessage(this._workerId); } catch { break; }
          if (raw === undefined) { this.emit("exit", 0); break; }
          if (raw instanceof Uint8Array) {
            const text = new TextDecoder().decode(raw);
            try {
              const data = JSON.parse(text);
              this.emit("message", data);
            } catch {
              this.emit("message", text);
            }
          } else if (typeof raw === "object" && raw !== null) {
            if (raw.type === "error") { this.emit("error", new Error(raw.message)); }
            if (raw.type === "exit") { this.emit("exit", raw.code); break; }
          }
        }
      }
      postMessage(data) {
        natives.workerPostMessage(this._workerId, JSON.stringify(data));
      }
      terminate() {
        natives.workerTerminate(this._workerId);
        return Promise.resolve(0);
      }
      ref() { return this; }
      unref() { return this; }
      getHeapSnapshot() { return Promise.resolve(new Uint8Array(0)); }
    }

    // Symbol.dispose support for the 'using' keyword
    if (typeof Symbol.dispose !== "undefined") {
      Worker.prototype[Symbol.dispose] = function() { this.terminate(); };
    }

    function getEnvironmentData(key) {
      const val = _envData.get(key);
      if (val === undefined) return undefined;
      // Return a clone via JSON round-trip
      return JSON.parse(JSON.stringify(val));
    }

    function setEnvironmentData(key, value) {
      if (value === undefined) {
        _envData.delete(key);
      } else {
        _envData.set(key, value);
      }
    }

    function markAsUntransferable(obj) {
      if (typeof obj !== "object" || obj === null) {
        throw new TypeError("markAsUntransferable expects an object");
      }
      _untransferableSet.add(obj);
    }

    const isMain = natives.workerIsMainThread();
    const threadId = natives.workerThreadId();
    const workerData = natives.workerGetData();

    let parentPort = null;
    if (!isMain) {
      parentPort = new MessagePort();
      parentPort.postMessage = function postMessage(data) {
        natives.parentPortPostMessage(JSON.stringify(data));
      };
      (async () => {
        while (true) {
          let raw;
          try { raw = await natives.parentPortRecvMessage(); } catch { break; }
          if (raw === undefined) break;
          if (raw instanceof Uint8Array) {
            const text = new TextDecoder().decode(raw);
            try { parentPort.emit("message", JSON.parse(text)); }
            catch { parentPort.emit("message", text); }
          }
        }
      })();
    }

    const _bc = typeof globalThis.BroadcastChannel !== "undefined" ? globalThis.BroadcastChannel : null;
    return {
      isMainThread: isMain,
      parentPort,
      workerData,
      threadId,
      resourceLimits: {},
      SHARE_ENV,
      MessageChannel,
      MessagePort,
      Worker,
      BroadcastChannel: _bc || class BroadcastChannel { constructor() { throw new Error("BroadcastChannel not available"); } },
      receiveMessageOnPort: () => undefined,
      markAsUntransferable,
      moveMessagePortToContext: () => { throw new Error("moveMessagePortToContext is not supported in oam"); },
      getEnvironmentData,
      setEnvironmentData,
    };
  };'''

assert OLD in src, "anchor not found -- worker_threads factory text does not match"
src = src.replace(OLD, NEW, 1)
p.write_text(src, encoding="utf-8")
print("OK")
