// EventEmitter must be callable as a plain function, not just `new`-able:
// transpiled CJS (TS __extends, ioredis, pg) does `_super.call(this)`. An
// ES6-class EventEmitter rejects that ("cannot be invoked without 'new'").
// Regression guard for the redis-on-oam fix.
import { EventEmitter } from "node:events";

// 1) Transpiled TypeScript __extends pattern (what ioredis ships as CJS).
var __extends = (function () {
  var extendStatics = function (d, b) { Object.setPrototypeOf(d, b); };
  return function (d, b) {
    extendStatics(d, b);
    function __() { this.constructor = d; }
    d.prototype = b === null ? Object.create(b) : ((__.prototype = b.prototype), new __());
  };
})();
var Sub = (function (_super) {
  __extends(Sub, _super);
  function Sub() { return _super.call(this) || this; }
  return Sub;
})(EventEmitter);
const s = new Sub();
const seen = [];
s.on("evt", (x) => seen.push(x));
s.emit("evt", "hello");
console.log("transpiled", seen.join(","), s instanceof EventEmitter, typeof s.emit);

// 2) EventEmitter.call(this) onto a hand-rolled prototype chain.
function Plain() { EventEmitter.call(this); }
Plain.prototype = Object.create(EventEmitter.prototype);
const p = new Plain();
let got = 0;
p.on("z", () => { got++; });
p.emit("z");
console.log("plainCall", got);

// 3) Native ES6 class extends still works (a class can extend a function).
class NativeSub extends EventEmitter {
  constructor() { super(); this.tag = "native"; }
}
const n = new NativeSub();
let n2 = 0;
n.on("q", () => { n2++; });
n.emit("q");
console.log("nativeExtends", n2, n.tag);
