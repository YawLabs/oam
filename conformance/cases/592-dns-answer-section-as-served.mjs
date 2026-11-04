// A resolve query is ONE datagram and its answer section is taken as served,
// as c-ares hands it to node (measured on node v22.22.2 against a fake server
// on 127.0.0.1):
//
// * an answer holding records, none of the type asked, is [] for TLSA, MX,
//   TXT, SRV, CAA and NAPTR, and for A / AAAA when a CNAME is among them;
//   ENODATA for A / AAAA otherwise, NS and PTR; the name asked, for CNAME.
//   oam's resolver dropped such an answer (ENODATA for every type). SOA is
//   left out: node's EBADRESP there is oam's ENODATA (docs/node-divergences.md
//   #30).
// * a CNAME is not chased: c-ares sends no second query for its target, so a
//   CNAME alone is [] even where the target has records. oam's resolver asked
//   the target too and answered with its records.
// * RFC 6761 names (localhost, *.localhost, *.invalid) are asked of the
//   server like any other. oam's resolver answered localhost itself
//   (127.0.0.1) without asking.
import dgram from "node:dgram";
import dns from "node:dns";

const TYPE = { A: 1, NS: 2, CNAME: 5, SOA: 6, PTR: 12, MX: 15, TXT: 16, AAAA: 28, SRV: 33, NAPTR: 35, TLSA: 52, CAA: 257 };

function encodeName(name) {
  const parts = [];
  for (const label of name.split(".").filter(Boolean)) parts.push(label.length, ...Buffer.from(label));
  parts.push(0);
  return Buffer.from(parts);
}

function rdata(type) {
  switch (type) {
    case TYPE.A: return Buffer.from([10, 0, 0, 1]);
    case TYPE.TXT: return Buffer.from([2, 104, 105]);
    case TYPE.MX: return Buffer.concat([Buffer.from([0, 10]), encodeName("mx.oamt.test")]);
    case TYPE.TLSA: return Buffer.concat([Buffer.from([3, 1, 1]), Buffer.alloc(4, 0xab)]);
    case TYPE.AAAA: return Buffer.alloc(16, 1);
    default: throw new Error(`no rdata for type ${type}`);
  }
}

function record(name, type, data) {
  const fixed = Buffer.alloc(10);
  fixed.writeUInt16BE(type, 0);
  fixed.writeUInt16BE(1, 2);
  fixed.writeUInt32BE(60, 4);
  fixed.writeUInt16BE(data.length, 8);
  return Buffer.concat([encodeName(name), fixed, data]);
}

const asked = [];
const socket = dgram.createSocket("udp4");
socket.on("message", (msg, rinfo) => {
  let off = 12;
  const labels = [];
  while (msg[off]) {
    labels.push(msg.subarray(off + 1, off + 1 + msg[off]).toString());
    off += msg[off] + 1;
  }
  off += 1;
  const qtype = msg.readUInt16BE(off);
  const qname = labels.join(".").toLowerCase();
  asked.push(`${qname}/${qtype}`);
  const answers = [];
  let rcode = 0;
  const other = qtype === TYPE.A ? TYPE.TXT : TYPE.A;
  const base = qname.split(".")[0];
  if (base === "other") answers.push(record(qname, other, rdata(other)));
  else if (base === "cnamenodata") answers.push(record(qname, TYPE.CNAME, encodeName("nodata.oamt.test")));
  else if (base === "nodata") { /* NOERROR, no answer */ }
  else if (base === "cnamewith") {
    answers.push(record(qname, TYPE.CNAME, encodeName("target.oamt.test")));
    answers.push(record("target.oamt.test", qtype, rdata(qtype)));
  } else if (base === "cnameonly") answers.push(record(qname, TYPE.CNAME, encodeName("target.oamt.test")));
  else if (base === "target") answers.push(record(qname, qtype, rdata(qtype)));
  else rcode = 3; // NXDOMAIN, localhost included
  const header = Buffer.from(msg.subarray(0, 12));
  header[2] = 0x84 | (msg[2] & 1);
  header[3] = 0x80 | rcode;
  header.writeUInt16BE(1, 4);
  header.writeUInt16BE(answers.length, 6);
  header.writeUInt16BE(0, 8);
  header.writeUInt16BE(0, 10);
  socket.send(Buffer.concat([header, msg.subarray(12, off + 4), ...answers]), rinfo.port, rinfo.address);
});
await new Promise((resolve) => socket.bind(0, "127.0.0.1", resolve));
const resolver = new dns.promises.Resolver();
resolver.setServers([`127.0.0.1:${socket.address().port}`]);

function shape(value) {
  if (!Array.isArray(value)) return JSON.stringify(value);
  return `${value.length}:${JSON.stringify(Object.keys(value))}:${JSON.stringify(value, (k, v) =>
    v instanceof ArrayBuffer ? `ArrayBuffer(${v.byteLength})` : v)}`;
}

async function ask(name, method) {
  const before = asked.length;
  const out = await resolver[method](name).then(shape, (e) => `${e.code} ${e.syscall}`);
  console.log(method, name, "->", out, "| asked", JSON.stringify(asked.slice(before)));
}

const ALL = ["resolve4", "resolve6", "resolveCname", "resolveMx", "resolveTxt", "resolveNs",
  "resolveSrv", "resolvePtr", "resolveCaa", "resolveNaptr", "resolveTlsa"];
for (const method of ALL) await ask("other.oamt.test", method);
// The CNAME rule names the question as the response carries it, not as typed.
await ask("Other.Oamt.Test", "resolveCname");
for (const method of ["resolve4", "resolve6", "resolveMx", "resolveTxt", "resolveTlsa", "resolveNs"]) {
  await ask("cnamenodata.oamt.test", method);
}
for (const method of ["resolve4", "resolve6", "resolveMx", "resolveTxt", "resolveTlsa"]) {
  await ask("cnameonly.oamt.test", method);
  await ask("cnamewith.oamt.test", method);
}
for (const method of ["resolve4", "resolveTxt", "resolveTlsa"]) await ask("nodata.oamt.test", method);
for (const name of ["localhost", "a.localhost", "x.invalid"]) await ask(name, "resolve4");

socket.close();
