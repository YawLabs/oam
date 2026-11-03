// dns.lookup() and dns.promises.lookup() answer a falsy hostname without the
// resolver, for compatibility (#452). node's lib/dns.js checks `!hostname`
// before anything is looked up: it emits DEP0118 once per process (shared by
// both APIs, through process.emitWarning) and answers `null` with family 4 --
// 6 when family 6 was asked for -- or `[]` with `all`, on the next tick for
// the callback form, which returns `{}`. Every falsy value takes that path,
// not just "": undefined, null, false, 0, NaN, 0n. A TRUTHY non-string
// hostname is a TypeError ERR_INVALID_ARG_TYPE, as are a non-function
// callback, an options that is neither a number nor an object, and a
// non-boolean `options.all`; the promises API also refuses the callback
// API's 'IPv4' / 'IPv6' family names (measured on v22.22.2).
//
// oam handed "" to the native op, which threw `TypeError: hostname must be
// between 1 and 253 characters` with no code, synchronously out of both
// APIs; the other falsy values were stringified and failed ENOTFOUND, and a
// truthy non-string was stringified and looked up.
//
// Nothing here reaches a resolver, so the case is deterministic offline.
import dns from "node:dns";

const warnings = [];
process.on("warning", (w) => warnings.push(`${w.name} ${w.code} ${w.message}`));

const show = (v) => (typeof v === "bigint" ? `${v}n` : Number.isNaN(v) ? "NaN" : JSON.stringify(v));

for (const host of ["", undefined, null, false, 0, NaN, 0n]) {
  for (const opts of [undefined, { all: true }, 6, { family: 6 }, { family: 4 }, null]) {
    const got = await dns.promises.lookup(host, opts);
    console.log("promises", show(host), JSON.stringify(opts), JSON.stringify(got));
  }
}

let sync = true;
await new Promise((resolve) => {
  let pending = 4;
  const done = () => { if (--pending === 0) resolve(); };
  const r = dns.lookup("", (...args) => { console.log("cb plain", JSON.stringify(args), "sync", sync); done(); });
  console.log("cb returns", JSON.stringify(r));
  dns.lookup("", { all: true }, (...args) => { console.log("cb all", JSON.stringify(args), "sync", sync); done(); });
  dns.lookup(null, 6, (...args) => { console.log("cb null 6", JSON.stringify(args), "sync", sync); done(); });
  dns.lookup(undefined, { family: "IPv6" }, (...args) => { console.log("cb undefined family IPv6", JSON.stringify(args), "sync", sync); done(); });
  sync = false;
});

function threw(label, fn) {
  try {
    const r = fn();
    if (r && typeof r.then === "function") r.catch(() => {});
    console.log(label, "did not throw");
  } catch (e) {
    console.log(label, e.name, e.code, e.message);
  }
}

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
threw("promises bad hints", () => dns.promises.lookup("", { hints: "x" }));

// The warning is delivered on a later tick; one for the whole run.
await new Promise((resolve) => setImmediate(resolve));
console.log("warnings", warnings.length);
for (const w of warnings) console.log(w);
