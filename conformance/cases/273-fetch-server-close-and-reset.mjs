// What fetch() and http.get report when the server closes the connection
// (an orderly close, no reset) or resets it, before the response head or in
// the middle of the body.
//
// fetch: a close is undici's SocketError -- `other side closed`, code
// UND_ERR_SOCKET, the socket it was on (its two ends, the bytes it carried)
// -- the cause of `fetch failed` before the head, and of `terminated` when
// the body is being read; a reset mid-body is `terminated` with the socket's
// `read ECONNRESET` as the cause. http.get: a close before the head is
// `socket hang up`; a close or reset mid-body aborts the response
// (ECONNRESET `aborted`); a malformed chunk-size line is first the parser's
// HPE_INVALID_CHUNK_SIZE on the request, over oam's own transport and an
// agent's socket alike.
//
// bytesWritten is a number, not compared: it counts the request head, whose
// user-agent is the runtime's own.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const listen = (server) =>
  new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server.address().port)));

const head = (length) => `HTTP/1.1 200 OK\r\nContent-Length: ${length}\r\n\r\n`;
const CHUNKED = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n";

// The server: on the request, `act(conn)`.
async function server(act) {
  const srv = net.createServer((conn) => {
    conn.on("error", () => {});
    conn.once("data", () => act(conn));
  });
  return { srv, port: await listen(srv) };
}

const later = (fn) => setTimeout(fn, 30);
const closeAfter = (bytes) => (conn) => {
  if (bytes) conn.write(bytes);
  later(() => conn.end());
};
const resetAfter = (bytes) => (conn) => {
  conn.write(bytes);
  later(() => conn.resetAndDestroy());
};
const badChunk = (conn) => {
  conn.write(CHUNKED);
  later(() => conn.write("zz\r\n"));
};

function describeCause(cause, port) {
  const lines = [];
  // The class chain of undici's error (node's errno errors hide theirs).
  const chain = [];
  if ("socket" in cause) {
    for (let p = Object.getPrototypeOf(cause); p && p !== Object.prototype; p = Object.getPrototypeOf(p)) {
      chain.push(p.constructor.name);
    }
  }
  lines.push(`cause ${chain.join(" < ")} | ${cause.name}: ${cause.message} | code ${cause.code}` +
    ` | syscall ${cause.syscall} | errno ${typeof cause.errno}`);
  lines.push(`  own ${JSON.stringify(Object.getOwnPropertyNames(cause))}` +
    ` symbols ${Object.getOwnPropertySymbols(cause).map(String).join(" ")}`);
  if ("socket" in cause) {
    const s = cause.socket;
    lines.push(`  socket keys ${JSON.stringify(Object.keys(s))}`);
    lines.push(`  socket ${s.localAddress} ${typeof s.localPort} -> ${s.remoteAddress} ${s.remoteFamily}` +
      ` | remotePort is the server's ${s.remotePort === port} | timeout ${s.timeout}` +
      ` | bytesRead ${s.bytesRead} | bytesWritten ${typeof s.bytesWritten}`);
  }
  return lines;
}

async function viaFetch(label, act, consume) {
  const { srv, port } = await server(act);
  const lines = [];
  try {
    const res = await fetch(`http://127.0.0.1:${port}/`);
    lines.push(`resolved ${res.status}`);
    try {
      await consume(res);
      lines.push("body read to the end");
    } catch (e) {
      lines.push(`body: ${e.constructor.name} ${e.message} | own ${JSON.stringify(Object.keys(e))}`);
      if (e.cause) lines.push(...describeCause(e.cause, port));
    }
  } catch (e) {
    lines.push(`rejected: ${e.constructor.name} ${e.message}`);
    if (e.cause) lines.push(...describeCause(e.cause, port));
  }
  console.log(`--- fetch: ${label}`);
  for (const line of lines) console.log(`  ${line}`);
  srv.close();
}

const text = (res) => res.text();
async function reader(res) {
  const r = res.body.getReader();
  for (;;) {
    const { done } = await r.read();
    if (done) return;
  }
}

await viaFetch("close before the head", closeAfter(null), text);
await viaFetch("close halfway through the head", closeAfter("HTTP/1.1 200 OK\r\nContent-"), text);
await viaFetch("close mid-body (content-length), text()", closeAfter(head(100) + "x"), text);
await viaFetch("close mid-body (content-length), a reader", closeAfter(head(100) + "x"), reader);
await viaFetch("close mid-body (chunked)", closeAfter(CHUNKED), text);
await viaFetch("reset mid-body (content-length)", resetAfter(head(100) + "x"), text);
await viaFetch("reset mid-body (chunked), a reader", resetAfter(CHUNKED), reader);
await viaFetch("close after a close-delimited body", closeAfter("HTTP/1.1 200 OK\r\n\r\nabc"), text);

// Every event on the request and the response, until the request closes
// and, once there is one, the response too.
async function viaGet(label, act, agent) {
  const { srv, port } = await server(act);
  const events = [];
  const closed = [];
  await new Promise((resolve) => {
    const req = http.get({ host: "127.0.0.1", port, agent }, (res) => {
      events.push(`response ${res.statusCode}`);
      res.on("aborted", () => events.push("res aborted"));
      res.on("error", (e) => events.push(`res error ${e.code} ${e.message}`));
      res.on("end", () => events.push("res end"));
      closed.push(new Promise((r) => res.on("close", () => {
        events.push("res close");
        r();
      })));
      res.resume();
    });
    req.on("error", (e) =>
      events.push(`req error ${e.code} ${e.message} | syscall ${e.syscall} | reason ${e.reason}`));
    req.on("close", () => {
      events.push("req close");
      resolve();
    });
  });
  await Promise.all(closed);
  console.log(`--- http.get: ${label}`);
  console.log(`  ${events.join(", ")}`);
  srv.close();
}

await viaGet("close before the head", closeAfter(null), false);
await viaGet("close mid-body", closeAfter(head(100) + "x"), false);
await viaGet("reset mid-body", resetAfter(head(100) + "x"), false);
await viaGet("reset mid-body, the default agent", resetAfter(head(100) + "x"), undefined);
await viaGet("a malformed chunk-size line", badChunk, false);
await viaGet("a malformed chunk-size line, the default agent", badChunk, undefined);
