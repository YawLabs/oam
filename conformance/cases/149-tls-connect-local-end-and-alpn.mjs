// tls.connect takes the local end and the ALPN offer together: a connection
// given localAddress / localPort and ALPNProtocols leaves from that address
// and port and its ClientHello offers those protocols, and http2.connect,
// which hands its options to tls.connect for an https: origin, does the same
// with h2. oam's native connect takes the two as separate arguments, which
// were added on separate branches; before both were in, one or the other
// was dropped.
//
// A raw TCP server reads each connection's first TLS record (the
// ClientHello), notes the peer's address and port, and closes it; no
// handshake completes.
import http2 from "node:http2";
import net from "node:net";
import tls from "node:tls";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

// The ALPN extension (16) of a ClientHello record, or null without one.
function alpnOffer(hello) {
  let at = 5 + 4 + 2 + 32;
  at += 1 + hello[at];
  at += 2 + hello.readUInt16BE(at);
  at += 1 + hello[at];
  const end = at + 2 + hello.readUInt16BE(at);
  at += 2;
  while (at + 4 <= end) {
    const type = hello.readUInt16BE(at);
    const len = hello.readUInt16BE(at + 2);
    if (type === 16) {
      const names = [];
      for (let i = at + 6; i < at + 4 + len; i += 1 + hello[i]) {
        names.push(hello.toString("latin1", i + 1, i + 1 + hello[i]));
      }
      return names;
    }
    at += 4 + len;
  }
  return null;
}

const hellos = [];
const sniffer = net.createServer((c) => {
  const peer = { address: c.remoteAddress, port: c.remotePort };
  let buf = Buffer.alloc(0);
  c.on("data", (d) => {
    buf = Buffer.concat([buf, d]);
    if (buf.length >= 5 && buf.length >= 5 + buf.readUInt16BE(3)) {
      hellos.push({ peer, alpn: alpnOffer(buf) });
      c.destroy();
    }
  });
  c.on("error", () => {});
});
await new Promise((r) => sniffer.listen(0, "127.0.0.1", r));
const port = sniffer.address().port;

async function freePort() {
  const s = net.createServer();
  await new Promise((r) => s.listen(0, "127.0.0.1", r));
  const p = s.address().port;
  await new Promise((r) => s.close(r));
  return p;
}

function report(label, localPort) {
  const hello = hellos.shift();
  if (!hello) {
    console.log(label, "no ClientHello");
    return;
  }
  console.log(
    label,
    "| from",
    hello.peer.address,
    hello.peer.port === localPort ? "the local port" : "another port",
    "| offers",
    JSON.stringify(hello.alpn),
  );
}

async function viaTls(label, options) {
  const localPort = await freePort();
  const s = tls.connect({
    host: "127.0.0.1",
    port,
    servername: "localhost",
    localAddress: "127.0.0.1",
    localPort,
    ...options,
  });
  s.on("error", () => {});
  await new Promise((r) => s.on("close", r));
  report(label, localPort);
}

await viaTls("tls.connect ALPNProtocols", { ALPNProtocols: ["h2", "x-test"] });
await viaTls("tls.connect ALPN wire buffer", { ALPNProtocols: Buffer.from([2, 0x68, 0x32, 1, 0x7a]) });
await viaTls("tls.connect no ALPNProtocols", {});

// http2.connect: tls.connect with the session's options, offering h2.
for (const [label, options] of [
  ["http2.connect https", {}],
  ["http2.connect https allowHTTP1", { allowHTTP1: true }],
]) {
  const localPort = await freePort();
  const session = http2.connect(`https://127.0.0.1:${port}`, {
    ...options,
    servername: "localhost",
    localAddress: "127.0.0.1",
    localPort,
  });
  session.on("error", () => {});
  await new Promise((r) => session.on("close", r));
  report(label, localPort);
}

sniffer.close();
