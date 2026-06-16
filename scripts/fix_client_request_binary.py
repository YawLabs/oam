"""
Fix 2: ClientRequest.write binary body fix

Currently the write() method decodes Buffer to string via TextDecoder, which
corrupts binary bodies. Fix it to preserve Buffer/Uint8Array data as binary
through to the native fetch.

Also fix end() to concatenate binary body parts correctly instead of joining
as strings.
"""

FILE = r"C:\Users\jeff\yaw\oam_js_runtime\oam\.claude\worktrees\agent-a9b6eca4fb275c11c\js\node_compat.js"

with open(FILE, "r", encoding="utf-8") as f:
    content = f.read()

# ---------- Fix write() to preserve binary data ----------
old_write = '''      write(chunk, encoding, callback) {
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        if (typeof chunk !== "string") chunk = new TextDecoder().decode(chunk);
        this._body.push(chunk);
        if (callback) queueMicrotask(callback);
        return true;
      }'''

new_write = '''      write(chunk, encoding, callback) {
        if (typeof encoding === "function") { callback = encoding; encoding = undefined; }
        if (typeof chunk === "string") {
          this._body.push(globalThis.Buffer.from(chunk, encoding || "utf8"));
        } else if (chunk instanceof Uint8Array) {
          this._body.push(chunk);
        } else {
          this._body.push(globalThis.Buffer.from(chunk));
        }
        if (callback) queueMicrotask(callback);
        return true;
      }'''

if old_write not in content:
    print("ERROR: Could not find old ClientRequest.write()")
    exit(1)

content = content.replace(old_write, new_write, 1)
print("Fixed ClientRequest.write() to preserve binary data")

# ---------- Fix end() to use binary body concatenation ----------
old_end_body = '''        var bodyStr = self._body.length > 0 ? self._body.join("") : null;
        var fetchOpts = {
          method: self.method,
          headers: self._headers,
        };
        if (bodyStr && self.method !== "GET" && self.method !== "HEAD") {
          fetchOpts.body = bodyStr;
        }'''

new_end_body = '''        var bodyData = null;
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
        }'''

if old_end_body not in content:
    print("ERROR: Could not find old ClientRequest.end() body assembly")
    exit(1)

content = content.replace(old_end_body, new_end_body, 1)
print("Fixed ClientRequest.end() to use binary body concatenation")

with open(FILE, "w", encoding="utf-8") as f:
    f.write(content)

print("Done: ClientRequest now preserves binary body data")
