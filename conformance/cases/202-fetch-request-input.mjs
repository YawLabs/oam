// The Request constructor, and `fetch` with a Request as its input (#180).
// Measured on node v22.22.2 first. oam stringified fetch's first argument,
// so `fetch(new Request(url))` threw `Failed to parse URL from [object
// Request]`, and its Request kept only url, method, headers and a body.
//
// node's fetch begins with `new Request(input, init)`: a Request input is
// unwrapped field by field with init winning, its body is sent and the
// Request reads as used (unless init brings a body), its signal and redirect
// mode apply, and every constructor check -- a GET with a body, a bad method,
// an enum value outside its list -- applies to a plain URL too, ahead of an
// aborted signal.
//
// Not asserted: the prototype's full key list (node's has a stray
// `attribute` and a `formData` oam lacks), the class name in a setter's
// TypeError, and the stderr warning for `method: 'patch'`.
import http from "node:http";

const U = "http://a.test/p";
const t = (label, f) => {
  try {
    console.log(label, JSON.stringify(f()));
  } catch (e) {
    console.log(label, "THROWS", e.constructor.name, e.message, e.cause ? `| cause ${e.cause.message}` : "");
  }
};
const props = (r) => ({
  url: r.url, method: r.method, redirect: r.redirect, duplex: r.duplex, mode: r.mode,
  credentials: r.credentials, cache: r.cache, destination: r.destination, integrity: r.integrity,
  keepalive: r.keepalive, referrer: r.referrer, referrerPolicy: r.referrerPolicy,
  isHistoryNavigation: r.isHistoryNavigation, isReloadNavigation: r.isReloadNavigation,
  signal: Object.prototype.toString.call(r.signal), aborted: r.signal.aborted, bodyUsed: r.bodyUsed,
  body: r.body === null ? null : "stream",
});
t("defaults", () => props(new Request(U)));
t("own keys", () => Object.keys(new Request(U)));
t("url normalised", () => new Request("HTTP://A.test:80/a/../b?x#f").url);
t("relative url", () => new Request("/p").url);
t("URL object", () => new Request(new URL(U)).url);
t("bad url", () => new Request("nope").url);
t("credentials url", () => new Request("http://u:p@a.test/").url);
for (const m of ["get", "post", "patch", "Delete", "options", "head", "put", "custom", "CONNECT", "trace", "track", "bad method", ""]) {
  t(`method ${JSON.stringify(m)}`, () => new Request(U, { method: m }).method);
}
t("GET body", () => new Request(U, { body: "x" }));
t("HEAD body", () => new Request(U, { method: "HEAD", body: "x" }));
t("GET body null", () => new Request(U, { body: null }).method);
t("GET body undefined", () => new Request(U, { body: undefined }).method);
t("redirect manual", () => new Request(U, { redirect: "manual" }).redirect);
t("redirect bad", () => new Request(U, { redirect: "nope" }).redirect);
t("duplex bad", () => new Request(U, { method: "POST", body: "x", duplex: "full" }).duplex);
t("duplex half string", () => new Request(U, { method: "POST", body: "x", duplex: "half" }).duplex);
t("mode navigate", () => new Request(U, { mode: "navigate" }).mode);
t("mode no-cors", () => new Request(U, { mode: "no-cors" }).mode);
t("credentials omit", () => new Request(U, { credentials: "omit" }).credentials);
t("keepalive", () => new Request(U, { keepalive: true }).keepalive);
t("signal bad", () => new Request(U, { signal: {} }).signal);
t("signal null", () => new Request(U, { signal: null }).signal.aborted);
{
  const ac = new AbortController();
  const r = new Request(U, { signal: ac.signal });
  const same = r.signal === ac.signal;
  ac.abort(new Error("why"));
  console.log("follow signal", same, r.signal.aborted, r.signal.reason?.message);
  const ac2 = new AbortController();
  ac2.abort("early");
  console.log("aborted signal", new Request(U, { signal: ac2.signal }).signal.reason);
  const r2 = new Request(r);
  console.log("from request: signal", r2.signal === r.signal, r2.signal.aborted, r2.redirect);
  const r3 = new Request(new Request(U, { redirect: "manual", method: "POST", headers: { a: "1" } }), { headers: { b: "2" } });
  console.log("from request with init", r3.redirect, r3.method, JSON.stringify([...r3.headers]));
  t("from POST request, init GET", () => new Request(new Request(U, { method: "POST", body: "b" }), { method: "GET" }).method);
}
t("headers is Headers", () => new Request(U, { headers: { A: "1" } }).headers instanceof Headers);
t("headers identity", () => { const r = new Request(U); return r.headers === r.headers; });
t("toString", () => Object.prototype.toString.call(new Request(U)));
t("no args", () => new Request());
t("init not object", () => new Request(U, 5).method);
t("clone props", () => props(new Request(U, { redirect: "error", method: "POST", body: "x" }).clone()));
t("cache only-if-cached", () => new Request(U, { cache: "only-if-cached" }).cache);
t("cache only-if-cached same-origin", () => new Request(U, { cache: "only-if-cached", mode: "same-origin" }).cache);
t("cache bad", () => new Request(U, { cache: "x" }).cache);
t("credentials bad", () => new Request(U, { credentials: "x" }).credentials);
t("mode bad", () => new Request(U, { mode: "x" }).mode);
t("referrerPolicy bad", () => new Request(U, { referrerPolicy: "x" }).referrerPolicy);
t("referrerPolicy ok", () => new Request(U, { referrerPolicy: "origin" }).referrerPolicy);
t("referrer empty", () => new Request(U, { referrer: "" }).referrer);
t("referrer url", () => new Request(U, { referrer: "http://b.test/x" }).referrer);
t("referrer about:client", () => new Request(U, { referrer: "about:client" }).referrer);
t("referrer bad", () => new Request(U, { referrer: "::" }).referrer);
t("integrity", () => new Request(U, { integrity: 5 }).integrity);
t("no-cors POST", () => new Request(U, { mode: "no-cors", method: "POST" }).method);
t("no-cors PUT", () => new Request(U, { mode: "no-cors", method: "PUT" }).method);
t("window 1", () => new Request(U, { window: 1 }).url);
t("window null", () => new Request(U, { window: null }).url);
t("keepalive stream", () => new Request(U, { method: "POST", keepalive: true, duplex: "half", body: new ReadableStream() }).keepalive);
t("bad redirect and bad url", () => new Request("nope", { redirect: "x" }));
t("bad method and bad redirect", () => new Request(U, { method: "bad method", redirect: "x" }));
t("bad mode and bad redirect", () => new Request(U, { mode: "x", redirect: "y" }));
t("headers after init", () => [...new Request(new Request(U, { headers: { a: "1" } }), { method: "POST" }).headers]);
t("headers empty init", () => [...new Request(new Request(U, { headers: { a: "1" } }), {}).headers]);
t("method from request with redirect init", () => new Request(new Request(U, { method: "PUT" }), { redirect: "manual" }).method);
t("navigate kept?", () => new Request(new Request(U, { mode: "same-origin" })).mode);
t("init array", () => new Request(U, []).method);
t("init function", () => new Request(U, () => {}).method);
t("init string", () => new Request(U, "x").method);
t("input number", () => new Request(5).url);
t("input object toString", () => new Request({ toString: () => U }).url);
t("method number", () => new Request(U, { method: 5 }).method);
t("method null", () => new Request(U, { method: null }).method);
t("duplex without body", () => new Request(U, { duplex: "half" }).duplex);
t("Request.length", () => Request.length);
t("signal getter type", () => typeof Object.getOwnPropertyDescriptor(Request.prototype, "signal").get);
t("url desc", () => { const d = Object.getOwnPropertyDescriptor(Request.prototype, "url"); return [typeof d.get, d.set, d.enumerable, d.configurable]; });
t("clone desc", () => { const d = Object.getOwnPropertyDescriptor(Request.prototype, "clone"); return [typeof d.value, d.writable, d.enumerable, d.configurable]; });
t("used clone", () => { const r = new Request(U, { method: "POST", body: "x" }); r.body.getReader().read(); return r.clone(); });
{
  const r = new Request(U, { method: "POST", body: "x" });
  await r.text();
  try { r.clone(); } catch (e) { console.log("used clone2", e.constructor.name, e.message); }
}
t("prototype members", () =>
  ["url", "method", "headers", "signal", "redirect", "duplex", "bodyUsed", "body", "clone", "mode"].map((k) => {
    const d = Object.getOwnPropertyDescriptor(Request.prototype, k);
    return `${k}:${typeof d.get === "function" ? "get" : typeof d.value}:${d.enumerable}`;
  }));

const seen = [];
const srv = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const h = req.headers;
    seen.push(`${req.method} ${req.url} x-a=${h["x-a"] ?? "-"} x-b=${h["x-b"] ?? "-"} ct=${h["content-type"] ?? "-"} te=${h["transfer-encoding"] ?? "-"} body=${JSON.stringify(Buffer.concat(chunks).toString())}`);
    if (req.url === "/redir") {
      res.writeHead(302, { location: "/final" });
      res.end();
      return;
    }
    if (req.url === "/slow") {
      setTimeout(() => res.end("slow"), 500);
      return;
    }
    res.end("ok");
  });
});
await new Promise((r) => srv.listen(0, "127.0.0.1", r));
const B = `http://127.0.0.1:${srv.address().port}`;
const enc = (s) => new TextEncoder().encode(s);
const desc = (e) => `${e.name}: ${e.message}${e.cause ? ` | cause ${e.cause.name}: ${e.cause.message}` : ""}`;
async function go(label, make) {
  seen.length = 0;
  let out;
  try {
    const { input, init, after } = make();
    const r = await fetch(input, init);
    out = `${r.status} ${r.redirected} ${JSON.stringify(await r.text())}${after ? " " + after() : ""}`;
  } catch (e) {
    out = desc(e);
  }
  console.log(label.padEnd(34), out, "| server", JSON.stringify(seen));
}
await go("GET Request", () => ({ input: new Request(`${B}/get`) }));
await go("POST Request body+header", () => {
  const input = new Request(`${B}/post`, { method: "POST", headers: { "x-a": "1" }, body: "hello" });
  return { input, after: () => `used=${input.bodyUsed}` };
});
await go("init overrides body", () => {
  const input = new Request(`${B}/post2`, { method: "POST", body: "once" });
  return { input, init: { body: "override" }, after: () => `used=${input.bodyUsed}` };
});
await go("init overrides headers", () => {
  const input = new Request(`${B}/h`, { headers: { "x-a": "1" } });
  return { input, init: { headers: { "x-b": "2" } } };
});
await go("init overrides method", () => ({ input: new Request(`${B}/m`, { method: "PUT" }), init: { method: "DELETE" } }));
await go("init GET on a POST Request", () => ({ input: new Request(`${B}/m`, { method: "POST", body: "b" }), init: { method: "GET" } }));
await go("stream body Request", () => {
  const body = new ReadableStream({ start(c) { c.enqueue(enc("str")); c.enqueue(enc("eam")); c.close(); } });
  const input = new Request(`${B}/stream`, { method: "POST", body, duplex: "half" });
  return { input, after: () => `used=${input.bodyUsed}` };
});
await go("Blob body Request", () => ({ input: new Request(`${B}/blob`, { method: "POST", body: new Blob(["bb"], { type: "x/y" }) }) }));
{
  const used = new Request(`${B}/again`, { method: "POST", body: "once" });
  await go("first use", () => ({ input: used }));
  await go("reuse consumed Request", () => ({ input: used }));
  await go("reuse consumed, init body", () => ({ input: used, init: { body: "new" } }));
  const read = new Request(`${B}/read`, { method: "POST", body: "x" });
  await read.text();
  await go("Request read by text()", () => ({ input: read }));
}
{
  const plain = new Request(`${B}/plain`);
  await go("bodyless reuse 1", () => ({ input: plain }));
  await go("bodyless reuse 2", () => ({ input: plain }));
}
await go("redirect manual Request", () => ({ input: new Request(`${B}/redir`, { redirect: "manual" }) }));
await go("redirect error Request", () => ({ input: new Request(`${B}/redir`, { redirect: "error" }) }));
await go("redirect Request, init follow", () => ({ input: new Request(`${B}/redir`, { redirect: "manual" }), init: { redirect: "follow" } }));
{
  const ac = new AbortController();
  ac.abort(new Error("pre"));
  await go("aborted signal Request", () => ({ input: new Request(`${B}/a`, { signal: ac.signal }) }));
  const ac2 = new AbortController();
  setTimeout(() => ac2.abort(new Error("mid")), 100);
  await go("signal Request aborted later", () => ({ input: new Request(`${B}/slow`, { signal: ac2.signal }) }));
  const ac3 = new AbortController();
  ac3.abort(new Error("init"));
  await go("init signal over Request", () => ({ input: new Request(`${B}/b`), init: { signal: ac3.signal } }));
  const ac4 = new AbortController();
  ac4.abort(new Error("req"));
  await go("init signal null over aborted", () => ({ input: new Request(`${B}/c`, { signal: ac4.signal }), init: { signal: null } }));
}
await go("aborted signal + bad url", () => {
  const ac = new AbortController();
  ac.abort(new Error("x"));
  return { input: "nope", init: { signal: ac.signal } };
});
await go("fetch GET with body", () => ({ input: `${B}/g`, init: { body: "x" } }));
await go("fetch HEAD with body", () => ({ input: `${B}/g`, init: { method: "HEAD", body: "x" } }));
await go("fetch CONNECT", () => ({ input: `${B}/g`, init: { method: "CONNECT" } }));
await go("fetch bad method", () => ({ input: `${B}/g`, init: { method: "a b" } }));
await go("fetch mode navigate", () => ({ input: `${B}/g`, init: { mode: "navigate" } }));
await go("fetch stream no duplex", () => ({ input: `${B}/g`, init: { method: "POST", body: new ReadableStream() } }));
await go("fetch init 5", () => ({ input: `${B}/g`, init: 5 }));
try {
  await fetch();
} catch (e) {
  console.log("fetch()", desc(e));
}
srv.close();
srv.closeAllConnections();
