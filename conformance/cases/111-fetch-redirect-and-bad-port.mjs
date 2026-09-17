// fetch() follows redirects by undici's rules and refuses the Fetch spec's bad
// ports (#143). Node's fetch is undici 6.24.1: at most 20 redirects (the 21st
// response fails the fetch with "redirect count exceeded"), no Referer, a
// POST turned into a body-less GET by 301/302 and anything but GET/HEAD by
// 303 (a 303 GET keeps its content-type), a 307/308 that keeps method and
// body, and authorization / cookie / proxy-authorization dropped for good once
// a hop leaves the origin -- a later hop back to it does not get them back.
// A Location that does not parse, is not http(s), carries credentials or
// names a bad port fails the fetch; so does a bad port in the fetched URL
// itself, before anything dials. http.request has no bad-port block: port 1
// is dialled and refused.
//
// oam used to follow reqwest's rules: ten redirects, a Referer on every hop,
// content-type dropped on a 303, credentials back on a same-origin hop after a
// cross-origin one, and port 1 dialled.
//
// Not printed: user-agent / accept-encoding / accept-language /
// sec-fetch-mode / connection (oam's defaults are its own), statusText (oam
// reports the canonical reason), and the class of an "Invalid URL" cause
// (node: TypeError ERR_INVALID_URL; oam: Error -- a documented divergence).
import http from "node:http";

function listen(handler) {
  const server = http.createServer(handler);
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

const seen = [];
const hopView = (req, body) => ({
  method: req.method,
  path: req.url,
  authorization: req.headers.authorization,
  cookie: req.headers.cookie,
  "proxy-authorization": req.headers["proxy-authorization"],
  referer: req.headers.referer,
  "content-type": req.headers["content-type"],
  "content-length": req.headers["content-length"],
  "transfer-encoding": req.headers["transfer-encoding"],
  body,
});

function collect(req) {
  return new Promise((resolve) => {
    let body = "";
    req.setEncoding("utf8");
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => resolve(body));
  });
}

let bPort = 0;
let loops = 0;
const a = await listen(async (req, res) => {
  const body = await collect(req);
  const url = new URL(req.url, "http://a.invalid");
  seen.push({ server: "a", hostOk: req.headers.host === `127.0.0.1:${a.address().port}`, ...hopView(req, body) });
  const to = (status, location) => {
    res.writeHead(status, { location, "content-length": 0 });
    res.end();
  };
  switch (url.pathname) {
    case "/loop":
      loops++;
      return to(302, "/loop");
    case "/cross":
      return to(302, `http://127.0.0.1:${bPort}/b-same`);
    case "/back":
      res.writeHead(200, { "content-type": "text/plain" });
      return res.end("back at a");
    case "/status":
      return to(Number(url.searchParams.get("code")), "/final");
    case "/final":
      res.writeHead(200, { "content-type": "text/plain" });
      return res.end("final");
    case "/bad":
      return to(302, url.searchParams.get("to"));
    default:
      res.writeHead(404);
      return res.end();
  }
});
const b = await listen(async (req, res) => {
  const body = await collect(req);
  seen.push({ server: "b", hostOk: req.headers.host === `127.0.0.1:${bPort}`, ...hopView(req, body) });
  if (req.url === "/b-same") {
    res.writeHead(307, { location: "/b-next", "content-length": 0 });
    return res.end();
  }
  if (req.url === "/b-next") {
    res.writeHead(302, { location: `http://127.0.0.1:${a.address().port}/back`, "content-length": 0 });
    return res.end();
  }
  res.writeHead(404);
  res.end();
});
bPort = b.address().port;
const A = `http://127.0.0.1:${a.address().port}`;
const P = (s) => String(s).replaceAll(String(a.address().port), "APORT").replaceAll(String(bPort), "BPORT");

function printSeen(label) {
  console.log(label);
  for (const hop of seen.splice(0)) console.log("  ", P(JSON.stringify(hop)));
}

async function show(label, url, init) {
  try {
    const res = await fetch(url, init);
    const text = await res.text();
    console.log(label, res.status, res.redirected, P(res.url), JSON.stringify(text));
  } catch (e) {
    const c = e.cause;
    // An "Invalid URL" cause is node's ERR_INVALID_URL (code, input, base);
    // only its message is compared.
    const keys = c?.message === "Invalid URL" ? "" : JSON.stringify(c && Object.keys(c));
    console.log(label, e.constructor.name, e.message, "cause:", P(c?.message), keys);
  }
}

// The limit: the 21st redirect response fails the fetch.
await show("loop", `${A}/loop`);
console.log("loop requests", loops);
seen.splice(0);

// Cross-origin: credentials and cookies go at the first hop to b and stay
// gone, through a 307 on b and the hop back to a. The fragment is never sent
// and never reported.
await show("cross", `${A}/cross#frag`, {
  method: "POST",
  body: "payload",
  headers: {
    authorization: "Bearer secret",
    cookie: "sid=1",
    "proxy-authorization": "Basic cHJveHk=",
    "content-type": "text/plain",
    "x-custom": "kept",
  },
});
printSeen("cross hops");

// Method and body per status, POST and GET.
for (const code of [301, 302, 303, 307, 308]) {
  for (const method of ["POST", "GET"]) {
    const init = { method, headers: { "content-type": "application/json" } };
    if (method === "POST") init.body = '{"a":1}';
    await show(`status ${code} ${method}`, `${A}/status?code=${code}`, init);
    printSeen(`status ${code} ${method} hops`);
  }
}

// A 303 to a HEAD request keeps HEAD.
await show("status 303 HEAD", `${A}/status?code=303`, { method: "HEAD" });
printSeen("status 303 HEAD hops");

// Locations that fail the fetch, and what reaches the wire first.
for (const to of ["http://[", "ftp://127.0.0.1/", `http://user:pass@127.0.0.1:${a.address().port}/final`, "http://127.0.0.1:25/"]) {
  await show(`location ${P(to)}`, `${A}/bad?to=${encodeURIComponent(to)}`);
  console.log("  requests", seen.splice(0).length);
}

// The initial URL's bad port: refused before anything dials.
try {
  await fetch("http://127.0.0.1:1/");
  console.log("fetch port 1 resolved?!");
} catch (e) {
  const c = e.cause;
  console.log("fetch port 1", e.constructor.name, e.message, c.constructor.name, c.message, JSON.stringify(Object.keys(c)),
    Object.getPrototypeOf(c) === Error.prototype);
}

// http.request dials it.
await new Promise((resolve) => {
  const req = http.get({ host: "127.0.0.1", port: 1 }, (res) => {
    console.log("http.get port 1 response?!", res.statusCode);
    res.resume();
    resolve();
  });
  req.on("error", (e) => {
    console.log("http.get port 1", e.code, e.syscall, e.address, e.port);
    resolve();
  });
});

a.close();
b.close();
