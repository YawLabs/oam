// dns.lookupService(address, port, callback) and dns.promises.lookupService,
// which oam did not have (a recorded surface gap). Measured on node v22.22.2:
// getnameinfo with NI_NAMEREQD; the callback gets (null, hostname, service)
// and the promise { hostname, service }; the service is the port's name or
// its number. The arguments are checked in node's order: the exact count
// (ERR_MISSING_ARGS), an IP literal (ERR_INVALID_ARG_VALUE), a port
// (ERR_SOCKET_BAD_PORT, a numeric string allowed), then the callback
// (ERR_INVALID_ARG_TYPE). A failure is a DNSException with syscall
// getnameinfo; the promises one has no hostname.
//
// Every address is on this host and both runtimes ask the same system
// resolver, so the names themselves are compared.
import dns from "node:dns";

const show = (v) => JSON.stringify(v);

function threw(label, fn) {
  try {
    const r = fn();
    if (r && typeof r.then === "function") r.catch(() => {});
    console.log(label, "ok");
  } catch (e) {
    console.log(label, e.name, e.code, e.message);
  }
}

console.log("lengths", dns.lookupService.length, dns.promises.lookupService.length,
  "same as dns/promises", (await import("node:dns/promises")).lookupService === dns.promises.lookupService);
console.log("keys", show(Object.keys(dns).slice(0, 3)), show(Object.keys(dns.promises).slice(0, 3)));

for (const [address, port] of [
  ["127.0.0.1", 22], ["127.0.0.1", 0], ["127.0.0.1", 443], ["127.0.0.1", 54321],
  ["::1", 80], ["127.0.0.1", "80"], ["127.0.0.1", " 80 "], ["127.0.0.1", "0x50"], ["127.0.0.1", "1e1"],
]) {
  const cb = await new Promise((resolve) => {
    dns.lookupService(address, port, (err, hostname, service, ...rest) =>
      resolve(err ? `error ${err.code}` : `${show(hostname)} ${show(service)} extra ${rest.length}`));
  });
  const pr = await dns.promises.lookupService(address, port).then(
    (answer) => `${show(Object.keys(answer))} ${show(answer)}`,
    (err) => `error ${err.code}`,
  );
  console.log("lookupService", show(address), show(port), "|", cb, "|", pr);
}

// Asynchronous, whatever the outcome.
let sync = true;
dns.lookupService("127.0.0.1", 80, () => console.log("callback sync", sync));
sync = false;
await new Promise((resolve) => setTimeout(resolve, 0));

const fn = () => {};
threw("no arguments", () => dns.lookupService());
threw("two arguments", () => dns.lookupService("127.0.0.1", 80));
threw("four arguments", () => dns.lookupService("127.0.0.1", 80, fn, 1));
for (const address of ["foo", 1234, undefined, ""]) {
  threw(`address ${show(address)}`, () => dns.lookupService(address, 80, fn));
}
for (const port of [65536, -1, 1.5, null, undefined, "", "abc", true, [80], {}]) {
  threw(`port ${show(port)}`, () => dns.lookupService("127.0.0.1", port, fn));
}
threw("callback undefined", () => dns.lookupService("127.0.0.1", 80, undefined));
threw("callback string", () => dns.lookupService("127.0.0.1", 80, "x"));
threw("promises one argument", () => dns.promises.lookupService("127.0.0.1"));
threw("promises three arguments", () => dns.promises.lookupService("127.0.0.1", 80, fn));
threw("promises bad address", () => dns.promises.lookupService("foo", 80));
threw("promises bad port", () => dns.promises.lookupService("127.0.0.1", 65536));
