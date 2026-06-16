#!/usr/bin/env python3
"""Graceful JS stubs + IncomingMessage lazy body push.

Replaces throw-on-import stubs with EventEmitter-based graceful stubs
that emit 'error' asynchronously, so libraries that import the module
but don't call the missing feature still work.
"""
import re

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    content = f.read()

# 1. IncomingMessage: lazy body push in _read() instead of constructor
old_incoming = """    class IncomingMessage extends Readable {
      constructor(meta) {
        super({});
        this.method = meta.method;
        this.url = meta.uri;
        this.httpVersion = "1.1";
        this.headers = {};
        this.rawHeaders = [];
        for (const [name, value] of meta.headers) {
          const key = name.toLowerCase();
          this.headers[key] = key in this.headers ? `${this.headers[key]}, ${value}` : value;
          this.rawHeaders.push(name, value);
        }
        this.socket = { remoteAddress: "127.0.0.1", encrypted: false };
        const body = natives.httpRequestBody(meta.requestId);
        if (body.length > 0) {
          this.push(new globalThis.Buffer(body.buffer, body.byteOffset, body.length));
        }
        this.push(null);
      }
      _read() {}
    }"""
new_incoming = """    class IncomingMessage extends Readable {
      constructor(meta) {
        super({});
        this.method = meta.method;
        this.url = meta.uri;
        this.httpVersion = "1.1";
        this.headers = {};
        this.rawHeaders = [];
        for (const [name, value] of meta.headers) {
          const key = name.toLowerCase();
          this.headers[key] = key in this.headers ? `${this.headers[key]}, ${value}` : value;
          this.rawHeaders.push(name, value);
        }
        this.socket = { remoteAddress: "127.0.0.1", encrypted: false };
        this._requestId = meta.requestId;
        this._bodyPushed = false;
      }
      _read() {
        if (!this._bodyPushed) {
          this._bodyPushed = true;
          const body = natives.httpRequestBody(this._requestId);
          if (body.length > 0) {
            this.push(new globalThis.Buffer(body.buffer, body.byteOffset, body.length));
          }
          this.push(null);
        }
      }
    }"""
assert old_incoming in content, "IncomingMessage pattern not found"
content = content.replace(old_incoming, new_incoming, 1)

# 2. tls.createServer: graceful EventEmitter stub
old_tls_create = """    function createServer() {
      throw new Error(
        "tls.createServer is not yet implemented in oam -- use https.createServer for HTTPS servers",
      );
    }"""
new_tls_create = """    function createServer(_options, _requestListener) {
      const server = new EventEmitter();
      server.listen = function () {
        process.nextTick(() =>
          server.emit(
            "error",
            new Error("tls.createServer is not yet implemented in oam -- use https.createServer"),
          ),
        );
        return server;
      };
      server.close = function (cb) {
        if (typeof cb === "function") cb();
        return server;
      };
      server.address = function () {
        return null;
      };
      return server;
    }"""
assert old_tls_create in content, "tls.createServer pattern not found"
content = content.replace(old_tls_create, new_tls_create, 1)

# 3. dgram: graceful mock socket
old_dgram = """  // ------------------------------------------------------------------ dgram
  registry.factories.dgram = () => {
    function notImpl(name) {
      return () => {
        throw new Error(
          `dgram.${name} is not implemented in oam -- UDP sockets land with a later wave`,
        );
      };
    }
    return { createSocket: notImpl("createSocket") };
  };"""
new_dgram = """  // ------------------------------------------------------------------ dgram
  registry.factories.dgram = () => {
    const EventEmitter = registry.get("events");
    function createSocket(_type, _callback) {
      const socket = new EventEmitter();
      socket.bind = function () {
        process.nextTick(() =>
          socket.emit("error", new Error("dgram is not implemented in oam")),
        );
        return socket;
      };
      socket.send = function () {
        const cb = arguments[arguments.length - 1];
        if (typeof cb === "function")
          cb(new Error("dgram.send is not implemented in oam"));
      };
      socket.close = function (cb) {
        if (typeof cb === "function") cb();
      };
      socket.address = function () {
        return { address: "0.0.0.0", family: "IPv4", port: 0 };
      };
      socket.addMembership = function () {};
      socket.dropMembership = function () {};
      socket.setBroadcast = function () {};
      socket.setMulticastLoopback = function () {};
      socket.setMulticastTTL = function () {};
      socket.setTTL = function () {};
      socket.ref = function () {
        return socket;
      };
      socket.unref = function () {
        return socket;
      };
      return socket;
    }
    return { createSocket };
  };"""
assert old_dgram in content, "dgram pattern not found"
content = content.replace(old_dgram, new_dgram, 1)

# 4. http2: graceful EventEmitter stubs
old_http2_funcs = """    function notImpl(name) {
      return () => {
        throw new Error(
          `http2.${name} is not implemented in oam -- HTTP/2 lands with a later wave`,
        );
      };
    }
    return {
      createServer: notImpl("createServer"),
      createSecureServer: notImpl("createSecureServer"),
      connect: notImpl("connect"),"""
new_http2_funcs = """    const EventEmitter = registry.get("events");
    function createServer(_options, _handler) {
      const server = new EventEmitter();
      server.listen = function () {
        process.nextTick(() =>
          server.emit(
            "error",
            new Error("http2.createServer is not implemented in oam"),
          ),
        );
        return server;
      };
      server.close = function (cb) {
        if (typeof cb === "function") cb();
        return server;
      };
      return server;
    }
    function createSecureServer(_options, _handler) {
      return createServer(_options, _handler);
    }
    function connect(_authority, _options) {
      const session = new EventEmitter();
      session.close = function () {};
      session.destroy = function () {};
      session.ref = function () { return session; };
      session.unref = function () { return session; };
      process.nextTick(() =>
        session.emit(
          "error",
          new Error("http2.connect is not implemented in oam"),
        ),
      );
      return session;
    }
    return {
      createServer,
      createSecureServer,
      connect,"""
assert old_http2_funcs in content, "http2 notImpl pattern not found"
content = content.replace(old_http2_funcs, new_http2_funcs, 1)

# 5. cluster.fork: graceful mock Worker
old_cluster_fork = """      fork() { throw new Error("cluster.fork is not implemented in oam"); }"""
new_cluster_fork = """      fork(_env) {
        const worker = new EventEmitter();
        worker.id = Object.keys(this.workers).length + 1;
        worker.process = { pid: 0, kill() {} };
        worker.isDead = () => true;
        worker.isConnected = () => false;
        worker.send = function () { return false; };
        worker.kill = function () {};
        worker.disconnect = function () {};
        this.workers[worker.id] = worker;
        process.nextTick(() =>
          worker.emit(
            "error",
            new Error("cluster.fork is not implemented in oam"),
          ),
        );
        return worker;
      }"""
assert old_cluster_fork in content, "cluster.fork pattern not found"
content = content.replace(old_cluster_fork, new_cluster_fork, 1)

with open(path, "w", encoding="utf-8") as f:
    f.write(content)

print("All 5 JS stub upgrades applied successfully")
