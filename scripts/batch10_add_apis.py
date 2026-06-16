#!/usr/bin/env python3
"""Batch 10: http.OutgoingMessage, full STATUS_CODES, net.SocketAddress/BlockList,
crypto.Certificate, process.getuid/getgid/setuid/setgid."""

import sys

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    lines = f.readlines()

original_len = len(lines)

# ──────────────────────────────────────────────────────────────────────
# 1. http.OutgoingMessage class + full STATUS_CODES
# ──────────────────────────────────────────────────────────────────────

# Find the http return block: "    return {" after the Agent class (around line 6662)
http_return_idx = None
for i in range(len(lines)):
    if "return {" in lines[i] and i > 6600 and i < 6700:
        # Check context: should be after validateHeaderValue
        if any("validateHeaderValue" in lines[j] for j in range(max(0, i-10), i)):
            http_return_idx = i
            break

if http_return_idx is None:
    print("ERROR: Could not find http return block")
    sys.exit(1)

print(f"Found http return block at line {http_return_idx + 1}")

# Insert OutgoingMessage class BEFORE the return block
outgoing_message = [
    "    class OutgoingMessage extends EventEmitter {\n",
    "      constructor() {\n",
    "        super();\n",
    "        this.headersSent = false;\n",
    "        this.sendDate = true;\n",
    "        this.finished = false;\n",
    "        this.writableEnded = false;\n",
    "        this.writableFinished = false;\n",
    "        this._headers = {};\n",
    "      }\n",
    "      setHeader(name, value) { this._headers[name.toLowerCase()] = value; }\n",
    "      getHeader(name) { return this._headers[name.toLowerCase()]; }\n",
    "      getHeaderNames() { return Object.keys(this._headers); }\n",
    "      getHeaders() { return Object.assign({}, this._headers); }\n",
    "      hasHeader(name) { return name.toLowerCase() in this._headers; }\n",
    "      removeHeader(name) { delete this._headers[name.toLowerCase()]; }\n",
    "      flushHeaders() {}\n",
    "      appendHeader(name, value) {\n",
    "        var existing = this._headers[name.toLowerCase()];\n",
    "        if (existing !== undefined) {\n",
    "          this._headers[name.toLowerCase()] = Array.isArray(existing) ? existing.concat(value) : [existing, value];\n",
    "        } else {\n",
    "          this._headers[name.toLowerCase()] = value;\n",
    "        }\n",
    "      }\n",
    "    }\n",
    "    Object.setPrototypeOf(ServerResponse.prototype, OutgoingMessage.prototype);\n",
    "\n",
]

lines[http_return_idx:http_return_idx] = outgoing_message
offset1 = len(outgoing_message)
print(f"  Inserted OutgoingMessage class ({offset1} lines) before http return")

# Now find and replace the STATUS_CODES block with the full set.
# The old STATUS_CODES starts with "      STATUS_CODES: {"
sc_start = None
sc_end = None
for i in range(http_return_idx + offset1, len(lines)):
    if "STATUS_CODES: {" in lines[i]:
        sc_start = i
    if sc_start is not None and sc_end is None and lines[i].strip() == "},":
        sc_end = i + 1
        break

if sc_start is None or sc_end is None:
    print("ERROR: Could not find STATUS_CODES block")
    sys.exit(1)

print(f"Found STATUS_CODES at lines {sc_start + 1}-{sc_end}")

full_status_codes = [
    "      STATUS_CODES: {\n",
    "        100: \"Continue\", 101: \"Switching Protocols\", 102: \"Processing\", 103: \"Early Hints\",\n",
    "        200: \"OK\", 201: \"Created\", 202: \"Accepted\", 203: \"Non-Authoritative Information\",\n",
    "        204: \"No Content\", 205: \"Reset Content\", 206: \"Partial Content\", 207: \"Multi-Status\",\n",
    "        208: \"Already Reported\", 226: \"IM Used\",\n",
    "        300: \"Multiple Choices\", 301: \"Moved Permanently\", 302: \"Found\", 303: \"See Other\",\n",
    "        304: \"Not Modified\", 305: \"Use Proxy\", 307: \"Temporary Redirect\", 308: \"Permanent Redirect\",\n",
    "        400: \"Bad Request\", 401: \"Unauthorized\", 402: \"Payment Required\", 403: \"Forbidden\",\n",
    "        404: \"Not Found\", 405: \"Method Not Allowed\", 406: \"Not Acceptable\",\n",
    "        407: \"Proxy Authentication Required\", 408: \"Request Timeout\", 409: \"Conflict\",\n",
    "        410: \"Gone\", 411: \"Length Required\", 412: \"Precondition Failed\",\n",
    "        413: \"Payload Too Large\", 414: \"URI Too Long\", 415: \"Unsupported Media Type\",\n",
    "        416: \"Range Not Satisfiable\", 417: \"Expectation Failed\", 418: \"I'm a Teapot\",\n",
    "        421: \"Misdirected Request\", 422: \"Unprocessable Entity\", 423: \"Locked\",\n",
    "        424: \"Failed Dependency\", 425: \"Too Early\", 426: \"Upgrade Required\",\n",
    "        428: \"Precondition Required\", 429: \"Too Many Requests\",\n",
    "        431: \"Request Header Fields Too Large\", 451: \"Unavailable For Legal Reasons\",\n",
    "        500: \"Internal Server Error\", 501: \"Not Implemented\", 502: \"Bad Gateway\",\n",
    "        503: \"Service Unavailable\", 504: \"Gateway Timeout\",\n",
    "        505: \"HTTP Version Not Supported\", 506: \"Variant Also Negotiates\",\n",
    "        507: \"Insufficient Storage\", 508: \"Loop Detected\",\n",
    "        510: \"Not Extended\", 511: \"Network Authentication Required\",\n",
    "      },\n",
]

lines[sc_start:sc_end] = full_status_codes
offset2 = len(full_status_codes) - (sc_end - sc_start)
print(f"  Replaced STATUS_CODES ({sc_end - sc_start} -> {len(full_status_codes)} lines, offset={offset2})")

# Add OutgoingMessage to the return block
# Find "      ClientRequest," in the return block
for i in range(http_return_idx + offset1, len(lines)):
    if "ClientRequest," in lines[i] and lines[i].strip() == "ClientRequest,":
        lines.insert(i + 1, "      OutgoingMessage,\n")
        offset2 += 1
        print(f"  Added OutgoingMessage to http return at line {i + 2}")
        break

# ──────────────────────────────────────────────────────────────────────
# 2. net.SocketAddress + net.BlockList
# ──────────────────────────────────────────────────────────────────────

# Find net return block: "    return {" with isIPv4, isIPv6 nearby
net_return_idx = None
for i in range(len(lines)):
    if "return {" in lines[i] and i > 6900:
        if any("isIPv4, isIPv6, isIP," in lines[j] for j in range(i, min(len(lines), i + 5))):
            net_return_idx = i
            break

if net_return_idx is None:
    print("ERROR: Could not find net return block")
    sys.exit(1)

print(f"Found net return block at line {net_return_idx + 1}")

net_classes = [
    "    class SocketAddress {\n",
    "      constructor(options) {\n",
    "        options = options || {};\n",
    "        this.address = options.address || \"127.0.0.1\";\n",
    "        this.port = options.port || 0;\n",
    "        this.family = options.family || \"ipv4\";\n",
    "        this.flowlabel = options.flowlabel || 0;\n",
    "      }\n",
    "    }\n",
    "\n",
    "    class BlockList {\n",
    "      constructor() { this._rules = []; }\n",
    "      addAddress(address, family) {\n",
    "        this._rules.push({ type: \"address\", address: address, family: family || \"ipv4\" });\n",
    "      }\n",
    "      addRange(start, end, family) {\n",
    "        this._rules.push({ type: \"range\", start: start, end: end, family: family || \"ipv4\" });\n",
    "      }\n",
    "      addSubnet(network, prefix, family) {\n",
    "        this._rules.push({ type: \"subnet\", network: network, prefix: prefix, family: family || \"ipv4\" });\n",
    "      }\n",
    "      check(address, family) {\n",
    "        var fam = (family || \"ipv4\").toLowerCase();\n",
    "        for (var ri = 0; ri < this._rules.length; ri++) {\n",
    "          var rule = this._rules[ri];\n",
    "          if (rule.family !== fam) continue;\n",
    "          if (rule.type === \"address\" && rule.address === address) return true;\n",
    "        }\n",
    "        return false;\n",
    "      }\n",
    "      get rules() { return this._rules.slice(); }\n",
    "    }\n",
    "\n",
]

lines[net_return_idx:net_return_idx] = net_classes
offset3 = len(net_classes)
print(f"  Inserted SocketAddress+BlockList ({offset3} lines) before net return")

# Add to net return object: find "      Socket, Server," line after insertion
for i in range(net_return_idx + offset3, min(len(lines), net_return_idx + offset3 + 10)):
    if "Socket, Server," in lines[i]:
        lines.insert(i + 1, "      SocketAddress, BlockList,\n")
        print(f"  Added SocketAddress+BlockList to net return at line {i + 2}")
        break

# ──────────────────────────────────────────────────────────────────────
# 3. crypto.Certificate class
# ──────────────────────────────────────────────────────────────────────

# Find crypto return block: "    return {" around line 5756 (shifted by prior inserts)
crypto_return_idx = None
for i in range(len(lines)):
    if "return {" in lines[i] and "hash: (algorithm, data, outputEncoding)" in (lines[i+1] if i+1 < len(lines) else ""):
        crypto_return_idx = i
        break

if crypto_return_idx is None:
    print("ERROR: Could not find crypto return block")
    sys.exit(1)

print(f"Found crypto return block at line {crypto_return_idx + 1}")

certificate_class = [
    "    class Certificate {\n",
    "      static exportChallenge() { return globalThis.Buffer.alloc(0); }\n",
    "      static exportPublicKey() { return globalThis.Buffer.alloc(0); }\n",
    "      static verifySpkac() { return false; }\n",
    "      exportChallenge() { return Certificate.exportChallenge.apply(null, arguments); }\n",
    "      exportPublicKey() { return Certificate.exportPublicKey.apply(null, arguments); }\n",
    "      verifySpkac() { return Certificate.verifySpkac.apply(null, arguments); }\n",
    "    }\n",
    "\n",
]

lines[crypto_return_idx:crypto_return_idx] = certificate_class
offset4 = len(certificate_class)
print(f"  Inserted Certificate class ({offset4} lines) before crypto return")

# Add Certificate to the crypto return block. Find "      constants: {" in crypto return
for i in range(crypto_return_idx + offset4, len(lines)):
    if lines[i].strip().startswith("constants: {"):
        lines.insert(i, "      Certificate,\n")
        print(f"  Added Certificate to crypto return at line {i + 1}")
        break

# ──────────────────────────────────────────────────────────────────────
# 4. process.getuid/getgid/setuid/setgid/geteuid/getegid
# ──────────────────────────────────────────────────────────────────────

# Find process Object.assign block -- look for "      umask: () => 0,"
for i in range(len(lines)):
    if "umask: () => 0," in lines[i]:
        process_insert = [
            "      getuid: () => 0,\n",
            "      getgid: () => 0,\n",
            "      geteuid: () => 0,\n",
            "      getegid: () => 0,\n",
            "      setuid: () => {},\n",
            "      setgid: () => {},\n",
            "      seteuid: () => {},\n",
            "      setegid: () => {},\n",
            "      getgroups: () => [0],\n",
            "      setgroups: () => {},\n",
            "      initgroups: () => {},\n",
        ]
        for j, line in enumerate(process_insert):
            lines.insert(i + 1 + j, line)
        print(f"  Added process uid/gid functions after line {i + 1}")
        break

with open(path, "w", encoding="utf-8") as f:
    f.writelines(lines)

print(f"\nDone. {original_len} -> {len(lines)} lines (+{len(lines) - original_len})")
