// http2.connect against a server that sends a graceful GOAWAY and keeps
// the connection open (#185): the session sends its own GOAWAY as soon as
// it is closed -- the peer's GOAWAY closes it -- while its stream is still
// open, so the server answers with one more GOAWAY and ends the connection
// once that stream is done, and the session closes. Also when the GOAWAY
// comes in the middle of a response, and when the client closes the session
// itself before the response.
//
// Regression guard: oam's session sent its GOAWAY only once its last stream
// was done. Node's server no longer reads once its own GOAWAY is out and
// its streams are done, so it never saw that GOAWAY, nor the end of the
// connection: it kept the connection open, the session never emitted
// 'close', and the process never exited. Case 352's server ends the
// connection itself, which hid it.
//
// The server is a separate `node` process (the harness's oracle, on PATH)
// in BOTH runs, so only the client differs.
import http2 from "node:http2";
import { spawn } from "node:child_process";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 50000).unref();

// before: GOAWAY(NO_ERROR, 2^31-1) once the request is in, then the answer.
// last:   GOAWAY(NO_ERROR, 1) -- the request's stream -- then the answer.
// mid:    the head and part of the body, GOAWAY(NO_ERROR, 1), the rest.
// client: no GOAWAY from the server; the client closes the session.
// The server never ends the connection itself.
const SERVER = `
import http2 from "node:http2";
const mode = process.argv[1];
const server = http2.createServer();
server.on("stream", (stream) => {
  stream.on("error", () => {});
  stream.resume();
  stream.on("end", () => {
    if (mode === "mid") {
      stream.respond({ ":status": 200 });
      stream.write("part1");
      stream.session.goaway(0, 1);
      setTimeout(() => stream.end("part2"), 300);
      return;
    }
    if (mode === "before") stream.session.goaway(0, 2147483647);
    if (mode === "last") stream.session.goaway(0, 1);
    setTimeout(() => {
      stream.respond({ ":status": 200 });
      stream.end("ok");
    }, 150);
  });
});
server.on("session", (session) => {
  session.on("error", () => {});
  session.on("goaway", (code, last) => console.log("goaway " + code + " " + last));
});
server.listen(0, "127.0.0.1", () => console.log(server.address().port));
`;

function startServer(mode) {
  const server = spawn("node", ["--input-type=module", "-e", SERVER, mode], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  const seen = [];
  return new Promise((resolve) => {
    let out = "";
    server.stdout.setEncoding("utf8");
    server.stdout.on("data", (d) => {
      out += d;
      const lines = out.split("\n");
      out = lines.pop();
      for (const line of lines) {
        if (/^\d+$/.test(line.trim())) resolve({ server, port: line.trim(), seen });
        else seen.push(line.trim());
      }
    });
  });
}

async function run(mode) {
  const { server, port, seen } = await startServer(mode);
  const lines = [];
  const session = http2.connect("http://127.0.0.1:" + port);
  session.on("error", (e) => lines.push("session error " + e.code));
  session.on("goaway", (code, last) =>
    lines.push("session goaway " + code + " " + last + " closed=" + session.closed));
  const closed = new Promise((resolve) => session.on("close", () => {
    lines.push("session close");
    resolve(true);
  }));
  const req = session.request({ ":path": "/", ":method": "POST" });
  req.setEncoding("utf8");
  req.on("response", (h) => lines.push("stream response " + h[":status"]));
  req.on("data", (d) => lines.push("stream data " + d));
  req.on("end", () => lines.push("stream end"));
  req.on("error", (e) => lines.push("stream error " + e.code));
  req.on("close", () => lines.push("stream close rstCode=" + req.rstCode));
  req.end("x");
  if (mode === "client") setTimeout(() => session.close(), 50);
  // node closes at once; 10 s is a session that never closes.
  const deadline = new Promise((resolve) => setTimeout(resolve, 10000, false).unref());
  const ok = await Promise.race([closed, deadline]);
  if (!ok) {
    lines.push("no session close; destroyed=" + session.destroyed);
    session.destroy();
  }
  await new Promise((r) => setTimeout(r, 100));
  server.kill();
  console.log(mode + ":");
  for (const line of lines) console.log("  " + line);
  // The client's GOAWAY, as the server's session reported it: node's
  // server may stop reading before a second one reaches it.
  console.log("  server saw: " + (seen.length > 0 ? seen[0] : "no GOAWAY"));
}

for (const mode of ["before", "last", "mid", "client"]) await run(mode);
process.exit(0);
