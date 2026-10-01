// `Headers` iterates sorted by name (#175), as the Fetch Standard's "sort and
// combine" says and node does. Measured on node v22.22.2 first. oam iterated
// its stored list as it was: the order a server sent the headers, or the
// order a script set them.
//
//   - entries(), keys(), values(), forEach() and for...of all sort; each
//     set-cookie line stays its own entry, in the order it came.
//   - The iterator is live: every step reads the sorted list as it is now,
//     so a name added or removed mid-walk shows (or stops showing).
//   - What is NOT sorted: the order a fetch writes request headers in and
//     the order an http.request response's rawHeaders arrive in. Both stay
//     the order they came in, here as in node.
import http from "node:http";
import net from "node:net";

// A server that answers with a fixed, unsorted head, repeated set-cookie
// included, and echoes the request's header names in order.
const raw = net.createServer((sock) => {
  let head = "";
  sock.on("data", (chunk) => {
    head += chunk.toString("latin1");
    if (!head.includes("\r\n\r\n")) return;
    const names = head
      .split("\r\n")
      .slice(1)
      .filter((line) => line.startsWith("x-"))
      .map((line) => line.slice(0, line.indexOf(":")));
    const body = JSON.stringify(names);
    sock.end(
      "HTTP/1.1 200 OK\r\nx-ows: 1\r\nX-B: 2\r\nx-a: 3\r\nset-cookie: b=2\r\nx-empty: \r\n" +
        `Set-Cookie: a=1\r\nx-a: 4\r\ncontent-length: ${body.length}\r\nconnection: close\r\n\r\n${body}`,
    );
  });
});
await new Promise((resolve) => raw.listen(0, "127.0.0.1", resolve));
const U = `http://127.0.0.1:${raw.address().port}/`;

const res = await fetch(U, { headers: { "x-z": "1", "x-c": "2", "x-m": "3" } });
console.log("request header order on the wire", await res.text());
console.log("entries", JSON.stringify([...res.headers]));
console.log("keys", JSON.stringify([...res.headers.keys()]));
console.log("values", JSON.stringify([...res.headers.values()]));
const walked = [];
res.headers.forEach((value, key, target) => walked.push([key, value, target === res.headers]));
console.log("forEach", JSON.stringify(walked));
console.log("getSetCookie", JSON.stringify(res.headers.getSetCookie()));
console.log("fromEntries", JSON.stringify(Object.fromEntries(res.headers)));

const h = new Headers([
  ["x-b", "1"],
  ["x-a", "2"],
  ["content-type", "t"],
  ["set-cookie", "z=1"],
  ["X-A", "3"],
  ["set-cookie", "y=2"],
  ["a", "0"],
]);
console.log("constructed", JSON.stringify([...h]));
const it = h.keys();
console.log("first step", it.next().value);
h.append("aa", "x");
h.delete("x-b");
console.log("rest after a change", JSON.stringify([...it]));
const later = h.entries();
h.set("0", "zero");
console.log("made before set()", JSON.stringify([...later]));
const seen = [];
h.forEach((value, key) => {
  seen.push(key);
  if (key === "a") h.append("ab", "1");
});
console.log("forEach that appends", JSON.stringify(seen));
console.log("copy", JSON.stringify([...new Headers(h)]));
console.log("record", JSON.stringify([...new Headers({ z: "1", b: "2", B: "3" })]));
console.log("Response", JSON.stringify([...new Response("x", { headers: { z: "1", a: "2" } }).headers]));
console.log("Request", JSON.stringify([...new Request(U, { headers: { z: "1", a: "2" } }).headers]));
console.log("iterator tag", Object.prototype.toString.call(h.entries()));
console.log("iterator is iterable", h.keys()[Symbol.iterator]() !== undefined);

// http.request's response keeps the order the server sent.
const viaHttp = await new Promise((resolve, reject) => http.get(U, resolve).on("error", reject));
const names = [];
for (let i = 0; i < viaHttp.rawHeaders.length; i += 2) names.push(viaHttp.rawHeaders[i].toLowerCase());
console.log("http.get rawHeaders names", JSON.stringify([...new Set(names)]));
viaHttp.resume();
raw.close();
