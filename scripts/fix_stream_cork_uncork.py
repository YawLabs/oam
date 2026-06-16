"""
Fix 4: stream._writev/cork/uncork

Currently cork/uncork are no-ops. Implement cork() to buffer writes and
uncork() to flush them. Add _writev support and wire it into the options
constructor.
"""

FILE = r"C:\Users\jeff\yaw\oam_js_runtime\oam\.claude\worktrees\agent-a9b6eca4fb275c11c\js\node_compat.js"

with open(FILE, "r", encoding="utf-8") as f:
    content = f.read()

# ---------- Step 1: Add corked field to initWritableState ----------
old_init_state = '''      self._wState = {
        objectMode,
        highWaterMark:
          options.writableHighWaterMark ??
          options.highWaterMark ??
          (objectMode ? 16 : 65536),
        queue: [], // {chunk, encoding, cb}
        length: 0,
        writing: false,
        ending: false,
        finished: false,
        destroyed: false,
        needDrain: false,
        endCallbacks: [],
      };
      if (options.write) self._write = options.write;
      if (options.final) self._final = options.final;
      if (options.destroy) self._destroy = options.destroy;'''

new_init_state = '''      self._wState = {
        objectMode,
        highWaterMark:
          options.writableHighWaterMark ??
          options.highWaterMark ??
          (objectMode ? 16 : 65536),
        queue: [], // {chunk, encoding, cb}
        length: 0,
        writing: false,
        ending: false,
        finished: false,
        destroyed: false,
        needDrain: false,
        endCallbacks: [],
        corked: 0,
      };
      if (options.write) self._write = options.write;
      if (options.writev) self._writev = options.writev;
      if (options.final) self._final = options.final;
      if (options.destroy) self._destroy = options.destroy;'''

if old_init_state not in content:
    print("ERROR: Could not find initWritableState body")
    exit(1)

content = content.replace(old_init_state, new_init_state, 1)
print("Added corked field and writev option to initWritableState")

# ---------- Step 2: Make _processWrites respect corked state and support _writev ----------
old_process_writes = '''      _processWrites() {
        const s = this._wState;
        if (s.writing || s.destroyed) return;
        const next = s.queue.shift();
        if (next === undefined) {
          this._maybeFinish();
          return;
        }
        s.writing = true;
        let called = false;
        const done = (err) => {
          if (called) return;
          called = true;
          s.writing = false;
          s.length -= s.objectMode ? 1 : (next.chunk.length ?? 1);
          if (err) {
            if (next.cb) next.cb(err);
            this.destroy(err);
            return;
          }
          if (next.cb) next.cb();
          if (s.queue.length === 0 && s.needDrain && !s.ending) {
            s.needDrain = false;
            this.emit("drain");
          }
          this._processWrites();
        };
        try {
          const result = this._write(next.chunk, next.encoding, done);
          if (result && typeof result.then === "function") {
            result.catch(done);
          }
        } catch (e) {
          done(e);
        }
      },'''

new_process_writes = '''      _processWrites() {
        const s = this._wState;
        if (s.writing || s.destroyed || s.corked > 0) return;
        if (s.queue.length === 0) {
          this._maybeFinish();
          return;
        }
        // If _writev is available and multiple chunks are queued, batch them
        if (this._writev && s.queue.length > 1) {
          const batch = s.queue.splice(0);
          s.writing = true;
          let called = false;
          let batchLen = 0;
          for (let i = 0; i < batch.length; i++) {
            batchLen += s.objectMode ? 1 : (batch[i].chunk.length ?? 1);
          }
          const done = (err) => {
            if (called) return;
            called = true;
            s.writing = false;
            s.length -= batchLen;
            if (err) {
              for (let i = 0; i < batch.length; i++) {
                if (batch[i].cb) batch[i].cb(err);
              }
              this.destroy(err);
              return;
            }
            for (let i = 0; i < batch.length; i++) {
              if (batch[i].cb) batch[i].cb();
            }
            if (s.queue.length === 0 && s.needDrain && !s.ending) {
              s.needDrain = false;
              this.emit("drain");
            }
            this._processWrites();
          };
          try {
            const result = this._writev(batch, done);
            if (result && typeof result.then === "function") {
              result.catch(done);
            }
          } catch (e) {
            done(e);
          }
          return;
        }
        const next = s.queue.shift();
        if (next === undefined) {
          this._maybeFinish();
          return;
        }
        s.writing = true;
        let called = false;
        const done = (err) => {
          if (called) return;
          called = true;
          s.writing = false;
          s.length -= s.objectMode ? 1 : (next.chunk.length ?? 1);
          if (err) {
            if (next.cb) next.cb(err);
            this.destroy(err);
            return;
          }
          if (next.cb) next.cb();
          if (s.queue.length === 0 && s.needDrain && !s.ending) {
            s.needDrain = false;
            this.emit("drain");
          }
          this._processWrites();
        };
        try {
          const result = this._write(next.chunk, next.encoding, done);
          if (result && typeof result.then === "function") {
            result.catch(done);
          }
        } catch (e) {
          done(e);
        }
      },'''

if old_process_writes not in content:
    print("ERROR: Could not find _processWrites method")
    exit(1)

content = content.replace(old_process_writes, new_process_writes, 1)
print("Updated _processWrites to respect corked state and support _writev")

# ---------- Step 3: Replace no-op cork/uncork with real implementations ----------
old_cork = '''      cork() {},
      uncork() {},'''

new_cork = '''      cork() {
        this._wState.corked++;
      },
      uncork() {
        const s = this._wState;
        if (s.corked > 0) s.corked--;
        if (s.corked === 0) this._processWrites();
      },'''

if old_cork not in content:
    print("ERROR: Could not find cork/uncork no-ops")
    exit(1)

content = content.replace(old_cork, new_cork, 1)
print("Replaced cork/uncork with real implementations")

# ---------- Step 4: Update the divergence comment ----------
old_comment = '''  // Documented divergences: setEncoding decodes per-chunk (a multi-byte
  // character split EXACTLY across chunks can mojibake \xe2\x80\x93 use
  // TextDecoderStream for byte-exact decoding); no _writev/cork batching
  // (cork/uncork are accepted no-ops); 'readable'-event pull scheduling is
  // simplified (emitted on every push).'''

new_comment = '''  // Documented divergences: setEncoding decodes per-chunk (a multi-byte
  // character split EXACTLY across chunks can mojibake -- use
  // TextDecoderStream for byte-exact decoding); 'readable'-event pull
  // scheduling is simplified (emitted on every push).'''

# The comment might have the mojibake version of the em-dash
if old_comment in content:
    content = content.replace(old_comment, new_comment, 1)
    print("Updated divergence comment")
else:
    # Try matching without the specific unicode chars
    print("Note: Could not find exact divergence comment to update (non-critical)")

with open(FILE, "w", encoding="utf-8") as f:
    f.write(content)

print("Done: stream._writev/cork/uncork now functional")
