// The cause of a fetch that undici itself refuses or fails is an instance of
// an undici error class (#177), measured on node v22.22.2 (undici 6.24.1):
// `InvalidArgumentError` / `NotSupportedError` /
// `RequestContentLengthMismatchError` for a request undici will not
// dispatch, `HeadersOverflowError` for a response head over the limit. Each
// carries `name` and `code` as own enumerable properties, and undici's
// brands: `Symbol.for('undici.error.UND_ERR')` and one for its own code.
//
// The brands are what `instanceof` checks (a static Symbol.hasInstance on
// every undici error class), so an error from one copy of undici is an
// instance of another copy's class. No undici is importable here under
// node, so the case declares a class with undici's check and asks it; that
// `instanceof undici.errors.X` holds for the built-in shim is an e2e test.
//
// oam's causes were plain `Error`s with the `name` set: no `code`, no
// brands, and the constructor `Error`.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const brandOf = (code) => Symbol.for(`undici.error.${code}`);
// What a second copy of undici would define for `code`.
function foreignClass(code) {
  const brand = brandOf(code);
  return class {
    static [Symbol.hasInstance](instance) {
      return instance && instance[brand] === true;
    }
  };
}
const ForeignUndiciError = foreignClass("UND_ERR");

function describe(c) {
  if (!(c instanceof Error)) return `not an Error: ${String(c)}`;
  return (
    `${c.constructor.name} name=${c.name} code=${c.code} ${JSON.stringify(c.message)} ` +
    `own=${JSON.stringify(Reflect.ownKeys(c).map(String))} keys=${JSON.stringify(Object.keys(c))} ` +
    `stack=${JSON.stringify(String(c.stack).split("\n")[0])} ` +
    `base=${Object.getPrototypeOf(c.constructor).name} ` +
    `brands=${c[brandOf("UND_ERR")]}/${c[brandOf(c.code)]} ` +
    `foreign=${c instanceof foreignClass(c.code)}/${c instanceof ForeignUndiciError}/` +
    `${c instanceof foreignClass("UND_ERR_SOCKET")} ` +
    `self=${c instanceof c.constructor} Error=${c instanceof Error} TypeError=${c instanceof TypeError}`
  );
}

const server = http.createServer((req, res) => {
  req.resume();
  req.on("end", () => res.end("ok"));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const U = `http://127.0.0.1:${server.address().port}/p`;

const refusals = {
  "transfer-encoding": { method: "POST", body: "abc", headers: { "transfer-encoding": "chunked" } },
  "keep-alive": { headers: { "keep-alive": "timeout=5" } },
  upgrade: { headers: { upgrade: "websocket" } },
  connection: { headers: { connection: "close, transfer-encoding" } },
  expect: { method: "POST", body: "abc", headers: { expect: "100-continue" } },
  "content-length long": { method: "POST", body: "abc", headers: { "content-length": "10" } },
};
for (const [label, init] of Object.entries(refusals)) {
  try {
    const res = await fetch(U, init);
    console.log(`${label}: resolved ${res.status}`);
  } catch (e) {
    console.log(`${label}: ${e.constructor.name} ${e.message} | ${describe(e.cause)}`);
  }
}
server.close();

// A response head over the 16 KiB limit.
const big = net.createServer((socket) => {
  socket.on("error", () => {});
  socket.once("data", () => {
    socket.end(`HTTP/1.1 200 OK\r\nx-big: ${"a".repeat(20000)}\r\ncontent-length: 2\r\n\r\nok`);
  });
});
await new Promise((r) => big.listen(0, "127.0.0.1", r));
try {
  const res = await fetch(`http://127.0.0.1:${big.address().port}/`);
  console.log(`headers overflow: resolved ${res.status}`);
} catch (e) {
  console.log(`headers overflow: ${e.constructor.name} ${e.message} | ${describe(e.cause)}`);
}
big.close();
