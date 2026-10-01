// A fetch response body that cannot be read to its end rejects the read
// with `TypeError: terminated`, and the `cause` says what failed (#168).
// Measured on node v22.22.2:
//
//   - a corrupt content-encoding: the decoder's error, an Error with own
//     `errno` and `code` (zlib: -3, Z_DATA_ERROR, and zlib's message; brotli:
//     `Decompression failed` with an `ERR__ERROR_FORMAT_*` code);
//   - a connection that ends inside the body (short of its content-length,
//     or between chunks) of a kept-alive response: undici's `SocketError:
//     other side closed`, code UND_ERR_SOCKET, with the connection's
//     addresses on `socket`. A response the server does not keep alive
//     (`connection: close`, HTTP/1.0) is completed with what arrived
//     instead: short of its content-length, undici's
//     ResponseContentLengthMismatchError; between chunks, the body just
//     ends there. oam reported `other side closed` for every one;
//   - a bad chunk-size line: undici's HTTPParserError, HPE_INVALID_CHUNK_SIZE.
//
// The same error rejects text(), arrayBuffer(), bytes(), json() and a
// reader's read(), and a second read() rejects with the very same object.
// oam used to reject all of these with one plain Error blaming decoding
// (`fetch: body read failed: error decoding response body`) and no cause, so
// a truncated download and a corrupt payload could not be told apart.
//
// Not compared, because the runtimes differ (docs/node-divergences.md):
//   - zlib's message for a corrupt DEFLATE STREAM (`invalid distance too far
//     back`, `invalid block type`): oam's inflater reports one failure for
//     all of them, so only `code` and `errno` are printed for those;
//   - which ERR__ERROR_FORMAT_* a corrupt brotli body is;
//   - the chunks handed over before a decode failure (oam delivers what
//     decoded; node none of the last write);
//   - `socket.bytesWritten` / `bytesRead`, which oam does not count, and
//     HTTPParserError's `data`.
//
// The server is a raw socket writing fixed bytes, so the only runtime under
// test is the fetch client. A decode failure is sent without a
// content-length and the close held for 250 ms, as in case 112: with an
// immediate close node's own fetch sometimes never settles on macOS.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

// gzip("hello hello hello hello"), compressed once by node's zlib.
const GZIP = Buffer.from("H4sIAAAAAAAACstIzcnJV8hAJwHjUT2NFwAAAA==", "base64");
const flip = (bytes, at) => {
  const copy = Buffer.from(bytes);
  copy[at < 0 ? copy.length + at : at] ^= 0xff;
  return copy;
};
const set = (bytes, at, value) => {
  const copy = Buffer.from(bytes);
  copy[at] = value;
  return copy;
};
// zlib("hello hello hello hello") with its Adler-32 broken.
const ZLIB_BAD_ADLER = flip(Buffer.from("eJzLSM3JyVfIQCcBaAMIsQ==", "base64"), -1);

// path -> [content-encoding, body, compare the cause's message]
const CORRUPT = {
  "/gzip-not": ["gzip", Buffer.from("NOTGZIPATALL"), true],
  "/gzip-junk": ["gzip", Buffer.concat([GZIP, Buffer.from("TRAILINGJUNK")]), true],
  "/gzip-crc": ["gzip", flip(GZIP, -8), true],
  "/gzip-length": ["gzip", flip(GZIP, -1), true],
  "/gzip-method": ["gzip", set(GZIP, 2, 7), true],
  "/gzip-flags": ["gzip", set(GZIP, 3, 0xe0), true],
  "/deflate-adler": ["deflate", ZLIB_BAD_ADLER, true],
  // A copy from before the start of the output, and an invalid block type
  // inside a gzip member.
  "/deflate-distance": ["deflate", Buffer.from("4b041200", "hex"), false],
  "/gzip-block": ["gzip", Buffer.concat([GZIP.subarray(0, 10), Buffer.from("07", "hex")]), false],
  "/br-not": ["br", Buffer.from("this is not brotli at all, really not"), false],
};
const WIRE = {
  "/short": "HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n0123456789",
  "/chunked-short": "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n",
  "/chunked-bad": "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\nZZ\r\n",
  "/short-close": "HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\n0123456789",
  "/short-http10": "HTTP/1.0 200 OK\r\ncontent-length: 100\r\n\r\n0123456789",
  "/chunked-short-close":
    "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n5\r\nhello\r\n",
};

const server = net.createServer((socket) => {
  socket.on("error", () => {});
  let head = "";
  socket.on("data", (chunk) => {
    head += chunk.toString("latin1");
    if (!head.includes("\r\n\r\n")) return;
    const path = head.split(" ")[1];
    head = "";
    if (WIRE[path]) {
      socket.end(WIRE[path]);
      return;
    }
    const [encoding, body] = CORRUPT[path];
    socket.write(
      Buffer.concat([
        Buffer.from(`HTTP/1.1 200 OK\r\ncontent-encoding: ${encoding}\r\nconnection: close\r\n\r\n`),
        body,
      ]),
    );
    setTimeout(() => socket.end(), 250);
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const base = `http://127.0.0.1:${port}`;

const names = (value) => JSON.stringify(Reflect.ownKeys(value).map(String));

function describe(e, message = true) {
  const c = e.cause;
  let line = `${e.constructor.name} ${JSON.stringify(e.message)} own=${names(e)} | cause `;
  if (!(c instanceof Error)) return `${line}${String(c)}`;
  line += `${c.constructor.name} name=${c.name} `;
  if (c.code === "UND_ERR_SOCKET") {
    const s = c.socket;
    return (
      `${line}${JSON.stringify(c.message)} code=${c.code} own=${names(c)} ` +
      `socket: local=${s.localAddress} localPort=${typeof s.localPort} remote=${s.remoteAddress} ` +
      `remotePort=${s.remotePort === port} family=${s.remoteFamily} timeout=${s.timeout}`
    );
  }
  if (typeof c.code === "string" && c.code.startsWith("HPE_")) {
    return `${line}${JSON.stringify(c.message)} code=${c.code} own=${names(c)}`;
  }
  if (typeof c.code === "string" && c.code.startsWith("ERR__ERROR_FORMAT_")) {
    return `${line}${JSON.stringify(c.message)} code=ERR__ERROR_FORMAT_* errno<0=${c.errno < 0} own=${names(c)}`;
  }
  return (
    `${line}${message ? JSON.stringify(c.message) : "(message not compared)"} ` +
    `code=${c.code} errno=${c.errno} own=${names(c)}`
  );
}

for (const [path, [, , message]] of Object.entries(CORRUPT)) {
  try {
    const res = await fetch(base + path);
    console.log(`${path} text: resolved ${JSON.stringify(await res.text())}`);
  } catch (e) {
    console.log(`${path} text: ${describe(e, message)}`);
  }
}

for (const path of Object.keys(WIRE)) {
  for (const method of ["text", "arrayBuffer", "bytes", "json"]) {
    try {
      const res = await fetch(base + path);
      await res[method]();
      console.log(`${path} ${method}: resolved`);
    } catch (e) {
      console.log(`${path} ${method}: ${describe(e)}`);
    }
  }
  // A reader gets the bytes that arrived, then the failure -- the same
  // object on every later read.
  const reader = (await fetch(base + path)).body.getReader();
  const got = [];
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      got.push(Buffer.from(value).toString("latin1"));
    }
    console.log(`${path} reader: ended ${JSON.stringify(got.join(""))}`);
  } catch (e) {
    let again;
    try {
      await reader.read();
    } catch (e2) {
      again = e2;
    }
    console.log(`${path} reader: ${JSON.stringify(got.join(""))} then ${describe(e)} | again same=${again === e}`);
  }
}

// http.request does not speak undici: node's request hears its parser's
// error for a bad chunk-size line before the response is aborted, and a
// connection that ends inside a body only aborts the response, whatever the
// response said about keeping it. oam's shared transport aborted the
// response without the request's 'error'.
for (const agent of [undefined, false]) {
  for (const path of Object.keys(WIRE)) {
    const events = [];
    // What arrived, printed last: when a chunk is delivered against the
    // parser's error is not compared.
    let got = "";
    await new Promise((resolve) => {
      const req = http.get(base + path, { agent }, (res) => {
        res.on("data", (d) => (got += d.toString("latin1")));
        res.on("end", () => events.push("res end"));
        res.on("aborted", () => events.push("res aborted"));
        res.on("error", (e) => events.push(`res error ${e.code} ${e.message}`));
        res.on("close", () => {
          events.push("res close");
          setTimeout(resolve, 50);
        });
      });
      req.on("error", (e) => {
        const own = Reflect.ownKeys(e).map(String).filter((k) => k !== "bytesParsed" && k !== "rawPacket");
        events.push(`req error ${e.code} ${JSON.stringify(e.message)} reason=${e.reason} own=${JSON.stringify(own)}`);
      });
    });
    console.log(`http.get ${agent === false ? "agent: false" : "default agent"} ${path}: ${events.join(", ")} | got ${JSON.stringify(got)}`);
  }
}
server.close();
