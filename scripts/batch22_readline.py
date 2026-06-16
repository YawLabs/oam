#!/usr/bin/env python3
"""Enhance node:readline -- real ANSI escapes for clearLine/cursorTo/moveCursor,
working prompt/write/pause/resume/setPrompt, crlfDelay option."""
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

OLD = r'''  registry.factories.readline = () => {
    const EventEmitter = registry.get("events");
    class Interface extends EventEmitter {
      constructor(options) {
        super();
        this.input = (options && options.input) || null;
        this.output = (options && options.output) || null;
        this.terminal = options && options.terminal === true;
        this._closed = false;
        if (this.input && typeof this.input.on === "function") {
          const dec = new TextDecoder();
          let buf = "";
          this.input.on("data", (chunk) => {
            buf += typeof chunk === "string" ? chunk : dec.decode(chunk, { stream: true });
            const parts = buf.split(/\r?\n/);
            buf = parts.pop() || "";
            for (const line of parts) this.emit("line", line);
          });
          this.input.on("end", () => {
            if (buf.length) { this.emit("line", buf); buf = ""; }
            this.close();
          });
        }
      }
      close() {
        if (this._closed) return;
        this._closed = true;
        this.emit("close");
      }
      question(prompt, cb) {
        if (this.output && typeof this.output.write === "function") this.output.write(prompt);
        const onLine = (line) => { this.removeListener("line", onLine); cb(line); };
        this.once("line", onLine);
      }
      setPrompt() {}
      prompt() {}
      write() {}
      pause() { return this; }
      resume() { return this; }
      [Symbol.asyncIterator]() {
        const self = this;
        const queue = [];
        let resolver = null;
        let done = false;
        self.on("line", (line) => {
          if (resolver) { const r = resolver; resolver = null; r({ value: line, done: false }); }
          else queue.push(line);
        });
        self.once("close", () => {
          done = true;
          if (resolver) { const r = resolver; resolver = null; r({ value: undefined, done: true }); }
        });
        return {
          next() {
            if (queue.length) return Promise.resolve({ value: queue.shift(), done: false });
            if (done) return Promise.resolve({ value: undefined, done: true });
            return new Promise((r) => { resolver = r; });
          },
        };
      }
    }
    function createInterface(options) {
      if (typeof options === "string" || (options && !("input" in options) && !("output" in options))) {
        return new Interface(typeof options === "string" ? { prompt: options } : options || {});
      }
      return new Interface(options || {});
    }
    function clearLine(stream, dir, cb) { if (typeof cb === "function") queueMicrotask(cb); }
    function clearScreenDown(stream, cb) { if (typeof cb === "function") queueMicrotask(cb); }
    function cursorTo(stream, x, y, cb) {
      if (typeof y === "function") { cb = y; }
      if (typeof cb === "function") queueMicrotask(cb);
    }
    function moveCursor(stream, dx, dy, cb) { if (typeof cb === "function") queueMicrotask(cb); }
    function emitKeypressEvents() {}
    return {
      Interface, createInterface,
      clearLine, clearScreenDown, cursorTo, moveCursor, emitKeypressEvents,
    };
  };

  registry.factories["readline/promises"] = () => {
    var rl = registry.get("readline");
    class Interface extends rl.Interface {
      question(prompt) {
        return new Promise(function (resolve) {
          rl.Interface.prototype.question.call(this, prompt, resolve);
        }.bind(this));
      }
    }
    return {
      createInterface: function (options) {
        var iface = rl.createInterface(options);
        Object.setPrototypeOf(iface, Interface.prototype);
        return iface;
      },
      Interface: Interface,
    };
  };'''

NEW = r'''  registry.factories.readline = () => {
    const EventEmitter = registry.get("events");
    class Interface extends EventEmitter {
      constructor(options) {
        super();
        const opts = options || {};
        this.input = opts.input || null;
        this.output = opts.output || null;
        this.terminal = opts.terminal === true;
        this._closed = false;
        this._paused = false;
        this._prompt = typeof opts.prompt === "string" ? opts.prompt : "> ";
        this.crlfDelay = typeof opts.crlfDelay === "number" ? opts.crlfDelay : 100;
        this.line = "";
        if (this.input && typeof this.input.on === "function") {
          const dec = new TextDecoder();
          let buf = "";
          this.input.on("data", (chunk) => {
            if (this._closed) return;
            buf += typeof chunk === "string" ? chunk : dec.decode(chunk, { stream: true });
            const parts = buf.split(/\r?\n/);
            buf = parts.pop() || "";
            for (const line of parts) {
              this.line = line;
              this.emit("line", line);
            }
          });
          this.input.on("end", () => {
            if (buf.length) {
              this.line = buf;
              this.emit("line", buf);
              buf = "";
            }
            this.close();
          });
        }
      }
      close() {
        if (this._closed) return;
        this._closed = true;
        if (this.input && typeof this.input.pause === "function") {
          try { this.input.pause(); } catch (_e) { /* ignore */ }
        }
        this.emit("close");
      }
      question(prompt, cb) {
        if (this.output && typeof this.output.write === "function") this.output.write(prompt);
        const onLine = (line) => { this.removeListener("line", onLine); cb(line); };
        this.once("line", onLine);
      }
      setPrompt(prompt) {
        this._prompt = typeof prompt === "string" ? prompt : "> ";
      }
      prompt(preserveCursor) {
        if (this.output && typeof this.output.write === "function") {
          this.output.write(this._prompt);
        }
      }
      write(data, key) {
        if (this.output && typeof this.output.write === "function" && data != null) {
          this.output.write(typeof data === "string" ? data : String(data));
        }
      }
      pause() {
        if (!this._paused) {
          this._paused = true;
          if (this.input && typeof this.input.pause === "function") {
            this.input.pause();
          }
          this.emit("pause");
        }
        return this;
      }
      resume() {
        if (this._paused) {
          this._paused = false;
          if (this.input && typeof this.input.resume === "function") {
            this.input.resume();
          }
          this.emit("resume");
        }
        return this;
      }
      [Symbol.asyncIterator]() {
        const self = this;
        const queue = [];
        let resolver = null;
        let done = false;
        self.on("line", (line) => {
          if (resolver) { const r = resolver; resolver = null; r({ value: line, done: false }); }
          else queue.push(line);
        });
        self.once("close", () => {
          done = true;
          if (resolver) { const r = resolver; resolver = null; r({ value: undefined, done: true }); }
        });
        return {
          next() {
            if (queue.length) return Promise.resolve({ value: queue.shift(), done: false });
            if (done) return Promise.resolve({ value: undefined, done: true });
            return new Promise((r) => { resolver = r; });
          },
        };
      }
    }
    function createInterface(options) {
      if (typeof options === "string" || (options && !("input" in options) && !("output" in options))) {
        return new Interface(typeof options === "string" ? { prompt: options } : options || {});
      }
      return new Interface(options || {});
    }
    function clearLine(stream, dir, cb) {
      if (!stream || typeof stream.write !== "function") {
        if (typeof cb === "function") queueMicrotask(cb);
        return false;
      }
      if (dir === -1) {
        stream.write("\x1b[1K");
      } else if (dir === 1) {
        stream.write("\x1b[0K");
      } else {
        stream.write("\x1b[2K");
      }
      if (typeof cb === "function") queueMicrotask(cb);
      return true;
    }
    function clearScreenDown(stream, cb) {
      if (stream && typeof stream.write === "function") {
        stream.write("\x1b[0J");
      }
      if (typeof cb === "function") queueMicrotask(cb);
    }
    function cursorTo(stream, x, y, cb) {
      if (typeof y === "function") { cb = y; y = undefined; }
      if (stream && typeof stream.write === "function") {
        if (typeof x === "number") {
          if (typeof y === "number") {
            stream.write("\x1b[" + (y + 1) + ";" + (x + 1) + "H");
          } else {
            stream.write("\x1b[" + (x + 1) + "G");
          }
        }
      }
      if (typeof cb === "function") queueMicrotask(cb);
      return true;
    }
    function moveCursor(stream, dx, dy, cb) {
      if (stream && typeof stream.write === "function") {
        if (dx !== 0 && typeof dx === "number") {
          if (dx > 0) stream.write("\x1b[" + dx + "C");
          else stream.write("\x1b[" + (-dx) + "D");
        }
        if (dy !== 0 && typeof dy === "number") {
          if (dy > 0) stream.write("\x1b[" + dy + "B");
          else stream.write("\x1b[" + (-dy) + "A");
        }
      }
      if (typeof cb === "function") queueMicrotask(cb);
      return true;
    }
    function emitKeypressEvents() {}
    return {
      Interface, createInterface,
      clearLine, clearScreenDown, cursorTo, moveCursor, emitKeypressEvents,
    };
  };

  registry.factories["readline/promises"] = () => {
    var rl = registry.get("readline");
    class Interface extends rl.Interface {
      question(prompt) {
        return new Promise(function (resolve) {
          rl.Interface.prototype.question.call(this, prompt, resolve);
        }.bind(this));
      }
    }
    return {
      createInterface: function (options) {
        var iface = rl.createInterface(options);
        Object.setPrototypeOf(iface, Interface.prototype);
        return iface;
      },
      Interface: Interface,
    };
  };'''

assert OLD in src, "anchor not found -- readline factory text does not match"
src = src.replace(OLD, NEW, 1)
p.write_text(src, encoding="utf-8")
print("OK -- readline enhanced")
