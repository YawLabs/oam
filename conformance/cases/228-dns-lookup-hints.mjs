// dns.lookup's `hints`: dns.ADDRCONFIG / V4MAPPED / ALL are the platform's
// AI_* flags, `hints` is validated as node's validateHints does -- from
// net.connect too, which reaches dns.lookup with its `hints` and throws
// synchronously on a bad one -- and every flag goes to getaddrinfo:
// V4MAPPED | ALL on an IPv6 lookup answers the IPv4 addresses as
// `::ffff:a.b.c.d` besides the IPv6 ones, and ADDRCONFIG answers only the
// families this host has an address configured for.
//
// Regression guard (#165): the three constants were 0, so a caller's
// `hints: dns.ADDRCONFIG | dns.V4MAPPED` passed 0; any `hints` was accepted,
// even bits no flag has, and net.connect accepted any `hints` up to 0.18.0;
// V4MAPPED was never applied, and ADDRCONFIG was dropped up to 0.18.0.
//
// The constants' values differ by platform (glibc's on Linux, the BSD ones
// elsewhere), not by runtime. The ADDRCONFIG answer depends on what this host
// has configured, the same way for both runtimes.
import dns from "node:dns";
import net from "node:net";

const { ADDRCONFIG, V4MAPPED, ALL } = dns;
console.log("distinct single bits:", [ADDRCONFIG, V4MAPPED, ALL].every((v) => v > 0 && (v & (v - 1)) === 0) &&
  new Set([ADDRCONFIG, V4MAPPED, ALL]).size === 3);
console.log("values:", JSON.stringify({ ADDRCONFIG, V4MAPPED, ALL }) ===
  JSON.stringify(process.platform === "linux" ? { ADDRCONFIG: 32, V4MAPPED: 8, ALL: 16 }
    : { ADDRCONFIG: 1024, V4MAPPED: 2048, ALL: 256 }));
console.log("dns.promises carries none:", "ADDRCONFIG" in dns.promises);

const describe = (e) => `${e.name} ${e.code} ${JSON.stringify(e.message.replace(/\d{3,}/g, (n) =>
  n === String(ADDRCONFIG | 1) || n === String(ADDRCONFIG | V4MAPPED | ALL | 1) ? "<flags+1>" : n))}`;

const invalid = [12345678, -1, 1.5, 7, "1024", ADDRCONFIG | 1, ADDRCONFIG | V4MAPPED | ALL | 1, true, {}];
for (const hints of invalid) {
  const label = typeof hints === "number" ? `number ${Number.isInteger(hints) && hints > 64 && hints !== 12345678 ? "flags+1" : hints}` : typeof hints;
  let callbackForm;
  try {
    dns.lookup("localhost", { hints }, () => {});
    callbackForm = "accepted";
  } catch (e) {
    callbackForm = describe(e);
  }
  let promiseForm;
  try {
    const p = dns.promises.lookup("localhost", { hints });
    p.catch(() => {});
    promiseForm = "returned a promise";
  } catch (e) {
    promiseForm = describe(e);
  }
  console.log(`hints ${label}:\n  callback: ${callbackForm}\n  promises: ${promiseForm}`);
}

// Every combination of the three flags (and null / undefined) is accepted.
const valid = [0, null, undefined, ADDRCONFIG, V4MAPPED, ALL, ADDRCONFIG | V4MAPPED, V4MAPPED | ALL,
  ADDRCONFIG | V4MAPPED | ALL];
let accepted = 0;
for (const hints of valid) {
  await new Promise((resolve) => dns.lookup("localhost", { hints, all: true }, (err, a) => {
    if (!err && a.length > 0) accepted++;
    resolve();
  }));
}
console.log(`valid hints accepted: ${accepted}/${valid.length}`);

// V4MAPPED | ALL on an IPv6 lookup: the IPv6 addresses and the IPv4 ones,
// mapped. Sorted: the order is the resolver's.
const mappedAll = await dns.promises.lookup("localhost", { family: 6, all: true, hints: V4MAPPED | ALL });
console.log("V4MAPPED|ALL family 6:", JSON.stringify(mappedAll.map((a) => `${a.family} ${a.address}`).sort()));
// Without family 6 the flags change nothing.
const plain = await dns.promises.lookup("localhost", { all: true });
const flagged = await dns.promises.lookup("localhost", { all: true, hints: V4MAPPED | ALL });
console.log("family 0 unchanged:", JSON.stringify(plain) === JSON.stringify(flagged));
// An IP literal is answered as written, flags or not.
console.log("literal:", JSON.stringify(await dns.promises.lookup("127.0.0.1", { family: 6, all: true, hints: V4MAPPED | ALL })));

// ADDRCONFIG goes to the resolver: its answer is a subset of the unflagged one
// (a family this host has no configured address for is dropped), never empty,
// and the same whether dns.lookup or a connect asks. Sorted: the order is the
// resolver's.
const sorted = (list) => JSON.stringify(list.map((a) => `${a.family} ${a.address}`).sort());
const configured = await dns.promises.lookup("localhost", { all: true, hints: ADDRCONFIG });
console.log("ADDRCONFIG:", sorted(configured));
console.log("ADDRCONFIG within unflagged:", configured.length > 0 &&
  configured.every((a) => plain.some((p) => p.address === a.address && p.family === a.family)));

// net.connect validates `hints` through dns.lookup, synchronously: a bad one
// throws out of connect() before anything is looked up or dialled, where a
// user's `lookup` hook is handed them as given. The default a hook sees is 0
// on Windows and dns.ADDRCONFIG elsewhere, and `family: 4` or a caller's
// own `hints` turn that default off.
for (const hints of [12345678, "x", ADDRCONFIG | 1]) {
  try {
    const s = net.connect({ host: "localhost", port: 1, hints });
    s.on("error", () => {});
    s.destroy();
    console.log(`connect hints ${JSON.stringify(hints)}: accepted`);
  } catch (e) {
    console.log(`connect hints ${JSON.stringify(hints)}: ${describe(e)}`);
  }
}
const hookSees = (options) => new Promise((resolve) => {
  const s = net.connect({
    host: "localhost",
    port: 1,
    ...options,
    lookup: (_host, opts, cb) => {
      resolve(JSON.stringify({ hints: opts.hints, family: opts.family, all: opts.all }));
      cb(null, [{ address: "127.0.0.1", family: 4 }]);
    },
  });
  s.on("error", () => {});
});
console.log("hook sees, no options:", (await hookSees({})) ===
  JSON.stringify({ hints: process.platform === "win32" ? 0 : ADDRCONFIG, family: undefined, all: true }));
console.log("hook sees, family 4:", await hookSees({ family: 4 }));
console.log("hook sees, own hints:", (await hookSees({ hints: V4MAPPED })) === JSON.stringify({ hints: V4MAPPED, family: undefined, all: true }));
console.log("hook sees, invalid hints:", await hookSees({ hints: 12345678 }));
