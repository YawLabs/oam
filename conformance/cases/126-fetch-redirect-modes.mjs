// fetch's `redirect` option, measured on node v22.22.2: 'follow' (the
// default) follows a 3xx, 'manual' returns the 3xx itself as the response
// (status, headers and body; `redirected` false, `url` the request's), and
// 'error' rejects with TypeError 'fetch failed' whose cause is
// `Error: unexpected redirect` -- for 301, 302, 303, 307 and 308, with or
// without a Location, and never for 300 or 304. An application that asks
// for 'manual' to vet each hop before following it relies on the target
// never being requested. oam used to follow in every mode.
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const hits = [];
const server = http.createServer((req, res) => {
  hits.push(`${req.method} ${req.url}`);
  const m = /^\/r(\d{3})(-noloc)?$/.exec(req.url);
  if (m) {
    const headers = { "content-type": "text/plain", "x-hop": "1" };
    if (!m[2]) headers.location = "/target";
    res.writeHead(Number(m[1]), headers);
    res.end(`redirect ${m[1]}`);
    return;
  }
  res.end(`target ${req.method}`);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;

async function run(label, path, init) {
  hits.length = 0;
  let line;
  try {
    const res = await fetch(base + path, init);
    const body = await res.text();
    line = `${res.status} ${res.statusText} redirected=${res.redirected} ` +
      `url=${res.url.slice(base.length)} location=${res.headers.get("location")} ` +
      `hop=${res.headers.get("x-hop")} ok=${res.ok} body=${JSON.stringify(body)}`;
  } catch (e) {
    line = `REJECT ${e.name}: ${e.message} | cause ${e.cause && `${e.cause.name}: ${e.cause.message}`}`;
  }
  console.log(`${label} ${path}: ${line} | server saw ${JSON.stringify(hits)}`);
}

for (const redirect of [undefined, "follow", "manual", "error"]) {
  for (const path of ["/r301", "/r302", "/r303", "/r307", "/r308", "/r302-noloc", "/r300", "/r304"]) {
    await run(`${redirect}`, path, redirect === undefined ? {} : { redirect });
  }
}
// A body and a non-GET method.
await run("manual POST", "/r307", { method: "POST", body: "payload", redirect: "manual" });
await run("error POST", "/r303", { method: "POST", body: "payload", redirect: "error" });
await run("follow POST", "/r303", { method: "POST", body: "payload", redirect: "follow" });
// Not a RequestRedirect value.
for (const redirect of ["MANUAL", "bogus", "", null]) {
  await run(`invalid ${JSON.stringify(redirect)}`, "/r302", { redirect });
}
server.close();
