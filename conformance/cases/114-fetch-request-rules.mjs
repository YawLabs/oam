// What `fetch` does with the request before it dials (#143), and what it
// hands back on the response. Every assertion here was measured on node
// v22.22.2 + undici 6.24.1 first; oam used to differ on all of them.
//
//   - A caller `host` header is node's ONE silent drop. Left through, the
//     caller picks the authority a name-based virtual host, a cache or an
//     SSRF filter sees while the connection goes somewhere else.
//   - `transfer-encoding`, `keep-alive`, `upgrade`, `expect` and a
//     `connection` that is neither `close` nor `keep-alive` are refused
//     before anything dials (the CL.TE evasion is the
//     `connection: "close, transfer-encoding"` case). `close` and
//     `keep-alive` ARE sent, lowercased. `te` -- also hop-by-hop -- is sent
//     untouched, because node sends it.
//   - A `content-length` longer than the body is refused. oam refuses a
//     SHORT one too, where node hangs instead -- not asserted here, it is an
//     e2e test.
//   - A URL carrying credentials is refused outright; oam used to convert it
//     to `Authorization: Basic ...` and send it.
//   - A URL that does not parse is a URL error with `ERR_INVALID_URL`, not a
//     network failure; a bad scheme says so.
//   - A method is uppercased only when it is one of the six the standard
//     normalises, so `{method: 'get'}` reaches the server as GET. That
//     `{method: 'patch'}` stays LOWERCASE is not asserted here: node's own
//     http server cannot parse a lowercase method token, so the case needs a
//     raw socket (it is an e2e test).
//   - Repeated header names are combined into one comma-joined line.
//   - A string body carries `content-type: text/plain;charset=UTF-8`.
//   - `set-cookie` is never combined, and `Headers.getSetCookie()` returns
//     each line: a cookie's `Expires` attribute contains a comma, so a
//     joined `a=1, b=2` cannot be split back.
//
// Not asserted, because the runtimes genuinely differ (docs/node-divergences.md
// entry 38): the default request headers oam does not send (`connection`,
// `accept-language`, `sec-fetch-mode`), the header order, and the undici
// `code` on a refusal cause (`UND_ERR_INVALID_ARG` and friends) -- oam's cause
// carries node's `name` and `message` but is a plain `Error`.
import http from "node:http";

const srv = http.createServer((req, res) => {
  if (req.url === "/cookies") {
    res.setHeader("set-cookie", ["a=1; Expires=Wed, 21 Oct 2026 07:28:00 GMT", "b=2"]);
    res.setHeader("x-rep", "1, 2");
    res.end("ok");
    return;
  }
  let body = "";
  req.setEncoding("utf8");
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    res.end(
      JSON.stringify({
        method: req.method,
        hostIsOrigin: req.headers.host === `127.0.0.1:${PORT}`,
        connectionClose: req.headers.connection === "close",
        te: req.headers.te ?? null,
        contentType: req.headers["content-type"] ?? null,
        xd: req.headers["x-d"] ?? null,
        auth: req.headers.authorization ?? null,
        body,
      }),
    );
  });
});
await new Promise((resolve) => srv.listen(0, "127.0.0.1", resolve));
const PORT = srv.address().port;
const U = `http://127.0.0.1:${PORT}/p`;

async function one(label, url, init) {
  let line;
  try {
    const r = await fetch(url, init);
    line = "ok " + (await r.text());
  } catch (e) {
    const cause = e.cause;
    line =
      "reject " + e.constructor.name + " " + e.message +
      (cause ? ` || cause ${cause.name}: ${cause.message}` : " || no cause");
  }
  console.log(label.padEnd(24) + " " + String(line).replaceAll(String(PORT), "PORT"));
}

await one("baseline", U, {});
await one("caller host dropped", U, { headers: { host: "spoof.test" } });
await one("connection close", U, { headers: { connection: "close" } });
// The `connection` rule turns on the VALUE, case-insensitively: `close` and
// `keep-alive` are both accepted, anything else is refused. oam refused
// `keep-alive` and `CLOSE` on a first cut of this guard.
await one("connection CLOSE", U, { headers: { connection: "CLOSE" } });
await one("connection keep-alive", U, { headers: { connection: "keep-alive" } });
await one("te alone", U, { headers: { te: "trailers" } });
await one("connection close,te", U, {
  method: "POST",
  body: "AB",
  headers: { connection: "close, transfer-encoding", te: "trailers" },
});
await one("transfer-encoding", U, { method: "POST", body: "AB", headers: { "transfer-encoding": "chunked" } });
await one("keep-alive", U, { headers: { "keep-alive": "timeout=5" } });
await one("upgrade", U, { headers: { upgrade: "websocket" } });
await one("expect", U, { headers: { expect: "100-continue" } });
await one("content-length long", U, { method: "POST", body: "AB", headers: { "content-length": "9" } });
await one("url credentials", `http://u:p@127.0.0.1:${PORT}/p`, {});
await one("url unparseable", "http://", {});
await one("url bad scheme", "ftp://example.invalid/x", {});
await one("method get lower", U, { method: "get" });
await one("repeated name", U, { headers: [["x-d", "1"], ["x-d", "2"]] });
await one("string body", U, { method: "POST", body: "AB" });
await one("caller content-type", U, {
  method: "POST",
  body: "AB",
  headers: { "content-type": "application/json" },
});

// Response headers: set-cookie stays uncombined and getSetCookie() has both.
const r = await fetch(`http://127.0.0.1:${PORT}/cookies`);
await r.text();
console.log("getSetCookie", JSON.stringify(r.headers.getSetCookie()));
console.log("get set-cookie", JSON.stringify(r.headers.get("set-cookie")));
console.log("get x-rep", JSON.stringify(r.headers.get("x-rep")));
console.log(
  "iterated set-cookie",
  JSON.stringify([...r.headers].filter(([k]) => k === "set-cookie")),
);

srv.close();
