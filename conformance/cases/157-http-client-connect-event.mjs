// A CONNECT request's answer is an UPGRADE, not a response (measured on
// node v22.22.2). node's parser returns "skip the body and treat this as an
// upgrade" for every answer to a CONNECT, whatever its status, so:
//
//   - the ClientRequest emits 'connect' (never 'response') with the answer,
//     the socket, and whatever bytes came behind the head -- a declared body
//     is simply the first bytes on the tunnel;
//   - the answer has upgrade true and complete true, and req.res is it;
//   - with no 'connect' listener the socket is destroyed;
//   - either way the request is destroyed and emits 'close'.
//
// That is what every proxy agent built on http.request({method:'CONNECT'})
// -- tunnel (behind @actions/http-client), hpagent -- waits for. oam used
// to deliver a 2xx answer as an ordinary 'response', so those agents'
// requests never settled once the agent's own socket was honoured.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

// A raw proxy that answers each CONNECT with whatever the case asks for.
let answer = "";
const seen = [];
const proxy = net.createServer((s) => {
  s.on("error", () => {});
  let buf = Buffer.alloc(0);
  s.on("data", (d) => {
    buf = Buffer.concat([buf, d]);
    if (buf.indexOf("\r\n\r\n") === -1) return;
    seen.push(buf.slice(0, buf.indexOf("\r\n")).toString("latin1"));
    buf = Buffer.alloc(0);
    s.write(answer);
  });
});
await new Promise((r) => proxy.listen(0, "127.0.0.1", r));
const port = proxy.address().port;

class SocketAgent extends http.Agent {
  createConnection(options, callback) {
    return net.connect(options.port, options.host, callback);
  }
}

function connect(label, reply, { listen = true, agent = false } = {}) {
  answer = reply;
  return new Promise((resolve) => {
    const events = [];
    const req = http.request({
      host: "127.0.0.1",
      port,
      method: "CONNECT",
      path: "target.test:443",
      agent,
    });
    if (listen) {
      req.on("connect", (res, socket, head) => {
        events.push(
          `connect ${res.statusCode} ${JSON.stringify(res.statusMessage)}` +
            ` upgrade=${res.upgrade} complete=${res.complete} isRes=${req.res === res}` +
            ` headers=${JSON.stringify(res.headers)} head=${JSON.stringify(head.toString("latin1"))}` +
            ` socketDestroyed=${socket.destroyed}`,
        );
        socket.destroy();
      });
    }
    req.on("response", (res) => {
      res.resume();
      events.push(`response ${res.statusCode}`);
    });
    req.on("error", (e) => events.push(`error ${e.code}`));
    req.on("close", () => events.push(`close destroyed=${req.destroyed}`));
    req.end();
    const t = setTimeout(() => {
      resolve(`${label}: ${events.length ? events.join(" | ") : "NOTHING"}`);
    }, 300);
    if (t.unref) t.unref();
  });
}

const established = "HTTP/1.1 200 Connection Established\r\n\r\n";
console.log(await connect("established", established));
console.log(await connect("bytes behind the head", "HTTP/1.1 200 OK\r\nx-a: b\r\n\r\nFIRST"));
console.log(
  await connect(
    "refused with a body",
    "HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 5\r\n\r\nnope!",
  ),
);
console.log(
  await connect(
    "refused, chunked body",
    "HTTP/1.1 502 Bad Gateway\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
  ),
);
console.log(await connect("no listener", established, { listen: false }));
const agent = new SocketAgent();
console.log(await connect("over the agent's own socket", established, { agent }));
agent.destroy();
proxy.close();
console.log(`the proxy saw: ${new Set(seen).size} distinct request lines, ${seen[0]}`);
