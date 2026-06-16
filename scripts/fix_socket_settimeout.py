"""
Fix 3: net.Socket.setTimeout -- implement actual idle timeout

Currently a no-op that just registers the callback. Implement it to set a
timer that emits 'timeout' when the socket is idle for the specified duration.
The timer resets on data activity.
"""

FILE = r"C:\Users\jeff\yaw\oam_js_runtime\oam\.claude\worktrees\agent-a9b6eca4fb275c11c\js\node_compat.js"

with open(FILE, "r", encoding="utf-8") as f:
    content = f.read()

# ---------- Step 1: Add _timeoutId and _timeoutMs fields to Socket constructor ----------
old_ctor_end = '''        this.allowHalfOpen = (options && options.allowHalfOpen) || false;
        if (options && options._handle !== undefined) {'''

new_ctor_end = '''        this.allowHalfOpen = (options && options.allowHalfOpen) || false;
        this._timeoutMs = 0;
        this._timeoutId = null;
        if (options && options._handle !== undefined) {'''

if old_ctor_end not in content:
    print("ERROR: Could not find Socket constructor allowHalfOpen line")
    exit(1)

content = content.replace(old_ctor_end, new_ctor_end, 1)
print("Added _timeoutMs and _timeoutId fields to Socket constructor")

# ---------- Step 2: Replace the no-op setTimeout with a real implementation ----------
old_settimeout = '''      setTimeout(ms, cb) { if (cb) this.once("timeout", cb); return this; }'''

new_settimeout = '''      setTimeout(ms, cb) {
        if (this._timeoutId !== null) {
          globalThis.clearTimeout(this._timeoutId);
          this._timeoutId = null;
        }
        if (cb) this.once("timeout", cb);
        this._timeoutMs = ms || 0;
        if (this._timeoutMs > 0) this._resetTimeout();
        return this;
      }
      _resetTimeout() {
        if (this._timeoutId !== null) globalThis.clearTimeout(this._timeoutId);
        if (this._timeoutMs > 0 && !this.destroyed) {
          this._timeoutId = globalThis.setTimeout(() => {
            this._timeoutId = null;
            this.emit("timeout");
          }, this._timeoutMs);
        }
      }'''

if old_settimeout not in content:
    print("ERROR: Could not find old Socket.setTimeout")
    exit(1)

content = content.replace(old_settimeout, new_settimeout, 1)
print("Replaced Socket.setTimeout with real implementation")

# ---------- Step 3: Reset timeout on data activity in _readLoop ----------
# The _readLoop emits "data" after receiving a chunk. We need to reset the
# timeout there. Find the line that emits data in the read loop.
old_readloop_data = '''          this.bytesRead += chunk.length;
          if (this._encoding) {
            this.emit("data", new TextDecoder(this._encoding).decode(chunk));
          } else {
            this.emit("data", globalThis.Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength));
          }'''

new_readloop_data = '''          this.bytesRead += chunk.length;
          if (this._timeoutMs > 0) this._resetTimeout();
          if (this._encoding) {
            this.emit("data", new TextDecoder(this._encoding).decode(chunk));
          } else {
            this.emit("data", globalThis.Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength));
          }'''

if old_readloop_data not in content:
    print("ERROR: Could not find _readLoop data emission")
    exit(1)

content = content.replace(old_readloop_data, new_readloop_data, 1)
print("Added timeout reset on data activity in _readLoop")

# ---------- Step 4: Reset timeout on write activity ----------
old_write_bytes = '''        const bytes = toBytes(data, encoding);
        this.bytesWritten += bytes.length;'''

# There might be multiple toBytes patterns. Let's be more specific by
# matching the full Socket.write context
old_socket_write = '''      write(data, encoding, cb) {
        if (typeof encoding === "function") { cb = encoding; encoding = undefined; }
        if (this.destroyed || !this.writable) {
          const err = new Error("This socket has been ended");
          if (cb) cb(err);
          else this.emit("error", err);
          return false;
        }
        const bytes = toBytes(data, encoding);
        this.bytesWritten += bytes.length;'''

new_socket_write = '''      write(data, encoding, cb) {
        if (typeof encoding === "function") { cb = encoding; encoding = undefined; }
        if (this.destroyed || !this.writable) {
          const err = new Error("This socket has been ended");
          if (cb) cb(err);
          else this.emit("error", err);
          return false;
        }
        if (this._timeoutMs > 0) this._resetTimeout();
        const bytes = toBytes(data, encoding);
        this.bytesWritten += bytes.length;'''

if old_socket_write not in content:
    print("ERROR: Could not find Socket.write for timeout reset")
    exit(1)

content = content.replace(old_socket_write, new_socket_write, 1)
print("Added timeout reset on write activity")

# ---------- Step 5: Clear timeout on destroy ----------
old_destroy = '''      destroy(err) {
        if (this.destroyed) return this;
        this.destroyed = true;
        this.readable = false;
        this.writable = false;
        this.connecting = false;
        if (this._handle !== null) {
          try { natives.tcpClose(this._handle); } catch (_) { /* noop */ }
          this._handle = null;
        }
        if (err) this.emit("error", err);
        this.emit("close", !!err);
        return this;
      }'''

new_destroy = '''      destroy(err) {
        if (this.destroyed) return this;
        this.destroyed = true;
        this.readable = false;
        this.writable = false;
        this.connecting = false;
        if (this._timeoutId !== null) {
          globalThis.clearTimeout(this._timeoutId);
          this._timeoutId = null;
        }
        if (this._handle !== null) {
          try { natives.tcpClose(this._handle); } catch (_) { /* noop */ }
          this._handle = null;
        }
        if (err) this.emit("error", err);
        this.emit("close", !!err);
        return this;
      }'''

if old_destroy not in content:
    print("ERROR: Could not find Socket.destroy for timeout cleanup")
    exit(1)

content = content.replace(old_destroy, new_destroy, 1)
print("Added timeout cleanup on destroy")

with open(FILE, "w", encoding="utf-8") as f:
    f.write(content)

print("Done: Socket.setTimeout now properly sets idle timers")
