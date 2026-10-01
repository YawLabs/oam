// The bytes a node:http server response's latin1 header value goes out as,
// measured on node v22.22.2 first. oam wrote every response header value as
// its UTF-8 (`café` was caf\xc3\xa9 whatever the response did).
//
// node keeps the head as a string and writes it joined to the first thing
// sent after it. Joined to a string body in utf8 (or no encoding) -- end(
// 'text') on a response whose length end() works out, a write('text') of a
// response with a content-length, flushHeaders() -- it goes out as that
// string's UTF-8; before a chunk-size line, a Buffer, a string in another
// encoding, or nothing at all, it goes out one byte per code point (caf\xe9).
// The same value therefore reaches the wire both ways from the same
// setHeader(); this case pins which way each response sends it.
import http from "node:http";
import net from "node:net";

const V = "caf\u00e9\u00ff";
const heads = {
  setHeader: (res) => res.setHeader("x-v", V),
  appendHeader: (res) => {
    res.appendHeader("x-v", V);
    res.appendHeader("x-v", "\u00e9");
  },
  setHeaders: (res) => res.setHeaders(new Map([["x-v", V]])),
  writeHeadObject: (res) => res.writeHead(200, { "x-v": V }),
  writeHeadFlat: (res) => res.writeHead(200, ["x-v", V]),
  writeHeadPairs: (res) => res.writeHead(200, [["x-v", V]]),
  setThenWriteHead: (res) => {
    res.setHeader("x-a", "1");
    res.writeHead(200, { "x-v": V });
  },
};
const bodies = {
  "end()": (res) => res.end(),
  "end('')": (res) => res.end(""),
  "end('x')": (res) => res.end("x"),
  "end('\u00e9')": (res) => res.end("\u00e9"),
  "end('x', 'utf8')": (res) => res.end("x", "utf8"),
  "end('x', 'latin1')": (res) => res.end("x", "latin1"),
  "end('x', 'utf-8')": (res) => res.end("x", "utf-8"),
  "end('78', 'hex')": (res) => res.end("78", "hex"),
  "end(buffer)": (res) => res.end(Buffer.from("x")),
  "write('x')": (res) => {
    res.write("x");
    res.end();
  },
  "write('')": (res) => {
    res.write("");
    res.end("x");
  },
  "write(buffer)": (res) => {
    res.write(Buffer.from("x"));
    res.end();
  },
  "flushHeaders()": (res) => {
    res.flushHeaders();
    res.end("x");
  },
};
// How the response frames its body decides whether a chunk-size line goes
// first.
const framed = {
  "content-length, write('x')": (res) => {
    res.setHeader("content-length", "2");
    res.setHeader("x-v", V);
    res.write("x");
    res.end("y");
  },
  "content-length, writeHead, end('x')": (res) => {
    res.writeHead(200, { "content-length": "1", "x-v": V });
    res.end("x");
  },
  "transfer-encoding chunked, end('x')": (res) => {
    res.setHeader("transfer-encoding", "chunked");
    res.setHeader("x-v", V);
    res.end("x");
  },
  "trailer, end('x')": (res) => {
    res.setHeader("trailer", "x-t");
    res.setHeader("x-v", V);
    res.end("x");
  },
  "content-length removed, end('x')": (res) => {
    res.setHeader("x-v", V);
    res.removeHeader("content-length");
    res.end("x");
  },
  "204, end('x')": (res) => {
    res.statusCode = 204;
    res.setHeader("x-v", V);
    res.end("x");
  },
  "304, end('x')": (res) => {
    res.statusCode = 304;
    res.setHeader("x-v", V);
    res.end("x");
  },
  "writeHead 204, end('x')": (res) => {
    res.writeHead(204, { "x-v": V });
    res.end("x");
  },
};

const routes = [];
for (const [h, head] of Object.entries(heads)) {
  for (const [b, body] of Object.entries(bodies)) {
    routes.push([`${h} + ${b}`, (res) => {
      head(res);
      body(res);
    }]);
  }
}
for (const [label, fn] of Object.entries(framed)) routes.push([label, fn]);

const server = http.createServer((req, res) => routes[Number(req.url.slice(1))][1](res));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

// The raw x-v lines of the response head, each byte outside printable
// ASCII as \xNN.
const xv = (method, path) =>
  new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1", () =>
      socket.write(`${method} ${path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n`),
    );
    let buf = Buffer.alloc(0);
    socket.on("data", (chunk) => (buf = Buffer.concat([buf, chunk])));
    socket.on("close", () => {
      const end = buf.indexOf("\r\n\r\n");
      const lines = buf
        .subarray(0, end < 0 ? buf.length : end)
        .toString("latin1")
        .split("\r\n")
        .filter((line) => /^x-v:/i.test(line))
        .map((line) =>
          [...Buffer.from(line, "latin1")]
            .map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : `\\x${b.toString(16).padStart(2, "0")}`))
            .join(""),
        );
      resolve(lines.join(" | "));
    });
  });

for (let i = 0; i < routes.length; i++) console.log(routes[i][0].padEnd(40), await xv("GET", `/${i}`));
// A HEAD response has no body: what end('x') would send is dropped.
console.log("HEAD: setHeader + end('x')".padEnd(40), await xv("HEAD", `/${routes.findIndex(([l]) => l === "setHeader + end('x')")}`));
server.close();
