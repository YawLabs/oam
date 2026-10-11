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
import http from "node:http";
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
