// V8's async WebAssembly.compile / instantiate settle from a FOREGROUND TASK
// the platform posts once the background compile is done, an
// Atomics.waitAsync settles from one (a notify, or a delayed task for its
// timeout), and a FinalizationRegistry cleanup is one too. oam never ran
// those tasks, so every such promise stayed pending forever: undici 6 from
// npm hung on its first request, which awaits the async compile of its llhttp
// parser, and a top-level `await WebAssembly.instantiate(bytes)` died as a
// deadlocked await. Node runs them from its loop, each followed by a tick +
// microtask drain, and in-flight compile work keeps the loop alive (its
// DrainTasks); a waiting waitAsync does not (measured on node v22.22.2).

// (module (func (export "f") (result i32) i32.const 42))
const bytes = new Uint8Array([
  0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 127, 3, 2, 1, 0, 7, 5, 1, 1, 102, 0, 0, 10, 6,
  1, 4, 0, 65, 42, 11,
]);

process.on("exit", () => console.log("exit"));

// Top-level await on the async paths.
const { module, instance } = await WebAssembly.instantiate(bytes);
console.log("instantiate(bytes)", module instanceof WebAssembly.Module, instance.exports.f());
const compiled = await WebAssembly.compile(bytes);
console.log("compile", compiled instanceof WebAssembly.Module);
const again = await WebAssembly.instantiate(compiled);
console.log("instantiate(module)", again instanceof WebAssembly.Instance, again.exports.f());
const bad = await WebAssembly.compile(new Uint8Array([0, 97, 115, 109, 2, 0, 0, 0])).catch((e) => e);
console.log("compile(bad)", bad.name, bad.message);
const viaStreaming = await WebAssembly.instantiate(bytes.buffer);
console.log("instantiate(ArrayBuffer)", viaStreaming.instance.exports.f());

// A tick and a microtask queued by the settled continuation run before any
// later macrotask.
await new Promise((resolve) => {
  const order = [];
  WebAssembly.compile(bytes).then(() => {
    order.push("compiled");
    process.nextTick(() => order.push("tick"));
    Promise.resolve().then(() => order.push("micro"));
    setTimeout(() => {
      order.push("timeout");
      console.log("order", order.join(" "));
      resolve();
    }, 0);
  });
});

// Atomics.waitAsync: a same-thread notify, and a timeout (a delayed task).
const ia = new Int32Array(new SharedArrayBuffer(16));
const notified = Atomics.waitAsync(ia, 0, 0);
console.log("waitAsync async", notified.async);
console.log("notify woke", Atomics.notify(ia, 0, 1));
console.log("notified", await notified.value);
// A waiting waitAsync does not keep the loop alive (in node either), so a
// timer holds it open across the timeout.
const keepAlive = setTimeout(() => {}, 10000);
const timed = Atomics.waitAsync(ia, 1, 0, 20);
console.log("timed out", await timed.value);
clearTimeout(keepAlive);

// Nothing else alive: the in-flight compile alone keeps the loop open until
// its continuation runs, then the process exits. A waitAsync left waiting
// does not hold it open.
Atomics.waitAsync(ia, 2, 0);
WebAssembly.instantiate(bytes).then(({ instance: last }) => console.log("last", last.exports.f()));
