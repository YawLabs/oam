// node 26's diagnostics_channel BoundedChannel / boundedChannel and
// Channel#withStoreScope, and AsyncLocalStorage's options (defaultValue,
// name) and withScope().
//
// Pins: the start / end pair a name or two channels make, the publish order
// of withScope / run, bound stores entered for the scope and restored after
// it (through `using` and explicit [Symbol.dispose]), the no-subscriber
// fast path (a no-op scope), the channel-type checks, subscribe /
// unsubscribe's return values, and RunScope's restore-once disposal.
import dc from "node:diagnostics_channel";
import { AsyncLocalStorage } from "node:async_hooks";

const show = (label, fn) => {
  try {
    console.log(label, fn());
  } catch (e) {
    console.log(label, "threw", e.name, e.code, e.message);
  }
};

console.log(Object.keys(dc).join(","));
const als = new AsyncLocalStorage({ name: "req", defaultValue: "dflt" });
console.log(als.name, als.getStore(), new AsyncLocalStorage().name, typeof AsyncLocalStorage.prototype.withScope);
show("als name coerced", () => new AsyncLocalStorage({ name: 42 }).name);
show("als options type", () => new AsyncLocalStorage(5));
console.log(als.run("in", () => als.getStore()), als.exit(() => als.getStore()), als.getStore());

{
  const scope = als.withScope("scoped");
  console.log("withScope", als.getStore(), scope.constructor.name, typeof scope.dispose, typeof scope[Symbol.dispose]);
  const inner = als.withScope("inner");
  console.log("nested", als.getStore());
  inner.dispose();
  inner.dispose();
  console.log("inner disposed", als.getStore());
  scope[Symbol.dispose]();
  console.log("outer disposed", als.getStore());
}
{
  using s = als.withScope("using");
  console.log("using", als.getStore(), typeof s);
}
console.log("after using", als.getStore());

const bc = dc.boundedChannel("job");
console.log(bc instanceof dc.BoundedChannel, bc.start.name, bc.end.name, bc.hasSubscribers);
console.log(Object.getOwnPropertyNames(dc.BoundedChannel.prototype).join(","));
console.log(Object.getOwnPropertyDescriptor(bc, "start").writable, Object.keys(bc).length);
const events = [];
const handlers = {
  start: (msg, name) => events.push(`start:${name}:${msg.id}:${als.getStore()}`),
  end: (msg, name) => events.push(`end:${name}:${msg.id}:${als.getStore()}`),
};
bc.subscribe(handlers);
console.log("hasSubscribers", bc.hasSubscribers);
bc.start.bindStore(als, (ctx) => `store-${ctx.id}`);
const result = bc.run({ id: 1 }, function (a, b) {
  events.push(`fn:${this.tag}:${a + b}:${als.getStore()}`);
  return "ret";
}, { tag: "T" }, 2, 3);
console.log("run", result, als.getStore());
{
  using scope = bc.withScope({ id: 2 });
  events.push(`body:${als.getStore()}`);
}
console.log(events.join(" | "));
console.log("after scope", als.getStore());
events.length = 0;
show("run throws", () => bc.run({ id: 3 }, () => {
  throw new Error("boom");
}));
console.log(events.join(" | "), als.getStore());
console.log("unsubscribe", bc.unsubscribe(handlers), bc.unsubscribe(handlers), bc.start.unbindStore(als));
console.log("hasSubscribers after", bc.hasSubscribers);
{
  const scope = bc.withScope({ id: 4 });
  scope[Symbol.dispose]();
  console.log("inactive scope", scope.constructor.name, events.length);
}

const start = dc.channel("custom:start");
const end = dc.channel("custom:end");
const custom = dc.boundedChannel({ start, end });
console.log(custom.start === start, custom.end === end);
show("bad channels", () => dc.boundedChannel({ start, end: {} }));
show("bad type", () => dc.boundedChannel(5));
show("missing end", () => new dc.BoundedChannel({ start }));

const ch = dc.channel("store-scope");
const noop = ch.withStoreScope({ x: 1 });
console.log("inactive withStoreScope", typeof noop[Symbol.dispose], Object.keys(noop).length);
noop[Symbol.dispose]();
const store2 = new AsyncLocalStorage();
const seen = [];
ch.subscribe((m) => seen.push(`msg:${m.x}:${store2.getStore()}`));
ch.bindStore(store2, (m) => m.x * 10);
{
  using sc = ch.withStoreScope({ x: 4 });
  seen.push(`inside:${store2.getStore()}`);
}
seen.push(`outside:${store2.getStore()}`);
console.log(seen.join(" | "));
