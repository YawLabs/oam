// dns.setServers() against in-process fake nameservers: reverse()'s error
// code, and which server a query goes to.
//
// 1) dns.reverse() is c-ares's ares_gethostbyaddr, not a PTR query, and it
//    reports EVERY failed lookup as ENOTFOUND -- a NOERROR answer with no PTR
//    (NODATA) included. resolvePtr() is a plain query and keeps node's
//    ENODATA/ENOTFOUND split. oam mapped reverse()'s NODATA to ENODATA.
//
// 2) c-ares (node creates its channels with ARES_FLAG_NOCHECKRESP and no
//    ROTATE) asks ONE server at a time, first server first, and moves on only
//    when a server does not answer: a SERVFAIL or REFUSED answer is final, and
//    a server that timed out is demoted for later queries. oam raced two
//    servers (hickory's default) and took whichever answered first, so a
//    broken forwarder listed second that answered NOERROR-empty instantly won
//    over a correct first server -- a CAA query came back ENODATA under oam
//    and with records under node.
//
// 3) dns.setServers() itself: node's parsing and getServers() formatting
//    (port only when not 53, IPv6 bracketed only with a port, duplicates
//    dropped), its argument errors, the promises API sharing the default
//    resolver, a new dns.Resolver() keeping the SYSTEM servers, and an empty
//    list.
//
// Every server is a node:dgram socket on 127.0.0.1, so nothing here touches
// the network. Ports are random and masked in the output; the addresses in
// the parsing section are documentation ranges (RFC 5737 / RFC 3849) that are
// never queried.
import dgram from "node:dgram";
import dns from "node:dns";

const PTR_IP = "192.0.2.5"; // TEST-NET-1: in no hosts file, never a real PTR
const PTR_NAME = "5.2.0.192.in-addr.arpa";

// ---- a minimal DNS responder ---------------------------------------------

function parseQuery(buf) {
  let off = 12;
  const labels = [];
  while (buf[off] !== 0) {
    labels.push(buf.subarray(off + 1, off + 1 + buf[off]).toString("latin1"));
    off += buf[off] + 1;
  }
  off += 1;
  return {
    id: buf.readUInt16BE(0),
    name: labels.join(".").toLowerCase(),
    type: buf.readUInt16BE(off),
    end: off + 4,
  };
}

function encodeName(name) {
  const parts = name.split(".").map((l) => Buffer.concat([Buffer.from([l.length]), Buffer.from(l)]));
  return Buffer.concat([...parts, Buffer.from([0])]);
}

function response(q, query, rcode, answers = []) {
  const header = Buffer.alloc(12);
  header.writeUInt16BE(q.id, 0);
  header.writeUInt16BE(0x8180 | rcode, 2); // QR, RD, RA
  header.writeUInt16BE(1, 4);
  header.writeUInt16BE(answers.length, 6);
  const records = answers.map(({ type, rdata }) => {
    const fixed = Buffer.alloc(10);
    fixed.writeUInt16BE(type, 0);
    fixed.writeUInt16BE(1, 2); // IN
    fixed.writeUInt32BE(60, 4);
    fixed.writeUInt16BE(rdata.length, 8);
    return Buffer.concat([Buffer.from([0xc0, 0x0c]), fixed, rdata]);
  });
  return Buffer.concat([header, query.subarray(12, q.end), ...records]);
}

// A record set the "good" servers answer with.
function goodAnswers(q, address) {
  if (q.type === 1) return [{ type: 1, rdata: Buffer.from(address) }];
  if (q.type === 12) return [{ type: 12, rdata: encodeName("host.example") }];
  if (q.type === 257) {
    // CAA: flags 0, tag "issue", value "ca.example"
    return [{ type: 257, rdata: Buffer.concat([Buffer.from([0, 5]), Buffer.from("issueca.example")]) }];
  }
  return [];
}

async function fakeServer(handler) {
  const socket = dgram.createSocket("udp4");
  socket.asked = 0;
  socket.on("message", (msg, rinfo) => {
    const q = parseQuery(msg);
    socket.asked++;
    handler(q, msg, (reply) => socket.send(reply, rinfo.port, rinfo.address));
  });
  await new Promise((resolve) => socket.bind(0, "127.0.0.1", resolve));
  socket.addr = `127.0.0.1:${socket.address().port}`;
  return socket;
}

// slow-but-correct first server; instant-but-broken second server
const good = await fakeServer((q, m, send) =>
  setTimeout(() => send(response(q, m, 0, goodAnswers(q, [10, 0, 0, 1]))), 50),
);
const nodata = await fakeServer((q, m, send) => send(response(q, m, 0)));
const nxdomain = await fakeServer((q, m, send) => send(response(q, m, 3)));
const servfail = await fakeServer((q, m, send) => send(response(q, m, 2)));
const refused = await fakeServer((q, m, send) => send(response(q, m, 5)));
const fast = await fakeServer((q, m, send) => send(response(q, m, 0, goodAnswers(q, [10, 0, 0, 2]))));
const dead = await fakeServer(() => {});
const all = { good, nodata, nxdomain, servfail, refused, fast, dead };

function asked() {
  const out = [];
  for (const [name, s] of Object.entries(all)) {
    if (s.asked > 0) out.push(name);
    s.asked = 0;
  }
  return out.join(",") || "none";
}

function errLine(e) {
  return `ERR ${e.code} syscall=${e.syscall} hostname=${e.hostname} errno=${e.errno} msg=${JSON.stringify(e.message)}`;
}

async function check(label, servers, run) {
  dns.setServers(servers.map((s) => s.addr));
  let line;
  try {
    line = JSON.stringify(await run());
  } catch (e) {
    line = errLine(e);
  }
  console.log(`${label}: ${line} | asked ${asked()}`);
}

const cb = (fn) => new Promise((resolve, reject) => fn((e, v) => (e ? reject(e) : resolve(v))));

const systemServers = JSON.stringify(dns.getServers());

// ---- 1) reverse() vs resolvePtr() ---------------------------------------

await check("reverse NODATA (callback)", [nodata], () => cb((done) => dns.reverse(PTR_IP, done)));
await check("reverse NODATA (promise)", [nodata], () => dns.promises.reverse(PTR_IP));
await check("reverse NXDOMAIN", [nxdomain], () => dns.promises.reverse(PTR_IP));
await check("reverse SERVFAIL", [servfail], () => dns.promises.reverse(PTR_IP));
await check("reverse REFUSED", [refused], () => dns.promises.reverse(PTR_IP));
await check("reverse answered", [fast], () => dns.promises.reverse(PTR_IP));
await check("resolvePtr NODATA (callback)", [nodata], () => cb((done) => dns.resolvePtr(PTR_NAME, done)));
await check("resolvePtr NODATA (promise)", [nodata], () => dns.promises.resolvePtr(PTR_NAME));
await check("resolvePtr NXDOMAIN", [nxdomain], () => dns.promises.resolvePtr(PTR_NAME));
await check("resolve4 SERVFAIL", [servfail], () => dns.promises.resolve4("servfail.example"));
await check("resolve4 REFUSED", [refused], () => dns.promises.resolve4("refused.example"));

// ---- 2) server order -----------------------------------------------------

await check("[good, nodata] resolve4", [good, nodata], () => dns.promises.resolve4("a.example"));
await check("[good, nodata] resolveCaa", [good, nodata], () => dns.promises.resolveCaa("caa.example"));
await check("[good, nodata] reverse", [good, nodata], () => dns.promises.reverse(PTR_IP));
await check("[nodata, good] resolve4", [nodata, good], () => dns.promises.resolve4("b.example"));
await check("[nxdomain, good] resolve4", [nxdomain, good], () => dns.promises.resolve4("c.example"));
await check("[servfail, good] resolve4", [servfail, good], () => dns.promises.resolve4("d.example"));
await check("[refused, good] resolve4", [refused, good], () => dns.promises.resolve4("e.example"));
// A server that never answers: the query fails over to the next one after
// the timeout, and the dead server is demoted, so the NEXT query on the same
// list goes straight to the live one.
dns.setServers([dead.addr, fast.addr]);
for (const name of ["f.example", "g.example"]) {
  let line;
  try {
    line = JSON.stringify(await dns.promises.resolve4(name));
  } catch (e) {
    line = errLine(e);
  }
  console.log(`[dead, fast] resolve4 ${name}: ${line} | dead asked ${dead.asked > 0} fast asked ${fast.asked > 0}`);
  asked();
}

// ---- 3) setServers itself ------------------------------------------------

const masked = (list) => JSON.stringify(list.map((s) => s.replace(/:\d+$/, ":PORT")));
// dns.setServers() replaces the default of BOTH APIs; dns.promises.setServers()
// only the promises one (node's promises module rebinds only itself).
dns.setServers([good.addr, nodata.addr]);
console.log("after dns.setServers: dns", masked(dns.getServers()), "promises", masked(dns.promises.getServers()));
dns.promises.setServers([fast.addr]);
console.log("after promises.setServers: dns", masked(dns.getServers()), "promises", masked(dns.promises.getServers()));
await check("callback resolve4 keeps [good, nodata]", [], () => cb((done) => {
  dns.setServers([good.addr, nodata.addr]);
  dns.promises.setServers([fast.addr]);
  dns.resolve4("i.example", done);
}));
await check("promises resolve4 goes to fast", [], () => {
  dns.setServers([good.addr, nodata.addr]);
  dns.promises.setServers([fast.addr]);
  return dns.promises.resolve4("j.example");
});
console.log("new Resolver keeps the system servers:", JSON.stringify(new dns.Resolver().getServers()) === systemServers);

dns.setServers([
  "192.0.2.1",
  "192.0.2.1:53",
  "192.0.2.2:5353",
  "[192.0.2.3]:54",
  "192.0.2.4:70000", // node narrows the port to 16 bits: 4464
  "2001:db8::1",
  "[2001:db8::1]",
  "[2001:db8::2]",
  "[2001:db8::2]:5353",
]);
console.log("parsed and formatted:", JSON.stringify(dns.getServers()));

for (const bad of ["192.0.2.1", [192], ["bogus"], ["192.0.2.1:port"], ["[2001:db8::1"]]) {
  const before = JSON.stringify(dns.getServers());
  try {
    dns.setServers(bad);
    console.log(`setServers(${JSON.stringify(bad)}): accepted`, JSON.stringify(dns.getServers()));
  } catch (e) {
    const unchanged = JSON.stringify(dns.getServers()) === before;
    console.log(`setServers(${JSON.stringify(bad)}): ${e.name} ${e.code} ${JSON.stringify(e.message)} unchanged=${unchanged}`);
  }
}

dns.setServers([]);
console.log("empty list:", JSON.stringify(dns.getServers()));
await check("empty resolve4", [], () => dns.promises.resolve4("h.example"));
await check("empty reverse", [], () => dns.promises.reverse(PTR_IP));

for (const s of Object.values(all)) s.close();
