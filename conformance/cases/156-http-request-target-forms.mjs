// The request target `http.request` puts on the wire, and the head a
// forward-proxy agent rewrites it through (measured on node v22.22.2).
//
// node writes `options.path` VERBATIM as the request target, in whichever
// form the caller chose: origin form (`/p?q`), absolute form for a forward
// proxy (`http://host/p` -- axios's `proxy` option, http-proxy-agent), `*`
// for OPTIONS, or authority form for CONNECT. oam used to force a leading
// `/` on anything that did not start with one, so every proxy got
// `GET /http://host/p` and `OPTIONS /*`.
//
// It also sets the Host header with setHeader() in the constructor, after
// the caller's headers and before the Authorization one, so an agent can
// read it back: http-proxy-agent builds the absolute-form target out of
// `req.getHeader('host')`, and re-renders the head with
// `req._implicitHeader()`. And an http/1.x SERVER reports an absolute-form
// target in req.url, which is how a proxy written in JS knows where to go.
//
// Header NAMES are lowercased before printing (oam sends them lowercase,
// node title-cases the ones it adds itself), and only the headers this case
// sets are printed, so a runtime's own default headers do not enter into it.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

// A raw recorder: it answers every head with 200 and keeps the first line.
const lines = [];
const heads = [];
const recorder = net.createServer((s) => {
  s.on("error", () => {});
  let buf = Buffer.alloc(0);
  s.on("data", (d) => {
    buf = Buffer.concat([buf, d]);
    const end = buf.indexOf("\r\n\r\n");
    if (end === -1) return;
    const head = buf.slice(0, end).toString("latin1").split("\r\n");
    buf = buf.slice(end + 4);
    lines.push(head[0]);
    heads.push(head.slice(1).map((l) => l.slice(0, l.indexOf(":")).toLowerCase()));
    s.write("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
  });
});
await new Promise((r) => recorder.listen(0, "127.0.0.1", r));
const rport = recorder.address().port;

// A stock agent goes over oam's own transport; one with its own
// createConnection goes over the socket it returns. The target must be the
// same on both.
class SocketAgent extends http.Agent {
  createConnection(options, callback) {
    return net.connect(options.port, options.host, callback);
  }
}
const socketAgent = new SocketAgent();

function send(options) {
  return new Promise((resolve) => {
    const req = http.request(Object.assign({ host: "127.0.0.1", port: rport }, options));
    req.on("connect", (res, socket) => {
      socket.destroy();
      resolve(`connect ${res.statusCode}`);
    });
    req.on("response", (res) => {
      res.resume();
      resolve(`response ${res.statusCode}`);
    });
    req.on("error", (e) => resolve(`error ${e.code}`));
    req.end();
  });
}

const forms = [
  ["origin", { path: "/p?q=1" }],
  ["absolute", { path: "http://abs.test:81/p?q=1" }],
  ["asterisk", { method: "OPTIONS", path: "*" }],
  ["authority (CONNECT)", { method: "CONNECT", path: "target.test:443" }],
];
for (const [name, options] of forms) {
  const stock = await send(Object.assign({ agent: false }, options));
  const over = await send(Object.assign({ agent: socketAgent }, options));
  console.log(`${name}: stock ${stock}, own socket ${over}`);
}
socketAgent.destroy();
console.log("request lines:");
for (const line of lines) console.log(`  ${line}`);

// The head a proxy agent rewrites: Host is an ordinary header from the
// constructor on, in node's order, and removing it really removes it.
const order = new SocketAgent();
await send({
  agent: order,
  auth: "u:p",
  headers: { "x-first": "1", "x-second": "2" },
  path: "/ordered",
});
const dropped = http.request({ host: "127.0.0.1", port: rport, path: "/x" });
const hostHeader = dropped.getHeader("host");
dropped.removeHeader("host");
await new Promise((resolve) => {
  dropped.on("response", (res) => {
    res.resume();
    resolve();
  });
  dropped.on("error", () => resolve());
  dropped.end();
});
order.destroy();
const shown = heads.map((names) =>
  names.filter((n) => n === "host" || n === "authorization" || n.startsWith("x-")),
);
console.log(`header order: ${shown[shown.length - 2].join(" ")}`);
console.log(`host header: ${hostHeader === `127.0.0.1:${rport}`}`);
recorder.close();

// The OutgoingMessage surface http-proxy-agent's connect() reaches for.
const probe = http.request({ host: "127.0.0.1", port: 1, path: "/p" });
probe.on("error", () => {});
console.log(
  `_implicitHeader ${typeof probe._implicitHeader}, outputData ${Array.isArray(probe.outputData)}, _header ${probe._header}`,
);
probe._implicitHeader();
console.log(`rendered: ${probe._header.split("\r\n")[0]}`);
try {
  probe._implicitHeader();
  console.log("rendered twice: no error");
} catch (e) {
  console.log(`rendered twice: ${e.code}`);
}
probe.destroy();

// The server end: node's req.url is the target as the client wrote it.
const seen = [];
const server = http.createServer((req, res) => {
  seen.push(`${req.method} ${req.url}`);
  res.end("ok");
});
server.on("connect", (req, socket) => {
  seen.push(`CONNECT ${req.url}`);
  socket.end("HTTP/1.1 200 OK\r\n\r\n");
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const sport = server.address().port;
function raw(line) {
  return new Promise((resolve) => {
    const s = net.connect(sport, "127.0.0.1", () => {
      s.write(`${line}\r\nHost: h.test\r\nConnection: close\r\n\r\n`);
    });
    // Read the answer, or the socket stays paused and never sees the FIN.
    s.resume();
    s.on("error", () => resolve());
    s.on("close", () => resolve());
  });
}
await raw("GET /p?q=1 HTTP/1.1");
await raw("GET http://abs.test:81/p?q=1 HTTP/1.1");
await raw("OPTIONS * HTTP/1.1");
await raw("CONNECT target.test:443 HTTP/1.1");
server.close();
console.log("server req.url:");
for (const s of seen) console.log(`  ${s}`);
