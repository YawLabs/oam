// Legacy stream inheritance: require('stream') is a FUNCTION-style constructor
// (not an ES6 class), so the classic util.inherits(X, Stream) + Stream.call(this)
// pattern works -- jws / jsonwebtoken and other real packages depend on it. An
// ES6 `class Stream` would reject Stream.call(this) with "Class constructor
// cannot be invoked without 'new'".
import stream from "node:stream";
import util from "node:util";

function Legacy() {
  stream.Stream.call(this);
  this.ok = true;
}
util.inherits(Legacy, stream.Stream);

const l = new Legacy();
console.log("instanceof-Stream=" + (l instanceof stream.Stream));
console.log("has-on=" + (typeof l.on === "function"));
console.log("has-emit=" + (typeof l.emit === "function"));
console.log("ctor-ran=" + l.ok);
console.log("super_=" + (Legacy.super_ === stream.Stream));

let got;
l.on("evt", (v) => {
  got = v;
});
l.emit("evt", 42);
console.log("event-payload=" + got);
