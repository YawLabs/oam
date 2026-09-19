// The localAddress / localPort connect options bind the socket before it
// dials, as node's net.connect does (lib/net.js internalConnect): the peer
// sees the connection come from that address and port, a bind that fails is
// the connect's error (`bind CODE address[:port]`, syscall 'bind'), and a
// bad value throws. http.request and tls.connect pass them through. oam used
// to validate localAddress and then ignore both, so a connection meant to
// leave from one address or port left from the default one.
import net from "node:net";
import tls from "node:tls";
import http from "node:http";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

let seen = null;
const server = net.createServer((c) => {
  seen = { address: c.remoteAddress, port: c.remotePort };
  c.on("error", () => {});
  c.on("data", () => c.end("HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

// A local port nothing is using: take one from the OS, then let it go.
async function freePort() {
  const s = net.createServer();
  await new Promise((r) => s.listen(0, "127.0.0.1", r));
  const p = s.address().port;
  await new Promise((r) => s.close(r));
  return p;
}

const describe = (e) => `ERROR ${e.code} syscall=${e.syscall} address=${e.address} port=${e.port} ${JSON.stringify(e.message)}`;

async function viaNet(label, options, useTls) {
  seen = null;
  let outcome;
  try {
    outcome = await new Promise((resolve) => {
      const s = useTls ? tls.connect({ ...options, rejectUnauthorized: false }) : net.connect(options);
      s.on("connect", () => {
        const local = { address: s.localAddress, port: s.localPort };
        setTimeout(() => {
          resolve(`connect local=${local.address}:${local.port === options.localPort ? "localPort" : "other"} server saw ${seen && seen.address}:${seen && seen.port === options.localPort ? "localPort" : "other"}`);
          s.destroy();
        }, 20);
      });
      s.on("error", (e) => {
        // The server here speaks no TLS: a handshake that fails still shows
        // where the connection came from.
        if (useTls && e.syscall !== "bind") {
          resolve(`handshake failed, server saw ${seen && seen.address}:${seen && seen.port === options.localPort ? "localPort" : "other"}`);
        } else {
          resolve(describe(e));
        }
      });
    });
  } catch (e) {
    outcome = `THROW ${e.code} ${JSON.stringify(e.message)}`;
  }
  console.log(`${label}: ${outcome}`);
}

let lp = await freePort();
await viaNet("net localPort", { host: "127.0.0.1", port, localPort: lp });
lp = await freePort();
await viaNet("net localAddress + localPort", { host: "127.0.0.1", port, localAddress: "127.0.0.1", localPort: lp });
lp = await freePort();
await viaNet("net localhost + localAddress + localPort", { host: "localhost", port, family: 4, localAddress: "127.0.0.1", localPort: lp });
await viaNet("net localAddress not on this host", { host: "127.0.0.1", port, localAddress: "192.0.2.1" });
await viaNet("net localAddress not on this host + localPort", { host: "127.0.0.1", port, localAddress: "192.0.2.1", localPort: 40123 });
await viaNet("net IPv6 localAddress, IPv4 peer", { host: "127.0.0.1", port, localAddress: "::1" });
await viaNet("net localAddress not an IP", { host: "127.0.0.1", port, localAddress: "localhost" });
await viaNet("net localPort not a number", { host: "127.0.0.1", port, localPort: "4000" });
await viaNet("net localAddress empty", { host: "127.0.0.1", port, localAddress: "" });
lp = await freePort();
await viaNet("tls localPort", { host: "127.0.0.1", port, localPort: lp }, true);
await viaNet("tls localAddress not on this host", { host: "127.0.0.1", port, localAddress: "192.0.2.1" }, true);

// http.request passes both to the socket it connects.
for (const [label, extra] of [["http localPort", {}], ["http localAddress + localPort, keepAlive agent", { agent: new http.Agent({ keepAlive: true }) }]]) {
  lp = await freePort();
  seen = null;
  const outcome = await new Promise((resolve) => {
    const req = http.get({ host: "127.0.0.1", port, localAddress: "127.0.0.1", localPort: lp, ...extra }, (res) => {
      res.resume();
      res.on("end", () => resolve(`${res.statusCode} server saw ${seen && seen.address}:${seen && seen.port === lp ? "localPort" : "other"}`));
    });
    req.on("error", (e) => resolve(describe(e)));
  });
  console.log(`${label}: ${outcome}`);
  if (extra.agent) extra.agent.destroy();
}
const bad = await new Promise((resolve) => {
  http.get({ host: "127.0.0.1", port, localAddress: "192.0.2.1" }, () => resolve("response?!"))
    .on("error", (e) => resolve(describe(e)));
});
console.log(`http localAddress not on this host: ${bad}`);
server.close();
