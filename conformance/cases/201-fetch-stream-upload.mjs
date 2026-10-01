// A `fetch` whose body is a stream, and the readers of a fetched response
// (#154). Measured on node v22.22.2 first.
//
//   - Each chunk goes out as node sends it: a string as UTF-8, any typed
//     array or DataView as its bytes in memory; an ArrayBuffer chunk, a
//     number, an object or null fails the fetch with node's
//     ERR_INVALID_ARG_TYPE as the cause.
//   - A source that errors fails the fetch with that very error as the cause.
//   - A streamed body cannot be sent twice, so every redirect but a 303 fails
//     the fetch (cause: an Error with no message) -- before the next hop's
//     scheme-independent checks, and even for the 301/302 that would turn a
//     POST into a body-less GET. A 303 drops the body and follows.
//   - A fetched response's `blob()` is typed with its content-type parsed and
//     re-serialised (`A/B; x=1` reads `a/b;x=1`, junk reads ""), and its body
//     is "used" once read from, not merely when locked.
//
// Not asserted: what happens to the source when the fetch is aborted or the
// connection drops (docs/node-divergences.md: node keeps pulling it, oam
// cancels it).
import http from "node:http";

const seen = [];
const types = [
  "A/B; x=1",
  'text/html; charset="utf-8"',
  "bad",
  'a/b;x="q\\"z";Y=2;x=3',
  "a/b; ;=;k=",
  'a/b;k="a b"',
  " a/b ; k = v",
  'a/b;k="unterminated\\',
];
const srv = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const body = Buffer.concat(chunks);
    seen.push(`${req.method} ${req.url} ${JSON.stringify(body.toString())}`);
    const redirect = /^\/r(\d+)(.*)$/.exec(req.url);
    if (redirect) {
      res.writeHead(Number(redirect[1]), { location: redirect[2] || "/done" });
      res.end();
    } else if (req.url.startsWith("/type/")) {
      res.setHeader("content-type", types[Number(req.url.slice(6))]);
      res.end("hi");
    } else {
      res.end(body.toString("hex"));
    }
  });
});
await new Promise((resolve) => srv.listen(0, "127.0.0.1", resolve));
const B = `http://127.0.0.1:${srv.address().port}`;
const utf8 = (text) => new TextEncoder().encode(text);
const once = (chunk) =>
  new ReadableStream({
    start(c) {
      c.enqueue(chunk);
      c.close();
    },
  });
const describe = (e) => {
  const c = e.cause;
  return (
    `${e.name}: ${e.message}` +
    (c ? ` | cause ${c.constructor.name} ${c.code ?? "-"} ${JSON.stringify(c.message)}` : "")
  );
};

// Each kind of chunk.
const chunks = {
  string: "hé",
  Uint8Array: new Uint8Array([1, 2]),
  Uint16Array: new Uint16Array([0x4142, 0x0102]),
  DataView: new DataView(new Uint8Array([5, 6]).buffer),
  Buffer: Buffer.from([7]),
  ArrayBuffer: new Uint8Array([3, 4]).buffer,
  number: 42,
  object: { a: 1 },
  array: [65, 66],
  null: null,
};
for (const [name, chunk] of Object.entries(chunks)) {
  let out;
  try {
    const r = await fetch(`${B}/p`, { method: "POST", body: once(chunk), duplex: "half" });
    out = await r.text();
  } catch (e) {
    out = describe(e);
  }
  console.log("chunk", name.padEnd(12), out);
}

// A source that errors, before its first chunk and after it.
for (const where of ["start", "pull", "later"]) {
  let out;
  try {
    let n = 0;
    const body = new ReadableStream({
      start(c) {
        if (where === "start") c.error(new RangeError("source broke"));
      },
      pull(c) {
        if (where === "pull" || n++ > 0) c.error(new RangeError("source broke"));
        else c.enqueue(utf8("first"));
      },
    });
    out = String((await fetch(`${B}/p`, { method: "POST", body, duplex: "half" })).status);
  } catch (e) {
    out = `${describe(e)} same error: ${e.cause instanceof RangeError}`;
  }
  console.log("source error", where.padEnd(5), out);
}

// Redirects of a streamed body.
for (const [status, method, location] of [
  [301, "POST", ""],
  [302, "POST", ""],
  [303, "POST", ""],
  [303, "PUT", ""],
  [307, "POST", ""],
  [308, "PUT", ""],
  [301, "PUT", ""],
  [302, "PATCH", ""],
  [307, "POST", "http://127.0.0.1:25/x"],
  [307, "POST", "ftp://x/"],
  [307, "POST", "/r307"],
]) {
  seen.length = 0;
  let out;
  try {
    const r = await fetch(`${B}/r${status}${location}`, {
      method,
      body: once(utf8("up")),
      duplex: "half",
    });
    out = `${r.status} redirected=${r.redirected} ${JSON.stringify(await r.text())}`;
  } catch (e) {
    out = describe(e);
  }
  console.log("redirect", status, method, location || "-", "->", out, "| server saw", JSON.stringify(seen));
}
// A buffered body is replayed on a 307, for contrast.
{
  seen.length = 0;
  const r = await fetch(`${B}/r307`, { method: "POST", body: "again" });
  console.log("redirect 307 string ->", r.status, await r.text(), JSON.stringify(seen));
}

// A fetched response's blob() and what counts as used.
for (let i = 0; i < types.length; i++) {
  const blob = await (await fetch(`${B}/type/${i}`)).blob();
  console.log("blob type", JSON.stringify(types[i]), "->", JSON.stringify(blob.type), blob.size, await blob.text());
}
{
  const r = await fetch(`${B}/p`);
  const reader = r.body.getReader();
  const lockedOnly = r.bodyUsed;
  let message;
  try {
    await r.text();
  } catch (e) {
    message = `${e.name}: ${e.message}`;
  }
  await reader.read();
  console.log("locked, not read:", lockedOnly, "| text():", message, "| after a read:", r.bodyUsed);
  const read = await fetch(`${B}/p`);
  await read.text();
  try {
    await read.blob();
  } catch (e) {
    console.log("blob() after text():", `${e.name}: ${e.message}`, read.bodyUsed);
  }
}

srv.close();
srv.closeAllConnections();
