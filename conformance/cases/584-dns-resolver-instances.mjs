// dns.Resolver and dns.promises.Resolver instances, each its own channel
// (#453). Measured on node v22.22.2:
//
// * dns.promises.Resolver exists and is node:dns/promises' Resolver; it is a
//   different class from dns.Resolver, over one shared ResolverBase whose
//   cancel / getServers / setServers / setLocalAddress are the same functions
//   on both. oam had no dns.promises.Resolver at all, so `import { Resolver }
//   from "node:dns/promises"` failed at link time.
// * new Resolver(options) validates timeout (-1..2^31-1), tries (1..2^31-1)
//   and maxTimeout (uint32), in that order; a non-object options is ignored.
//   oam read none of them.
// * an instance starts from the SYSTEM servers whatever dns.setServers() did;
//   its own setServers() reaches no other instance and neither default. oam
//   refused it with ENOSYS (every instance shared the system channel).
// * `timeout` / `tries` decide how long and how often a silent server is
//   asked; cancel() fails every pending query of that instance with
//   ECANCELLED ("queryA ECANCELLED <host>", errno undefined) and leaves the
//   instance usable. oam's cancel() did nothing.
// * setLocalAddress() validates its two addresses and binds the queries'
//   source; oam had no setLocalAddress.
// * dns.NOTFOUND and the other c-ares codes are the strings an error's
//   .code carries ("ENOTFOUND"), on dns and dns.promises alike. oam had them
//   on dns only, as the bare names ("NOTFOUND").
//
// Every server is a node:dgram socket on 127.0.0.1, so nothing here touches
// the network; ports are masked in the output.
import dgram from "node:dgram";
import dns from "node:dns";
import { Resolver as ImportedResolver } from "node:dns/promises";

// ---- a minimal DNS responder ---------------------------------------------

function parseQuery(buf) {
  let off = 12;
  const labels = [];
  while (buf[off] !== 0) {
    labels.push(buf.subarray(off + 1, off + 1 + buf[off]).toString("latin1"));
    off += buf[off] + 1;
  }
  off += 1;
  return { id: buf.readUInt16BE(0), type: buf.readUInt16BE(off), end: off + 4 };
}

function answerA(q, query, address) {
  const header = Buffer.alloc(12);
  header.writeUInt16BE(q.id, 0);
  header.writeUInt16BE(0x8180, 2); // QR, RD, RA, NOERROR
  header.writeUInt16BE(1, 4);
  const records = [];
  if (q.type === 1) {
    header.writeUInt16BE(1, 6);
    const fixed = Buffer.alloc(10);
    fixed.writeUInt16BE(1, 0); // A
    fixed.writeUInt16BE(1, 2); // IN
    fixed.writeUInt32BE(60, 4);
    fixed.writeUInt16BE(4, 8);
    records.push(Buffer.from([0xc0, 0x0c]), fixed, Buffer.from(address));
  }
  return Buffer.concat([header, query.subarray(12, q.end), ...records]);
}

async function fakeServer(address) {
  const socket = dgram.createSocket("udp4");
  socket.asked = 0;
  socket.sources = [];
  socket.on("message", (msg, rinfo) => {
    socket.asked++;
    socket.sources.push(rinfo.address);
    if (address) socket.send(answerA(parseQuery(msg), msg, address), rinfo.port, rinfo.address);
  });
  await new Promise((resolve) => socket.bind(0, "127.0.0.1", resolve));
  socket.addr = `127.0.0.1:${socket.address().port}`;
  return socket;
}

const one = await fakeServer([10, 0, 0, 1]);
const two = await fakeServer([10, 0, 0, 2]);
const silent = await fakeServer(null);
const mask = (v) => JSON.stringify(v).replace(/127\.0\.0\.1:\d+/g, (m) =>
  m === one.addr ? "<one>" : m === two.addr ? "<two>" : m === silent.addr ? "<silent>" : "<port>");

function showError(label, err) {
  console.log(label, err.name, err.code, mask(err.message), JSON.stringify(Object.keys(err)),
    "errno" in err ? String(err.errno) : "-", err.syscall, err.hostname);
}

function threw(label, fn) {
  try {
    fn();
    console.log(label, "ok");
  } catch (e) {
    console.log(label, e.name, e.code, e.message);
  }
}

// ---- 1. the classes ---------------------------------------------------------

const PR = dns.promises.Resolver;
const base = Object.getPrototypeOf(dns.Resolver);
console.log("promises.Resolver is a function", typeof PR === "function");
console.log("same as node:dns/promises", PR === ImportedResolver);
console.log("same as dns.Resolver", PR === dns.Resolver);
console.log("shared base", Object.getPrototypeOf(PR) === base, base.name, PR.name, dns.Resolver.name);
console.log("base methods", JSON.stringify(Object.getOwnPropertyNames(base.prototype)),
  JSON.stringify(Object.getOwnPropertyNames(base.prototype).map((n) => base.prototype[n].length)));
console.log("base methods enumerable", Object.keys(base.prototype).length);
for (const [label, C] of [["callback", dns.Resolver], ["promises", PR]]) {
  const names = Object.getOwnPropertyNames(C.prototype);
  console.log(label, "methods", JSON.stringify(names));
  console.log(label, "lengths", JSON.stringify(names.map((n) => C.prototype[n].length)));
  console.log(label, "enumerable", Object.keys(C.prototype).length);
}
console.log("instance own symbols", Object.getOwnPropertySymbols(new PR()).length);
threw("call without new", () => PR());
threw("getServers on a plain object", () => base.prototype.getServers.call({}));

// ---- 2. constructor options -------------------------------------------------

for (const options of [
  undefined, null, 1, "x", {}, { timeout: -1 }, { timeout: 0 }, { timeout: -2 }, { timeout: 1.5 },
  { timeout: "1" }, { timeout: null }, { tries: 0 }, { tries: "4" }, { timeout: -2, tries: 0 },
  { maxTimeout: -1 }, { maxTimeout: 1.5 }, { maxTimeout: 2147483647 }, { maxTimeout: 4294967296 },
]) {
  threw(`new Resolver(${JSON.stringify(options)})`, () => new dns.Resolver(options));
  threw(`new promises.Resolver(${JSON.stringify(options)})`, () => new PR(options));
}

// ---- 3. the error-code constants ---------------------------------------------

const CODES = ["NODATA", "FORMERR", "SERVFAIL", "NOTFOUND", "NOTIMP", "REFUSED", "BADQUERY",
  "BADNAME", "BADFAMILY", "BADRESP", "CONNREFUSED", "TIMEOUT", "EOF", "FILE", "NOMEM",
  "DESTRUCTION", "BADSTR", "BADFLAGS", "NONAME", "BADHINTS", "NOTINITIALIZED", "LOADIPHLPAPI",
  "ADDRGETNETWORKPARAMS", "CANCELLED"];
console.log("dns codes", JSON.stringify(CODES.map((c) => dns[c])));
console.log("promises codes", JSON.stringify(CODES.map((c) => dns.promises[c])));
console.log("promises has no getaddrinfo flags", ["ADDRCONFIG", "V4MAPPED", "ALL"].every((k) => !(k in dns.promises)));

// ---- 4. each instance its own servers ----------------------------------------

const system = dns.getServers();
const a = new dns.Resolver();
const b = new PR();
const c = new PR();
console.log("new instances start from the system servers",
  [a, b, c].every((r) => JSON.stringify(r.getServers()) === JSON.stringify(system)));
a.setServers([one.addr]);
b.setServers([two.addr]);
console.log("a", mask(a.getServers()), "b", mask(b.getServers()));
console.log("c and the defaults untouched",
  JSON.stringify(c.getServers()) === JSON.stringify(system),
  JSON.stringify(dns.getServers()) === JSON.stringify(system),
  JSON.stringify(dns.promises.getServers()) === JSON.stringify(system));
console.log("a answers", await new Promise((resolve) => a.resolve4("x.example", (e, r) => resolve(e ? e.code : r))));
console.log("b answers", JSON.stringify(await b.resolve4("x.example")));
console.log("b resolve()", JSON.stringify(await b.resolve("x.example")), JSON.stringify(await b.resolve("x.example", "A")));
threw("setServers bad entry", () => b.setServers(["example.com"]));
console.log("b kept its servers", mask(b.getServers()));
threw("setServers not an array", () => b.setServers("x"));

dns.setServers([one.addr]);
const d = new PR();
console.log("dns.setServers leaves instances", JSON.stringify(d.getServers()) === JSON.stringify(system),
  mask(b.getServers()));
dns.setServers(system);

// ---- 5. timeout and tries ------------------------------------------------------

{
  const r = new PR({ timeout: 100, tries: 2 });
  r.setServers([silent.addr]);
  const before = silent.asked;
  const started = performance.now();
  try {
    await r.resolve4("slow.example");
    console.log("silent server answered?");
  } catch (err) {
    showError("timeout", err);
  }
  const elapsed = performance.now() - started;
  // c-ares's default timeout is 2 s per try; 100 ms is floored at 250 ms.
  console.log("asked", silent.asked - before, "times; well under the default timing", elapsed < 1900);
}

// ---- 6. cancel() --------------------------------------------------------------

{
  const cb = new dns.Resolver();
  const pr = new PR();
  cb.setServers([silent.addr]);
  pr.setServers([silent.addr]);
  const pending = [
    new Promise((resolve) => cb.resolve4("c1.example", (err, ...rest) => resolve(["cb resolve4", err, rest.length]))),
    new Promise((resolve) => cb.resolve("c2.example", "MX", (err, ...rest) => resolve(["cb resolve MX", err, rest.length]))),
    new Promise((resolve) => cb.reverse("192.0.2.9", (err, ...rest) => resolve(["cb reverse", err, rest.length]))),
    pr.resolveTxt("c3.example").then(() => ["pr resolveTxt", null, 0], (err) => ["pr resolveTxt", err, 0]),
    pr.resolve("c4.example").then(() => ["pr resolve", null, 0], (err) => ["pr resolve", err, 0]),
  ];
  // Another instance on the same silent server is not cancelled.
  const other = new PR({ timeout: 100, tries: 1 });
  other.setServers([silent.addr]);
  const otherResult = other.resolve4("o.example").then(() => "answered", (err) => err.code);
  console.log("cancel() returns", cb.cancel(), pr.cancel());
  for (const [label, err, extra] of await Promise.all(pending)) {
    if (err) showError(label, err);
    else console.log(label, "not cancelled");
    if (label.startsWith("cb")) console.log(label, "extra callback args", extra);
  }
  console.log("other instance", await otherResult);
  pr.setServers([one.addr]);
  console.log("usable after cancel", JSON.stringify(await pr.resolve4("after.example")));
  console.log("cancel with nothing pending", new PR().cancel());
}

// ---- 7. setLocalAddress ---------------------------------------------------------

{
  const r = new PR();
  for (const args of [[], [undefined, "::1"], [null], [1], ["127.0.0.1", 1], ["x"], [""],
    ["127.0.0.1", "x"], ["127.0.0.1", ""], ["127.0.0.1", "127.0.0.2"], ["::1", "::2"],
    ["127.0.0.1"], ["::1"], ["127.0.0.1", "::1"], ["::1", "127.0.0.1"], ["fe80::1%1"]]) {
    threw(`setLocalAddress(${args.map((x) => JSON.stringify(x)).join(", ")})`, () => r.setLocalAddress(...args));
  }
  r.setLocalAddress("127.0.0.1");
  r.setServers([one.addr]);
  const before = one.sources.length;
  console.log("bound query answers", JSON.stringify(await r.resolve4("bound.example")));
  console.log("sent from", JSON.stringify(one.sources.slice(before)));
}

// A local address the host does not have: the socket cannot be bound, which
// c-ares reports as ECONNREFUSED (192.0.2.0/24 is TEST-NET-1).
{
  const r = new PR({ timeout: 300, tries: 1 });
  r.setServers([one.addr]);
  r.setLocalAddress("192.0.2.77");
  try {
    await r.resolve4("unbound.example");
    console.log("unbindable address answered?");
  } catch (err) {
    showError("unbindable local address", err);
  }
}

// ---- 8. query arguments -----------------------------------------------------
//
// Checked before anything is sent, synchronously, for every query method of
// both classes and of the module APIs (up to 0.19.2 none were: a missing
// callback surfaced as an unhandled rejection when the answer came back).

{
  const cb = new dns.Resolver();
  const pr = new PR();
  for (const r of [cb, pr]) r.setServers([one.addr]);
  const fn = () => {};
  const cases = [
    ["cb resolve4 no callback", () => cb.resolve4("x.example")],
    ["cb resolve4 number name", () => cb.resolve4(1, fn)],
    ["cb resolve4 options and callback", () => cb.resolve4("x.example", { ttl: true }, fn)],
    ["cb resolveMx string callback", () => cb.resolveMx("x.example", "x")],
    ["cb resolveMx bad third argument", () => cb.resolveMx("x.example", fn, "y")],
    ["cb resolveAny number name", () => cb.resolveAny(1, fn)],
    ["cb resolve undefined rrtype", () => cb.resolve("x.example", undefined, fn)],
    ["cb resolve lowercase rrtype", () => cb.resolve("x.example", "a", fn)],
    ["cb resolve unknown rrtype", () => cb.resolve("x.example", "BOGUS", fn)],
    ["cb resolve number rrtype", () => cb.resolve("x.example", 5, fn)],
    ["cb resolve no callback", () => cb.resolve("x.example", "A")],
    ["cb reverse number", () => cb.reverse(1, fn)],
    ["cb reverse not an address", () => cb.reverse("x", fn)],
    ["cb reverse no callback", () => cb.reverse("192.0.2.1")],
    ["pr resolve4 number name", () => pr.resolve4(1)],
    ["pr resolve lowercase rrtype", () => pr.resolve("x.example", "a")],
    ["pr resolve number rrtype", () => pr.resolve("x.example", 5)],
    ["pr resolve null rrtype", () => pr.resolve("x.example", null)],
    ["pr resolveAny number name", () => pr.resolveAny(1)],
    ["pr reverse number", () => pr.reverse(1)],
    ["pr reverse not an address", () => pr.reverse("x")],
    ["module resolve4 number name", () => dns.resolve4(1, fn)],
    ["module resolve lowercase rrtype", () => dns.resolve("x.example", "a", fn)],
    ["module promises resolve lowercase rrtype", () => dns.promises.resolve("x.example", "a")],
    ["module resolveAny no callback", () => dns.resolveAny("x.example")],
  ];
  for (const [label, call] of cases) {
    try {
      const out = call();
      if (out && typeof out.then === "function") {
        const err = await out.then(() => null, (e) => e);
        if (err) {
          console.log(label, "rejects", err.code, err.message, "errno" in err ? typeof err.errno : "-");
          continue;
        }
      }
      console.log(label, "ok");
    } catch (e) {
      console.log(label, e.name, e.code, e.message, "errno" in e ? typeof e.errno : "-", e.syscall);
    }
  }
  const order = Object.keys(dns.promises).filter((k) => k.startsWith("resolve") || k === "reverse");
  console.log("promises query keys", JSON.stringify(order));
}

for (const s of [one, two, silent]) s.close();
