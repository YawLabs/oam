// An http server answers a chunked request body node's parser refuses with
// node's status and closes the connection: whitespace after a chunk size
// (`3 \r\n`, which parsers disagree about), a size that is not hex, a chunk
// without its CRLF, a bare LF in the trailers -- 400; chunk extensions over
// the limit -- 413. The handler, dispatched on the headers, sees the request
// abort. oam used to take whitespace after a size as part of the size line,
// and answered every other malformed body by closing the connection
// without a status.
//
// Every request is written raw, one per connection; the status lines and
// what the handler saw are printed.
import http from "node:http";
import net from "node:net";

const HEAD = "POST /c HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n";
const cases = {
  "well-formed": "3\r\nabc\r\n0\r\n\r\n",
  "leading zeros and uppercase hex": "003\r\nabc\r\nA\r\n0123456789\r\n0\r\n\r\n",
  "an extension": "3;name=value\r\nabc\r\n0\r\n\r\n",
  "trailers": "3\r\nabc\r\n0\r\nX-T: 1\r\n\r\n",
  "space after the size": "3 \r\nabc\r\n0\r\n\r\n",
  "tab after the size": "3\t\r\nabc\r\n0\r\n\r\n",
  "space before an extension": "3 ;name=value\r\nabc\r\n0\r\n\r\n",
  "space after the last size": "3\r\nabc\r\n0 \r\n\r\n",
  "space after a later size": "3\r\nabc\r\n2 \r\nde\r\n0\r\n\r\n",
  "space before the size": " 3\r\nabc\r\n0\r\n\r\n",
  "size then garbage": "3x\r\nabc\r\n0\r\n\r\n",
  "not a size": "zz\r\nabc\r\n0\r\n\r\n",
  "chunk without its CRLF": "3\r\nabcX\r\n0\r\n\r\n",
  "bare LF in the trailers": "3\r\nabc\r\n0\r\nX-T: 1\n\r\n",
  "extensions over the limit": "3;e=" + "a".repeat(20000) + "\r\nabc\r\n0\r\n\r\n",
};

const seen = [];
const server = http.createServer((req, res) => {
  const events = [];
  seen.push(events);
  let body = "";
  req.on("data", (d) => (body += d));
  req.on("end", () => {
    events.push(`end ${JSON.stringify(body)}`);
    res.end("ok");
  });
  req.on("aborted", () => events.push("aborted"));
  req.on("error", (e) => events.push(`error ${e.code} ${JSON.stringify(e.message)}`));
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));

function send(bytes) {
  return new Promise((resolve) => {
    const socket = net.connect(server.address().port, "127.0.0.1");
    let raw = "";
    let settled = false;
    let quiet = null;
    const backstop = setTimeout(() => done(), 3000);
    const done = () => {
      if (settled) return;
      settled = true;
      clearTimeout(quiet);
      clearTimeout(backstop);
      socket.destroy();
      const statuses = [...raw.matchAll(/HTTP\/1\.1 \d{3}[^\r\n]*/g)].map((m) => m[0]);
      resolve(statuses.join(" + ") || "(nothing)");
    };
    socket.on("data", (d) => {
      raw += d.toString("latin1");
      clearTimeout(quiet);
      quiet = setTimeout(done, 300);
    });
    socket.on("close", done);
    socket.on("error", () => {});
    socket.write(bytes);
  });
}

for (const [name, body] of Object.entries(cases)) {
  const before = seen.length;
  const status = await send(HEAD + body);
  await new Promise((r) => setTimeout(r, 30));
  const handler = seen.slice(before).map((events) => events.join(", ")).join(" ; ");
  console.log(`${name} | ${status} | ${handler || "no handler"}`);
}
server.close();
