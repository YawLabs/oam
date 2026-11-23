// node 26's Web Storage, Navigator interface and QuotaExceededError globals.
//
// sessionStorage: an in-memory Storage -- string keys and values, named
// properties (shadowed by the prototype's own names), key order (node's
// SQLite order: UTF-16LE bytes), the 5 MiB key+value quota and its
// QuotaExceededError, the arity / brand errors, inspect output, and the
// replaceable global. localStorage reads undefined without
// --localstorage-file (node warns on stderr, which is not compared).
// Navigator: an illegal constructor, enumerable prototype getters, and the
// one instance behind a read-only accessor; navigator.userAgent and the
// locale-dependent language are left out. QuotaExceededError: a
// DOMException subclass with validated quota / requested.
import util from "node:util";

const show = (label, fn) => {
  try {
    console.log(label, fn());
  } catch (e) {
    console.log(label, "threw", e.constructor.name, e.name, e.code, e.message);
  }
};
const desc = (name) => {
  const d = Object.getOwnPropertyDescriptor(globalThis, name);
  return d && Object.keys(d).map((k) => `${k}:${typeof d[k] === "function" ? `fn(${d[k].name})` : d[k]}`).join(" ");
};
for (const name of ["Storage", "localStorage", "sessionStorage", "Navigator", "navigator", "QuotaExceededError"]) {
  console.log(name, desc(name));
}

console.log("localStorage", typeof localStorage);
const s = sessionStorage;
console.log(s instanceof Storage, s.length, Object.prototype.toString.call(s), Object.getPrototypeOf(s) === Storage.prototype);
console.log(Object.getOwnPropertyNames(Storage.prototype).join(","), Storage.name, Storage.length);
s.setItem("a", 1);
s.setItem("b", { x: 1 });
s.c = 3;
s["d"] = null;
console.log(s.length, s.getItem("a"), typeof s.getItem("a"), s.getItem("b"), s.c, s.getItem("d"), s.getItem("zz"));
console.log(Object.keys(s).join(","), JSON.stringify(s));
console.log(s.key(0), s.key(3), s.key(99), s.key(-1), s.key("1"), s.key(1.9), s.key(4294967296), s.key(NaN));
delete s.a;
console.log(s.getItem("a"), s.length, "a" in s, "b" in s, s.hasOwnProperty("b"));
s.setItem("getItem", "v");
console.log(typeof s.getItem, s.getItem("getItem"), Object.keys(s).join(","));
s.setItem("length", "L");
console.log(s.length, s.getItem("length"));
console.log(Object.getOwnPropertyDescriptor(s, "b"), Object.getOwnPropertyDescriptor(s, "zz"));
Object.defineProperty(s, "q", { value: 7 });
console.log("defineProperty", s.getItem("q"), JSON.stringify({ ...s }));
s.clear();
console.log("cleared", s.length, Object.keys(s).length);
for (const k of ["b", "B", "a", "A", "é", "\u{1f600}", "€", "10", "9", "_", ""]) s.setItem(k, "1");
console.log("order", JSON.stringify(Object.keys(s)));
s.clear();
s.setItem("k", "v");
console.log(util.inspect(s), Object.entries(s));
s.clear();
console.log(util.inspect(s));

const L = 5 * 1024 * 1024;
const fits = (k, n, ch = "x") => {
  s.clear();
  try {
    s.setItem(k, ch.repeat(n));
    return "ok";
  } catch (e) {
    return `${e.constructor.name}:${e.name}:${e.code}:${e.message}:${e instanceof DOMException}`;
  }
};
console.log("quota", fits("big", L - 3), fits("big", L - 2), fits("", L), fits("", L + 1));
console.log("quota utf16", fits("", L, "é"), fits("", L / 2, "\u{1f600}"), fits("", L / 2 + 1, "\u{1f600}"));
s.clear();
s.setItem("a", "x".repeat(L - 1));
show("replace within quota", () => (s.setItem("a", "yy"), s.getItem("a")));
s.clear();
s.setItem("a", "x".repeat(L - 1));
show("over quota", () => s.setItem("b", ""));
show("failed set keeps old", () => {
  try {
    s.setItem("a", "z".repeat(L));
  } catch {}
  return s.getItem("a").length;
});
s.clear();

show("setItem arity", () => s.setItem("only"));
show("getItem arity", () => s.getItem());
show("key arity", () => s.key());
show("removeItem arity", () => s.removeItem());
show("symbol key", () => s.setItem(Symbol("a"), 1));
show("brand", () => Storage.prototype.getItem.call({}, "a"));
show("new Storage", () => new Storage());
s[Symbol.for("sym")] = 1;
console.log("symbol prop", s[Symbol.for("sym")], Object.keys(s).length);

console.log(navigator instanceof Navigator, Object.prototype.toString.call(navigator));
show("new Navigator", () => new Navigator());
show("navigator getter brand", () => Object.getOwnPropertyDescriptor(Navigator.prototype, "platform").get.call({}));
console.log(typeof navigator.hardwareConcurrency, navigator.hardwareConcurrency >= 1);
console.log(Array.isArray(navigator.languages), Object.isFrozen(navigator.languages), navigator.languages[0] === navigator.language);
console.log(navigator.languages === navigator.languages, navigator.onLine, Object.keys(navigator).length);
console.log(navigator.platform === { win32: "Win32", darwin: "MacIntel" }[process.platform] ||
  (process.platform === "linux" && navigator.platform.startsWith("Linux ")));
for (const name of ["hardwareConcurrency", "language", "languages", "userAgent", "platform"]) {
  const d = Object.getOwnPropertyDescriptor(Navigator.prototype, name);
  console.log("Navigator#" + name, typeof d.get, d.set, d.enumerable, d.configurable);
}

const q = new QuotaExceededError("m", { quota: 5, requested: 10 });
console.log(q.name, q.message, q.code, q.quota, q.requested, q instanceof DOMException, Object.prototype.toString.call(q));
console.log(QuotaExceededError.length, Object.getPrototypeOf(QuotaExceededError) === DOMException);
const q2 = new QuotaExceededError();
console.log(JSON.stringify(q2.message), q2.quota, q2.requested);
show("quota NaN", () => new QuotaExceededError("x", { quota: NaN }));
show("requested Infinity", () => new QuotaExceededError("x", { requested: Infinity }));
show("quota negative", () => new QuotaExceededError("x", { quota: -1 }));
show("requested < quota", () => new QuotaExceededError("x", { quota: 5, requested: 2 }));
show("quota brand", () => Object.getOwnPropertyDescriptor(QuotaExceededError.prototype, "quota").get.call({}));
show("getRandomValues quota", () => {
  try {
    crypto.getRandomValues(new Uint8Array(65537));
  } catch (e) {
    return [e.constructor.name, e.name, e.code, e.message, e instanceof QuotaExceededError, e.quota, e.requested].join(" ");
  }
});

globalThis.sessionStorage = 5;
console.log("replaced", typeof sessionStorage, sessionStorage, desc("sessionStorage"));
