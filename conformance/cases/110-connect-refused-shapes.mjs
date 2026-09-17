// A refused or unresolvable connect through net.connect and tls.connect
// reports node's exact error (#143). An IP-literal host is ONE attempt and a
// plain Error -- node's ExceptionWithHostPort: its prototype is a subclass of
// Error.prototype, own `stack` and `message`, then enumerable errno, code,
// syscall, address, port in that order, and a `connect CODE address:port`
// message naming the address as written. A name that resolves to several
// addresses is tried address by address, and when every one fails the error is
// node's NodeAggregateError: `code` from the first attempt its only enumerable
// key, the attempts as non-enumerable `errors`, no own message, a stack header
// `AggregateError [CODE]: `. oam used to report `localhost` itself as the
// address, name only the last refusal, and (tls) word it like an fs error.
//
// `localhost` prints the full shape only on win32, where node and oam both call
// getaddrinfo with no flags. Off Windows node adds AI_ADDRCONFIG and oam does
// not (a documented divergence), so on a host with no routable IPv6 address
// node may resolve localhost to 127.0.0.1 alone -- a plain error -- while oam
// also tries ::1 -- an aggregate. There only invariants that hold for either
// list are printed. A failure connect(2) reports synchronously carries the
// local socket's ephemeral port (` - Local (0.0.0.0:55035)`), so that detail is
// redacted everywhere.
import net from "node:net";
import tls from "node:tls";

const FULL = process.platform === "win32";

// A port that was listening a moment ago and is closed now.
const probe = net.createServer();
await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
const port = probe.address().port;
await new Promise((resolve) => probe.close(resolve));

const P = (s) => String(s).replaceAll(String(port), "PORT");
const unlocal = (message) => String(message).replace(/ - Local \([^)]*\)$/, " - Local (LOCAL)");
const timing = (since) => (Date.now() - since < 1000 ? "fast" : "slow");

const own = (e) =>
  Reflect.ownKeys(e)
    .map((k) => `${String(k)}${Object.getOwnPropertyDescriptor(e, k).enumerable ? "+" : "-"}`)
    .join(",");

function plain(e) {
  return P(JSON.stringify({
    ctor: e.constructor.name,
    protoIsError: Object.getPrototypeOf(e) === Error.prototype,
    message: unlocal(e.message),
    own: own(e),
    keys: Object.keys(e),
    errno: e.errno, code: e.code, syscall: e.syscall,
    address: e.address, port: e.port, hostname: e.hostname,
  }));
}

function full(e) {
  if (!(e instanceof AggregateError)) return plain(e);
  return P(JSON.stringify({
    ctor: e.constructor.name,
    protoIsAggregate: Object.getPrototypeOf(e) === AggregateError.prototype,
    protoParentIsAggregate: Object.getPrototypeOf(Object.getPrototypeOf(e)) === AggregateError.prototype,
    ownMessage: Object.hasOwn(e, "message"),
    own: own(e),
    keys: Object.keys(e),
    code: e.code,
    string: String(e),
    header: String(e.stack).split("\n")[0],
    json: JSON.stringify(e),
  })) + "\n    " + e.errors.map(plain).join("\n    ");
}

function invariant(e) {
  const agg = e instanceof AggregateError;
  const errs = agg ? e.errors : [e];
  const childOk = (c) =>
    Object.keys(c).join() === "errno,code,syscall,address,port" &&
    c.syscall === "connect" && c.port === port && typeof c.errno === "number" &&
    unlocal(c.message).replace(" - Local (LOCAL)", "") === `connect ${c.code} ${c.address}:${port}` &&
    net.isIP(c.address) !== 0;
  return JSON.stringify({
    aggregateHasSeveral: agg ? errs.length >= 2 : true,
    aggregateShape: agg
      ? Object.keys(e).join() === "code" && e.code === errs[0].code && !Object.hasOwn(e, "message") &&
        String(e.stack).startsWith(`AggregateError [${e.code}]: `) && e.constructor === AggregateError
      : true,
    childrenOk: errs.every(childOk),
    refused127: errs.some((c) => c.address === "127.0.0.1" && c.code === "ECONNREFUSED"),
  });
}

const render = (host, e) => (host === "localhost" && !FULL ? invariant(e) : full(e));

function failure(label, host, open) {
  return new Promise((resolve) => {
    const since = Date.now();
    const socket = open();
    socket.on("connect", () => {
      console.log(label, host, "connected?!");
      socket.destroy();
      resolve();
    });
    socket.on("error", (e) => {
      console.log(label, host, timing(since), render(host, e));
      resolve();
    });
  });
}

for (const host of ["127.0.0.1", "::1", "localhost"]) {
  await failure("net", host, () => net.connect({ host, port }));
  await failure("tls", host, () => tls.connect({ host, port, rejectUnauthorized: false }));
}

// Unresolvable: node's DNSException, never aggregated.
const nowhere = "oam-conformance-110.invalid";
await failure("net", nowhere, () => net.connect({ host: nowhere, port }));
await failure("tls", nowhere, () => tls.connect({ host: nowhere, port, rejectUnauthorized: false }));

// An unspecified target reaches a loopback listener (libuv dials 127.0.0.1 on
// Windows; the kernel does elsewhere).
const listener = net.createServer((c) => c.end());
await new Promise((resolve) => listener.listen(0, "127.0.0.1", resolve));
await new Promise((resolve) => {
  const since = Date.now();
  const c = net.connect(listener.address().port, "0.0.0.0");
  c.on("connect", () => console.log("net 0.0.0.0 -> 127.0.0.1 listener:", c.remoteAddress, timing(since)));
  c.on("error", (e) => console.log("net 0.0.0.0 error?!", e.code));
  c.on("close", resolve);
});
listener.close();
