// net over a pipe (#219): a Windows named pipe, a Unix domain socket
// elsewhere. listen(path) and connect({ path }) / connect(path), what each
// side's socket reports, the data both ways, what each side sees when one of
// them ends the connection (on Windows a named pipe has no half-close: the
// side that ends reads EOF too), node's error shapes for a pipe nobody
// listens on (`connect ENOENT <path>`) and for one already taken (`listen
// EADDRINUSE`, port -1), a closed server's path dialling ENOENT again,
// http.get({ socketPath }), and the refusals node makes synchronously.
//
// Regression guard: oam had no pipe client or server -- connect({ path })
// and listen(path) failed with ERR_FEATURE_UNAVAILABLE_ON_PLATFORM -- so
// @playwright/mcp --isolated, whose browser server listens on a pipe and
// whose client dials it, booted and then failed every browser tool.
//
// Each side's events are logged on their own and printed at the end: the
// order between the two sides is the scheduler's, not node's contract.
import net from "node:net";
import http from "node:http";
import os from "node:os";
import path from "node:path";

const base = process.platform === "win32"
  ? String.raw`\\.\pipe\oam-case-375-` + process.pid
  : path.join(os.tmpdir(), "oam-case-375-" + process.pid);
let serial = 0;
const fresh = () => base + "-" + ++serial + (process.platform === "win32" ? "" : ".sock");
const scrub = (s) => (typeof s === "string" ? s.split(base).join("PIPE") : s);
const shape = (e) => JSON.stringify({
  keys: Object.keys(e),
  message: scrub(e.message),
  code: e.code,
  syscall: e.syscall,
  address: scrub(e.address),
  port: e.port,
  errnoIsNumber: typeof e.errno === "number",
});
const props = (s) => JSON.stringify({
  remoteAddress: s.remoteAddress,
  remotePort: s.remotePort,
  remoteFamily: s.remoteFamily,
  localAddress: s.localAddress,
  localPort: s.localPort,
  address: s.address(),
});

// 1. Listen, connect both ways, exchange data.
{
  const P = fresh();
  const log = { server: [], client: [] };
  let serverClosed = null;
  const srv = net.createServer((c) => {
    log.server.push("connection " + props(c));
    c.setEncoding("utf8");
    c.on("data", (d) => {
      log.server.push("data " + d);
      if (d === "ping") c.write("pong");
    });
    c.on("end", () => log.server.push("end"));
    c.on("close", (hadError) => {
      log.server.push("close " + hadError);
      serverClosed();
    });
  });
  srv.on("listening", () => log.server.push("listening " + scrub(srv.address())));
  console.log("address before listen", srv.address());
  srv.listen(P);
  console.log("address after listen()", scrub(srv.address()));
  console.log("resources", process.getActiveResourcesInfo().includes("PipeWrap"));
  await new Promise((r) => srv.once("listening", r));
  for (const dial of [() => net.connect({ path: P }), () => net.createConnection(P)]) {
    const bothClosed = new Promise((r) => (serverClosed = r));
    await new Promise((resolve) => {
      const s = dial();
      log.client.push("connecting " + s.connecting + " " + s.readyState);
      s.setEncoding("utf8");
      s.on("connect", () => {
        log.client.push("connect " + props(s) + " " + s.readyState);
        s.write("ping");
      });
      s.on("ready", () => log.client.push("ready"));
      s.on("data", (d) => {
        log.client.push("data " + d);
        s.destroy();
      });
      s.on("close", (hadError) => {
        log.client.push("close " + hadError + " " + s.bytesRead + " " + s.bytesWritten);
        resolve();
      });
    });
    await bothClosed;
  }
  await new Promise((r) => srv.close(r));
  console.log("address after close", scrub(srv.address()), srv.listening);
  console.log("server:", log.server.join(" | "));
  console.log("client:", log.client.join(" | "));
}

// 2. One side ends the connection: what each side sees.
for (const who of ["server", "client"]) {
  for (const allowHalfOpen of [false, true]) {
    const P = fresh();
    const log = { server: [], client: [] };
    await new Promise((resolve) => {
      let closed = 0;
      const srv = net.createServer({ allowHalfOpen }, (c) => {
        watch("server", c);
        // Without allowHalfOpen the side that reads the end ends itself; with
        // it, that side ends (late) on its own.
        if (who === "server") c.end("bye");
        else if (allowHalfOpen) c.on("end", () => setTimeout(() => c.end("late"), 20));
      });
      const done = () => {
        if (++closed === 2) srv.close(resolve);
      };
      function watch(side, s) {
        s.on("data", (d) => log[side].push("data " + d));
        s.on("end", () => log[side].push("end " + s.readyState));
        s.on("finish", () => log[side].push("finish"));
        s.on("error", (e) => log[side].push("error " + e.code + " " + e.syscall));
        s.on("close", (hadError) => {
          log[side].push("close " + hadError);
          done();
        });
      }
      srv.listen(P, () => {
        const s = net.connect({ path: P, allowHalfOpen }, () => {
          if (who === "client") s.end("bye");
        });
        watch("client", s);
        if (who === "server" && allowHalfOpen) s.on("end", () => setTimeout(() => s.end(), 20));
      });
    });
    console.log(who, "ends, allowHalfOpen", allowHalfOpen);
    console.log("  server:", log.server.join(", "));
    console.log("  client:", log.client.join(", "));
  }
}

// 3. Errors: nobody listening, a path already taken, a closed server's path.
{
  const P = fresh();
  console.log("ENOENT", await new Promise((resolve) => {
    net.connect(P).on("error", (e) => resolve(shape(e)));
  }));
  const a = net.createServer();
  await new Promise((r) => a.listen(P, r));
  const b = net.createServer();
  console.log("EADDRINUSE", await new Promise((resolve) => {
    b.on("error", (e) => resolve(shape(e) + " listening " + b.listening + " address " + scrub(b.address())));
    b.listen(P);
  }));
  await new Promise((r) => a.close(r));
  console.log("after close", await new Promise((resolve) => {
    net.connect({ path: P }).on("error", (e) => resolve(e.code));
  }));
}

// 4. http.get({ socketPath }): the request goes to the pipe, never to
// host:port.
{
  const P = fresh();
  const srv = net.createServer((c) => {
    let head = "";
    c.on("data", (d) => {
      head += d;
      if (!head.includes("\r\n\r\n")) return;
      const line = head.slice(0, head.indexOf("\r\n"));
      c.end(`HTTP/1.1 200 OK\r\nContent-Length: ${line.length}\r\nConnection: close\r\n\r\n${line}`);
    });
  });
  await new Promise((r) => srv.listen(P, r));
  console.log("http", await new Promise((resolve) => {
    http.get({ socketPath: P, host: "127.0.0.1", port: 1, path: "/x?y=1" }, (res) => {
      let body = "";
      res.setEncoding("utf8");
      res.on("data", (d) => (body += d));
      res.on("end", () => resolve(res.statusCode + " " + body));
    }).on("error", (e) => resolve("error " + e.code));
  }));
  await new Promise((r) => srv.close(r));
}

// 5. What node refuses synchronously.
try {
  net.connect({ path: 7 });
} catch (e) {
  console.log("path 7", e.code, e.message);
}
{
  const P = fresh();
  const srv = net.createServer();
  await new Promise((r) => srv.listen(P, r));
  const s = net.connect(P);
  await new Promise((r) => s.once("connect", r));
  try {
    s.resetAndDestroy();
    console.log("resetAndDestroy did not throw");
  } catch (e) {
    console.log("resetAndDestroy", e.code);
  }
  s.destroy();
  await new Promise((r) => srv.close(r));
}
