// An IPv6 address with a zone id (`fe80::1%eth0`, `::1%1`) is an address:
// net.isIP / isIPv6 accept node's grammar, a zoned literal host is dialled
// without a lookup, a `lookup` hook may answer one, and an error names the
// address with its zone, as written.
//
// Regression guard (#162): isIP refused every zoned address (fixed in
// 0.16.x); after that a zoned hook answer was still refused before the dial
// (`tcpConnect: pin ip '::1%1' is not an IP`), and a zoned literal was
// resolved through getaddrinfo, its errors naming `::1` without the zone.
//
// The zone `oamnone0` names no interface anywhere, so libuv reads it as
// scope id 0 (Windows' atoi, POSIX's if_nametoindex) and the connect reaches
// the ::1 listener. A refused connect prints only the shape both runtimes
// share on every platform: whether the zone is in the message and address.
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

for (const s of [
  "fe80::1%lo0", "fe80::1%eth0", "fe80::1%1", "::1%1", "::%x", "::ffff:1.2.3.4%3",
  "fe80::1%a-b.c:d", "fe80::1%", "fe80::1%a%b", "fe80::1%en 0", "fe80::1%eth_0",
  "fe80::1%a/b", "fe80::1%é", "1.2.3.4%1", "%1",
]) {
  console.log(JSON.stringify(s), net.isIP(s), net.isIPv6(s), net.isIPv4(s));
}

const server = net.createServer((socket) => socket.end("hi"));
await new Promise((resolve) => server.listen(0, "::1", resolve));
const { port } = server.address();
const closed = await new Promise((resolve) => {
  const probe = net.createServer();
  probe.listen(0, "::1", () => {
    const p = probe.address().port;
    probe.close(() => resolve(p));
  });
});

function connect(label, options, portShown) {
  return new Promise((resolve) => {
    const socket = net.connect(options);
    const seen = [];
    socket.on("lookup", (err, address, family) => seen.push(`lookup ${address} ${family}`));
    socket.on("connect", () => seen.push(`connect ${socket.remoteAddress} ${socket.remoteFamily}`));
    socket.on("data", (d) => seen.push(`data ${d}`));
    socket.on("end", () => socket.end());
    socket.on("error", (e) => {
      const message = e.message.replace(String(portShown), "PORT");
      seen.push(`error syscall=${e.syscall} address=${e.address} zone-in-message=${/%[^:]+:PORT$/.test(message)}`);
    });
    socket.on("close", () => {
      console.log(label);
      for (const line of seen) console.log(`  ${line}`);
      resolve();
    });
  });
}

await connect("literal ::1%oamnone0", { host: "::1%oamnone0", port }, port);
await connect("literal ::1%1, closed port", { host: "::1%1", port: closed }, closed);
const answer = (address) => (host, options, cb) =>
  options.all ? cb(null, [{ address, family: 6 }]) : cb(null, address, 6);
await connect("hook ::1%oamnone0", { host: "zoned.test", port, lookup: answer("::1%oamnone0") }, port);
await connect("hook ::1%1, closed port", { host: "zoned.test", port: closed, lookup: answer("::1%1") }, closed);

server.close();
console.log("done");
