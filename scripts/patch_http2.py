#!/usr/bin/env python3
"""Replace the http2 stub in node_compat.js with a working implementation.

Uses exact byte-level string replacement to avoid smart-quote corruption
on Windows ARM64.
"""
import sys

TARGET = "js/node_compat.js"

# The OLD stub text to find and replace (exact match)
OLD = '''  // ------------------------------------------------------------------ http2
  registry.factories.http2 = () => {
    const EventEmitter = registry.get("events");
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
      connect,
      constants: {
        NGHTTP2_SESSION_SERVER: 0,
        NGHTTP2_SESSION_CLIENT: 1,
        NGHTTP2_STREAM_STATE_IDLE: 1,
        NGHTTP2_STREAM_STATE_OPEN: 2,
        NGHTTP2_STREAM_STATE_RESERVED_LOCAL: 3,
        NGHTTP2_STREAM_STATE_RESERVED_REMOTE: 4,
        NGHTTP2_STREAM_STATE_HALF_CLOSED_LOCAL: 5,
        NGHTTP2_STREAM_STATE_HALF_CLOSED_REMOTE: 6,
        NGHTTP2_STREAM_STATE_CLOSED: 7,
        NGHTTP2_NO_ERROR: 0,
        NGHTTP2_PROTOCOL_ERROR: 1,
        NGHTTP2_INTERNAL_ERROR: 2,
        NGHTTP2_FLOW_CONTROL_ERROR: 3,
        NGHTTP2_SETTINGS_TIMEOUT: 4,
        NGHTTP2_STREAM_CLOSED: 5,
        NGHTTP2_FRAME_SIZE_ERROR: 6,
        NGHTTP2_REFUSED_STREAM: 7,
        NGHTTP2_CANCEL: 8,
        NGHTTP2_COMPRESSION_ERROR: 9,
        NGHTTP2_CONNECT_ERROR: 10,
        NGHTTP2_ENHANCE_YOUR_CALM: 11,
        NGHTTP2_INADEQUATE_SECURITY: 12,
        NGHTTP2_HTTP_1_1_REQUIRED: 13,
        NGHTTP2_DEFAULT_WEIGHT: 16,
        HTTP2_HEADER_STATUS: ":status",
        HTTP2_HEADER_METHOD: ":method",
        HTTP2_HEADER_AUTHORITY: ":authority",
        HTTP2_HEADER_SCHEME: ":scheme",
        HTTP2_HEADER_PATH: ":path",
        HTTP2_HEADER_CONTENT_TYPE: "content-type",
        HTTP2_HEADER_CONTENT_LENGTH: "content-length",
        HTTP2_HEADER_ACCEPT_ENCODING: "accept-encoding",
        HTTP2_METHOD_GET: "GET",
        HTTP2_METHOD_POST: "POST",
        HTTP_STATUS_OK: 200,
        HTTP_STATUS_NOT_FOUND: 404,
        HTTP_STATUS_INTERNAL_SERVER_ERROR: 500,
      },
      sensitiveHeaders: Symbol.for("nodejs.http2.sensitiveHeaders"),
    };
  };'''

# The NEW working implementation
NEW = r'''  // ------------------------------------------------------------------ http2
  registry.factories.http2 = (natives) => {
    const EventEmitter = registry.get("events");
    const { Duplex } = registry.get("stream");

    // ------ ServerHttp2Stream: wraps a single HTTP/2 request on the server ------
    class ServerHttp2Stream extends Duplex {
      constructor(requestId, inHeaders) {
        super({ allowHalfOpen: true });
        this._requestId = requestId;
        this._streamId = null;
        this._ended = false;
        this._responded = false;
        this._chain = Promise.resolve();
        this.sentHeaders = null;
        // Pseudo-headers from the client request
        this._inHeaders = inHeaders;
        this.id = requestId; // stream identifier
      }
      // Server sends response headers
      respond(headers, options) {
        if (this._responded) return;
        this._responded = true;
        var status = 200;
        var outPairs = [];
        if (headers) {
          var keys = Object.keys(headers);
          for (var i = 0; i < keys.length; i++) {
            var k = keys[i];
            if (k === ":status") {
              status = Number(headers[k]);
            } else if (k.charAt(0) !== ":") {
              outPairs.push([k.toLowerCase(), String(headers[k])]);
            }
          }
        }
        this.sentHeaders = headers || {};
        var endStream = options && options.endStream;
        if (endStream) {
          this._ended = true;
          natives.httpRespond(
            this._requestId,
            status,
            JSON.stringify(outPairs),
            new Uint8Array(0),
          );
          var self = this;
          queueMicrotask(function() { self.emit("finish"); self.push(null); });
        } else {
          this._streamId = natives.httpRespondStream(
            this._requestId,
            status,
            JSON.stringify(outPairs),
          );
        }
      }
      // Additional response headers (trailing): not fully implemented
      additionalHeaders() {}
      // Duplex _write: push data through the streaming response
      _write(chunk, encoding, callback) {
        if (this._ended) { callback(); return; }
        if (!this._responded) {
          this.respond({ ":status": 200 });
        }
        var bytes;
        if (typeof chunk === "string") {
          bytes = globalThis.Buffer.from(chunk, encoding || "utf8");
        } else {
          bytes = chunk;
        }
        if (this._streamId === null) { callback(); return; }
        var streamId = this._streamId;
        this._chain = this._chain
          .then(function() { return natives.httpBodyPush(streamId, bytes); })
          .then(function() { callback(); }, function(err) { callback(err); });
      }
      _final(callback) {
        if (this._ended) { callback(); return; }
        this._ended = true;
        if (!this._responded) {
          this.respond({ ":status": 200 });
        }
        if (this._streamId !== null) {
          var streamId = this._streamId;
          var self = this;
          this._chain = this._chain.then(function() {
            natives.httpBodyEnd(streamId);
            self.emit("finish");
            callback();
          });
        } else {
          callback();
        }
      }
      _read() {
        // Request body: deliver once then EOF
        if (!this._bodyPushed) {
          this._bodyPushed = true;
          var body = natives.httpRequestBody(this._requestId);
          if (body && body.length > 0) {
            this.push(globalThis.Buffer.from(body.buffer, body.byteOffset, body.length));
          }
          this.push(null);
        }
      }
      // Node compat: stream.close(code) sends RST_STREAM
      close(code, callback) {
        if (typeof code === "function") { callback = code; code = 0; }
        this.end();
        if (callback) this.once("close", callback);
      }
    }

    // ------ Http2Server ------
    class Http2Server extends EventEmitter {
      constructor(options, handler) {
        super();
        if (typeof options === "function") {
          handler = options;
          options = {};
        }
        this._options = options || {};
        if (handler) this.on("stream", handler);
        this._serverId = null;
        this._port = null;
        this._host = null;
        this.listening = false;
      }
      listen(port, host, callback) {
        if (typeof port === "object" && port !== null) {
          callback = host;
          host = port.host;
          port = port.port;
        }
        if (typeof host === "function") {
          callback = host;
          host = undefined;
        }
        if (typeof callback === "function") this.once("listening", callback);
        var hostname = host || "127.0.0.1";
        var self = this;
        natives.http2Serve(hostname, port || 0).then(
          function(bound) {
            self._serverId = bound.serverId;
            self._port = bound.port;
            self._host = hostname;
            self.listening = true;
            self.emit("listening");
            (async function() {
              for (;;) {
                var meta = await natives.httpAccept(bound.serverId);
                if (meta === undefined) break;
                var hdrs = {};
                for (var i = 0; i < meta.headers.length; i++) {
                  var key = meta.headers[i][0].toLowerCase();
                  hdrs[key] = meta.headers[i][1];
                }
                // Inject pseudo-headers from the request line
                hdrs[":method"] = meta.method;
                hdrs[":path"] = meta.uri;
                hdrs[":scheme"] = "http";
                var stream = new ServerHttp2Stream(meta.requestId, hdrs);
                self.emit("stream", stream, hdrs);
              }
              self.emit("close");
            })();
          },
          function(err) { self.emit("error", err); },
        );
        return this;
      }
      address() {
        return this.listening
          ? { port: this._port, address: this._host, family: "IPv4" }
          : null;
      }
      close(callback) {
        if (this._serverId !== null) {
          natives.httpClose(this._serverId);
          this.listening = false;
        }
        if (callback) this.once("close", callback);
        return this;
      }
      setTimeout() { return this; }
    }

    function createServer(options, handler) {
      return new Http2Server(options, handler);
    }

    function createSecureServer(options, handler) {
      // TLS-wrapped HTTP/2 (h2 over TLS) -- stub for now, falls back to h2c
      return createServer(options, handler);
    }

    // ------ Client HTTP/2 session (backed by fetch/reqwest h2 support) ------
    class ClientHttp2Stream extends Duplex {
      constructor(session, headers) {
        super({ allowHalfOpen: true });
        this._session = session;
        this._reqHeaders = headers;
        this._bodyChunks = [];
        this._ended = false;
        this.sentHeaders = headers;
        this.id = 1;
        this._responseEmitted = false;
      }
      _write(chunk, encoding, callback) {
        if (typeof chunk === "string") {
          this._bodyChunks.push(globalThis.Buffer.from(chunk, encoding || "utf8"));
        } else {
          this._bodyChunks.push(chunk);
        }
        callback();
      }
      _final(callback) {
        this._ended = true;
        this._doFetch(callback);
      }
      _read() {}
      _doFetch(callback) {
        var self = this;
        var method = this._reqHeaders[":method"] || "GET";
        var path = this._reqHeaders[":path"] || "/";
        var scheme = this._reqHeaders[":scheme"] || "http";
        var authority = this._reqHeaders[":authority"] || this._session._authority;
        var url = scheme + "://" + authority + path;
        var fetchHeaders = {};
        var keys = Object.keys(this._reqHeaders);
        for (var i = 0; i < keys.length; i++) {
          if (keys[i].charAt(0) !== ":") {
            fetchHeaders[keys[i]] = this._reqHeaders[keys[i]];
          }
        }
        var bodyData = null;
        if (this._bodyChunks.length > 0) {
          var totalLen = 0;
          for (var bi = 0; bi < this._bodyChunks.length; bi++) totalLen += this._bodyChunks[bi].length;
          var merged = new Uint8Array(totalLen);
          var boff = 0;
          for (var bi = 0; bi < this._bodyChunks.length; bi++) {
            merged.set(this._bodyChunks[bi], boff);
            boff += this._bodyChunks[bi].length;
          }
          bodyData = merged;
        }
        var fetchOpts = { method: method, headers: fetchHeaders };
        if (bodyData && method !== "GET" && method !== "HEAD") {
          fetchOpts.body = bodyData;
        }
        globalThis.fetch(url, fetchOpts).then(
          function(resp) {
            var respHeaders = { ":status": resp.status };
            resp.headers.forEach(function(value, name) {
              respHeaders[name.toLowerCase()] = value;
            });
            self.emit("response", respHeaders, 0);
            resp.arrayBuffer().then(function(ab) {
              if (ab.byteLength > 0) {
                self.push(globalThis.Buffer.from(ab));
              }
              self.push(null);
              callback();
            }, function(err) {
              self.destroy(err);
              callback(err);
            });
          },
          function(err) {
            self.emit("error", typeof err === "string" ? new Error(err) : err);
            callback(err);
          },
        );
      }
      close(code, callback) {
        if (typeof code === "function") { callback = code; code = 0; }
        this.end();
        if (callback) this.once("close", callback);
      }
    }

    class ClientHttp2Session extends EventEmitter {
      constructor(authority) {
        super();
        this._authority = authority.replace(/^https?:\/\//, "");
        this._scheme = authority.startsWith("https") ? "https" : "http";
        this._closed = false;
        this._destroyed = false;
        this.socket = {};
        this.alpnProtocol = "h2c";
        // Emit 'connect' on next tick to match Node behavior
        var self = this;
        process.nextTick(function() { self.emit("connect", self); });
      }
      request(headers) {
        if (this._closed || this._destroyed) {
          throw new Error("Session is closed");
        }
        var merged = {};
        merged[":method"] = "GET";
        merged[":path"] = "/";
        merged[":scheme"] = this._scheme;
        merged[":authority"] = this._authority;
        if (headers) {
          var keys = Object.keys(headers);
          for (var i = 0; i < keys.length; i++) {
            merged[keys[i]] = headers[keys[i]];
          }
        }
        var stream = new ClientHttp2Stream(this, merged);
        return stream;
      }
      close(callback) {
        this._closed = true;
        if (callback) this.once("close", callback);
        var self = this;
        process.nextTick(function() { self.emit("close"); });
      }
      destroy(err) {
        this._destroyed = true;
        this._closed = true;
        if (err) this.emit("error", err);
        var self = this;
        process.nextTick(function() { self.emit("close"); });
      }
      ref() { return this; }
      unref() { return this; }
      ping(payload, callback) {
        if (typeof payload === "function") { callback = payload; payload = undefined; }
        if (callback) process.nextTick(function() { callback(null, 0, globalThis.Buffer.alloc(8)); });
      }
      get closed() { return this._closed; }
      get destroyed() { return this._destroyed; }
    }

    function connect(authority, options) {
      if (typeof options === "function") options = {};
      return new ClientHttp2Session(authority);
    }

    return {
      createServer,
      createSecureServer,
      connect,
      constants: {
        NGHTTP2_SESSION_SERVER: 0,
        NGHTTP2_SESSION_CLIENT: 1,
        NGHTTP2_STREAM_STATE_IDLE: 1,
        NGHTTP2_STREAM_STATE_OPEN: 2,
        NGHTTP2_STREAM_STATE_RESERVED_LOCAL: 3,
        NGHTTP2_STREAM_STATE_RESERVED_REMOTE: 4,
        NGHTTP2_STREAM_STATE_HALF_CLOSED_LOCAL: 5,
        NGHTTP2_STREAM_STATE_HALF_CLOSED_REMOTE: 6,
        NGHTTP2_STREAM_STATE_CLOSED: 7,
        NGHTTP2_NO_ERROR: 0,
        NGHTTP2_PROTOCOL_ERROR: 1,
        NGHTTP2_INTERNAL_ERROR: 2,
        NGHTTP2_FLOW_CONTROL_ERROR: 3,
        NGHTTP2_SETTINGS_TIMEOUT: 4,
        NGHTTP2_STREAM_CLOSED: 5,
        NGHTTP2_FRAME_SIZE_ERROR: 6,
        NGHTTP2_REFUSED_STREAM: 7,
        NGHTTP2_CANCEL: 8,
        NGHTTP2_COMPRESSION_ERROR: 9,
        NGHTTP2_CONNECT_ERROR: 10,
        NGHTTP2_ENHANCE_YOUR_CALM: 11,
        NGHTTP2_INADEQUATE_SECURITY: 12,
        NGHTTP2_HTTP_1_1_REQUIRED: 13,
        NGHTTP2_DEFAULT_WEIGHT: 16,
        HTTP2_HEADER_STATUS: ":status",
        HTTP2_HEADER_METHOD: ":method",
        HTTP2_HEADER_AUTHORITY: ":authority",
        HTTP2_HEADER_SCHEME: ":scheme",
        HTTP2_HEADER_PATH: ":path",
        HTTP2_HEADER_CONTENT_TYPE: "content-type",
        HTTP2_HEADER_CONTENT_LENGTH: "content-length",
        HTTP2_HEADER_ACCEPT_ENCODING: "accept-encoding",
        HTTP2_METHOD_GET: "GET",
        HTTP2_METHOD_POST: "POST",
        HTTP_STATUS_OK: 200,
        HTTP_STATUS_NOT_FOUND: 404,
        HTTP_STATUS_INTERNAL_SERVER_ERROR: 500,
      },
      sensitiveHeaders: Symbol.for("nodejs.http2.sensitiveHeaders"),
    };
  };'''

def main():
    with open(TARGET, "r", encoding="utf-8") as f:
        content = f.read()

    if OLD not in content:
        print("ERROR: old text not found in", TARGET, file=sys.stderr)
        # Try to find partial match for debugging
        lines = OLD.split("\n")
        for i, line in enumerate(lines):
            if line.strip() and line.strip() not in content:
                print(f"  First mismatch at line {i}: {line!r}", file=sys.stderr)
                break
        sys.exit(1)

    count = content.count(OLD)
    if count != 1:
        print(f"ERROR: found {count} occurrences, expected 1", file=sys.stderr)
        sys.exit(1)

    content = content.replace(OLD, NEW, 1)

    with open(TARGET, "w", encoding="utf-8", newline="\n") as f:
        f.write(content)

    print(f"OK: replaced http2 stub in {TARGET}")

if __name__ == "__main__":
    main()
