// oam test: the runner's JS half, registered as the `oam:test` module.
//
// Design: importing `oam:test` gives describe/test/expect/mock; evaluating
// a test file REGISTERS tests into a suite tree; the Rust runner then calls
// globalThis.__oamTestRun(filter) and pumps the event loop until the
// returned promise settles. Results travel back as a plain JSON-able
// object — the CLI renders pretty output or ODIF from the same data.
//
// Day-1 surface (per roadmap: mocking + fake timers from the start):
// describe/test/it with .skip/.only/.todo, beforeAll/afterAll/beforeEach/
// afterEach, Jest-shape expect (incl. .not / .resolves / .rejects),
// mock.fn / mock.spyOn / mock.restoreAll / mock.clearAll, and
// mock.timers (fake timers: enable/tick/runAll/setSystemTime/restore;
// they cover the timer globals and Date -- Date.now(), new Date(), Date()
// -- but not performance.now()).
// Tests are promise/async only (no done-callback legacy shape).
//
// SNAPSHOT CONSTRAINT (same as the other js/ files): evaluated at BUILD
// time — top level only registers the factory; anything touching natives,
// timers, or Date happens inside the factory or later.
"use strict";
(() => {
  globalThis.__oamNode.factories["oam:test"] = () => {
    const util = globalThis.__oamNode.get("util");
    const deepEqual = util._deepEqual;
    const inspect = (v) => util.inspect(v, { depth: 4 });

    // Captured at factory instantiation (runtime): the REAL timer pair,
    // immune to fake-timer installation. Used for per-test timeouts.
    const realSetTimeout = globalThis.setTimeout;
    const realClearTimeout = globalThis.clearTimeout;

    // ------------------------------------------------------------ suites
    const rootSuite = makeSuite("", null);
    let currentSuite = rootSuite;
    let hasOnly = false;

    function makeSuite(name, parent) {
      return {
        name,
        parent,
        children: [], // suites and tests in declaration order
        beforeAll: [],
        afterAll: [],
        beforeEach: [],
        afterEach: [],
        mode: "normal", // normal | skip | only
      };
    }

    function suitePath(suite) {
      const parts = [];
      for (let s = suite; s && s.name !== ""; s = s.parent) parts.unshift(s.name);
      return parts;
    }

    function describe(name, fn) {
      addSuite(String(name), fn, "normal");
    }
    describe.skip = (name, fn) => addSuite(String(name), fn, "skip");
    describe.only = (name, fn) => {
      hasOnly = true;
      addSuite(String(name), fn, "only");
    };

    function addSuite(name, fn, mode) {
      const suite = makeSuite(name, currentSuite);
      suite.mode = mode;
      currentSuite.children.push({ kind: "suite", suite });
      const prev = currentSuite;
      currentSuite = suite;
      try {
        fn(); // Jest semantics: describe bodies run synchronously at registration
      } finally {
        currentSuite = prev;
      }
    }

    function test(name, fn, options) {
      addTest(String(name), fn, "normal", options);
    }
    test.skip = (name, fn, options) => addTest(String(name), fn, "skip", options);
    test.only = (name, fn, options) => {
      hasOnly = true;
      addTest(String(name), fn, "only", options);
    };
    test.todo = (name) => addTest(String(name), undefined, "todo", undefined);
    const it = test;

    function addTest(name, fn, mode, options) {
      const timeout =
        typeof options === "number" ? options : (options && options.timeout) || 5000;
      currentSuite.children.push({
        kind: "test",
        name,
        fn,
        mode,
        timeout,
        suite: currentSuite,
      });
    }

    const beforeAll = (fn) => currentSuite.beforeAll.push(fn);
    const afterAll = (fn) => currentSuite.afterAll.push(fn);
    const beforeEach = (fn) => currentSuite.beforeEach.push(fn);
    const afterEach = (fn) => currentSuite.afterEach.push(fn);

    // ------------------------------------------------------------ expect
    class ExpectationError extends Error {
      constructor(message) {
        super(message);
        this.name = "ExpectationError";
      }
    }

    function fail(matcher, actual, expectedText, negated) {
      throw new ExpectationError(
        `expect(${inspect(actual)})${negated ? ".not" : ""}.${matcher}${expectedText}`,
      );
    }

    function buildMatchers(actual, negated) {
      const check = (pass, matcher, expectedText) => {
        if (pass === negated) fail(matcher, actual, expectedText, negated);
      };
      // The mock matchers refuse a value that is not a mock or spy, in
      // either polarity: read as "no calls", `.not.toHaveBeenCalled()` on a
      // plain function could never fail.
      const mockCalls = (matcher) => {
        const calls = typeof actual === "function" ? actual.mock?.calls : undefined;
        if (!Array.isArray(calls)) {
          throw new ExpectationError(
            `expect(${inspect(actual)})${negated ? ".not" : ""}.${matcher}: the value must be a mock or spy function`,
          );
        }
        return calls;
      };
      const matchers = {
        toBe: (expected) =>
          check(Object.is(actual, expected), "toBe", `(${inspect(expected)})`),
        toEqual: (expected) =>
          check(deepEqual(actual, expected, false), "toEqual", `(${inspect(expected)})`),
        toStrictEqual: (expected) =>
          check(
            deepEqual(actual, expected, true),
            "toStrictEqual",
            `(${inspect(expected)})`,
          ),
        toBeTruthy: () => check(!!actual, "toBeTruthy", "()"),
        toBeFalsy: () => check(!actual, "toBeFalsy", "()"),
        toBeNull: () => check(actual === null, "toBeNull", "()"),
        toBeUndefined: () => check(actual === undefined, "toBeUndefined", "()"),
        toBeDefined: () => check(actual !== undefined, "toBeDefined", "()"),
        toBeNaN: () => check(Number.isNaN(actual), "toBeNaN", "()"),
        toBeGreaterThan: (n) => check(actual > n, "toBeGreaterThan", `(${inspect(n)})`),
        toBeGreaterThanOrEqual: (n) =>
          check(actual >= n, "toBeGreaterThanOrEqual", `(${inspect(n)})`),
        toBeLessThan: (n) => check(actual < n, "toBeLessThan", `(${inspect(n)})`),
        toBeLessThanOrEqual: (n) =>
          check(actual <= n, "toBeLessThanOrEqual", `(${inspect(n)})`),
        toBeCloseTo: (n, digits = 2) =>
          check(
            Math.abs(actual - n) < 10 ** -digits / 2,
            "toBeCloseTo",
            `(${inspect(n)}, ${digits})`,
          ),
        toBeInstanceOf: (ctor) =>
          check(actual instanceof ctor, "toBeInstanceOf", `(${ctor.name ?? inspect(ctor)})`),
        toContain: (item) => {
          const pass =
            typeof actual === "string"
              ? actual.includes(item)
              : Array.from(actual ?? []).includes(item);
          check(pass, "toContain", `(${inspect(item)})`);
        },
        toContainEqual: (item) => {
          const pass = Array.from(actual ?? []).some((el) => deepEqual(el, item, false));
          check(pass, "toContainEqual", `(${inspect(item)})`);
        },
        toHaveLength: (len) =>
          check(actual?.length === len, "toHaveLength", `(${len}) — got ${actual?.length}`),
        toHaveProperty: (path, ...rest) => {
          const parts = String(path).split(".");
          let cursor = actual;
          let found = true;
          for (const part of parts) {
            if (cursor != null && part in Object(cursor)) cursor = cursor[part];
            else {
              found = false;
              break;
            }
          }
          const pass = found && (rest.length === 0 || deepEqual(cursor, rest[0], false));
          check(pass, "toHaveProperty", `(${inspect(path)})`);
        },
        toMatch: (pattern) => {
          const re = pattern instanceof RegExp ? pattern : null;
          const pass = re ? re.test(actual) : String(actual).includes(String(pattern));
          check(pass, "toMatch", `(${inspect(pattern)})`);
        },
        toMatchObject: (expected) => {
          // Jest's subset rule: a plain object compares only the expected
          // keys; an array must also agree on length; a value whose state is
          // not in enumerable keys (Date, RegExp, Map, Set, Error, binary
          // data) compares by full equality -- walking its keys would find
          // none and match anything.
          const opaque = (v) =>
            v instanceof Date ||
            v instanceof RegExp ||
            v instanceof Map ||
            v instanceof Set ||
            v instanceof Error ||
            v instanceof ArrayBuffer ||
            ArrayBuffer.isView(v);
          const subset = (exp, act) => {
            if (exp === null || typeof exp !== "object") return deepEqual(act, exp, false);
            if (act === null || typeof act !== "object") return false;
            if (opaque(exp)) return deepEqual(act, exp, false);
            if (Array.isArray(exp) && (!Array.isArray(act) || act.length !== exp.length)) {
              return false;
            }
            return Object.keys(exp).every((k) => subset(exp[k], act[k]));
          };
          check(subset(expected, actual), "toMatchObject", `(${inspect(expected)})`);
        },
        toThrow: (expected) => {
          if (typeof actual !== "function") {
            throw new ExpectationError("expect(...).toThrow requires a function");
          }
          let threw = false;
          let error;
          try {
            actual();
          } catch (e) {
            threw = true;
            error = e;
          }
          let pass = threw;
          if (threw && expected !== undefined) {
            const message = error instanceof Error ? error.message : String(error);
            if (typeof expected === "string") pass = message.includes(expected);
            else if (expected instanceof RegExp) pass = expected.test(message);
            else if (typeof expected === "function") pass = error instanceof expected;
            else if (expected !== null && typeof expected === "object") {
              // An error object (or any object carrying `message`) matches
              // on an identical message, as in Jest.
              pass = message === expected.message;
            } else {
              throw new ExpectationError(
                "expect(...).toThrow: expected must be a string, a RegExp, a class or an error object",
              );
            }
          }
          check(pass, "toThrow", expected === undefined ? "()" : `(${inspect(expected)})`);
        },
        // Mock-aware matchers.
        toHaveBeenCalled: () =>
          check(mockCalls("toHaveBeenCalled").length > 0, "toHaveBeenCalled", "()"),
        toHaveBeenCalledTimes: (n) => {
          const calls = mockCalls("toHaveBeenCalledTimes");
          check(calls.length === n, "toHaveBeenCalledTimes", `(${n}) — got ${calls.length}`);
        },
        toHaveBeenCalledWith: (...args) =>
          check(
            mockCalls("toHaveBeenCalledWith").some((call) => deepEqual(call, args, false)),
            "toHaveBeenCalledWith",
            `(${args.map(inspect).join(", ")})`,
          ),
        toHaveBeenLastCalledWith: (...args) => {
          const calls = mockCalls("toHaveBeenLastCalledWith");
          check(
            calls.length > 0 && deepEqual(calls[calls.length - 1], args, false),
            "toHaveBeenLastCalledWith",
            `(${args.map(inspect).join(", ")})`,
          );
        },
      };
      return matchers;
    }

    function expect(actual) {
      const matchers = buildMatchers(actual, false);
      matchers.not = buildMatchers(actual, true);

      const promiseSide = (rejects) => {
        const wrap = (negated) => {
          const out = {};
          // Build matcher names from a probe instance; each async matcher
          // awaits the promise, then applies the sync matcher to the
          // settled value (resolves) or the thrown error (rejects).
          for (const name of Object.keys(buildMatchers(undefined, false))) {
            out[name] = async (...args) => {
              let value;
              let settledOk;
              try {
                value = await actual;
                settledOk = true;
              } catch (e) {
                value = e;
                settledOk = false;
              }
              if (rejects && settledOk) {
                throw new ExpectationError(
                  `expect(...).rejects: promise resolved with ${inspect(value)}`,
                );
              }
              if (!rejects && !settledOk) {
                throw new ExpectationError(
                  `expect(...).resolves: promise rejected with ${inspect(value)}`,
                );
              }
              if (rejects && name === "toThrow") {
                // rejects.toThrow matches against the rejection reason.
                return buildMatchers(() => {
                  throw value;
                }, negated)[name](...args);
              }
              return buildMatchers(value, negated)[name](...args);
            };
          }
          return out;
        };
        const side = wrap(false);
        side.not = wrap(true);
        return side;
      };
      Object.defineProperty(matchers, "resolves", { get: () => promiseSide(false) });
      Object.defineProperty(matchers, "rejects", { get: () => promiseSide(true) });
      return matchers;
    }
    expect.fail = (message) => {
      throw new ExpectationError(message ?? "expect.fail()");
    };

    // -------------------------------------------------------------- mock
    const activeSpies = [];
    const allMockFns = [];

    function mockFn(impl) {
      let implementations = [];
      let defaultImpl = impl;
      const f = function (...args) {
        f.mock.calls.push(args);
        f.mock.contexts.push(this);
        const chosen = implementations.length > 0 ? implementations.shift() : defaultImpl;
        try {
          const value = chosen ? chosen.apply(this, args) : undefined;
          f.mock.results.push({ type: "return", value });
          return value;
        } catch (error) {
          f.mock.results.push({ type: "throw", value: error });
          throw error;
        }
      };
      f.mock = { calls: [], results: [], contexts: [] };
      f.mockImplementation = (next) => {
        defaultImpl = next;
        return f;
      };
      f.mockImplementationOnce = (next) => {
        implementations.push(next);
        return f;
      };
      f.mockReturnValue = (value) => f.mockImplementation(() => value);
      f.mockReturnValueOnce = (value) => f.mockImplementationOnce(() => value);
      f.mockResolvedValue = (value) => f.mockImplementation(() => Promise.resolve(value));
      f.mockResolvedValueOnce = (value) =>
        f.mockImplementationOnce(() => Promise.resolve(value));
      f.mockRejectedValue = (value) => f.mockImplementation(() => Promise.reject(value));
      f.mockRejectedValueOnce = (value) =>
        f.mockImplementationOnce(() => Promise.reject(value));
      f.mockClear = () => {
        f.mock.calls = [];
        f.mock.results = [];
        f.mock.contexts = [];
        return f;
      };
      f.mockReset = () => {
        f.mockClear();
        implementations = [];
        defaultImpl = impl;
        return f;
      };
      allMockFns.push(f);
      return f;
    }

    function spyOn(object, key) {
      const original = object[key];
      if (typeof original !== "function") {
        throw new TypeError(`mock.spyOn: '${String(key)}' is not a function`);
      }
      const spy = mockFn(function (...args) {
        return original.apply(this, args);
      });
      spy.mockRestore = () => {
        object[key] = original;
      };
      object[key] = spy;
      activeSpies.push(spy);
      return spy;
    }

    // -------------------------------------------------------- fake timers
    class FakeTimers {
      constructor() {
        this._installed = false;
      }
      enable(options = {}) {
        if (this._installed) return;
        this._installed = true;
        this._now = options.now ?? Date.now();
        this._queue = []; // {id, due, interval|null, fn, args}
        this._nextId = 1;
        this._saved = {
          setTimeout: globalThis.setTimeout,
          setInterval: globalThis.setInterval,
          clearTimeout: globalThis.clearTimeout,
          clearInterval: globalThis.clearInterval,
          setImmediate: globalThis.setImmediate,
          clearImmediate: globalThis.clearImmediate,
          Date: globalThis.Date,
          dateNow: globalThis.Date.now,
        };
        const schedule = (fn, ms, interval, args) => {
          const id = this._nextId++;
          this._queue.push({
            id,
            due: this._now + Math.max(0, Number(ms) || 0),
            interval,
            fn,
            args,
          });
          return id;
        };
        const cancel = (id) => {
          this._queue = this._queue.filter((t) => t.id !== id);
        };
        globalThis.setTimeout = (fn, ms, ...args) => schedule(fn, ms, null, args);
        globalThis.setInterval = (fn, ms, ...args) =>
          schedule(fn, ms, Math.max(1, Number(ms) || 1), args);
        globalThis.setImmediate = (fn, ...args) => schedule(fn, 0, null, args);
        globalThis.clearTimeout = cancel;
        globalThis.clearInterval = cancel;
        globalThis.clearImmediate = cancel;
        // The clock covers every way of reading "now" through Date: the
        // real Date.now (for code that captured the constructor earlier)
        // and a stand-in global whose zero-argument construction and
        // call-as-function read the fake clock. The stand-in shares the
        // real prototype and builds real Date objects, so instanceof and
        // deep equality treat dates made before and after enable() alike.
        // performance.now() is NOT faked.
        const RealDate = this._saved.Date;
        const clock = this;
        const FakeDate = function Date(...args) {
          if (new.target === undefined) return new RealDate(clock._now).toString();
          return Reflect.construct(
            RealDate,
            args.length === 0 ? [clock._now] : args,
            new.target === FakeDate ? RealDate : new.target,
          );
        };
        FakeDate.prototype = RealDate.prototype;
        FakeDate.now = () => this._now;
        FakeDate.parse = RealDate.parse;
        FakeDate.UTC = RealDate.UTC;
        RealDate.now = FakeDate.now;
        globalThis.Date = FakeDate;
      }
      tick(ms) {
        this._assertInstalled("tick");
        // Fire timers in due order, advancing the clock to each due time so
        // callbacks scheduling follow-ups land deterministically.
        const target = this._now + (Number(ms) || 0);
        // Runaway guard: callbacks fired without the clock moving. A timer
        // that re-arms itself at 0 ms is always due, and this loop is
        // synchronous, so no per-test timeout could interrupt it. A long
        // tick over a real interval advances the clock and never trips it.
        let stalled = 0;
        for (;;) {
          const next = this._queue
            .filter((t) => t.due <= target)
            .sort((a, b) => a.due - b.due || a.id - b.id)[0];
          if (!next) break;
          if (next.due > this._now) stalled = 0;
          else if (++stalled > 100000) {
            throw new Error(
              "mock.timers.tick: runaway timer loop (100000 callbacks without the clock advancing)",
            );
          }
          this._now = Math.max(this._now, next.due);
          if (next.interval != null) next.due += next.interval;
          else this._queue = this._queue.filter((t) => t.id !== next.id);
          next.fn(...next.args);
        }
        this._now = target;
      }
      runAll() {
        this._assertInstalled("runAll");
        let guard = 0;
        while (this._queue.length > 0) {
          if (++guard > 100000) {
            throw new Error("mock.timers.runAll: runaway timer loop");
          }
          const next = this._queue.sort((a, b) => a.due - b.due || a.id - b.id)[0];
          this._now = Math.max(this._now, next.due);
          // Unlike tick(), runAll drops intervals by design (Node test parity).
          this._queue = this._queue.filter((t) => t.id !== next.id);
          next.fn(...next.args);
        }
      }
      setSystemTime(time) {
        this._assertInstalled("setSystemTime");
        this._now = time instanceof Date ? time.getTime() : Number(time);
      }
      now() {
        return this._installed ? this._now : Date.now();
      }
      pendingCount() {
        return this._installed ? this._queue.length : 0;
      }
      restore() {
        if (!this._installed) return;
        this._installed = false;
        globalThis.setTimeout = this._saved.setTimeout;
        globalThis.setInterval = this._saved.setInterval;
        globalThis.clearTimeout = this._saved.clearTimeout;
        globalThis.clearInterval = this._saved.clearInterval;
        globalThis.setImmediate = this._saved.setImmediate;
        globalThis.clearImmediate = this._saved.clearImmediate;
        globalThis.Date = this._saved.Date;
        this._saved.Date.now = this._saved.dateNow;
        this._queue = [];
      }
      _assertInstalled(op) {
        if (!this._installed) {
          throw new Error(`mock.timers.${op}: fake timers are not enabled`);
        }
      }
    }

    const mock = {
      fn: mockFn,
      spyOn,
      timers: new FakeTimers(),
      clearAll: () => {
        for (const f of allMockFns) f.mockClear();
      },
      restoreAll: () => {
        while (activeSpies.length > 0) activeSpies.pop().mockRestore();
      },
    };

    // --------------------------------------------------------------- run
    async function callHook(fn) {
      await fn();
    }

    // What happens to one registered test under this run's filter and the
    // skip/only modes: "filtered" (not reported), "todo", "skip" or "run".
    // The single source for both runTest and the does-this-suite-run check
    // that gates beforeAll/afterAll.
    function disposition(entry, filter) {
      if (filter) {
        const fullName = [...suitePath(entry.suite), entry.name].join(" > ");
        if (!matchesFilter(fullName, filter)) return "filtered";
      }
      if (entry.mode === "todo") return "todo";
      const inherited = inheritedMode(entry.suite);
      if (
        entry.mode === "skip" ||
        inherited === "skip" ||
        (hasOnly && entry.mode !== "only" && inherited !== "only")
      ) {
        return "skip";
      }
      return "run";
    }

    function suiteHasRunnable(suite, filter) {
      return suite.children.some((child) =>
        child.kind === "suite"
          ? suiteHasRunnable(child.suite, filter)
          : disposition(child, filter) === "run",
      );
    }

    async function runTest(entry, results, filter, hookError) {
      const path = [...suitePath(entry.suite), entry.name];
      const fullName = path.join(" > ");
      const fate = disposition(entry, filter);
      if (fate === "filtered") {
        return; // filtered out entirely: not even reported (todo included)
      }
      if (fate === "todo" || fate === "skip") {
        results.push({ name: fullName, status: fate, durationMs: 0, error: null });
        return;
      }
      if (hookError) {
        // An enclosing suite's beforeAll threw: the test cannot run, and it
        // is reported as failed with that error rather than left out.
        results.push({
          name: fullName,
          status: "fail",
          durationMs: 0,
          error: {
            message: `beforeAll failed: ${
              hookError instanceof Error ? hookError.message : String(hookError)
            }`,
            stack: hookError instanceof Error ? trimStack(hookError.stack) : null,
          },
        });
        return;
      }

      const start = Date.now();
      let error = null;
      try {
        try {
          // A throwing beforeEach fails the test without running its body,
          // but stays inside this try so the afterEach pass below still
          // closes whatever the earlier beforeEach hooks opened.
          for (const suite of lineage(entry.suite)) {
            for (const hook of suite.beforeEach) await callHook(hook);
          }
          await withTimeout(entry.fn(), entry.timeout, fullName);
        } catch (e) {
          error = e;
        }
        // afterEach ALWAYS runs (Jest semantics); its failure fails the
        // test only if the hooks and body before it had not already failed.
        for (const suite of lineage(entry.suite).reverse()) {
          for (const hook of suite.afterEach) {
            try {
              await callHook(hook);
            } catch (e) {
              error ??= e;
            }
          }
        }
      } catch (e) {
        error = e;
      } finally {
        // Per-test hygiene: spies restored, fake timers can leak across
        // tests only deliberately (a test that enables them and doesn't
        // restore gets them restored here).
        mock.restoreAll();
        mock.timers.restore();
      }
      results.push({
        name: fullName,
        status: error ? "fail" : "pass",
        durationMs: Date.now() - start,
        error: error
          ? {
              message: error instanceof Error ? error.message : String(error),
              stack: error instanceof Error ? trimStack(error.stack) : null,
            }
          : null,
      });
    }

    function lineage(suite) {
      const chain = [];
      for (let s = suite; s; s = s.parent) chain.unshift(s);
      return chain;
    }

    // A skip anywhere up the chain wins over an only nested inside it: a
    // describe.only under a describe.skip stays skipped.
    function inheritedMode(suite) {
      let mode = "normal";
      for (let s = suite; s; s = s.parent) {
        if (s.mode === "skip") return "skip";
        if (s.mode === "only") mode = "only";
      }
      return mode;
    }

    function matchesFilter(fullName, filter) {
      try {
        if (new RegExp(filter).test(fullName)) return true;
      } catch {
        // not a valid regex: fall through to substring
      }
      return fullName.includes(filter);
    }

    function withTimeout(value, timeoutMs, name) {
      if (!value || typeof value.then !== "function") return Promise.resolve(value);
      return new Promise((resolve, reject) => {
        const timer = realSetTimeout(() => {
          reject(new Error(`test timed out after ${timeoutMs}ms: ${name}`));
        }, timeoutMs);
        Promise.resolve(value).then(
          (v) => {
            realClearTimeout(timer);
            resolve(v);
          },
          (e) => {
            realClearTimeout(timer);
            reject(e);
          },
        );
      });
    }

    function trimStack(stack) {
      if (!stack) return null;
      // Drop runner-internal frames: the runner's own frames carry the
      // location `oam:test_runner.js:L:C` (a native callback or an unnamed
      // script reads '<anonymous>'), while user test files always carry a
      // real path. Only FRAME lines are
      // candidates, and only by their location (an `oam:`/`node:` builtin
      // or a bare `<anonymous>`): the header line survives whatever its
      // message says ("Cannot find module 'node:x'"), and so does a user
      // frame whose function is `Object.<anonymous>` or whose path merely
      // contains one of those words.
      const internalFrame =
        /^at (?:.* \()?(?:(?:oam|node):[^()]*|<anonymous>(?::\d+:\d+)?)\)?$/;
      return stack
        .split("\n")
        .filter((line) => !internalFrame.test(line.trim()))
        .slice(0, 8)
        .join("\n");
    }

    // `inheritedError` is an enclosing suite's beforeAll failure. A suite's
    // own hooks run only when at least one of its tests will actually run
    // (so a skipped or fully filtered suite sets nothing up) and no
    // enclosing beforeAll has failed. A beforeAll that throws fails the
    // tests of THIS suite and its children; sibling suites carry on.
    async function runSuite(suite, results, filter, inheritedError) {
      const ownHooks = !inheritedError && suiteHasRunnable(suite, filter);
      let hookError = inheritedError ?? null;
      try {
        if (ownHooks) {
          try {
            for (const hook of suite.beforeAll) await callHook(hook);
          } catch (e) {
            // Normalised so ANY falsy thrown value (undefined, null, 0, "",
            // false) still marks the failure: the checks downstream are
            // truthiness tests.
            hookError = e || new Error(`beforeAll threw ${String(e)}`);
          }
        }
        for (const child of suite.children) {
          if (child.kind === "suite") await runSuite(child.suite, results, filter, hookError);
          else await runTest(child, results, filter, hookError);
        }
      } finally {
        for (const hook of ownHooks ? suite.afterAll : []) {
          try {
            await callHook(hook);
          } catch (e) {
            results.push({
              name: `${suite.name} > afterAll`,
              status: "fail",
              durationMs: 0,
              error: {
                message: e instanceof Error ? e.message : String(e),
                stack: e instanceof Error ? trimStack(e.stack) : null,
              },
            });
          }
        }
      }
    }

    async function __run(filter) {
      const results = [];
      try {
        await runSuite(rootSuite, results, filter || null, null);
      } catch (e) {
        // Backstop: hook failures are scoped inside runSuite, so only an
        // unexpected runner error reaches here; it is still reported.
        results.push({
          name: "(suite hooks)",
          status: "fail",
          durationMs: 0,
          error: {
            message: e instanceof Error ? e.message : String(e),
            stack: e instanceof Error ? trimStack(e.stack) : null,
          },
        });
      }
      const counts = { pass: 0, fail: 0, skip: 0, todo: 0 };
      for (const r of results) counts[r.status]++;
      return { tests: results, counts };
    }

    return {
      describe,
      test,
      it,
      expect,
      beforeAll,
      afterAll,
      beforeEach,
      afterEach,
      mock,
      __run,
    };
  };

  // Called by the Rust runner after the test file's module graph evaluates.
  // Lazy lookup: the module may never have been imported (a file with no
  // oam:test import registers nothing) — that is a reportable condition,
  // not a crash.
  globalThis.__oamTestRun = function __oamTestRun(filter) {
    const registry = globalThis.__oamNode;
    if (!registry.cache.has("oam:test")) {
      return Promise.resolve({
        tests: [],
        counts: { pass: 0, fail: 0, skip: 0, todo: 0 },
        noTestModule: true,
      });
    }
    return registry.get("oam:test").__run(filter);
  };
})();
