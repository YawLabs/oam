// The cause of a fetch failure that undici raises itself carries undici's
// error class name, code and message -- measured on node v22.22.2, whose
// fetch reports an oversized response head as `TypeError: fetch failed` with
// a `HeadersOverflowError` (`UND_ERR_HEADERS_OVERFLOW`, `Headers Overflow
// Error`) cause. oam's transport raised the code and message under the plain
// name `Error`, so code that tells undici's failures apart by `cause.name`
// (as by `instanceof errors.HeadersOverflowError` with undici imported) saw
// none of them. The headers and body timeouts share the mapping; they need a
// dispatcher with short limits, so their pins are e2e
// (fetch_rides_its_dispatchers_headers_and_body_timeouts).
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

const server = net.createServer((s) => {
  s.on("error", () => {});
  s.once("data", () => s.end(`HTTP/1.1 200 OK\r\nx-big: ${"a".repeat(20000)}\r\ncontent-length: 0\r\n\r\n`));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
try {
  await fetch(`http://127.0.0.1:${server.address().port}/`);
  console.log("ok");
} catch (e) {
  const c = e.cause;
  console.log(e.name, e.message);
  console.log("cause", c.name, c.code, JSON.stringify(c.message), c instanceof Error);
}
process.exit(0);
