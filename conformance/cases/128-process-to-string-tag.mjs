// process carries node's own Symbol.toStringTag, 'process' (an own data
// property: writable, not enumerable, not configurable), so
// Object.prototype.toString.call(process) is '[object process]'. axios 1.x
// decides whether its node http adapter is usable with exactly that check
// (utils.kindOf(process) === 'process'); on oam it read '[object Object]',
// so axios fell back to its fetch adapter, which ignores httpAgent /
// httpsAgent -- the agents guard packages hand it.
const d = Object.getOwnPropertyDescriptor(process, Symbol.toStringTag);
console.log("descriptor", JSON.stringify(d));
console.log("toString", Object.prototype.toString.call(process));
console.log("template", `${process}`);
console.log("in keys", Object.keys(process).includes(Symbol.toStringTag.toString()));
// axios lib/utils.js kindOf + its platform check.
const kindOf = ((cache) => (thing) => {
  const str = Object.prototype.toString.call(thing);
  return cache[str] || (cache[str] = str.slice(8, -1).toLowerCase());
})(Object.create(null));
console.log("axios isHttpAdapterSupported", typeof process !== "undefined" && kindOf(process) === "process");
