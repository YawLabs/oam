#!/usr/bin/env python3
"""Harden net.Socket for library compat (ws, express, fastify).

Adds: pending getter, bufferSize property, real pause/resume with _paused
flag checked in _readLoop, unpipe(dest), cork()/uncork() no-ops.
Also fixes stale child_process "stub" comment.
"""

path = "js/node_compat.js"
with open(path, "r", encoding="utf-8") as f:
    content = f.read()

# ---------- 1. Fix stale child_process comment ----------
old_cp_comment = (
    "  // ------------------------------------------------------ child_process\n"
    "  // Stub: throws a clear \"not implemented\" error. Subprocess ops land with a\n"
    "  // later wave."
)
new_cp_comment = "  // ------------------------------------------------------ child_process"
assert old_cp_comment in content, "child_process stale comment pattern not found"
content = content.replace(old_cp_comment, new_cp_comment, 1)

# ---------- 2. Add bufferSize and _paused to Socket constructor ----------
old_constructor_tail = (
    '        this.bytesWritten = 0;\n'
    '        this.allowHalfOpen = (options && options.allowHalfOpen) || false;'
)
new_constructor_tail = (
    '        this.bytesWritten = 0;\n'
    '        this.bufferSize = 0;\n'
    '        this.allowHalfOpen = (options && options.allowHalfOpen) || false;\n'
    '        this._paused = false;\n'
    '        this._pipeHandler = null;'
)
assert old_constructor_tail in content, "Socket constructor tail pattern not found"
content = content.replace(old_constructor_tail, new_constructor_tail, 1)

# ---------- 3. Add pending getter (after readyState getter) ----------
old_readystate_end = (
    "      get readyState() {\n"
    '        if (this.connecting) return "opening";\n'
    '        if (this.readable && this.writable) return "open";\n'
    '        if (this.readable) return "readOnly";\n'
    '        if (this.writable) return "writeOnly";\n'
    '        return "closed";\n'
    "      }"
)
new_readystate_end = (
    "      get readyState() {\n"
    '        if (this.connecting) return "opening";\n'
    '        if (this.readable && this.writable) return "open";\n'
    '        if (this.readable) return "readOnly";\n'
    '        if (this.writable) return "writeOnly";\n'
    '        return "closed";\n'
    "      }\n"
    "      get pending() { return this.connecting; }"
)
assert old_readystate_end in content, "readyState getter pattern not found"
content = content.replace(old_readystate_end, new_readystate_end, 1)

# ---------- 4. Harden _readLoop with pause check ----------
old_readloop = (
    "      async _readLoop() {\n"
    "        while (!this.destroyed) {\n"
    "          let chunk;\n"
    "          try {\n"
    "            chunk = await natives.tcpRead(this._handle, 65536);"
)
new_readloop = (
    "      async _readLoop() {\n"
    "        while (!this.destroyed) {\n"
    "          if (this._paused) return;\n"
    "          let chunk;\n"
    "          try {\n"
    "            chunk = await natives.tcpRead(this._handle, 65536);"
)
assert old_readloop in content, "_readLoop pattern not found"
content = content.replace(old_readloop, new_readloop, 1)

# ---------- 5. Replace pipe/pause/resume with full implementations ----------
old_pipe_block = (
    '      pipe(dest) {\n'
    '        this.on("data", (chunk) => dest.write(chunk));\n'
    '        this.on("end", () => { if (typeof dest.end === "function") dest.end(); });\n'
    '        return dest;\n'
    '      }\n'
    '      pause() { return this; }\n'
    '      resume() { return this; }'
)
new_pipe_block = (
    '      pipe(dest) {\n'
    '        this._pipeHandler = (chunk) => dest.write(chunk);\n'
    '        this.on("data", this._pipeHandler);\n'
    '        this.on("end", () => { if (typeof dest.end === "function") dest.end(); });\n'
    '        return dest;\n'
    '      }\n'
    '      unpipe(dest) {\n'
    '        if (this._pipeHandler) {\n'
    '          this.removeListener("data", this._pipeHandler);\n'
    '          this._pipeHandler = null;\n'
    '        }\n'
    '        return this;\n'
    '      }\n'
    '      pause() { this._paused = true; return this; }\n'
    '      resume() {\n'
    '        if (this._paused) {\n'
    '          this._paused = false;\n'
    '          this._readLoop();\n'
    '        }\n'
    '        return this;\n'
    '      }\n'
    '      cork() { return this; }\n'
    '      uncork() { return this; }'
)
assert old_pipe_block in content, "pipe/pause/resume pattern not found"
content = content.replace(old_pipe_block, new_pipe_block, 1)

with open(path, "w", encoding="utf-8") as f:
    f.write(content)

print("All net.Socket hardening + child_process comment fix applied successfully")
