// The request headers fetch adds of its own (#178), read off a raw socket
// and sorted (the order differs: node writes `host` first, oam last; and
// `user-agent` is each runtime's own, so both are left out). Measured on
// node v22.22.2 first; oam sent only `accept` and `accept-encoding:
// gzip,deflate`.
//
//   - `connection: keep-alive`, `accept-language: *` and `sec-fetch-mode`
//     (the request's mode, whatever the caller set) on every request;
//     `accept-encoding: gzip, deflate` over http (https is an e2e test:
//     `br, gzip, deflate`). A caller's own value wins, but for
//     sec-fetch-mode.
//   - `content-length: 0` on a POST, PUT, PATCH, QUERY, PROPFIND or PROPPATCH
//     -- the method as written, so `patch` gets none -- with no body or an
//     empty one; a DELETE or an OPTIONS gets none.
//   - `cache: 'no-store'` / `'reload'` add `pragma: no-cache` and
//     `cache-control: no-cache`, `'no-cache'` adds `cache-control: max-age=0`.
//
// Not asserted: a stream body that ends empty. undici holds the request
// head until the first chunk and so sends `content-length: 0`; oam sends
// the head at once, chunked (docs/node-divergences.md).
import net from "node:net";
const raw = net.createServer((sock) => {
  let head = "";
  sock.on("data", (c) => {
    head += c.toString("latin1");
    const end = head.indexOf("\r\n\r\n");
    if (end < 0) return;
    const lines = head.slice(0, end).split("\r\n");
    const shown = lines
      .slice(1)
      .map((l) => l.toLowerCase())
      .filter((l) => !l.startsWith("host:") && !l.startsWith("user-agent:"))
      .sort();
    const body = JSON.stringify([lines[0], ...shown]);
    sock.end(`HTTP/1.1 200 OK\r\ncontent-length: ${body.length}\r\nconnection: close\r\n\r\n${body}`);
  });
});
await new Promise((r) => raw.listen(0, "127.0.0.1", r));
const U = `http://127.0.0.1:${raw.address().port}/echo`;
const cases = {
  "QUERY no body": { method: "QUERY" },
  "PROPFIND no body": { method: "PROPFIND" },
  "PROPPATCH no body": { method: "PROPPATCH" },
  "patch lower no body": { method: "patch" },
  "post lower no body": { method: "post" },
  GET: {},
  "POST no body": { method: "POST" },
  "POST ''": { method: "POST", body: "" },
  "POST 'x'": { method: "POST", body: "x" },
  "POST empty bytes": { method: "POST", body: new Uint8Array(0) },
  "POST empty blob": { method: "POST", body: new Blob([]) },
  "PUT no body": { method: "PUT" },
  "PATCH no body": { method: "PATCH" },
  "PATCH ''": { method: "PATCH", body: "" },
  "DELETE no body": { method: "DELETE" },
  "DELETE ''": { method: "DELETE", body: "" },
  "OPTIONS no body": { method: "OPTIONS" },
  "custom no body": { method: "FOO" },
  "caller connection close": { headers: { connection: "close" } },
  "caller connection KEEP-ALIVE": { headers: { connection: "KEEP-ALIVE" } },
  "caller accept-language": { headers: { "accept-language": "en" } },
  "caller sec-fetch-mode": { headers: { "sec-fetch-mode": "navigate" } },
  "caller accept-encoding": { headers: { "accept-encoding": "identity" } },
  "caller content-length 0": { method: "POST", headers: { "content-length": "0" } },
  "mode same-origin": { mode: "same-origin" },
  "mode no-cors": { mode: "no-cors" },
  "cache no-store": { cache: "no-store" },
  "cache reload": { cache: "reload" },
  "cache no-cache": { cache: "no-cache" },
  "cache no-cache, caller cache-control": { cache: "no-cache", headers: { "cache-control": "x" } },
  "cache no-store, caller pragma": { cache: "no-store", headers: { pragma: "p" } },
};
for (const [label, init] of Object.entries(cases)) {
  let out;
  try {
    out = await (await fetch(U, init)).text();
  } catch (e) {
    out = `${e.name}: ${e.message} | ${e.cause?.message}`;
  }
  console.log(label.padEnd(36), out);
}
raw.close();
