#!/usr/bin/env python3
"""Batch 11: Duplex.from/fromWeb/toWeb, http2.constants, dns error codes,
perf_hooks.PerformanceEntry, process.resourceUsage."""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ──────────────────────────────────────────────────────────────────────
# 1. Duplex.from / Duplex.fromWeb / Duplex.toWeb
# ──────────────────────────────────────────────────────────────────────

# Find the line after Duplex.prototype.destroy is defined
# Look for the end of the Duplex destroy defineProperty block
duplex_after = None
for i in range(len(lines)):
    if "class Transform extends Duplex" in lines[i]:
        duplex_after = i
        break

if duplex_after is None:
    print("ERROR: Could not find Transform class after Duplex")
    sys.exit(1)

print(f"Found Transform class at line {duplex_after + 1}")

duplex_statics = [
    "    Duplex.from = function duplexFrom(source) {\n",
    "      if (source && typeof source.pipe === \"function\" && typeof source.write === \"function\") return source;\n",
    "      if (source && typeof source[Symbol.asyncIterator] === \"function\") {\n",
    "        return new Duplex({\n",
    "          objectMode: true,\n",
    "          write(chunk, enc, cb) { cb(); },\n",
    "          async read() {\n",
    "            for await (var v of source) { if (!this.push(v)) break; }\n",
    "            this.push(null);\n",
    "          },\n",
    "        });\n",
    "      }\n",
    "      if (source && typeof source.readable === \"object\" && typeof source.writable === \"object\") {\n",
    "        var d = new Duplex({\n",
    "          write(chunk, enc, cb) { source.writable.write(chunk); cb(); },\n",
    "          read() {},\n",
    "        });\n",
    "        if (source.readable && typeof source.readable.on === \"function\") {\n",
    "          source.readable.on(\"data\", function(ch) { d.push(ch); });\n",
    "          source.readable.on(\"end\", function() { d.push(null); });\n",
    "        }\n",
    "        return d;\n",
    "      }\n",
    "      throw new TypeError(\"Duplex.from: unsupported source\");\n",
    "    };\n",
    "    Duplex.fromWeb = function duplexFromWeb(pair) {\n",
    "      var readable = pair.readable;\n",
    "      var writable = pair.writable;\n",
    "      var reader = readable.getReader();\n",
    "      var writer = writable.getWriter();\n",
    "      return new Duplex({\n",
    "        async read() {\n",
    "          try {\n",
    "            var r = await reader.read();\n",
    "            if (r.done) this.push(null); else this.push(r.value);\n",
    "          } catch(e) { this.destroy(e); }\n",
    "        },\n",
    "        write(chunk, enc, cb) { writer.write(chunk).then(function() { cb(); }, cb); },\n",
    "        final(cb) { writer.close().then(function() { cb(); }, cb); },\n",
    "      });\n",
    "    };\n",
    "    Duplex.toWeb = function duplexToWeb(duplex) {\n",
    "      return {\n",
    "        readable: Readable.toWeb(duplex),\n",
    "        writable: Writable.toWeb(duplex),\n",
    "      };\n",
    "    };\n",
    "\n",
]

lines[duplex_after:duplex_after] = duplex_statics
offset1 = len(duplex_statics)
print(f"  Inserted Duplex statics ({offset1} lines)")

# ──────────────────────────────────────────────────────────────────────
# 2. http2.constants -- full NGHTTP2 constant set
# ──────────────────────────────────────────────────────────────────────

# Find "      constants: {}," in http2 factory
for i in range(len(lines)):
    if "constants: {}," in lines[i] and i > 8100:
        # Check context: should be near http2
        if any("http2" in lines[j] for j in range(max(0, i-20), i)):
            http2_const_idx = i
            break

print(f"Found http2 constants at line {http2_const_idx + 1}")

http2_constants = [
    "      constants: {\n",
    "        NGHTTP2_SESSION_SERVER: 0,\n",
    "        NGHTTP2_SESSION_CLIENT: 1,\n",
    "        NGHTTP2_STREAM_STATE_IDLE: 1,\n",
    "        NGHTTP2_STREAM_STATE_OPEN: 2,\n",
    "        NGHTTP2_STREAM_STATE_RESERVED_LOCAL: 3,\n",
    "        NGHTTP2_STREAM_STATE_RESERVED_REMOTE: 4,\n",
    "        NGHTTP2_STREAM_STATE_HALF_CLOSED_LOCAL: 5,\n",
    "        NGHTTP2_STREAM_STATE_HALF_CLOSED_REMOTE: 6,\n",
    "        NGHTTP2_STREAM_STATE_CLOSED: 7,\n",
    "        NGHTTP2_NO_ERROR: 0,\n",
    "        NGHTTP2_PROTOCOL_ERROR: 1,\n",
    "        NGHTTP2_INTERNAL_ERROR: 2,\n",
    "        NGHTTP2_FLOW_CONTROL_ERROR: 3,\n",
    "        NGHTTP2_SETTINGS_TIMEOUT: 4,\n",
    "        NGHTTP2_STREAM_CLOSED: 5,\n",
    "        NGHTTP2_FRAME_SIZE_ERROR: 6,\n",
    "        NGHTTP2_REFUSED_STREAM: 7,\n",
    "        NGHTTP2_CANCEL: 8,\n",
    "        NGHTTP2_COMPRESSION_ERROR: 9,\n",
    "        NGHTTP2_CONNECT_ERROR: 10,\n",
    "        NGHTTP2_ENHANCE_YOUR_CALM: 11,\n",
    "        NGHTTP2_INADEQUATE_SECURITY: 12,\n",
    "        NGHTTP2_HTTP_1_1_REQUIRED: 13,\n",
    "        NGHTTP2_DEFAULT_WEIGHT: 16,\n",
    "        HTTP2_HEADER_STATUS: \":status\",\n",
    "        HTTP2_HEADER_METHOD: \":method\",\n",
    "        HTTP2_HEADER_AUTHORITY: \":authority\",\n",
    "        HTTP2_HEADER_SCHEME: \":scheme\",\n",
    "        HTTP2_HEADER_PATH: \":path\",\n",
    "        HTTP2_HEADER_CONTENT_TYPE: \"content-type\",\n",
    "        HTTP2_HEADER_CONTENT_LENGTH: \"content-length\",\n",
    "        HTTP2_HEADER_ACCEPT_ENCODING: \"accept-encoding\",\n",
    "        HTTP2_METHOD_GET: \"GET\",\n",
    "        HTTP2_METHOD_POST: \"POST\",\n",
    "        HTTP_STATUS_OK: 200,\n",
    "        HTTP_STATUS_NOT_FOUND: 404,\n",
    "        HTTP_STATUS_INTERNAL_SERVER_ERROR: 500,\n",
    "      },\n",
]

lines[http2_const_idx:http2_const_idx + 1] = http2_constants
offset2 = len(http2_constants) - 1
print(f"  Replaced http2 constants (1 -> {len(http2_constants)} lines)")

# ──────────────────────────────────────────────────────────────────────
# 3. dns error code constants
# ──────────────────────────────────────────────────────────────────────

# Find dns return block: "    return {" with "lookup," nearby
dns_return_idx = None
for i in range(len(lines)):
    if "return {" in lines[i] and i > 8100:
        if any("lookup," in lines[j] for j in range(i, min(len(lines), i + 5))):
            dns_return_idx = i
            break

if dns_return_idx is None:
    print("ERROR: Could not find dns return block")
    sys.exit(1)

print(f"Found dns return block at line {dns_return_idx + 1}")

# Find the closing of the dns return block: line with "    };"
dns_return_end = None
brace_depth = 0
for i in range(dns_return_idx, len(lines)):
    for ch in lines[i]:
        if ch == '{': brace_depth += 1
        elif ch == '}': brace_depth -= 1
    if brace_depth == 0:
        dns_return_end = i
        break

if dns_return_end is None:
    print("ERROR: Could not find dns return end")
    sys.exit(1)

print(f"Found dns return end at line {dns_return_end + 1}")

# Insert DNS error codes before the closing "    };"
dns_error_codes = [
    "      NODATA: \"NODATA\",\n",
    "      FORMERR: \"FORMERR\",\n",
    "      SERVFAIL: \"SERVFAIL\",\n",
    "      NOTFOUND: \"NOTFOUND\",\n",
    "      NOTIMP: \"NOTIMP\",\n",
    "      REFUSED: \"REFUSED\",\n",
    "      BADQUERY: \"BADQUERY\",\n",
    "      BADNAME: \"BADNAME\",\n",
    "      BADFAMILY: \"BADFAMILY\",\n",
    "      BADRESP: \"BADRESP\",\n",
    "      CONNREFUSED: \"CONNREFUSED\",\n",
    "      TIMEOUT: \"TIMEOUT\",\n",
    "      EOF: \"EOF\",\n",
    "      FILE: \"FILE\",\n",
    "      NOMEM: \"NOMEM\",\n",
    "      DESTRUCTION: \"DESTRUCTION\",\n",
    "      BADSTR: \"BADSTR\",\n",
    "      BADFLAGS: \"BADFLAGS\",\n",
    "      NONAME: \"NONAME\",\n",
    "      BADHINTS: \"BADHINTS\",\n",
    "      NOTINITIALIZED: \"NOTINITIALIZED\",\n",
    "      LOADIPHLPAPI: \"LOADIPHLPAPI\",\n",
    "      ADDRGETNETWORKPARAMS: \"ADDRGETNETWORKPARAMS\",\n",
    "      CANCELLED: \"CANCELLED\",\n",
]

lines[dns_return_end:dns_return_end] = dns_error_codes
offset3 = len(dns_error_codes)
print(f"  Inserted dns error codes ({offset3} lines)")

# ──────────────────────────────────────────────────────────────────────
# 4. perf_hooks.PerformanceEntry + PerformanceObserverEntryList
# ──────────────────────────────────────────────────────────────────────

# Find perf_hooks return block
perf_return_idx = None
for i in range(len(lines)):
    if "registry.factories.perf_hooks" in lines[i]:
        # Find the return block inside
        for j in range(i, min(len(lines), i + 20)):
            if "return {" in lines[j]:
                perf_return_idx = j
                break
        break

if perf_return_idx is None:
    print("ERROR: Could not find perf_hooks return block")
    sys.exit(1)

print(f"Found perf_hooks return block at line {perf_return_idx + 1}")

perf_classes = [
    "    class PerformanceEntry {\n",
    "      constructor(name, entryType, startTime, duration) {\n",
    "        this.name = name || \"\";\n",
    "        this.entryType = entryType || \"\";\n",
    "        this.startTime = startTime || 0;\n",
    "        this.duration = duration || 0;\n",
    "      }\n",
    "      toJSON() {\n",
    "        return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration };\n",
    "      }\n",
    "    }\n",
    "    class PerformanceObserverEntryList {\n",
    "      constructor() { this._entries = []; }\n",
    "      getEntries() { return this._entries.slice(); }\n",
    "      getEntriesByName(name) { return this._entries.filter(function(e) { return e.name === name; }); }\n",
    "      getEntriesByType(type) { return this._entries.filter(function(e) { return e.entryType === type; }); }\n",
    "    }\n",
    "    class PerformanceNodeTiming extends PerformanceEntry {\n",
    "      constructor() {\n",
    "        super(\"node\", \"node\", 0, 0);\n",
    "        this.nodeStart = 0;\n",
    "        this.v8Start = 0;\n",
    "        this.bootstrapComplete = 0;\n",
    "        this.environment = 0;\n",
    "        this.loopStart = 0;\n",
    "        this.loopExit = 0;\n",
    "        this.idleTime = 0;\n",
    "      }\n",
    "    }\n",
    "\n",
]

lines[perf_return_idx:perf_return_idx] = perf_classes
offset4 = len(perf_classes)
print(f"  Inserted PerformanceEntry classes ({offset4} lines)")

# Add to perf_hooks return: find "      PerformanceObserver," after insertion
for i in range(perf_return_idx + offset4, min(len(lines), perf_return_idx + offset4 + 10)):
    if "PerformanceObserver," in lines[i]:
        insert_lines = [
            "      PerformanceEntry,\n",
            "      PerformanceObserverEntryList,\n",
            "      PerformanceNodeTiming,\n",
            "      nodeTiming: new PerformanceNodeTiming(),\n",
        ]
        for j, line in enumerate(insert_lines):
            lines.insert(i + 1 + j, line)
        print(f"  Added PerformanceEntry/etc to perf_hooks return")
        break

# ──────────────────────────────────────────────────────────────────────
# 5. process.resourceUsage
# ──────────────────────────────────────────────────────────────────────

# Find "      release: { name: \"node\" }," in process
for i in range(len(lines)):
    if 'release: { name: "node" },' in lines[i]:
        process_insert = [
            "      resourceUsage: () => ({\n",
            "        userCPUTime: 0, systemCPUTime: 0, maxRSS: 0,\n",
            "        sharedMemorySize: 0, unsharedDataSize: 0, unsharedStackSize: 0,\n",
            "        minorPageFault: 0, majorPageFault: 0, swappedOut: 0,\n",
            "        fsRead: 0, fsWrite: 0, ipcSent: 0, ipcReceived: 0,\n",
            "        signalsCount: 0, voluntaryContextSwitches: 0, involuntaryContextSwitches: 0,\n",
            "      }),\n",
        ]
        for j, line in enumerate(process_insert):
            lines.insert(i, line)
        print(f"  Added process.resourceUsage before line {i + 1}")
        break

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
