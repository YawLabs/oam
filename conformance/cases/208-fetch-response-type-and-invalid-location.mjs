// Two pieces of fetch's redirect handling (#149), measured on node v22.22.2:
//
//   - `response.type` is "basic" for every response fetch() returns, under
//     each `redirect` mode -- the 3xx that "manual" hands back included -- and
//     "default" for a constructed Response. oam had no `type` at all.
//   - A followed redirect whose Location does not parse fails with the error
//     `new URL(location, currentURL)` throws as the cause: a TypeError with
//     `code` ERR_INVALID_URL, `input` (the Location, read as UTF-8) and
//     `base` (the URL that answered, fragment included), own names in that
//     order. oam's cause was a plain `Error('Invalid URL')`. "manual" returns
//     that 3xx and "error" fails on its status, without parsing anything.
//   - A fetch() URL that does not parse carries `input` on its cause too, and
//     no `base`.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const LOCATIONS = {
  "/bracket": "http://[zz/",
  "/space": "//exa mple.test/",
  "/port": "http://a.test:99999/",
};
const server = http.createServer((req, res) => {
  const path = req.url.split("?")[0];
  if (path === "/r") res.writeHead(302, { location: "/final" });
  else if (path === "/hop") res.writeHead(307, { location: "/bracket?via=hop" });
  else if (LOCATIONS[path]) res.writeHead(302, { location: LOCATIONS[path] });
  res.end(`body of ${path}`);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;
const P = (s) => String(s).replaceAll(base, "BASE");

for (const redirect of [undefined, "follow", "manual", "error"]) {
  try {
    const res = await fetch(`${base}/r`, redirect === undefined ? {} : { redirect });
    await res.text();
    console.log(`type ${redirect}:`, res.status, JSON.stringify(res.type), res.redirected);
  } catch (e) {
    console.log(`type ${redirect}: REJECT ${e.message} | ${e.cause?.message}`);
  }
}
console.log("constructed:", JSON.stringify(new Response("x").type));
console.log("constructed 404:", JSON.stringify(new Response(null, { status: 404 }).type));
console.log("Response.json:", JSON.stringify(Response.json({ a: 1 }).type));

function describe(e) {
  const c = e.cause;
  if (!(c instanceof Error)) return `${e.name}: ${e.message} | no cause`;
  return (
    `${e.name}: ${P(e.message)} | cause ${c.constructor.name} ${JSON.stringify(c.message)} ` +
    `TypeError=${c instanceof TypeError} names=${JSON.stringify(Object.getOwnPropertyNames(c))} ` +
    `keys=${JSON.stringify(Object.keys(c))} code=${c.code} input=${JSON.stringify(c.input)} ` +
    `base=${typeof c.base} ${P(c.base)} stack=${JSON.stringify(String(c.stack).split("\n")[0])}`
  );
}

for (const path of ["/bracket", "/space", "/port", "/hop"]) {
  for (const redirect of ["follow", "manual", "error"]) {
    let line;
    try {
      const res = await fetch(`${base}${path}#frag`, { redirect });
      line = `${res.status} ${JSON.stringify(await res.text())} url=${P(res.url)}`;
    } catch (e) {
      line = describe(e);
    }
    console.log(`${path} ${redirect}: ${line}`);
  }
}

// A Location holding UTF-8 bytes is read as UTF-8, so `input` shows the
// character. Written by a raw socket: the bytes on the wire are the point.
const raw = net.createServer((socket) => {
  socket.once("data", () => {
    socket.end(
      Buffer.concat([
        Buffer.from("HTTP/1.1 302 Found\r\nlocation: http://["),
        Buffer.from("é", "utf8"),
        Buffer.from("/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"),
      ]),
    );
  });
});
await new Promise((r) => raw.listen(0, "127.0.0.1", r));
try {
  await fetch(`http://127.0.0.1:${raw.address().port}/utf8`);
  console.log("/utf8 follow: resolved");
} catch (e) {
  console.log(`/utf8 follow: ${describe(e).replaceAll(String(raw.address().port), "RAWPORT")}`);
}
raw.close();

for (const url of ["http://", "http://[zz/", "nope"]) {
  try {
    await fetch(url);
    console.log(`${JSON.stringify(url)}: resolved`);
  } catch (e) {
    console.log(`${JSON.stringify(url)}: ${describe(e)}`);
  }
}
server.close();
