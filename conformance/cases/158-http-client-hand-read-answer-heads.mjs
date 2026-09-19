// The answer to a CONNECT or an upgrade request, read as node's llhttp reads
// it (measured on node v22.22.2). oam reads these heads off the socket by
// hand -- the socket is handed on after them -- and used to take almost any
// bytes for one: a garbage status line was a 'connect' with status 0, a
// header line without a colon was skipped, obs-fold, control characters, a
// second Content-Length and Content-Length with Transfer-Encoding all went
// through, and a head ending in bare LFs was never seen to end. node refuses
// each of those as the byte that breaks the head arrives (a bad first line
// needs no CRLF), with its own code and reason.
//
// Also: the socket a 'connect' / 'upgrade' listener receives is handed over
// unflowing, as node's is (socket.readableFlowing = null), so what the peer
// sends before the new owner reads -- an owner that attaches 'data' after an
// await -- waits in the socket, the end of the stream included. oam emitted
// those bytes to no listener.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

// Each connection gets the next scripted answer once the request head is in.
let script = null;
const server = net.createServer((s) => {
  s.on("error", () => {});
  let got = Buffer.alloc(0);
  const onData = (d) => {
    got = Buffer.concat([got, d]);
    if (got.indexOf("\r\n\r\n") === -1) return;
    s.removeListener("data", onData);
    s.resume();
    script(s);
  };
  s.on("data", onData);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

function request(kind) {
  return http.request(
    kind === "connect"
      ? { host: "127.0.0.1", port, method: "CONNECT", path: "target.test:443", agent: false }
      : { host: "127.0.0.1", port, path: "/", headers: { connection: "upgrade", upgrade: "x" }, agent: false },
  );
}

// One exchange: the answer's bytes, then (unless `open`) the server ends
// the connection. Everything the request reports, up to its 'close'.
function exchange(kind, answer, open) {
  script = (s) => {
    s.write(Buffer.from(answer, "latin1"));
    if (!open) s.end();
  };
  return new Promise((resolve) => {
    const events = [];
    const req = request(kind);
    req.on(kind, (res, socket, head) => {
      events.push(
        `${kind} ${res.statusCode} ${JSON.stringify(res.statusMessage)} v${res.httpVersion}` +
          ` raw=${JSON.stringify(res.rawHeaders)} head=${JSON.stringify(head.toString("latin1"))}`,
      );
      socket.destroy();
    });
    req.on("response", (res) => {
      events.push(`response ${res.statusCode} raw=${JSON.stringify(res.rawHeaders)}`);
      res.resume();
    });
    req.on("error", (e) => events.push(`error ${e.code} ${JSON.stringify(e.message)} reason=${JSON.stringify(e.reason)}`));
    req.on("close", () => resolve(events.join(" | ")));
    req.end();
  });
}

const answers = [
  // Accepted.
  "HTTP/1.1 200 Connection Established\r\n\r\n",
  "HTTP/1.1 200\r\n\r\n",
  "HTTP/1.1 200 \r\n\r\n",
  "HTTP/1.0 200 OK\r\n\r\n",
  "HTTP/0.9 200 OK\r\n\r\n",
  "HTTP/2.0 200 OK\r\n\r\n",
  "RTSP/1.0 200 OK\r\n\r\n",
  "ICE/1.0 200 OK\r\n\r\n",
  "HTTP/1.1 099 Odd\r\n\r\n",
  "HTTP/1.1 999 Odd\r\n\r\n",
  "HTTP/1.1 200 O\tK \x01caf\xe9\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX: caf\xe9\r\nY:\ta\tb \r\nZ:\r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length:  7 \r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\n",
  "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\nTransfer-Encoding: chunked\r\n\r\n",
  "HTTP/1.1 200 OK\r\nTransfer-Encoding:\r\nContent-Length: 1\r\n\r\n",
  "HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 4\r\n\r\nnope",
  "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\n\r\nafter",
  // Refused: the first line.
  "garbage\r\n\r\n",
  "http/1.1 200 OK\r\n\r\n",
  "HTTP/x.1 200 OK\r\n\r\n",
  "HTTP/11 200 OK\r\n\r\n",
  "HTTP/1.x 200 OK\r\n\r\n",
  "HTTP/1.2 200 OK\r\n\r\n",
  "HTTP/3.0 200 OK\r\n\r\n",
  "HTTP/1.1X200 OK\r\n\r\n",
  "HTTP/1.1\r\n\r\n",
  "HTTP/1.1  200 OK\r\n\r\n",
  "HTTP/1.1 20 OK\r\n\r\n",
  "HTTP/1.1 2x0 OK\r\n\r\n",
  "HTTP/1.1 2000 OK\r\n\r\n",
  "HTTP/1.1 200OK\r\n\r\n",
  "HTTP/1.1 200\n\r\n",
  "HTTP/1.1 200 OK\n\r\n",
  "HTTP/1.1 200 OK\nX: y\n\nafter",
  "HTTP/1.1 200 OK\rX\r\n\r\n",
  // Refused: the header lines.
  "HTTP/1.1 200 OK\r\nbad header line\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX\r\n\r\n",
  "HTTP/1.1 200 OK\r\n: v\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX : v\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX Y: z\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX-\xe9: v\r\n\r\n",
  "HTTP/1.1 200 OK\r\n X: v\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX: a\r\n b\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX: a\x00b\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX: \x01\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX: \x7f\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX: v\n\r\n",
  "HTTP/1.1 200 OK\r\nX: v\r\r\n\r\n",
  "HTTP/1.1 200 OK\r\nX: v\r\n\n",
  "HTTP/1.1 200 OK\r\nX: v\r\n\rZ",
  // Refused: the framing fields.
  "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
  "HTTP/1.1 200 OK\r\ncontent-length: 1\r\nCONTENT-LENGTH: 1\r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length: +1\r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length: 1 2\r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length: 7\t\r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551616\r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
  "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: \r\n\r\n",
  "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 1\r\n\r\n",
];
console.log("CONNECT answers:");
for (const answer of answers) {
  console.log(`  ${JSON.stringify(answer)}: ${await exchange("connect", answer)}`);
}
console.log("upgrade answers:");
for (const answer of [
  "HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: x\r\n\r\nfirst",
  "HTTP/1.1 101 Switching Protocols\r\nX: a\r\n b\r\n\r\n",
  "garbage\r\n\r\n",
  "HTTP/1.1 20 OK\r\n\r\n",
  "HTTP/1.1 101 OK\r\nbad header line\r\n\r\n",
  "HTTP/1.1 101 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
]) {
  console.log(`  ${JSON.stringify(answer)}: ${await exchange("upgrade", answer)}`);
}

// A bad first line is refused at once: no CRLF, and the connection stays open.
for (const [kind, answer] of [["connect", "garbage"], ["connect", "HTTX"], ["upgrade", "HTTP/1.1 2x"]]) {
  console.log(`${kind} answered ${JSON.stringify(answer)} and left open: ${await exchange(kind, answer, true)}`);
}

// The new owner reads late: what came behind the head, what came after it,
// and the end of the stream all wait for it.
for (const kind of ["connect", "upgrade"]) {
  script = (s) => {
    s.write(
      kind === "connect"
        ? "HTTP/1.1 200 OK\r\n\r\nEARLY"
        : "HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: x\r\n\r\nEARLY",
    );
    setTimeout(() => s.write("LATE1"), 30);
    setTimeout(() => s.end("LATE2"), 60);
  };
  const got = await new Promise((resolve) => {
    const req = request(kind);
    req.on(kind, (res, socket, head) => {
      setTimeout(() => {
        let data = head.toString();
        socket.on("data", (d) => (data += d));
        socket.on("end", () => resolve(data));
      }, 200);
    });
    req.on("error", (e) => resolve(`error ${e.code}`));
    req.end();
  });
  console.log(`${kind}, read 200 ms later: ${got}`);
}
server.close();
