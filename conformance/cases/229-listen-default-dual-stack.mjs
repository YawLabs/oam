// A server's listen() without a host binds `::` dual-stack, as node's does,
// for every server kind; an explicit `::` is dual-stack too unless
// `ipv6Only`; and address() reports the family of the address bound.
//
// Regression guard (#172): net and tls bound 0.0.0.0 and http, https and
// http2 127.0.0.1 -- an http server started without a host could only be
// reached from the same machine, no default listener took an IPv6 client,
// `listen(0, '::')` took IPv6 clients only, and address() said IPv4 for
// every one of them.
import net from "node:net";
import tls from "node:tls";
import http from "node:http";
import https from "node:https";
import http2 from "node:http2";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const kinds = {
  net: () => net.createServer((socket) => socket.end()),
  tls: () => tls.createServer({}),
  http: () => http.createServer((req, res) => res.end("ok")),
  https: () => https.createServer({}),
  http2: () => http2.createServer(),
};

// Whether a plain TCP connect from `host` reaches the port.
function reach(host, port) {
  return new Promise((resolve) => {
    const socket = net.connect({ host, port });
    socket.on("connect", () => {
      socket.on("error", () => {});
      socket.destroy();
      resolve("connects");
    });
    socket.on("error", (e) => resolve(e.code));
  });
}

function listen(server, ...args) {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(...args, () => resolve(server.address()));
  });
}

const shape = (a) => JSON.stringify({ ...a, port: typeof a.port });

for (const [name, make] of Object.entries(kinds)) {
  for (const [label, args] of [
    ["listen(0)", [0]],
    ["listen(0, '::')", [0, "::"]],
    ["listen({ port: 0, host: '::', ipv6Only: true })", [{ port: 0, host: "::", ipv6Only: true }]],
    ["listen({ port: 0, ipv6Only: true })", [{ port: 0, ipv6Only: true }]],
    ["listen(0, '0.0.0.0')", [0, "0.0.0.0"]],
  ]) {
    const server = make();
    server.on("connection", (socket) => socket.on("error", () => {}));
    server.on("tlsClientError", () => {});
    const address = await listen(server, ...args);
    console.log(`${name} ${label}: ${shape(address)} | 127.0.0.1 ${await reach("127.0.0.1", address.port)} | ::1 ${await reach("::1", address.port)}`);
    await new Promise((resolve) => server.close(resolve));
  }
}

// Which address `localhost` reaches first depends on AI_ADDRCONFIG off
// Windows, which node's net passes and oam's resolver does not
// (docs/node-divergences.md entry 37): there a line through `localhost`
// says only that it connected.
const viaLocalhost = (text) => (process.platform === "win32" ? text : "connected");

// Both ends of net.connect(port) against a default listener: the client
// looks up `localhost`, and an IPv4 client of a dual-stack server is a
// v4-mapped IPv6 address on the server's side.
{
  const seen = [];
  const server = net.createServer((socket) => {
    seen.push(`server sees ${socket.remoteFamily} ${socket.remoteAddress}, local ${socket.localFamily} ${socket.localAddress}`);
    socket.end();
  });
  const { port } = await listen(server, 0);
  for (const args of [[port], [port, "127.0.0.1"], [port, "::1"]]) {
    const client = net.connect(...args);
    await new Promise((resolve) => client.on("connect", resolve));
    const sees = `client sees ${client.remoteFamily} ${client.remoteAddress}`;
    await new Promise((resolve) => client.on("close", resolve).resume());
    if (args.length === 1) {
      console.log(`net.connect(port): ${viaLocalhost(sees)}`);
      console.log(`  ${viaLocalhost(seen.shift())}`);
    } else {
      console.log(`net.connect(port, '${args[1]}'): ${sees}`);
      console.log(`  ${seen.shift()}`);
    }
  }
  server.close();
}

// http.get through `localhost` reaches a default http server.
{
  const server = http.createServer((req, res) => res.end(`via ${req.socket.remoteFamily}`));
  const { port } = await listen(server, 0);
  const body = await new Promise((resolve, reject) => {
    http.get(`http://localhost:${port}/`, (res) => {
      let text = "";
      res.on("data", (chunk) => (text += chunk));
      res.on("end", () => resolve(`${res.statusCode} ${viaLocalhost(text)}`));
    }).on("error", reject);
  });
  console.log(`http.get localhost: ${body}`);
  server.close();
}

// An http server's req.socket on an explicit `::` listener (entry 39): an
// IPv4 client is accepted, and both ends read as v4-mapped IPv6.
{
  const server = http.createServer((req, res) => {
    const s = req.socket;
    res.end(`${s.remoteFamily} ${s.remoteAddress}, local ${s.localFamily} ${s.localAddress}`);
  });
  const { port } = await listen(server, 0, "::");
  for (const host of ["127.0.0.1", "::1"]) {
    const body = await new Promise((resolve) => {
      http
        .get({ host, port, agent: false }, (res) => {
          let text = "";
          res.on("data", (chunk) => (text += chunk));
          res.on("end", () => resolve(text));
        })
        .on("error", (e) => resolve(`error ${e.code}`));
    });
    console.log(`http '::' req.socket from ${host}: ${body}`);
  }
  server.close();
}

console.log("done");
