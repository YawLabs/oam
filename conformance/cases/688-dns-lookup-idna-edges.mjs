// What dns.lookup, net.connect and http.get hand getaddrinfo for an IDNA
// edge-case hostname, and which names are refused up front.
//
// node's GetAddrInfo (src/cares_wrap.cc) runs the host through UTS #46
// ToASCII (ada::idna::to_ascii) first; a name ToASCII refuses or maps to
// nothing is getaddrinfo EINVAL, anything else goes to the resolver (and,
// here, fails ENOTFOUND or resolves). node 26 bundles ada 4.0.0, whose
// ToASCII passes an all-ASCII xn-- label through without decoding it
// ("xn--a", "xn--", "XN--A" lowercased) where ada 2/3 refused it, and
// applies the Bidi rule (a lone Arabic-Indic digit label is refused). Every
// name ends in .localhost (or is an empty-label shape) so nothing reaches
// a real DNS server. url.domainToASCII and the WHATWG host parser use the
// same ada, so their answers are printed alongside.
import dns from "node:dns";
import http from "node:http";
import net from "node:net";
import url from "node:url";

const j = (v) => JSON.stringify(v);
const L = ".localhost";
const hosts = [
  // Undecodable or odd punycode labels.
  `xn--a${L}`, `xn--${L}`, `XN--A${L}`, `Xn--a${L}`, `xn--a-${L}`,
  `xn--zz${L}`, `xn--ab--c${L}`, `xn--1ug${L}`, `xn--a.`, `xn--a.${L.slice(1)}.`,
  // Valid punycode, any case.
  `xn--ls8h${L}`, `XN--LS8H${L}`, `xn--nxasmq6b${L}`, `xn--55qX5d${L}`,
  // Names ToASCII refuses or maps to nothing.
  `a\uFFFDb${L}`, "\uFFFD", "\u00AD", `\u200D${L}`, `a\u200Db${L}`,
  `\u0663${L}`, `a\u0663${L}`, `\u05D0${L}`, `\u05D0a${L}`, `a\u05D0${L}`,
  // Mapped forms that resolve as localhost.
  "LOCALHOST", "loc\u00ADalhost", "\uFF4C\uFF4F\uFF43\uFF41\uFF4C\uFF48\uFF4F\uFF53\uFF54",
  "localhost.", "localhost\u3002",
  // Mapped forms that do not.
  `\u00DF${L}`, `\u00E9${L}`, `e\u0301${L}`, `a\u3002b${L}`, `\uFF0E${L}`,
  // Empty labels, hyphens, length.
  `a.${L}`, L, `localhost..`, ".", "..", `-a${L}`, `a-${L}`, `ab--c${L}`,
  `a_b${L}`, `*${L}`, `${"a".repeat(63)}${L}`, `${"a".repeat(64)}${L}`,
  // libuv writes the ToASCII form into a 256-byte buffer, NUL included:
  // 255 bytes is looked up, 256 is EINVAL -- counted on the ToASCII form,
  // so a host shorter as written (\u00FC, fullwidth letters) can cross it.
  `${"a".repeat(245)}${L}`, `${"a".repeat(246)}${L}`, `${"a".repeat(245)}${L}.`,
  `\u00FC${"a".repeat(237)}${L}`, `\u00FC${"a".repeat(238)}${L}`,
  `${"\uFF41".repeat(5)}${"a".repeat(240)}${L}`, `${"\uFF41".repeat(5)}${"a".repeat(241)}${L}`,
  // IP-like strings.
  "127.0.0.1", "127.1", "0x7f.0.0.1", "1.2.3.4.5", "999.1.1.1", "::1", "[::1]",
];

const srv = net.createServer((s) => s.destroy());
await new Promise((r) => srv.listen(0, "127.0.0.1", r));
const port = srv.address().port;

const describe = (e) => `ERROR ${e.code} ${e.syscall} ${j(e.hostname)} ${j(e.message)}`;
for (const host of hosts) {
  const ascii = url.domainToASCII(host);
  let whatwg;
  try {
    whatwg = new URL(`http://${host}/`).host;
  } catch (e) {
    whatwg = `threw ${e.code}`;
  }
  const looked = await new Promise((resolve) =>
    dns.lookup(host, { family: 4, all: true }, (e, a) =>
      resolve(e ? describe(e) : a.map((x) => x.address).join(","))),
  );
  const promised = await dns.promises.lookup(host, { family: 4 }).then(
    (a) => a.address,
    (e) => describe(e),
  );
  const connected = await new Promise((resolve) => {
    const s = net.connect({ host, port, family: 4 });
    s.on("connect", () => {
      resolve(`connect ${s.remoteAddress}`);
      s.destroy();
    });
    s.on("error", (e) => resolve(`ERROR ${e.code} ${j(e.hostname)}`));
  });
  const got = await new Promise((resolve) => {
    let req;
    try {
      req = http.get({ host, port, family: 4, path: "/" }, (res) => {
        res.resume();
        resolve(`RESPONSE ${res.statusCode}`);
      });
    } catch (e) {
      resolve(`THROW ${e.code}`);
      return;
    }
    req.on("error", (e) => resolve(`ERROR ${e.code}`));
  });
  console.log(`${j(host)} toASCII=${j(ascii)} url=${j(whatwg)}`);
  console.log(`  lookup ${looked}`);
  console.log(`  promises ${promised}`);
  console.log(`  net ${connected} | http ${got}`);
}
srv.close();
