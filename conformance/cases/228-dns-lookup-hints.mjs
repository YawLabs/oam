// dns.lookup's `hints`: dns.ADDRCONFIG / V4MAPPED / ALL are the platform's
// AI_* flags, `hints` is validated as node's validateHints does, and
// V4MAPPED | ALL on an IPv6 lookup answers the IPv4 addresses as
// `::ffff:a.b.c.d` besides the IPv6 ones.
//
// Regression guard (#165): the three constants were 0, so a caller's
// `hints: dns.ADDRCONFIG | dns.V4MAPPED` passed 0; any `hints` was accepted,
// even bits no flag has; and V4MAPPED was never applied.
//
// The constants' values differ by platform (glibc's on Linux, the BSD ones
// elsewhere), not by runtime. AI_ADDRCONFIG is not applied by oam (see
// docs/node-divergences.md), so no line depends on what is configured.
import dns from "node:dns";

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
