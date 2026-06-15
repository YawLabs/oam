#!/usr/bin/env python3
"""Batch 19: TLS client sockets + HTTPS server.

Replaces:
1. The https module stub (re-export of http) with a real HTTPS server
2. The tls module stubs with real TLSSocket + tls.connect()
"""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ======================================================================
# 1. Replace the https module
# ======================================================================

# Find the https comment + factory line block
https_start = None
https_end = None
for i in range(len(lines)):
    if '// ------' in lines[i] and 'https' in lines[i].lower() and 'registry.factories.https' not in lines[i]:
        https_start = i
    if 'registry.factories.https = () => registry.get("http")' in lines[i]:
        https_end = i + 1
        break

if https_start is None or https_end is None:
    print("ERROR: Could not find https module block")
    sys.exit(1)

print(f"Found https block at lines {https_start + 1}-{https_end}")

https_code = [
    '  // -------------------------------------------------------------- https\n',
    '  // HTTPS server (TLS-wrapped HTTP) and client. The server uses\n',
    '  // httpsServe (Rust TLS termination) but shares the same accept/respond\n',
    '  // ops as plain HTTP. Client request/get delegate to fetch (which\n',
    '  // supports HTTPS natively via reqwest+rustls).\n',
    '  registry.factories.https = (natives) => {\n',
    '    const http = registry.get("http");\n',
    '    const EventEmitter = registry.get("events");\n',
    '\n',
    '    class Server extends EventEmitter {\n',
    '      constructor(options, handler) {\n',
    '        super();\n',
    '        if (typeof options === "function") {\n',
    '          handler = options;\n',
    '          options = {};\n',
    '        }\n',
    '        this._options = options || {};\n',
    '        if (handler) this.on("request", handler);\n',
    '        this._serverId = null;\n',
    '        this._port = null;\n',
    '        this._host = null;\n',
    '        this.listening = false;\n',
    '      }\n',
    '      listen(port, host, callback) {\n',
    '        if (typeof port === "object" && port !== null) {\n',
    '          callback = host;\n',
    '          host = port.host;\n',
    '          port = port.port;\n',
    '        }\n',
    '        if (typeof host === "function") {\n',
    '          callback = host;\n',
    '          host = undefined;\n',
    '        }\n',
    '        if (typeof callback === "function") this.once("listening", callback);\n',
    '        var hostname = host || "127.0.0.1";\n',
    '        var certPem = typeof this._options.cert === "object" && this._options.cert instanceof Uint8Array\n',
    '          ? new TextDecoder().decode(this._options.cert) : String(this._options.cert || "");\n',
    '        var keyPem = typeof this._options.key === "object" && this._options.key instanceof Uint8Array\n',
    '          ? new TextDecoder().decode(this._options.key) : String(this._options.key || "");\n',
    '        natives.httpsServe(hostname, port || 0, certPem, keyPem).then(\n',
    '          (bound) => {\n',
    '            this._serverId = bound.serverId;\n',
    '            this._port = bound.port;\n',
    '            this._host = hostname;\n',
    '            this.listening = true;\n',
    '            this.emit("listening");\n',
    '            (async () => {\n',
    '              for (;;) {\n',
    '                const meta = await natives.httpAccept(bound.serverId);\n',
    '                if (meta === undefined) break;\n',
    '                const req = new http.IncomingMessage(meta);\n',
    '                req.socket = { remoteAddress: "127.0.0.1", encrypted: true };\n',
    '                const res = new http.ServerResponse(meta.requestId);\n',
    '                this.emit("request", req, res);\n',
    '              }\n',
    '              this.emit("close");\n',
    '            })();\n',
    '          },\n',
    '          (err) => this.emit("error", typeof err === "string" ? new Error(err) : err),\n',
    '        );\n',
    '        return this;\n',
    '      }\n',
    '      address() {\n',
    '        return this.listening\n',
    '          ? { port: this._port, address: this._host, family: "IPv4" }\n',
    '          : null;\n',
    '      }\n',
    '      close(callback) {\n',
    '        if (this._serverId !== null) {\n',
    '          natives.httpClose(this._serverId);\n',
    '          this.listening = false;\n',
    '        }\n',
    '        if (callback) this.once("close", callback);\n',
    '        return this;\n',
    '      }\n',
    '    }\n',
    '\n',
    '    function createServer(options, handler) {\n',
    '      return new Server(options, handler);\n',
    '    }\n',
    '\n',
    '    function request(options, callback) {\n',
    '      if (typeof options === "string") options = new URL(options);\n',
    '      if (options instanceof URL) {\n',
    '        options = {\n',
    '          hostname: options.hostname,\n',
    '          port: options.port || 443,\n',
    '          path: options.pathname + options.search,\n',
    '          protocol: "https:",\n',
    '        };\n',
    '      } else if (typeof options === "object") {\n',
    '        if (!options.protocol) options.protocol = "https:";\n',
    '        if (!options.port) options.port = 443;\n',
    '      }\n',
    '      return http.request(options, callback);\n',
    '    }\n',
    '\n',
    '    function get(options, callback) {\n',
    '      var req = request(options, callback);\n',
    '      req.end();\n',
    '      return req;\n',
    '    }\n',
    '\n',
    '    var merged = {};\n',
    '    var httpKeys = Object.keys(http);\n',
    '    for (var i = 0; i < httpKeys.length; i++) merged[httpKeys[i]] = http[httpKeys[i]];\n',
    '    merged.createServer = createServer;\n',
    '    merged.Server = Server;\n',
    '    merged.request = request;\n',
    '    merged.get = get;\n',
    '    return merged;\n',
    '  };\n',
]

lines[https_start:https_end] = https_code
offset1 = len(https_code) - (https_end - https_start)
print(f"  Replaced https module (+{offset1} lines)")

# ======================================================================
# 2. Replace the tls module
# ======================================================================

# Find the tls comment + factory block by searching for the unique
# checkServerIdentity line near the end, then finding the closing `};`
tls_start = None
tls_end = None
for i in range(len(lines)):
    if '// ---' in lines[i] and ' tls' in lines[i] and 'registry.factories.tls' not in lines[i]:
        if i + 1 < len(lines) and 'registry.factories.tls' in lines[i + 1]:
            tls_start = i
    if tls_start is not None and tls_end is None and 'checkServerIdentity' in lines[i]:
        # The factory close is 2 lines after checkServerIdentity: `};` then `};`
        for j in range(i + 1, min(i + 5, len(lines))):
            if lines[j].strip() == '};' and lines[j].startswith('  '):
                tls_end = j + 1
                break
        break

if tls_start is None or tls_end is None:
    print("ERROR: Could not find tls module block")
    sys.exit(1)

print(f"Found tls block at lines {tls_start + 1}-{tls_end}")

tls_code = [
    '  // ------------------------------------------------------------------- tls\n',
    '  registry.factories.tls = (natives) => {\n',
    '    const EventEmitter = registry.get("events");\n',
    '    const { Duplex } = registry.get("stream");\n',
    '\n',
    '    class TLSSocket extends Duplex {\n',
    '      constructor(socket, options) {\n',
    '        super();\n',
    '        this.encrypted = true;\n',
    '        this.authorized = false;\n',
    '        this.authorizationError = null;\n',
    '        this.alpnProtocol = false;\n',
    '        this._handle = null;\n',
    '        this._reading = false;\n',
    '        this._protocol = null;\n',
    '        this._cipher = null;\n',
    '      }\n',
    '      _read(size) {\n',
    '        if (this._handle === null || this._reading) return;\n',
    '        this._reading = true;\n',
    '        natives.tlsRead(this._handle, size || 65536).then(\n',
    '          (data) => {\n',
    '            this._reading = false;\n',
    '            if (data === undefined) {\n',
    '              this.push(null);\n',
    '            } else {\n',
    '              this.push(new globalThis.Buffer(data.buffer, data.byteOffset, data.length));\n',
    '            }\n',
    '          },\n',
    '          (err) => {\n',
    '            this._reading = false;\n',
    '            this.destroy(typeof err === "string" ? new Error(err) : err);\n',
    '          },\n',
    '        );\n',
    '      }\n',
    '      _write(chunk, encoding, callback) {\n',
    '        if (this._handle === null) {\n',
    '          callback(new Error("TLSSocket: not connected"));\n',
    '          return;\n',
    '        }\n',
    '        var data = typeof chunk === "string"\n',
    '          ? globalThis.Buffer.from(chunk, encoding) : chunk;\n',
    '        natives.tlsWrite(this._handle, data).then(\n',
    '          () => callback(),\n',
    '          (err) => callback(typeof err === "string" ? new Error(err) : err),\n',
    '        );\n',
    '      }\n',
    '      _destroy(err, callback) {\n',
    '        if (this._handle !== null) {\n',
    '          natives.tlsClose(this._handle);\n',
    '          this._handle = null;\n',
    '        }\n',
    '        callback(err);\n',
    '      }\n',
    '      getPeerCertificate() { return {}; }\n',
    '      getProtocol() { return this._protocol || null; }\n',
    '      getCipher() {\n',
    '        return this._cipher ? { name: this._cipher, standardName: this._cipher, version: this._protocol } : null;\n',
    '      }\n',
    '      setMaxSendFragment() { return true; }\n',
    '      enableTrace() {}\n',
    '      get remoteAddress() { return this._remoteAddress || undefined; }\n',
    '      get remotePort() { return this._remotePort || undefined; }\n',
    '    }\n',
    '\n',
    '    function connect(optionsOrPort, hostOrCb, cb) {\n',
    '      var options, callback;\n',
    '      if (typeof optionsOrPort === "number") {\n',
    '        options = { port: optionsOrPort, host: typeof hostOrCb === "string" ? hostOrCb : "localhost" };\n',
    '        callback = typeof hostOrCb === "function" ? hostOrCb : cb;\n',
    '      } else {\n',
    '        options = optionsOrPort || {};\n',
    '        callback = typeof hostOrCb === "function" ? hostOrCb : undefined;\n',
    '      }\n',
    '      var host = options.host || options.hostname || "localhost";\n',
    '      var port = options.port || 443;\n',
    '      var serverName = options.servername || host;\n',
    '      var ca = options.ca != null ? String(options.ca) : undefined;\n',
    '      var cert = options.cert != null ? String(options.cert) : undefined;\n',
    '      var key = options.key != null ? String(options.key) : undefined;\n',
    '      var rejectUnauthorized = options.rejectUnauthorized !== false;\n',
    '\n',
    '      var socket = new TLSSocket(null, options);\n',
    '      socket._remoteAddress = host;\n',
    '      socket._remotePort = port;\n',
    '      if (callback) socket.once("secureConnect", callback);\n',
    '\n',
    '      natives.tlsConnect(host, port, serverName, ca, rejectUnauthorized, cert, key).then(\n',
    '        (info) => {\n',
    '          socket._handle = info.handle;\n',
    '          socket.authorized = info.authorized;\n',
    '          socket._protocol = info.protocol;\n',
    '          socket._cipher = info.cipher;\n',
    '          socket.alpnProtocol = info.alpnProtocol || false;\n',
    '          socket.emit("secureConnect");\n',
    '        },\n',
    '        (err) => {\n',
    '          socket.destroy(typeof err === "string" ? new Error(err) : err);\n',
    '        },\n',
    '      );\n',
    '\n',
    '      return socket;\n',
    '    }\n',
    '\n',
    '    function createSecureContext(options) {\n',
    '      return Object.assign({}, options);\n',
    '    }\n',
    '\n',
    '    function createServer() {\n',
    '      throw new Error(\n',
    '        "tls.createServer is not yet implemented in oam -- use https.createServer for HTTPS servers",\n',
    '      );\n',
    '    }\n',
    '\n',
    '    return {\n',
    '      connect,\n',
    '      createServer,\n',
    '      createSecureContext,\n',
    '      TLSSocket,\n',
    '      DEFAULT_ECDH_CURVE: "auto",\n',
    '      DEFAULT_MAX_VERSION: "TLSv1.3",\n',
    '      DEFAULT_MIN_VERSION: "TLSv1.2",\n',
    '      rootCertificates: [],\n',
    '      getCiphers: () => ["TLS_AES_128_GCM_SHA256", "TLS_AES_256_GCM_SHA384", "TLS_CHACHA20_POLY1305_SHA256"],\n',
    '      checkServerIdentity: () => undefined,\n',
    '    };\n',
    '  };\n',
]

lines[tls_start:tls_end] = tls_code
offset2 = len(tls_code) - (tls_end - tls_start)
print(f"  Replaced tls module (+{offset2} lines)")

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
