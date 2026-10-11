// http.Server over a pipe (#282): a Windows named pipe, a Unix domain socket
// elsewhere. listen(path) binds and names the server by the path (address()
// answers it, while listening and after close()), a GET and a POST with a
// body are served, req.socket's address fields are undefined (a pipe has
// none), close() drains, and a NUL in the path is refused asynchronously
// (an 'error' event: EINVAL, syscall listen, port -1) -- the same lines
// print on Node and oam, and the oracle is that they match.
//
// Each section awaits its events before the next starts, so the printed
// order is the case's, not the scheduler's.
import http from "node:http";
import os from "node:os";
import path from "node:path";


const P = process.platform === "win32"
  ? String.raw`\\.\pipe\oam-case-389-` + process.pid
  : path.join(os.tmpdir(), "oam-case-389-" + process.pid + ".sock");
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

const server = new http.Server((req, res) => {
  log("req " + req.method + " " + req.url + " remote=" + JSON.stringify(req.socket.remoteAddress));
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    res.writeHead(200, { "content-type": "text/plain" });
    res.end(req.method === "POST" ? Buffer.concat(chunks).toString() : "hello pipe");
  });
});
server.on("error", (e) => log("server error " + e.code));
await new Promise((r) => server.listen(P, r));
log("listening address=" + JSON.stringify(server.address()));

// 1. GET over the pipe.
{
  const out = await new Promise((resolve) => {
    http
      .get({ socketPath: P, path: "/get" }, (res) => {
        let body = "";
        res.on("data", (c) => (body += c));
        res.on("end", () => resolve(res.statusCode + " " + body));
      })
      .on("error", (e) => resolve("error " + e.code));
  });
  log("get " + out);
}

// 2. POST with a body, echoed back.
{
  const out = await new Promise((resolve) => {
    const req = http.request(
      { socketPath: P, path: "/post", method: "POST" },
      (res) => {
        let body = "";
        res.on("data", (c) => (body += c));
        res.on("end", () => resolve(res.statusCode + " " + body));
      },
    );
    req.on("error", (e) => resolve("error " + e.code));
    req.end("posted body");
  });
  log("post " + out);
}

// 3. close() drains; the server keeps its name after.
await new Promise((r) => server.close(r));
log("drained address=" + JSON.stringify(server.address()));

// 4. A NUL in the path: the refusal is ASYNC on both engines (an 'error'
//    event, not a synchronous throw), with the bind-shaped fields. Only the
//    fields that are node's contract print; the message text does not.
{
  const bad = new http.Server();
  bad.on("error", (e) => {
    log("nul " + e.code + " syscall=" + e.syscall + " port=" + e.port);
    finish();
  });
  bad.listen("a\0b");
}
