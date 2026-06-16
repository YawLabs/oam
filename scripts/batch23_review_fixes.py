#!/usr/bin/env python3
"""Apply all review findings from M3 node-compat review.

Fixes:
1. perf_hooks measure() -- throw on missing named mark
2. perf_hooks getEntries -- sort by startTime
3. perf_hooks observer entryTypes+buffered combo
4. readline terminal -- auto-infer from output.isTTY
5. readline question() -- cleanup listener on close
6. readline/promises question -- AbortSignal support
7. vm createContext -- throw TypeError on null/non-object
8. MessagePort.postMessage -- structuredClone
9. worker_threads getEnvironmentData -- structuredClone + primitive shortcircuit
10. worker_threads setEnvironmentData -- clone on store
"""
import pathlib, sys

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")
orig = src

# --- 1. perf_hooks measure(): throw on missing named mark ---
# String-form branch: lines ~8104-8110
src = src.replace(
    'if (typeof startMarkOrOptions === "string") {\n'
    '          var sm = _findMark(startMarkOrOptions);\n'
    '          if (sm) startTime = sm.startTime;\n'
    '          if (typeof endMark === "string") {\n'
    '            var em = _findMark(endMark);\n'
    '            if (em) endTime = em.startTime;\n'
    '          }',
    'if (typeof startMarkOrOptions === "string") {\n'
    '          var sm = _findMark(startMarkOrOptions);\n'
    '          if (!sm) throw new Error("Failed to execute \'measure\': The mark \'" + startMarkOrOptions + "\' does not exist.");\n'
    '          startTime = sm.startTime;\n'
    '          if (typeof endMark === "string") {\n'
    '            var em = _findMark(endMark);\n'
    '            if (!em) throw new Error("Failed to execute \'measure\': The mark \'" + endMark + "\' does not exist.");\n'
    '            endTime = em.startTime;\n'
    '          }',
    1
)

# Object-options branch: opts.start string
src = src.replace(
    'if (typeof opts.start === "string") {\n'
    '              var smk = _findMark(opts.start);\n'
    '              startTime = smk ? smk.startTime : 0;',
    'if (typeof opts.start === "string") {\n'
    '              var smk = _findMark(opts.start);\n'
    '              if (!smk) throw new Error("Failed to execute \'measure\': The mark \'" + opts.start + "\' does not exist.");\n'
    '              startTime = smk.startTime;',
    1
)

# Object-options branch: opts.end string
src = src.replace(
    'if (typeof opts.end === "string") {\n'
    '              var emk = _findMark(opts.end);\n'
    '              endTime = emk ? emk.startTime : globalThis.performance.now();',
    'if (typeof opts.end === "string") {\n'
    '              var emk = _findMark(opts.end);\n'
    '              if (!emk) throw new Error("Failed to execute \'measure\': The mark \'" + opts.end + "\' does not exist.");\n'
    '              endTime = emk.startTime;',
    1
)

# --- 2. perf_hooks getEntries -- sort by startTime ---
src = src.replace(
    'getEntries: function() { return _marks.concat(_measures); },',
    'getEntries: function() { return _marks.concat(_measures).sort(function(a,b){return a.startTime-b.startTime;}); },',
    1
)
src = src.replace(
    'getEntriesByName: function(name, type) {\n'
    '        return _marks.concat(_measures).filter(function(e) {',
    'getEntriesByName: function(name, type) {\n'
    '        return _marks.concat(_measures).sort(function(a,b){return a.startTime-b.startTime;}).filter(function(e) {',
    1
)

# --- 3. perf_hooks observer entryTypes+buffered combo ---
# Move buffered delivery into a shared helper that fires after either branch
src = src.replace(
    '      observe(options) {\n'
    '        if (options && options.entryTypes) {\n'
    '          this._types = options.entryTypes.slice();\n'
    '        } else if (options && options.type) {\n'
    '          if (this._types.indexOf(options.type) === -1) {\n'
    '            this._types.push(options.type);\n'
    '          }\n'
    '          if (options.buffered) {\n'
    '            var existing = [];\n'
    '            var t = options.type;\n'
    '            if (t === "mark") {\n'
    '              for (var i = 0; i < _marks.length; i++) existing.push(_marks[i]);\n'
    '            } else if (t === "measure") {\n'
    '              for (var i = 0; i < _measures.length; i++) existing.push(_measures[i]);\n'
    '            }\n'
    '            if (existing.length > 0) {\n'
    '              var self = this;\n'
    '              try { self._cb(new PerformanceObserverEntryList(existing), self); } catch (e) {}\n'
    '            }\n'
    '          }\n'
    '        }',
    '      observe(options) {\n'
    '        if (options && options.entryTypes) {\n'
    '          this._types = options.entryTypes.slice();\n'
    '        } else if (options && options.type) {\n'
    '          if (this._types.indexOf(options.type) === -1) {\n'
    '            this._types.push(options.type);\n'
    '          }\n'
    '        }\n'
    '        if (options && options.buffered) {\n'
    '          var existing = [];\n'
    '          var types = this._types;\n'
    '          for (var ti = 0; ti < types.length; ti++) {\n'
    '            var t = types[ti];\n'
    '            if (t === "mark") {\n'
    '              for (var i = 0; i < _marks.length; i++) existing.push(_marks[i]);\n'
    '            } else if (t === "measure") {\n'
    '              for (var i = 0; i < _measures.length; i++) existing.push(_measures[i]);\n'
    '            }\n'
    '          }\n'
    '          if (existing.length > 0) {\n'
    '            var self = this;\n'
    '            try { self._cb(new PerformanceObserverEntryList(existing), self); } catch (e) {}\n'
    '          }\n'
    '        }',
    1
)

# --- 4. readline terminal -- auto-infer from output.isTTY ---
src = src.replace(
    'this.terminal = opts.terminal === true;',
    'this.terminal = opts.terminal != null ? opts.terminal === true : !!(opts.output && opts.output.isTTY);',
    1
)

# --- 5. readline question() -- cleanup listener on close ---
src = src.replace(
    '      question(prompt, cb) {\n'
    '        if (this.output && typeof this.output.write === "function") this.output.write(prompt);\n'
    '        const onLine = (line) => { this.removeListener("line", onLine); cb(line); };\n'
    '        this.once("line", onLine);\n'
    '      }',
    '      question(prompt, cb) {\n'
    '        if (this.output && typeof this.output.write === "function") this.output.write(prompt);\n'
    '        const cleanup = () => { this.removeListener("line", onLine); };\n'
    '        const onLine = (line) => { this.removeListener("close", cleanup); cb(line); };\n'
    '        this.once("line", onLine);\n'
    '        this.once("close", cleanup);\n'
    '      }',
    1
)

# --- 6. readline/promises question -- AbortSignal support ---
src = src.replace(
    '    class Interface extends rl.Interface {\n'
    '      question(prompt) {\n'
    '        return new Promise(function (resolve) {\n'
    '          rl.Interface.prototype.question.call(this, prompt, resolve);\n'
    '        }.bind(this));\n'
    '      }\n'
    '    }',
    '    class Interface extends rl.Interface {\n'
    '      question(prompt, options) {\n'
    '        var signal = options && options.signal ? options.signal : null;\n'
    '        return new Promise(function (resolve, reject) {\n'
    '          if (signal && signal.aborted) { reject(new DOMException("The operation was aborted", "AbortError")); return; }\n'
    '          var onAbort;\n'
    '          rl.Interface.prototype.question.call(this, prompt, function(answer) {\n'
    '            if (signal && onAbort) signal.removeEventListener("abort", onAbort);\n'
    '            resolve(answer);\n'
    '          });\n'
    '          if (signal) {\n'
    '            onAbort = function() { reject(new DOMException("The operation was aborted", "AbortError")); };\n'
    '            signal.addEventListener("abort", onAbort, { once: true });\n'
    '          }\n'
    '        }.bind(this));\n'
    '      }\n'
    '    }',
    1
)

# --- 7. vm createContext -- throw TypeError on null/non-object ---
src = src.replace(
    '    function createContext(sandbox, _options) {\n'
    '      const obj = sandbox != null && typeof sandbox === "object"\n'
    '        ? sandbox\n'
    '        : Object.create(null);',
    '    function createContext(sandbox, _options) {\n'
    '      if (sandbox !== undefined && (sandbox === null || typeof sandbox !== "object")) {\n'
    '        throw new TypeError("The \'sandbox\' argument must be of type object. Received " + typeof sandbox);\n'
    '      }\n'
    '      const obj = sandbox != null ? sandbox : Object.create(null);',
    1
)

# --- 8. MessagePort.postMessage -- structuredClone ---
src = src.replace(
    '      postMessage(data) {\n'
    '        const twin = this._twin;\n'
    '        if (twin && twin._active) queueMicrotask(() => twin.emit("message", data));\n'
    '      }',
    '      postMessage(data) {\n'
    '        const twin = this._twin;\n'
    '        if (twin && twin._active) {\n'
    '          const cloned = (typeof data === "object" && data !== null) ? structuredClone(data) : data;\n'
    '          queueMicrotask(() => twin.emit("message", cloned));\n'
    '        }\n'
    '      }',
    1
)

# --- 9. worker_threads getEnvironmentData -- structuredClone + primitive shortcircuit ---
src = src.replace(
    '    function getEnvironmentData(key) {\n'
    '      const val = _envData.get(key);\n'
    '      if (val === undefined) return undefined;\n'
    '      // Return a clone via JSON round-trip\n'
    '      return JSON.parse(JSON.stringify(val));\n'
    '    }',
    '    function getEnvironmentData(key) {\n'
    '      const val = _envData.get(key);\n'
    '      if (val === undefined) return undefined;\n'
    '      if (val === null || typeof val !== "object") return val;\n'
    '      return structuredClone(val);\n'
    '    }',
    1
)

# --- 10. worker_threads setEnvironmentData -- clone on store ---
src = src.replace(
    '    function setEnvironmentData(key, value) {\n'
    '      if (value === undefined) {\n'
    '        _envData.delete(key);\n'
    '      } else {\n'
    '        _envData.set(key, value);\n'
    '      }\n'
    '    }',
    '    function setEnvironmentData(key, value) {\n'
    '      if (value === undefined) {\n'
    '        _envData.delete(key);\n'
    '      } else {\n'
    '        _envData.set(key, (typeof value === "object" && value !== null) ? structuredClone(value) : value);\n'
    '      }\n'
    '    }',
    1
)

if src == orig:
    print("ERROR: no changes applied", file=sys.stderr)
    sys.exit(1)

p.write_text(src, encoding="utf-8")

# Count changes
changes = 0
for i, (a, b) in enumerate(zip(orig.split("\n"), src.split("\n"))):
    if a != b:
        changes += 1
extra = len(src.split("\n")) - len(orig.split("\n"))
print(f"OK -- {changes} lines changed, {extra:+d} net lines")
