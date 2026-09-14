// tls.TLSSocket carries the net.Socket API (#132). In Node it extends
// net.Socket, so every public member of net.Socket is reachable on a
// tls.connect() socket and `instanceof net.Socket` holds; ioredis relies on
// setNoDelay/setKeepAlive on every connection and crashed over rediss://
// when they were missing. The walk is mechanical so the next missing member
// is caught here, not by a library at runtime.
//
// The list is the RUNNING runtime's: net.Socket.prototype's public names
// plus the public own keys of a fresh net.Socket (Node keeps most of the
// socket state on the prototype; oam's net.Socket keeps bufferSize,
// bytesRead, remoteAddress and friends as instance fields, which a
// prototype-only walk never saw). A Node member that oam's net.Socket ALSO
// lacks is invisible to the walk on oam by construction -- those are
// listed in docs/node-divergences.md under the TLSSocket entry.
import tls from "node:tls";
import net from "node:net";

const s = tls.connect({ host: "127.0.0.1", port: 1 });
s.on("error", () => {});

const wanted = [...new Set([
  ...Object.getOwnPropertyNames(net.Socket.prototype),
  ...Object.keys(new net.Socket()),
])].filter((name) => !name.startsWith("_") && name !== "constructor");
console.log("missing from TLSSocket:", JSON.stringify(wanted.filter((name) => !(name in s))));
console.log("instanceof net.Socket:", String(s instanceof net.Socket));
console.log("chainable:", String(
  s.setNoDelay(true) === s && s.setKeepAlive(true, 0) === s && s.setTimeout(0) === s &&
  s.ref() === s && s.unref() === s,
));
console.log("before connect:", JSON.stringify({ pending: s.pending, readyState: s.readyState, address: s.address(), bufferSize: s.bufferSize, timeout: s.timeout }));
