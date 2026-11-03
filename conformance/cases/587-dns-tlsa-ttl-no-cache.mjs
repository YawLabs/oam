// TLSA queries, record TTLs, and no answer cache, against a fake nameserver
// on 127.0.0.1 (measured on node v22.22.2):
//
// * resolveTlsa (dns, dns.promises, both Resolver classes, resolve(h,'TLSA'))
//   answers [{ certUsage, selector, match, data }] in wire order, `data` an
//   ArrayBuffer (not a Buffer); a record with no certificate data fails the
//   whole query EBADRESP; errors carry syscall queryTlsa. oam had no TLSA
//   query at all (resolve(h,'TLSA') refused with ENOSYS).
// * resolve4 / resolve6 with { ttl: true } report each record's TTL as
//   served, any u32; oam reported ttl: 0 for every address.
// * c-ares keeps no answer cache: every query is a datagram to the server,
//   NXDOMAIN and NODATA included. oam's resolver answered repeats from its
//   own cache without asking.
import dgram from "node:dgram";
import dns from "node:dns";

function encodeName(name) {
  return Buffer.concat([
    ...name.split(".").map((l) => Buffer.concat([Buffer.from([l.length]), Buffer.from(l)])),
    Buffer.from([0]),
  ]);
}

function parseQuery(buf) {
  let off = 12;
  const labels = [];
  while (buf[off] !== 0) {
    labels.push(buf.subarray(off + 1, off + 1 + buf[off]).toString("latin1"));
    off += buf[off] + 1;
  }
  return { id: buf.readUInt16BE(0), name: labels.join(".").toLowerCase(), type: buf.readUInt16BE(off + 1), end: off + 5 };
}

function record(type, ttl, rdata) {
  const fixed = Buffer.alloc(10);
  fixed.writeUInt16BE(type, 0);
  fixed.writeUInt16BE(1, 2);
  fixed.writeUInt32BE(ttl, 4);
  fixed.writeUInt16BE(rdata.length, 8);
  return Buffer.concat([Buffer.from([0xc0, 0x0c]), fixed, rdata]);
}

const tlsa = (usage, selector, match, data) => Buffer.concat([Buffer.from([usage, selector, match]), Buffer.from(data)]);
const v6 = (last) => { const b = Buffer.alloc(16); b[0] = 0xfd; b[15] = last; return b; };

// name/type -> [rcode, records]
const ZONE = {
  "one.example/52": [0, [record(52, 60, tlsa(3, 1, 1, [0xde, 0xad, 0xbe, 0xef]))]],
  "two.example/52": [0, [record(52, 60, tlsa(3, 1, 1, [1, 2, 3])), record(52, 60, tlsa(2, 0, 2, [9]))]],
  "mixed.example/52": [0, [record(1, 60, Buffer.from([10, 0, 0, 9])), record(52, 60, tlsa(1, 1, 2, [7, 7]))]],
  "emptydata.example/52": [0, [record(52, 60, tlsa(3, 1, 1, [1])), record(52, 60, tlsa(3, 1, 1, []))]],
  "empty.example/52": [0, []],
  "trailing.example/52": [0, [record(52, 60, tlsa(3, 1, 1, [1])), record(1, 60, Buffer.from([10, 0, 0, 9]))]],
  "holes.example/52": [0, [
    record(1, 60, Buffer.from([10, 0, 0, 9])), record(1, 60, Buffer.from([10, 0, 0, 8])),
    record(52, 60, tlsa(3, 1, 1, [1])), record(1, 60, Buffer.from([10, 0, 0, 7])), record(52, 60, tlsa(3, 1, 1, [2])),
  ]],
  "aonly.example/52": [0, [record(1, 60, Buffer.from([10, 0, 0, 9]))]],
  "multi.example/1": [0, [record(1, 300, Buffer.from([10, 0, 0, 1])), record(1, 5, Buffer.from([10, 0, 0, 2]))]],
  "multi.example/28": [0, [record(28, 120, v6(1)), record(28, 7, v6(2))]],
  "big.example/1": [0, [
    record(1, 100000, Buffer.from([10, 0, 0, 3])),
    record(1, 2147483648, Buffer.from([10, 0, 0, 4])),
    record(1, 4294967295, Buffer.from([10, 0, 0, 5])),
  ]],
  "zero.example/1": [0, [record(1, 0, Buffer.from([10, 0, 0, 6]))]],
  "nodata.example/1": [0, []],
};

const asked = {};
const socket = dgram.createSocket("udp4");
socket.on("message", (msg, rinfo) => {
  const q = parseQuery(msg);
  const key = `${q.name}/${q.type}`;
  asked[key] = (asked[key] || 0) + 1;
  const [rcode, records] = ZONE[key] ?? [3, []];
  const header = Buffer.alloc(12);
  header.writeUInt16BE(q.id, 0);
  header.writeUInt16BE(0x8180 | rcode, 2);
  header.writeUInt16BE(1, 4);
  header.writeUInt16BE(records.length, 6);
  socket.send(Buffer.concat([header, msg.subarray(12, q.end), ...records]), rinfo.port, rinfo.address);
});
await new Promise((resolve) => socket.bind(0, "127.0.0.1", resolve));
const server = `127.0.0.1:${socket.address().port}`;

// The answer can be sparse: node keeps a hole where a record of another type
// sat (c-ares's parser), so the own keys are printed too.
function showTlsa(label, records) {
  console.log(label, records.length, JSON.stringify(Object.keys(records)), JSON.stringify(records.map((r) => ({
    keys: Object.keys(r),
    certUsage: r.certUsage,
    selector: r.selector,
    match: r.match,
    arrayBuffer: r.data instanceof ArrayBuffer,
    buffer: Buffer.isBuffer(r.data),
    bytes: [...new Uint8Array(r.data)],
  }))));
}
function showError(label, err) {
  console.log(label, err.code, err.message, err.syscall, JSON.stringify(Object.keys(err)));
}

const pr = new dns.promises.Resolver();
const cb = new dns.Resolver();
pr.setServers([server]);
cb.setServers([server]);
dns.setServers([server]);

// ---- TLSA ----
console.log("typeof", typeof dns.resolveTlsa, typeof dns.promises.resolveTlsa,
  typeof dns.Resolver.prototype.resolveTlsa, typeof dns.promises.Resolver.prototype.resolveTlsa);
showTlsa("promises one", await pr.resolveTlsa("one.example"));
showTlsa("promises two", await pr.resolveTlsa("two.example"));
showTlsa("promises mixed answer", await pr.resolveTlsa("mixed.example"));
showTlsa("promises trailing other record", await pr.resolveTlsa("trailing.example"));
showTlsa("promises holes", await pr.resolveTlsa("holes.example"));
showTlsa("promises only another type", await pr.resolveTlsa("aonly.example"));
showTlsa("promises resolve TLSA", await pr.resolve("one.example", "TLSA"));
showTlsa("module promises", await dns.promises.resolveTlsa("one.example"));
showTlsa("ttl option ignored", await pr.resolveTlsa("one.example", { ttl: true }));
showTlsa("callback", await new Promise((resolve, reject) =>
  cb.resolveTlsa("two.example", (err, r) => (err ? reject(err) : resolve(r)))));
showTlsa("module callback resolve", await new Promise((resolve, reject) =>
  dns.resolve("one.example", "TLSA", (err, r) => (err ? reject(err) : resolve(r)))));
for (const name of ["emptydata.example", "empty.example", "nx.example"]) {
  await pr.resolveTlsa(name).then(() => console.log(name, "answered?"), (err) => showError(name, err));
}
console.log("method lengths", dns.resolveTlsa.length, dns.promises.resolveTlsa.length,
  dns.Resolver.prototype.resolveTlsa.length, dns.resolve4.length, dns.reverse.length, dns.resolve.length);

// ---- TTLs ----
console.log("resolve4 ttl", JSON.stringify(await pr.resolve4("multi.example", { ttl: true })));
console.log("resolve6 ttl", JSON.stringify(await pr.resolve6("multi.example", { ttl: true })));
console.log("resolve4 plain", JSON.stringify(await pr.resolve4("multi.example")));
console.log("resolve4 ttl:1", JSON.stringify(await pr.resolve4("multi.example", { ttl: 1 })));
console.log("resolve4 ttl:false", JSON.stringify(await pr.resolve4("multi.example", { ttl: false })));
console.log("resolve A", JSON.stringify(await pr.resolve("multi.example", "A")));
console.log("big", JSON.stringify(await pr.resolve4("big.example", { ttl: true })));
console.log("zero", JSON.stringify(await pr.resolve4("zero.example", { ttl: true })));
console.log("callback ttl", JSON.stringify(await new Promise((resolve, reject) =>
  cb.resolve4("multi.example", { ttl: true }, (err, r) => (err ? reject(err) : resolve(r))))));
console.log("module ttl", JSON.stringify(await new Promise((resolve, reject) =>
  dns.resolve6("multi.example", { ttl: true }, (err, r) => (err ? reject(err) : resolve(r))))));

// ---- no cache ----
for (const key of Object.keys(asked)) delete asked[key];
for (let i = 0; i < 3; i++) {
  await pr.resolve4("multi.example");
  await new Promise((resolve) => cb.resolve4("multi.example", resolve));
  await new Promise((resolve) => dns.resolve4("multi.example", resolve));
  await pr.resolve4("nx.example").catch(() => {});
  await pr.resolve4("nodata.example").catch(() => {});
}
console.log("datagrams per name", JSON.stringify(Object.keys(asked).sort().map((k) => [k, asked[k]])));

socket.close();
