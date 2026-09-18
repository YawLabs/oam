// http.request / https.request argument and option handling (#143). Node's
// three documented signatures are `(options[, cb])`, `(url[, cb])` and
// `(url, options[, cb])`; oam threw `The "listener" argument must be a
// function` on the three-argument form, which is the one `http.request(url,
// {method}, cb)` every wrapper writes.
//
// The `auth` option, and a URL's userinfo -- which IS that option, through
// node's urlToHttpOptions -- become `Authorization: Basic base64(auth)` over
// the string's UTF-8 bytes. oam dropped both, so every caller of the
// documented option talked to the server unauthenticated.
//
// `path` and `port` are validated before anything dials, with node's exact
// messages, because oam builds its connect target by concatenating them into
// a URL: an `@` in either used to move the request to ANOTHER ORIGIN and hand
// that origin `Authorization: Basic base64(<intended host:port>)`. The two
// guards are node's own (INVALID_PATH_REGEX / validatePort), so a request
// node sends is unaffected; the last block proves nothing reaches the second
// server on either runtime.
//
// Not asserted here, because the runtimes genuinely differ (see
// docs/node-divergences.md entry 38): a `path` node sends verbatim that is
// not a legal URL path (no leading slash, a raw non-ASCII byte), and the
// resolver error code for a `host` the URL parser cannot hold.
import http from "node:http";

const seen = [];
const main = http.createServer((req, res) => {
  seen.push(req.url);
  res.end(
    JSON.stringify({
      url: req.url,
      method: req.method,
      auth: req.headers.authorization ?? null,
    }),
  );
});
await new Promise((resolve) => main.listen(0, "127.0.0.1", resolve));
const PORT = main.address().port;

// A second origin no case may ever reach.
const reached = [];
const other = http.createServer((req, res) => {
  reached.push(req.url);
  res.end("OTHER");
});
await new Promise((resolve) => other.listen(0, "127.0.0.1", resolve));
const OTHER = other.address().port;

// Ephemeral ports are not output: redact both everywhere they can surface
// (a request target a case deliberately aims at the other origin, and the
// port error's `Received type string (...)`).
const P = (text) =>
  String(text).replaceAll(String(PORT), "PORT").replaceAll(String(OTHER), "OTHER");

function send(label, make) {
  return new Promise((resolve) => {
    let settled = false;
    const done = (text) => {
      if (settled) return;
      settled = true;
      console.log(P(`${label.padEnd(22)} ${text}`));
      resolve();
    };
    let req;
    try {
      req = make((res) => {
        let body = "";
        res.setEncoding("utf8");
        res.on("data", (c) => (body += c));
        res.on("end", () => done(`ok ${body}`));
      });
    } catch (e) {
      return done(`throw ${e.constructor.name} ${e.code} :: ${e.message}`);
    }
    req.on("error", (e) => done(`error ${e.code ?? e.constructor.name}`));
    req.end();
  });
}

// ---- the three signatures ------------------------------------------------
await send("url,options,cb", (cb) => http.request(`http://127.0.0.1:${PORT}/a`, { method: "POST" }, cb));
await send("url,cb", (cb) => http.request(`http://127.0.0.1:${PORT}/b`, cb));
await send("options,cb", (cb) => http.request({ hostname: "127.0.0.1", port: PORT, path: "/c", method: "PUT" }, cb));
await send("URL,options,cb", (cb) => http.request(new URL(`http://127.0.0.1:${PORT}/d`), { method: "DELETE" }, cb));
// The second object is merged OVER the url's options, so its `path` wins.
await send("options override path", (cb) => http.request(`http://127.0.0.1:${PORT}/ignored`, { path: "/e" }, cb));
await send("get url,options,cb", (cb) => http.get(`http://127.0.0.1:${PORT}/f`, { headers: { "x-t": "1" } }, cb));

// ---- auth ----------------------------------------------------------------
await send("auth option", (cb) => http.request({ hostname: "127.0.0.1", port: PORT, path: "/g", auth: "u:p" }, cb));
await send("auth utf8", (cb) => http.request({ hostname: "127.0.0.1", port: PORT, path: "/h", auth: "café:p" }, cb));
await send("auth no colon", (cb) => http.request({ hostname: "127.0.0.1", port: PORT, path: "/i", auth: "onlyu" }, cb));
await send("auth empty", (cb) => http.request({ hostname: "127.0.0.1", port: PORT, path: "/j", auth: "" }, cb));
await send("auth loses to header", (cb) =>
  http.request({ hostname: "127.0.0.1", port: PORT, path: "/k", auth: "u:p", headers: { Authorization: "Bearer T" } }, cb));
await send("url userinfo", (cb) => http.request(`http://u:p@127.0.0.1:${PORT}/l`, cb));
await send("url userinfo pct", (cb) => http.request(`http://u:p%40w@127.0.0.1:${PORT}/m`, cb));
await send("url user only", (cb) => http.request(`http://onlyu@127.0.0.1:${PORT}/n`, cb));
await send("options auth wins", (cb) => http.request(`http://u:p@127.0.0.1:${PORT}/o`, { auth: "x:y" }, cb));

// ---- path and port validation, and the connect target --------------------
const refuse = (label, options) => {
  try {
    http.request(options);
    console.log(P(`${label.padEnd(22)} NO THROW`));
  } catch (e) {
    console.log(P(`${label.padEnd(22)} ${e.constructor.name} ${e.code} :: ${e.message}`));
  }
};
refuse("path space", { hostname: "h", path: "/a b" });
refuse("path tab", { hostname: "h", path: "/a\tb" });
refuse("path crlf", { hostname: "h", path: "/a\r\nX-Injected: 1\r\n" });
refuse("path beyond 0xff", { hostname: "h", path: "/cafĀ" });
refuse("port 65536", { hostname: "h", port: 65536 });
refuse("port -1", { hostname: "h", port: -1 });
refuse("port 1.5", { hostname: "h", port: 1.5 });
refuse("port 'abc'", { hostname: "h", port: "abc" });
refuse("port ' '", { hostname: "h", port: " " });
refuse("port boolean", { hostname: "h", port: true });
refuse("port object", { hostname: "h", port: {} });
refuse("host number", { host: 0 });
refuse("hostname number", { hostname: 0, host: "h" });
// path is validated BEFORE port: with both bad, the path error wins.
refuse("path beats port", { hostname: "h", port: "abc", path: "/a b" });
// The taint sink: an `@` in `port` is a port error, never another origin.
refuse("port carries authority", { hostname: "127.0.0.1", port: `${PORT}@127.0.0.1:${OTHER}` });

// A path node DOES send, and that a URL can hold: it stays a path, and the
// request goes to the host `hostname` names.
await send("path with //host", (cb) =>
  http.request({ hostname: "127.0.0.1", port: PORT, path: `//127.0.0.1:${OTHER}/pwned` }, cb));
await send("path percent-escaped", (cb) => http.request({ hostname: "127.0.0.1", port: PORT, path: "/%20" }, cb));

console.log("other origin reached:", P(JSON.stringify(reached)));
console.log("main origin saw:", seen.length, "requests");

main.close();
other.close();
