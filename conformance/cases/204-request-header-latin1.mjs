// Request header values outside ASCII (#174), measured on node v22.22.2
// first: what reaches the wire through fetch, http.get and setHeader(), and
// what each refuses. oam wrote a value's UTF-8 (`café` went out as
// `caf\xc3\xa9`) and sent a code point node refuses.
//
//   - The wire carries one byte per code point: U+00E9 is the byte 0xE9.
//   - Headers (and so fetch) take names and values as ByteStrings: a code
//     unit above 0xFF -- U+20AC, either half of a surrogate pair -- is a
//     TypeError naming its index and value. A value is stripped of leading
//     and trailing whitespace, and NUL, CR or LF in it is refused; another
//     control character passes Headers and fails the fetch as undici's
//     `invalid <name> header`.
//   - http.request refuses every such value from its constructor and from
//     setHeader() with ERR_INVALID_CHAR, as node's checkInvalidHeaderChar.
import http from "node:http";
import net from "node:net";

// Echoes the request's x-v lines, each byte above 0x7E or below 0x20 as \xNN.
const raw = net.createServer((sock) => {
  let head = Buffer.alloc(0);
  sock.on("data", (chunk) => {
    head = Buffer.concat([head, chunk]);
    const end = head.indexOf("\r\n\r\n");
    if (end < 0) return;
    const lines = head
      .subarray(0, end)
      .toString("latin1")
      .split("\r\n")
      .filter((line) => /^x-v:/i.test(line))
      .map((line) =>
        [...Buffer.from(line, "latin1")]
          .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : `\\x${b.toString(16).padStart(2, "0")}`))
          .join(""),
      );
    const body = JSON.stringify(lines);
    sock.end(`HTTP/1.1 200 OK\r\ncontent-length: ${body.length}\r\nconnection: close\r\n\r\n${body}`);
  });
});
await new Promise((resolve) => raw.listen(0, "127.0.0.1", resolve));
const port = raw.address().port;
const U = `http://127.0.0.1:${port}/`;

const describe = (e) =>
  JSON.stringify(
    `${e.constructor.name}${e.code ? `[${e.code}]` : ""}: ${e.message}` +
      (e.cause ? ` | cause ${e.cause.message}` : ""),
  );
const read = (res) =>
  new Promise((resolve) => {
    let text = "";
    res.on("data", (c) => (text += c));
    res.on("end", () => resolve(text));
  });

const values = {
  ascii: "plain",
  e9: "caf\u00e9",
  ff: "\u00ff\u0080",
  euro: "\u20ac",
  mixed: "a\u00e9\u20ac",
  pair: "\ud83d\ude00",
  lone: "\ud800",
  tab: "a\tb",
  ctl: "a\u0001b",
  del: "a\u007fb",
  nul: "a\u0000b",
  spaces: " x ",
  cr: "a\rb",
  lf: "a\nb",
};
for (const [name, value] of Object.entries(values)) {
  const row = [name.padEnd(7)];
  try {
    row.push("fetch " + (await (await fetch(U, { headers: { "x-v": value } })).text()));
  } catch (e) {
    row.push("fetch " + describe(e));
  }
  try {
    const h = new Headers();
    h.set("x-v", value);
    row.push("Headers.set " + JSON.stringify(h.get("x-v")));
  } catch (e) {
    row.push("Headers.set " + describe(e));
  }
  try {
    row.push(
      "http.get " +
        (await new Promise((resolve, reject) => {
          http.get({ port, host: "127.0.0.1", headers: { "x-v": value } }, (res) => resolve(read(res))).on("error", reject);
        })),
    );
  } catch (e) {
    row.push("http.get " + describe(e));
  }
  const req = http.request({ port, host: "127.0.0.1" });
  req.on("error", () => {});
  try {
    req.setHeader("x-v", value);
    const done = new Promise((resolve) => req.on("response", (res) => resolve(read(res))));
    req.end();
    row.push("setHeader " + (await done));
  } catch (e) {
    req.destroy();
    row.push("setHeader " + describe(e));
  }
  console.log(row.join(" | "));
}

// The other Headers entries, and a Response's.
for (const [label, make] of [
  ["append euro", () => new Headers().append("x-v", "\u20ac")],
  ["init euro", () => new Headers({ "x-v": "\u20ac" })],
  ["name euro", () => new Headers({ "x-\u20ac": "1" })],
  ["name space", () => new Headers({ "x v": "1" })],
  ["Response euro", () => new Response("x", { headers: { "x-v": "\u20ac" } })],
  ["Response e9", () => new Response("x", { headers: { "x-v": "\u00e9" } }).headers.get("x-v")],
]) {
  try {
    console.log(label, JSON.stringify(make() ?? "ok"));
  } catch (e) {
    console.log(label, describe(e));
  }
}
raw.close();
