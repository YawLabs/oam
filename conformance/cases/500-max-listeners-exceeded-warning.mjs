// Past its listener limit an EventEmitter, or an EventTarget, emits a process
// warning -- process.emitWarning with a MaxListenersExceededWarning Error that
// carries the emitter (or target), the event type and the count -- once per
// event (per target, for an EventTarget). It is an ordinary warning: a
// 'warning' listener hears it, and --no-warnings, or taking the default
// listener off, leaves it unprinted (#365). The text names the emitter as
// util.inspect does at depth -1.
//
// The printing goes to stderr, which the harness does not compare, so each
// scenario runs as a child and its stderr is printed here, with the pid and
// the program name in the one-time hint taken out.
import { spawnSync } from "node:child_process";
import { EventEmitter, getMaxListeners, setMaxListeners } from "node:events";

const scenario = process.argv[2];
const flags = process.argv.slice(3);

const names = new Map();
const describe = (who) => names.get(who) ?? (who === undefined ? "undefined" : "an unnamed object");
const hear = () =>
  process.on("warning", (w) => {
    console.log(
      `heard ${w.name}: ${w.message}\n` +
        `  keys ${JSON.stringify(Object.keys(w))} emitter ${describe(w.emitter)} target ${describe(w.target)}` +
        ` type ${typeof w.type} ${String(w.type)} count ${w.count}`,
    );
  });

if (scenario === "emitter") {
  if (flags.includes("removed")) process.removeAllListeners("warning");
  hear();
  const ee = new EventEmitter();
  names.set(ee, "ee");
  for (let i = 0; i < 10; i++) ee.on("x", () => {});
  console.log("10 listeners: no warning yet");
  ee.on("x", () => {});
  // Once per event: no second warning for x.
  for (let i = 0; i < 3; i++) ee.on("x", () => {});
  const sym = Symbol("sym");
  for (let i = 0; i < 11; i++) ee.on(sym, () => {});
  class Sub extends EventEmitter {}
  const sub = new Sub();
  names.set(sub, "sub");
  sub.setMaxListeners(1);
  sub.on("a", () => {});
  sub.prependListener("a", () => {});
  const onceOnly = new EventEmitter();
  names.set(onceOnly, "onceOnly");
  onceOnly.setMaxListeners(1);
  onceOnly.once("b", () => {});
  onceOnly.once("b", () => {});
  const unlimited = new EventEmitter();
  unlimited.setMaxListeners(0);
  for (let i = 0; i < 20; i++) unlimited.on("c", () => {});
  // Removing them all resets the event: past the limit again, it warns again.
  ee.removeAllListeners("x");
  for (let i = 0; i < 11; i++) ee.on("x", () => {});
  const later = new EventEmitter();
  names.set(later, "later");
  EventEmitter.defaultMaxListeners = 1;
  later.on("d", () => {});
  later.on("d", () => {});
  EventEmitter.defaultMaxListeners = 10;
} else if (scenario === "target") {
  if (flags.includes("removed")) process.removeAllListeners("warning");
  hear();
  const et = new EventTarget();
  names.set(et, "et");
  setMaxListeners(2, et);
  for (let i = 0; i < 4; i++) et.addEventListener("foo", () => {});
  // Once per target, whatever the type.
  for (let i = 0; i < 4; i++) et.addEventListener("other", () => {});
  const plain = new EventTarget();
  names.set(plain, "plain");
  console.log("default limit:", getMaxListeners(plain));
  for (let i = 0; i < 11; i++) plain.addEventListener("bar", () => {});
  class MyTarget extends EventTarget {}
  const mine = new MyTarget();
  names.set(mine, "mine");
  setMaxListeners(1, mine);
  const same = () => {};
  mine.addEventListener("dup", same);
  mine.addEventListener("dup", same);
  console.log("the same listener twice: no warning");
  mine.addEventListener("dup", () => {});
  // An AbortSignal's limit is 0 (none) until it is set.
  const ac = new AbortController();
  console.log("signal limit:", getMaxListeners(ac.signal));
  for (let i = 0; i < 11; i++) ac.signal.addEventListener("abort", () => {});
  const ac2 = new AbortController();
  names.set(ac2.signal, "signal");
  setMaxListeners(2, ac2.signal);
  for (let i = 0; i < 3; i++) ac2.signal.addEventListener("abort", () => {});
  // A target takes the default limit when it is made.
  const before = new EventTarget();
  names.set(before, "before");
  EventEmitter.defaultMaxListeners = 1;
  const after = new EventTarget();
  names.set(after, "after");
  console.log("limits:", getMaxListeners(before), getMaxListeners(after));
  for (const t of [before, after]) {
    t.addEventListener("e", () => {});
    t.addEventListener("e", () => {});
  }
  EventEmitter.defaultMaxListeners = 10;
} else {
  const normalize = (text) =>
    text
      .split(/\r?\n/)
      .filter((line) => line !== "")
      .map((line) => line.replace(/\((node|oam):\d+\)/g, "(node:PID)").replace(/^\(Use `(node|oam) /, "(Use `node "));
  const run = (name, nodeFlags = [], extra = []) => {
    const child = spawnSync(process.execPath, [...nodeFlags, import.meta.filename, name, ...extra], {
      encoding: "utf8",
    });
    console.log(`## ${[...nodeFlags, name, ...extra].join(" ")}`);
    console.log(`exit ${child.status}`);
    for (const line of child.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
    for (const line of normalize(child.stderr)) console.log(`  err | ${line}`);
  };
  for (const name of ["emitter", "target"]) {
    run(name);
    run(name, ["--no-warnings"]);
    run(name, [], ["removed"]);
    run(name, ["--disable-warning=MaxListenersExceededWarning"]);
  }
}
