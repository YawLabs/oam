// EventEmitter ordering and semantics.
import { EventEmitter, once } from "node:events";

const ee = new EventEmitter();
const order = [];
ee.on("x", () => order.push("on"));
ee.prependListener("x", () => order.push("pre"));
ee.once("x", () => order.push("once"));
ee.emit("x");
ee.emit("x");
console.log(order.join(","), ee.listenerCount("x"));
let threw = false;
try {
  ee.emit("error", new Error("unhandled-err"));
} catch (e) {
  threw = e.message === "unhandled-err";
}
console.log(threw);
ee.removeAllListeners("x");
console.log(ee.listenerCount("x"), ee.eventNames().length);
setTimeout(() => ee.emit("ready", 41, 42), 1);
const args = await once(ee, "ready");
console.log(args.join("+"));
