// The shape of node's coded errors (`codes.ERR_*`), beyond `code` and the
// message. Node builds each code as a class (lib/internal/errors.js
// makeNodeErrorWithCode): `code` is a class field, `message` is defined by
// the constructor as a non-enumerable own property, and `toString()` lives on
// the class prototype. oam assigned `code` after the base constructor had set
// `message` and defined `toString` on the instance, so:
//
// - Object.getOwnPropertyNames() gave stack, message, code, toString where
//   node gives stack, code, message. The common serialization
//   `JSON.stringify(err, Object.getOwnPropertyNames(err))` wrote its keys in
//   a different order.
// - `delete err.toString` changed String(err); in node it has no effect.
// - The instance's prototype was the base class's own prototype; in node a
//   per-code prototype sits in between (and `constructor` still answers the
//   base class).
//
// That order is for a code whose message is a function. A code whose message
// is a string is built with `super(message)` in node, so `message` precedes
// `code` there; one of those is pinned too, as is a code whose message
// function sets extra fields (they land between `code` and `message`).
import net from "node:net";
import util from "node:util";

function caught(fn) {
  try {
    fn();
  } catch (err) {
    return err;
  }
  return undefined;
}

// `make` yields one error of the code per call (possibly asynchronously).
async function describe(label, make) {
  const e = await make();
  if (e === undefined) {
    console.log(label, "-> no error");
    return;
  }
  const Base = globalThis[e.name];
  const names = Object.getOwnPropertyNames(e);
  console.log(label, "->", e.name, e.code);
  console.log("   own:", JSON.stringify(names), "| enumerable:", JSON.stringify(Object.keys(e)));
  console.log(
    "   descriptors:",
    names
      .filter((n) => n !== "stack")
      .map((n) => {
        const d = Object.getOwnPropertyDescriptor(e, n);
        return `${n}=${d.writable ? "w" : "-"}${d.enumerable ? "e" : "-"}${d.configurable ? "c" : "-"}`;
      })
      .join(" "),
  );
  // Where toString is found: not on the instance, not on Base.prototype.
  let holder = e;
  let depth = 0;
  while (holder && !Object.hasOwn(holder, "toString")) {
    holder = Object.getPrototypeOf(holder);
    depth++;
  }
  console.log(
    "   toString: own", Object.hasOwn(e, "toString"),
    "| found on the instance:", depth === 0,
    "| found on Base.prototype:", holder === Base.prototype,
    "| found below Base.prototype:", depth > 0 && holder !== Base.prototype && holder instanceof Base,
  );
  console.log(
    "   prototype is Base.prototype:", Object.getPrototypeOf(e) === Base.prototype,
    "| instanceof Base:", e instanceof Base,
    "| constructor === Base:", e.constructor === Base,
    "| own constructor on prototype:", Object.hasOwn(Object.getPrototypeOf(e), "constructor"),
  );
  console.log("   String:", String(e));
  console.log("   stack head:", e.stack.split("\n")[0]);
  console.log("   JSON by own names:", JSON.stringify(e, names.filter((n) => n !== "stack")));
  console.log("   JSON:", JSON.stringify(e));
  // Two errors of one code share the prototype that carries toString.
  const again = await make();
  console.log("   same prototype on a second throw:", Object.getPrototypeOf(again) === Object.getPrototypeOf(e));
  delete e.toString;
  console.log("   String after `delete err.toString`:", String(e));
  e.message = "replaced";
  console.log("   String after message is replaced:", String(e));
}

console.log("== a RangeError code");
await describe("Buffer.alloc(-1)", () => caught(() => Buffer.alloc(-1)));

console.log("== a TypeError code");
await describe("Buffer.from(1)", () => caught(() => Buffer.from(1)));

console.log("== an Error code, raised from a pre-built Error");
await describe("setUncaughtExceptionCaptureCallback twice", () => {
  process.setUncaughtExceptionCaptureCallback(() => {});
  try {
    return caught(() => process.setUncaughtExceptionCaptureCallback(() => {}));
  } finally {
    process.setUncaughtExceptionCaptureCallback(null);
  }
});

console.log("== a code whose message function sets extra fields");
// A lookup that answers with an address family that is neither 4 nor 6:
// ERR_INVALID_ADDRESS_FAMILY, which carries the host and port. Nothing is
// dialled.
await describe(
  "net.connect with a lookup answering family 5",
  () =>
    new Promise((resolve) => {
      const socket = net.connect({
        host: "name.invalid",
        port: 9,
        autoSelectFamily: false,
        lookup: (host, options, cb) => cb(null, "127.0.0.1", 5),
      });
      socket.on("error", resolve);
    }),
);

console.log("== a code with a string message: message precedes code");
await describe("Buffer.from('x', 'bogus')", () => caught(() => Buffer.from("x", "bogus")));

console.log("== a field set by the message function of a directly shaped error");
// ERR_FALSY_VALUE_REJECTION is built outside the E() factory, and its
// `reason` used to be set before the shape re-created `code` and `message`,
// so it came first: [stack, reason, code, message]. Node's message function
// sets it between the two.
await describe(
  "util.callbackify rejecting with null",
  () => new Promise((resolve) => util.callbackify(() => Promise.reject(null))(resolve)),
);
