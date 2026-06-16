#!/usr/bin/env python3
"""Patch ClientRequest in node_compat.js to support HTTP upgrade on the client side.

When ClientRequest has Connection: Upgrade headers, uses raw TCP instead of fetch
so the 'upgrade' event can fire with a real net.Socket.
"""

import sys, os

path = os.path.join(os.path.dirname(__file__), '..', 'js', 'node_compat.js')
with open(path, 'r', encoding='utf-8') as f:
    src = f.read()

# The old end() method that uses fetch for everything.
# We replace the fetch call with a check: if Connection: Upgrade, use raw TCP.
old_end = '''      end(data, encoding, callback) {
        if (typeof data === "function") { callback = data; data = undefined; }
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        if (data != null) this.write(data, encoding);
        this._ended = true;
        this.headersSent = true;
        var self = this;
        var bodyData = null;
        if (self._body.length > 0) {
          var totalLen = 0;
          for (var bi = 0; bi < self._body.length; bi++) totalLen += self._body[bi].length;
          var merged = new Uint8Array(totalLen);
          var boff = 0;
          for (var bi = 0; bi < self._body.length; bi++) {
            merged.set(self._body[bi], boff);
            boff += self._body[bi].length;
          }
          bodyData = merged;
        }
        var fetchOpts = {
          method: self.method,
          headers: self._headers,
        };
        if (bodyData && self.method !== "GET" && self.method !== "HEAD") {
          fetchOpts.body = bodyData;
        }
        globalThis.fetch(self._url, fetchOpts).then(function (resp) {
          if (self._aborted) return;
          var res = new Readable({ read: function () {} });
          res.statusCode = resp.status;
          res.statusMessage = resp.statusText || "";
          res.httpVersion = "1.1";
          res.headers = {};
          res.rawHeaders = [];
          resp.headers.forEach(function (value, name) {
            var key = name.toLowerCase();
            res.headers[key] = key in res.headers ? res.headers[key] + ", " + value : value;
            res.rawHeaders.push(name, value);
          });
          self.emit("response", res);
          resp.arrayBuffer().then(function (ab) {
            if (ab.byteLength > 0) res.push(globalThis.Buffer.from(ab));
            res.push(null);
            process.nextTick(function() { self.emit("close"); });
          }, function (err) { res.destroy(err); });
        }, function (err) {
          self.emit("error", typeof err === "string" ? new Error(err) : err);
        });
        if (callback) self.once("response", callback);
        return this;
      }'''

new_end = '''      end(data, encoding, callback) {
        if (typeof data === "function") { callback = data; data = undefined; }
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        if (data != null) this.write(data, encoding);
        this._ended = true;
        this.headersSent = true;
        var self = this;
        var bodyData = null;
        if (self._body.length > 0) {
          var totalLen = 0;
          for (var bi = 0; bi < self._body.length; bi++) totalLen += self._body[bi].length;
          var merged = new Uint8Array(totalLen);
          var boff = 0;
          for (var bi = 0; bi < self._body.length; bi++) {
            merged.set(self._body[bi], boff);
            boff += self._body[bi].length;
          }
          bodyData = merged;
        }
        var connHdr = (self._headers["connection"] || "").toLowerCase();
        if (connHdr.indexOf("upgrade") !== -1) {
          self._doUpgradeRequest(bodyData);
        } else {
          self._doFetchRequest(bodyData);
        }
        if (callback) self.once("response", callback);
        return this;
      }
      _doFetchRequest(bodyData) {
        var self = this;
        var fetchOpts = {
          method: self.method,
          headers: self._headers,
        };
        if (bodyData && self.method !== "GET" && self.method !== "HEAD") {
          fetchOpts.body = bodyData;
        }
        globalThis.fetch(self._url, fetchOpts).then(function (resp) {
          if (self._aborted) return;
          var res = new Readable({ read: function () {} });
          res.statusCode = resp.status;
          res.statusMessage = resp.statusText || "";
          res.httpVersion = "1.1";
          res.headers = {};
          res.rawHeaders = [];
          resp.headers.forEach(function (value, name) {
            var key = name.toLowerCase();
            res.headers[key] = key in res.headers ? res.headers[key] + ", " + value : value;
            res.rawHeaders.push(name, value);
          });
          self.emit("response", res);
          resp.arrayBuffer().then(function (ab) {
            if (ab.byteLength > 0) res.push(globalThis.Buffer.from(ab));
            res.push(null);
            process.nextTick(function() { self.emit("close"); });
          }, function (err) { res.destroy(err); });
        }, function (err) {
          self.emit("error", typeof err === "string" ? new Error(err) : err);
        });
      }
      _doUpgradeRequest(bodyData) {
        var self = this;
        var parsed = new URL(self._url);
        var host = parsed.hostname;
        var port = Number(parsed.port) || (parsed.protocol === "https:" ? 443 : 80);
        var reqPath = parsed.pathname + parsed.search;
        natives.tcpConnect(host, port).then(function (result) {
          var handle = result.handle;
          if (!self._headers["host"]) {
            self._headers["host"] = port === 80 ? host : host + ":" + port;
          }
          var reqLine = self.method + " " + reqPath + " HTTP/1.1\\r\\n";
          var headerStr = "";
          var hkeys = Object.keys(self._headers);
          for (var hi = 0; hi < hkeys.length; hi++) {
            headerStr += hkeys[hi] + ": " + self._headers[hkeys[hi]] + "\\r\\n";
          }
          var reqBytes = globalThis.Buffer.from(reqLine + headerStr + "\\r\\n");
          natives.tcpWrite(handle, reqBytes).then(function () {
            var responseBuf = globalThis.Buffer.alloc(0);
            function readMore() {
              natives.tcpRead(handle, 4096).then(function (chunk) {
                if (chunk === undefined) {
                  self.emit("error", new Error("connection closed before upgrade response"));
                  return;
                }
                responseBuf = globalThis.Buffer.concat([responseBuf, globalThis.Buffer.from(chunk)]);
                var headerEnd = -1;
                for (var si = 0; si < responseBuf.length - 3; si++) {
                  if (responseBuf[si] === 13 && responseBuf[si+1] === 10 && responseBuf[si+2] === 13 && responseBuf[si+3] === 10) {
                    headerEnd = si;
                    break;
                  }
                }
                if (headerEnd === -1) { readMore(); return; }
                var headStr = responseBuf.slice(0, headerEnd).toString();
                var headBytes = headerEnd + 4;
                var remaining = responseBuf.slice(headBytes);
                var lines = headStr.split("\\r\\n");
                var statusLine = lines[0] || "";
                var statusMatch = statusLine.match(/HTTP\\/\\d\\.\\d (\\d+)/);
                var statusCode = statusMatch ? Number(statusMatch[1]) : 0;
                var resHeaders = {};
                var rawHeaders = [];
                for (var li = 1; li < lines.length; li++) {
                  var colonIdx = lines[li].indexOf(":");
                  if (colonIdx !== -1) {
                    var hname = lines[li].slice(0, colonIdx);
                    var hval = lines[li].slice(colonIdx + 1).trim();
                    var lname = hname.toLowerCase();
                    resHeaders[lname] = lname in resHeaders ? resHeaders[lname] + ", " + hval : hval;
                    rawHeaders.push(hname, hval);
                  }
                }
                if (statusCode === 101) {
                  var NetSocket = registry.get("net").Socket;
                  var socket = new NetSocket({
                    _handle: handle,
                    _remoteAddr: result.remoteAddr,
                  });
                  socket._readLoop();
                  var res = new Readable({ read: function () {} });
                  res.statusCode = statusCode;
                  res.statusMessage = statusLine.slice(statusLine.indexOf(" " + statusCode) + String(statusCode).length + 2) || "";
                  res.httpVersion = "1.1";
                  res.headers = resHeaders;
                  res.rawHeaders = rawHeaders;
                  self.emit("upgrade", res, socket, remaining);
                } else {
                  var res = new Readable({ read: function () {} });
                  res.statusCode = statusCode;
                  res.statusMessage = "";
                  res.httpVersion = "1.1";
                  res.headers = resHeaders;
                  res.rawHeaders = rawHeaders;
                  self.emit("response", res);
                  if (remaining.length > 0) res.push(remaining);
                  natives.tcpRead(handle, 65536).then(function readRest(chunk) {
                    if (chunk === undefined) { res.push(null); return; }
                    res.push(globalThis.Buffer.from(chunk));
                    natives.tcpRead(handle, 65536).then(readRest);
                  });
                }
              }, function (err) { self.emit("error", err); });
            }
            readMore();
          }, function (err) { self.emit("error", err); });
        }, function (err) { self.emit("error", err); });
      }'''

if old_end not in src:
    print("ERROR: could not find ClientRequest.end() to patch", file=sys.stderr)
    sys.exit(1)

src = src.replace(old_end, new_end, 1)

with open(path, 'w', encoding='utf-8', newline='\n') as f:
    f.write(src)

print("Patched ClientRequest for client-side upgrade support")
