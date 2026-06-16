#!/usr/bin/env python3
import pathlib

p = pathlib.Path("js/node_compat.js")
src = p.read_text(encoding="utf-8")

OLD = r'''  // ----------------------------------------------------------- perf_hooks
  // Wrap globalThis.performance (installed post-restore). Factory defers the
  // lookup to call time so requiring perf_hooks before installRuntimeGlobals
  // still works (performance will be live when methods are invoked).
  registry.factories.perf_hooks = () => {
    class PerformanceObserver {
      constructor(cb) { this._cb = cb; this._types = []; }
      observe(options) { this._types = (options && options.entryTypes) || []; }
      disconnect() { this._types = []; }
    }
    PerformanceObserver.supportedEntryTypes = Object.freeze(["measure", "mark"]);
    class PerformanceEntry {
      constructor(name, entryType, startTime, duration) {
        this.name = name || "";
        this.entryType = entryType || "";
        this.startTime = startTime || 0;
        this.duration = duration || 0;
      }
      toJSON() {
        return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration };
      }
    }
    class PerformanceObserverEntryList {
      constructor() { this._entries = []; }
      getEntries() { return this._entries.slice(); }
      getEntriesByName(name) { return this._entries.filter(function(e) { return e.name === name; }); }
      getEntriesByType(type) { return this._entries.filter(function(e) { return e.entryType === type; }); }
    }
    class PerformanceNodeTiming extends PerformanceEntry {
      constructor() {
        super("node", "node", 0, 0);
        this.nodeStart = 0;
        this.v8Start = 0;
        this.bootstrapComplete = 0;
        this.environment = 0;
        this.loopStart = 0;
        this.loopExit = 0;
        this.idleTime = 0;
      }
    }

    return {
      get performance() { return globalThis.performance; },
      PerformanceObserver,
      PerformanceEntry,
      PerformanceObserverEntryList,
      PerformanceNodeTiming,
      nodeTiming: new PerformanceNodeTiming(),
      createHistogram: () => ({
        record: () => {},
        percentile: () => 0,
        mean: 0,
        max: 0,
        min: 0,
        count: 0,
      }),
      monitorEventLoopDelay: () => ({
        enable() {},
        disable() {},
        reset() {},
        mean: 0,
        max: 0,
        min: 0,
        stddev: 0,
        percentile: () => 0,
        percentiles: new Map(),
      }),
    };
  };'''

NEW = r'''  // ----------------------------------------------------------- perf_hooks
  // Wrap globalThis.performance (installed post-restore). Factory defers the
  // lookup to call time so requiring perf_hooks before installRuntimeGlobals
  // still works (performance will be live when methods are invoked).
  registry.factories.perf_hooks = () => {
    const _marks = [];
    const _measures = [];
    const _observers = [];

    class PerformanceEntry {
      constructor(name, entryType, startTime, duration, detail) {
        this.name = name || "";
        this.entryType = entryType || "";
        this.startTime = startTime || 0;
        this.duration = duration || 0;
        this.detail = detail !== undefined ? detail : null;
      }
      toJSON() {
        return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration, detail: this.detail };
      }
    }

    class PerformanceObserverEntryList {
      constructor(entries) { this._entries = entries || []; }
      getEntries() { return this._entries.slice(); }
      getEntriesByName(name, type) {
        return this._entries.filter(function(e) {
          return e.name === name && (type === undefined || e.entryType === type);
        });
      }
      getEntriesByType(type) { return this._entries.filter(function(e) { return e.entryType === type; }); }
    }

    function _notifyObservers(entry) {
      for (var i = 0; i < _observers.length; i++) {
        var obs = _observers[i];
        if (obs._types && obs._types.indexOf(entry.entryType) !== -1) {
          try {
            obs._cb(new PerformanceObserverEntryList([entry]), obs);
          } catch (e) { /* observer callback errors are swallowed per spec */ }
        }
      }
    }

    class PerformanceObserver {
      constructor(cb) { this._cb = cb; this._types = []; }
      observe(options) {
        if (options && options.entryTypes) {
          this._types = options.entryTypes.slice();
        } else if (options && options.type) {
          if (this._types.indexOf(options.type) === -1) {
            this._types.push(options.type);
          }
          if (options.buffered) {
            var existing = [];
            var t = options.type;
            if (t === "mark") {
              for (var i = 0; i < _marks.length; i++) existing.push(_marks[i]);
            } else if (t === "measure") {
              for (var i = 0; i < _measures.length; i++) existing.push(_measures[i]);
            }
            if (existing.length > 0) {
              var self = this;
              try { self._cb(new PerformanceObserverEntryList(existing), self); } catch (e) {}
            }
          }
        }
        if (_observers.indexOf(this) === -1) {
          _observers.push(this);
        }
      }
      disconnect() {
        this._types = [];
        var idx = _observers.indexOf(this);
        if (idx !== -1) _observers.splice(idx, 1);
      }
    }
    PerformanceObserver.supportedEntryTypes = Object.freeze(["mark", "measure"]);

    class PerformanceNodeTiming extends PerformanceEntry {
      constructor() {
        super("node", "node", 0, 0);
        this.nodeStart = 0;
        this.v8Start = 0;
        this.bootstrapComplete = 0;
        this.environment = 0;
        this.loopStart = 0;
        this.loopExit = 0;
        this.idleTime = 0;
      }
    }

    function _findMark(name) {
      for (var i = _marks.length - 1; i >= 0; i--) {
        if (_marks[i].name === name) return _marks[i];
      }
      return null;
    }

    var _nodeTiming = new PerformanceNodeTiming();

    var perf = {
      now: function() { return globalThis.performance.now(); },
      get timeOrigin() { return globalThis.performance.timeOrigin; },

      mark: function(name, options) {
        var startTime = (options && options.startTime !== undefined) ? options.startTime : globalThis.performance.now();
        var detail = (options && options.detail !== undefined) ? options.detail : null;
        var entry = new PerformanceEntry(name, "mark", startTime, 0, detail);
        _marks.push(entry);
        _notifyObservers(entry);
        return entry;
      },

      measure: function(name, startMarkOrOptions, endMark) {
        var startTime = 0;
        var endTime = globalThis.performance.now();
        var detail = null;
        var duration;

        if (typeof startMarkOrOptions === "string") {
          var sm = _findMark(startMarkOrOptions);
          if (sm) startTime = sm.startTime;
          if (typeof endMark === "string") {
            var em = _findMark(endMark);
            if (em) endTime = em.startTime;
          }
          duration = endTime - startTime;
        } else if (startMarkOrOptions && typeof startMarkOrOptions === "object") {
          var opts = startMarkOrOptions;
          detail = opts.detail !== undefined ? opts.detail : null;
          if (opts.start !== undefined) {
            if (typeof opts.start === "string") {
              var smk = _findMark(opts.start);
              startTime = smk ? smk.startTime : 0;
            } else {
              startTime = opts.start;
            }
          }
          if (opts.end !== undefined) {
            if (typeof opts.end === "string") {
              var emk = _findMark(opts.end);
              endTime = emk ? emk.startTime : globalThis.performance.now();
            } else {
              endTime = opts.end;
            }
          }
          if (opts.duration !== undefined) {
            duration = opts.duration;
          } else {
            duration = endTime - startTime;
          }
        } else {
          duration = endTime - startTime;
        }

        var entry = new PerformanceEntry(name, "measure", startTime, duration, detail);
        _measures.push(entry);
        _notifyObservers(entry);
        return entry;
      },

      getEntries: function() { return _marks.concat(_measures); },
      getEntriesByName: function(name, type) {
        return _marks.concat(_measures).filter(function(e) {
          return e.name === name && (type === undefined || e.entryType === type);
        });
      },
      getEntriesByType: function(type) {
        if (type === "mark") return _marks.slice();
        if (type === "measure") return _measures.slice();
        return [];
      },

      clearMarks: function(name) {
        if (name !== undefined) {
          for (var i = _marks.length - 1; i >= 0; i--) {
            if (_marks[i].name === name) _marks.splice(i, 1);
          }
        } else {
          _marks.length = 0;
        }
      },
      clearMeasures: function(name) {
        if (name !== undefined) {
          for (var i = _measures.length - 1; i >= 0; i--) {
            if (_measures[i].name === name) _measures.splice(i, 1);
          }
        } else {
          _measures.length = 0;
        }
      },

      toJSON: function() {
        return { timeOrigin: globalThis.performance.timeOrigin, nodeTiming: {} };
      },

      eventLoopUtilization: function() {
        return { idle: 0, active: 0, utilization: 0 };
      }
    };

    return {
      performance: perf,
      PerformanceObserver,
      PerformanceEntry,
      PerformanceObserverEntryList,
      PerformanceNodeTiming,
      nodeTiming: _nodeTiming,
      createHistogram: () => ({
        record: () => {},
        percentile: () => 0,
        mean: 0,
        max: 0,
        min: 0,
        count: 0,
      }),
      monitorEventLoopDelay: () => ({
        enable() {},
        disable() {},
        reset() {},
        mean: 0,
        max: 0,
        min: 0,
        stddev: 0,
        percentile: () => 0,
        percentiles: new Map(),
      }),
    };
  };'''

assert OLD in src, "anchor not found -- perf_hooks factory text does not match"
src = src.replace(OLD, NEW, 1)
p.write_text(src, encoding="utf-8")
print("OK")
