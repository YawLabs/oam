// Every DNS name oam returns must be shaped the way c-ares shapes it for node:
// no trailing root dot, punycode labels left as wire bytes, and the root name
// rendered as the empty string rather than ".".
//
// hickory's Display emits a fully-qualified name WITH the final dot and
// un-punycodes IDNA labels, so a straight to_string() diverged on both counts
// at once -- `dns.reverse("8.8.8.8")` gave ["dns.google."], and an IDN
// nameserver came back as decoded Cyrillic where node reports "xn--p1acf".
//
// Also pins the ENOTFOUND/ENODATA split. "no records" is two different node
// errors -- the name does not exist (NXDOMAIN -> ENOTFOUND) versus the name
// exists but has no record of that type (NOERROR -> ENODATA) -- and collapsing
// them made a live name look like a missing one.
//
// Fixtures are chosen to be stable rather than convenient: 8.8.8.8's PTR,
// example.com's RFC 7505 null MX (an intentionally empty root name), the IANA
// reserved .invalid TLD, and a.iana-servers.net which publishes AAAA but no
// TXT or MX. This case does need the network; it is in the same family as
// 88-dns-error-shape.mjs.
import dns from "node:dns";

const done = [];
const record = (label, value) => done.push(`${label} ${value}`);

function shape(label, err, value) {
  record(label, err ? `ERR ${err.code}` : JSON.stringify(value));
}

// 1) Trailing dot: a PTR name, both through reverse() and resolvePtr().
await new Promise((r) =>
  dns.reverse("8.8.8.8", (e, v) => {
    shape("reverse", e, v);
    r();
  }),
);
await new Promise((r) =>
  dns.resolvePtr("8.8.8.8.in-addr.arpa", (e, v) => {
    shape("resolvePtr", e, v);
    r();
  }),
);

// 2) The ROOT name must be "", not ".". example.com publishes RFC 7505's null
//    MX -- priority 0 with an empty exchange -- which is the only widely
//    deployed record whose name field is the root label.
await new Promise((r) =>
  dns.resolveMx("example.com", (e, v) => {
    shape("nullMX", e, v);
    r();
  }),
);

// 3) Punycode must survive as the wire form.
await new Promise((r) =>
  dns.resolveNs("xn--p1acf", (e, v) => {
    // Nameserver order is round-robin, so sort before printing.
    shape("punycodeNS", e, v && v.slice().sort());
    r();
  }),
);

// 4) NXDOMAIN -> ENOTFOUND. .invalid is reserved by RFC 2606 and can never
//    resolve, so this is deterministic without depending on a live zone.
await new Promise((r) =>
  dns.resolve4("no-such-host.invalid", (e, v) => {
    shape("nxdomain", e, v);
    r();
  }),
);

// 5) NOERROR-with-no-answers -> ENODATA. The name resolves (it has AAAA), so a
//    TXT miss is an absence of that TYPE, not of the name.
await new Promise((r) =>
  dns.resolveTxt("a.iana-servers.net", (e, v) => {
    shape("nodata", e, v);
    r();
  }),
);

// 5b) CAA records are keyed BY THEIR TAG: node's shape is
//     {critical, <tag>: <value>}, not a fixed `issue` key with a separate
//     `value`. wikipedia.org is the useful fixture because it publishes an
//     `iodef` record alongside two `issue` records, so a hardcoded key shows up
//     immediately. `critical` is the wire flags octet (128 when the issuer
//     critical bit is set), not a 0/1 boolean -- no public fixture here
//     publishes a critical record, so only the 0 path is covered live.
await new Promise((r) =>
  dns.resolveCaa("wikipedia.org", (e, v) => {
    // Record order is not guaranteed; sort by the serialized form.
    shape("caa", e, v && v.slice().map((x) => JSON.stringify(x)).sort());
    r();
  }),
);

// 6) getServers is a non-empty array of strings on both runtimes. The VALUES
//    are the host's own nameservers, so they are deliberately not printed --
//    only the shape, which is what can regress.
const servers = dns.getServers();
record(
  "getServers",
  JSON.stringify({
    isArray: Array.isArray(servers),
    nonEmpty: servers.length > 0,
    allStrings: servers.every((s) => typeof s === "string"),
    allNonEmpty: servers.every((s) => s.length > 0),
  }),
);

// Emitted in a fixed order: the awaits above are sequential, but printing at
// the end keeps the output stable even if that ever changes.
for (const line of done) {
  console.log(line);
}
