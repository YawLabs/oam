// dns.lookup() and dns.promises.lookup() answer a falsy hostname without the
// resolver, for compatibility (#452). node's lib/dns.js checks `!hostname`
// before anything is looked up: it emits DEP0118 once per process (shared by
// both APIs, through process.emitWarning) and answers `null` with family 4 --
// 6 when family 6 was asked for -- or `[]` with `all`, on the next tick for
// the callback form, which returns `{}`. Every falsy value takes that path,
// not just "": undefined, null, false, 0, NaN, 0n. A TRUTHY non-string
// hostname is a TypeError ERR_INVALID_ARG_TYPE, as are a non-function
// callback, an options that is neither a number nor an object, and a
// non-boolean `options.all` or `options.verbatim`; an `options.order` outside
// node's three is ERR_INVALID_ARG_VALUE, and the promises API also refuses the
// callback API's 'IPv4' / 'IPv6' family names. util.promisify(dns.lookup)
// resolves { address, family } (measured on v22.22.2).
//
// oam handed "" to the native op, which threw `TypeError: hostname must be
// between 1 and 253 characters` with no code, synchronously out of both
// APIs; the other falsy values were stringified and failed ENOTFOUND, and a
// truthy non-string was stringified and looked up.
//
// Nothing here reaches a resolver, so the case is deterministic offline.
//
// Edited for node v26.13.1, which cannot run the old case: DEP0118 reached
// its end of life, so a falsy hostname is ERR_INVALID_ARG_VALUE ("must be a
// non-empty string") -- thrown by dns.lookup() once its options and callback
// are validated, a rejection from dns.promises.lookup() -- and the case's
// first `await` died of it. Every probe is kept; each now prints what it
// answers or how it fails (the promises loop and promisify catch the
// rejection, the callback calls the throw), and the warning count stays (0
// now). A hostname with a NUL byte is added: v26 refuses it before anything
// else (validateStringWithoutNullBytes).
import dns from "node:dns";
import { promisify } from "node:util";

const warnings = [];
process.on("warning", (w) => warnings.push(`${w.name} ${w.code} ${w.message}`));

const show = (v) => (typeof v === "bigint" ? `${v}n` : Number.isNaN(v) ? "NaN" : JSON.stringify(v));

for (const host of ["", undefined, null, false, 0, NaN, 0n]) {
  for (const opts of [undefined, { all: true }, 6, { family: 6 }, { family: 4 }, null]) {
    const got = await dns.promises.lookup(host, opts).then(
      (v) => JSON.stringify(v),
      (e) => `REJECTS ${e.name} ${e.code} ${e.message}`,
    );
    console.log("promises", show(host), JSON.stringify(opts), got);
  }
}

function threw(label, fn) {
  try {
    const r = fn();
    if (r && typeof r.then === "function") r.catch(() => {});
    console.log(label, "did not throw");
  } catch (e) {
    console.log(label, e.name, e.code, e.message);
  }
}

threw("cb plain", () => dns.lookup("", (...args) => console.log("cb plain called", JSON.stringify(args))));
threw("cb all", () => dns.lookup("", { all: true }, (...args) => console.log("cb all called", JSON.stringify(args))));
threw("cb null 6", () => dns.lookup(null, 6, (...args) => console.log("cb null 6 called", JSON.stringify(args))));
threw("cb undefined family IPv6", () => dns.lookup(undefined, { family: "IPv6" }, (...args) => console.log("cb undefined called", JSON.stringify(args))));
threw("promises NUL byte", () => dns.promises.lookup("a\0b"));
threw("cb NUL byte before callback", () => dns.lookup("a\0b"));

for (const host of [123, true, {}, [], ["localhost"]]) {
  threw(`promises hostname ${JSON.stringify(host)}`, () => dns.promises.lookup(host));
  threw(`cb hostname ${JSON.stringify(host)}`, () => dns.lookup(host, () => {}));
}
threw("cb no callback", () => dns.lookup(""));
threw("cb no callback all", () => dns.lookup("", { all: true }));
threw("cb no callback family", () => dns.lookup("", 4));
threw("cb two-arg bad options", () => dns.lookup("", "x"));
threw("cb bad options", () => dns.lookup("", "x", () => {}));
threw("cb bad hostname before callback", () => dns.lookup(123));
threw("cb bad family", () => dns.lookup("", { family: 5 }, () => {}));
threw("cb bad all", () => dns.lookup("", { all: 1 }, () => {}));
threw("promises bad options", () => dns.promises.lookup("", "x"));
threw("promises bad family", () => dns.promises.lookup("", { family: 5 }));
threw("promises bad all", () => dns.promises.lookup("", { all: "yes" }));
threw("promises family name", () => dns.promises.lookup("", { family: "IPv6" }));
threw("cb bad verbatim", () => dns.lookup("", { verbatim: 1 }, () => {}));
threw("cb bad order", () => dns.lookup("", { order: "bogus" }, () => {}));
threw("promises bad verbatim", () => dns.promises.lookup("", { verbatim: "x" }));
threw("promises bad order", () => dns.promises.lookup("", { order: "x" }));
threw("cb good order", () => dns.lookup("", { verbatim: false, order: "ipv6first" }, () => {}));
threw("promises bad hints", () => dns.promises.lookup("", { hints: "x" }));

const lookupP = promisify(dns.lookup);
const settled = (p) => p.then((v) => JSON.stringify(v), (e) => `REJECTS ${e.code}`);
console.log("promisified", await settled(lookupP("")), await settled(lookupP("", 6)), await settled(lookupP("", { all: true })));
console.log("lookup own symbols", Object.getOwnPropertySymbols(dns.lookup).length);

// The warning is delivered on a later tick; one for the whole run.
await new Promise((resolve) => setImmediate(resolve));
console.log("warnings", warnings.length);
for (const w of warnings) console.log(w);
