// http2.createServer() over a pipe (#282): a Windows named pipe, a Unix
// domain socket elsewhere. listen(path) binds and names the server by the
// path, an h2 prior-knowledge client completes a stream over it, close()
// drains, and a NUL in the path is refused asynchronously (EINVAL, syscall
// listen, port -1) -- the same lines print on Node and oam, and the oracle
// is that they match.
//
// NOT pinned here: oam's cleartext http2 server also serves HTTP/1.1
// clients (its own fallback, on TCP and pipes alike) -- Node's refuses one
// (the client reads h2 frames as HTTP/1 and fails HPE_INVALID_CONSTANT).
// A divergence of the TCP server, recorded in docs/node-divergences.md;
// the pipe server keeps it for parity with the TCP one.
//
// The h2 client dials through its net socket with the `path` option (the
// plain pipe dial, not the refused tls.connect({ path })).
import http2 from "node:http2";
import net from "node:net";
import os from "node:os";
import path from "node:path";


const P = process.platform === "win32"
  ? String.raw`\\.\pipe\oam-case-391-` + process.pid
  : path.join(os.tmpdir(), "oam-case-391-" + process.pid + ".sock");
const lines = [];
// The path embeds the pid; the log scrubs it (case 375's convention), in
// both its raw and JSON-escaped spellings.
const scrub = (s) => {
  if (typeof s !== "string") return s;
  return s.split(JSON.stringify(P)).join('"PIPE"').split(P).join("PIPE");
};
const log = (...parts) => lines.push(scrub(parts.join(" ")));
const watchdog = setTimeout(() => {
  console.log(lines.join("\n"));
  console.log("WATCHDOG");
  process.exit(9);
}, 30000);
const finish = () => {
  clearTimeout(watchdog);
  console.log(lines.join("\n"));
  process.exit(0);
};

const server = http2.createServer();
server.on("stream", (stream, headers) => {
  log("h2 stream " + headers[":method"] + " " + headers[":path"]);
  stream.respond({ ":status": 200, "content-type": "text/plain" });
  stream.end("hello h2c pipe");
});
server.on("error", (e) => log("server error " + e.code));
await new Promise((r) => server.listen(P, r));
log("listening address=" + JSON.stringify(server.address()));

// An h2 prior-knowledge client over the pipe.
{
  const out = await new Promise((resolve) => {
    const session = http2.connect("http://localhost", { path: P });
    session.on("error", (e) => resolve("error " + e.code));
    const req = session.request({ ":method": "GET", ":path": "/h2" });
    req.on("response", (headers) => {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        session.close();
        resolve(headers[":status"] + " " + body);
      });
    });
    req.end();
  });
  log("h2 " + out);
}

// An h2 preface split across two writes, from a raw socket: node's client
// writes it whole, so this pins the detection against the pipe's peekless
// read losing the first chunk to a retry. The client sends 10 preface
// bytes, then the rest plus an empty SETTINGS frame, and the server's
// SETTINGS (frame type 4) back is the proof it ran h2 and not the HTTP/1.1
// fallback. The first 9 response bytes' frame type is node's contract; a
// fallback answers HTTP/1.1 text and fails the check.
{
  const out = await new Promise((resolve) => {
    const s = net.connect({ path: P });
    const PREFACE = "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    const SETTINGS = Buffer.from([0, 0, 0, 4, 0, 0, 0, 0, 0]);
    let got = Buffer.alloc(0);
    s.on("error", (e) => resolve("error " + e.code));
    s.on("connect", () => {
      s.write(Buffer.from(PREFACE.slice(0, 10)));
      setTimeout(() => {
        s.write(Buffer.concat([Buffer.from(PREFACE.slice(10)), SETTINGS]));
      }, 20);
    });
    s.on("data", (d) => {
      got = Buffer.concat([got, d]);
      if (got.length >= 9) {
        // The verdict is in; the socket must go or the server's close()
        // below drains forever waiting for it.
        s.destroy();
        resolve("frame type " + got[3]);
      }
    });
  });
  log("split preface " + out);
}

// close() drains; the server keeps its name after.
await new Promise((r) => server.close(r));
log("drained address=" + JSON.stringify(server.address()));

// A NUL in the path: the refusal is ASYNC on both engines (an 'error'
// event, not a synchronous throw), with the bind-shaped fields. Only the
// fields that are node's contract print; the message text does not.
{
  const bad = http2.createServer();
  bad.on("error", (e) => {
    log("nul " + e.code + " syscall=" + e.syscall + " port=" + e.port);
    finish();
  });
  bad.listen("a\0b");
}
