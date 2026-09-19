// Chunk extensions (`3;name=value\r\n`) are read to their grammar, as node's
// parser reads them, in request bodies an http server receives and in
// response bodies http.get and fetch read: an extension is `;` then a name of
// token characters, optionally `=` and a value of token characters and quoted
// strings; anything else -- whitespace, a separator, a control character, a
// byte after a closing quote -- makes the body malformed. A server answers
// such a body 400 and closes the connection; a client's response fails.
//
// oam skipped everything between the `;` and the CR, so it read bodies node
// refuses. Parsers that disagree about where a chunked body ends are how a
// front end and a server come to frame a request differently.
import http from "node:http";
import net from "node:net";

const BS = String.fromCharCode(92);
const lines = {
  "name=value": "3;e=1",
  "name only": "3;e",
  "two extensions": "3;a=1;b=2",
  "no name": "3;=1",
  "empty value": "3;e=",
  "quoted value": '3;e="a b"',
  "empty quoted value": '3;e=""',
  "quoted pair": `3;e="a${BS}"b"`,
  "quoted then token": '3;e=a"b"',
  "quoted, then another": '3;e="x";f',
  "tchars": "3;!#$%&'*+-.^_`|~=!#$%&'*+-.^_`|~",
  "DEL and obs-text quoted": '3;e="\x7f\xff"',
  "on the last chunk": "LAST 0;e=1",
  "space after ;": "3; e=1",
  "space before =": "3;e =1",
  "space after =": "3;e= 1",
  "trailing space": "3;e=1 ",
  "tab in name": "3;e\t=1",
  "tab in value": "3;e=\t1",
  "; alone": "3;",
  "trailing ;": "3;e=1;",
  ";;": "3;;",
  "separator in value": "3;e=a,b",
  "slash in value": "3;e=a/b",
  "= in value": "3;e==1",
  "@ in name": "3;e@=1",
  "quoted name": '3;"e"',
  "byte after a quote": '3;e="q"x',
  "unclosed quote": '3;e="x',
  "control char quoted": '3;e="\x01"',
  "NUL in name": "3;e\x00",
  "DEL in value": "3;e=\x7f",
  "DEL after a backslash": `3;e="${BS}\x7f"`,
  "obs-text in name": "3;\x80=1",
  "bad, on the last chunk": "LAST 0;e =1",
};

const body = (line) =>
  line.startsWith("LAST ")
    ? `3\r\nabc\r\n${line.slice(5)}\r\n\r\n`
    : `${line}\r\nabc\r\n0\r\n\r\n`;

// Requests: raw, one per connection.
const server = http.createServer((req, res) => {
  let got = "";
  req.on("data", (d) => (got += d));
  req.on("end", () => res.end("got " + got));
  req.on("error", () => {});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));

function send(bytes) {
  return new Promise((resolve) => {
    const socket = net.connect(server.address().port, "127.0.0.1");
    let raw = "";
    let settled = false;
    const done = (closed) => {
      if (settled) return;
      settled = true;
      clearTimeout(backstop);
      socket.destroy();
      const status = raw.split("\r\n")[0] || "(nothing)";
      const got = raw.includes("got ") ? " " + raw.slice(raw.indexOf("got ")) : "";
      resolve(`${status}${got}${closed ? ", closed" : ""}`);
    };
    const backstop = setTimeout(() => done(false), 1500);
    socket.on("data", (d) => (raw += d.toString("latin1")));
    socket.on("close", () => done(true));
    socket.on("error", () => {});
    socket.write(Buffer.from(bytes, "latin1"));
  });
}

for (const [name, line] of Object.entries(lines)) {
  const head = "POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
  console.log(`request, ${name} | ${await send(head + body(line))}`);
}
server.close();

// Responses: a raw server, read by http.get and fetch. Only whether the body
// is read is compared (the errors are not the same objects).
let current = "";
const upstream = net.createServer((socket) => {
  socket.on("error", () => {});
  socket.once("data", () =>
    socket.end(
      Buffer.from(
        `HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n${body(current)}`,
        "latin1",
      ),
    ),
  );
});
await new Promise((resolve) => upstream.listen(0, "127.0.0.1", resolve));
const url = `http://127.0.0.1:${upstream.address().port}/`;
for (const name of ["name=value", "quoted value", "space after ;", "separator in value", "; alone", "bad, on the last chunk"]) {
  current = lines[name];
  const viaHttp = await new Promise((resolve) => {
    http
      .get(url, (res) => {
        let got = "";
        res.on("data", (d) => (got += d));
        res.on("end", () => resolve(`body ${got}`));
        res.on("error", () => resolve("fails"));
      })
      .on("error", () => resolve("fails"));
  });
  let viaFetch;
  try {
    viaFetch = `body ${await (await fetch(url)).text()}`;
  } catch {
    viaFetch = "fails";
  }
  console.log(`response, ${name} | http.get ${viaHttp} | fetch ${viaFetch}`);
}
upstream.close();
