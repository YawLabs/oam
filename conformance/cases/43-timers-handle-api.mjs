// setTimeout/setInterval/setImmediate return Node Timeout/Immediate OBJECTS,
// not bare numeric ids: .ref()/.unref()/.hasRef() (chainable), a numeric
// [Symbol.toPrimitive] (so clearTimeout(+t) and object-key stringification
// work), and [Symbol.dispose]. process.ref/unref drive the legacy + Symbol
// API. (unref() is best-effort on the loop; here every timer is cleared.)
const t = setTimeout(() => {}, 1000);
console.log("hasRef.init=" + t.hasRef());
console.log("unref-chain=" + (t.unref().ref().unref() === t));
console.log("hasRef.after=" + t.hasRef());
console.log("toPrimitive-isnumber=" + (typeof t[Symbol.toPrimitive]() === "number"));
console.log("plus-isnumber=" + (typeof +t === "number"));
console.log("plus-eq-prim=" + (+t === t[Symbol.toPrimitive]()));
console.log("objkey=" + (Object.keys({ [t]: 1 })[0] === `${t}`));
clearTimeout(+t);

const i = setInterval(() => {}, 1000);
console.log("interval-hasRef=" + i.hasRef());
process.unref(i);
console.log("interval-hasRef-unref=" + i.hasRef());
process.ref(i);
console.log("interval-hasRef-ref=" + i.hasRef());
clearInterval(i);

// process.ref/unref against an object with the legacy ref()/unref() methods.
let refCalled = 0;
let unrefCalled = 0;
const legacy = { ref() { refCalled++; }, unref() { unrefCalled++; } };
process.ref(legacy);
process.unref(legacy);
console.log("legacy-ref=" + refCalled + " legacy-unref=" + unrefCalled);

// Symbol.dispose clears the timer + marks it destroyed.
const d = setTimeout(() => {}, 1000);
d[Symbol.dispose]();
console.log("dispose-destroyed=" + d._destroyed);

// getActiveResourcesInfo reflects a pending timer.
const keep = setTimeout(() => {}, 50);
console.log("active-has-timeout=" + process.getActiveResourcesInfo().includes("Timeout"));
clearTimeout(keep);
