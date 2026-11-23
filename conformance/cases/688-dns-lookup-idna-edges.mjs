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
  `a�b${L}`, "�", "­", `‍${L}`, `a‍b${L}`,
  `٣${L}`, `a٣${L}`, `א${L}`, `אa${L}`, `aא${L}`,
  // Mapped forms that resolve as localhost.
  "LOCALHOST", "loc­alhost", "ｌｏｃａｌｈｏｓｔ",
  "localhost.", "localhost。",
  // Mapped forms that do not.
  `ß${L}`, `é${L}`, `é${L}`, `a。b${L}`, `．${L}`,
  // Empty labels, hyphens, length.
  `a.${L}`, L, `localhost..`, ".", "..", `-a${L}`, `a-${L}`, `ab--c${L}`,
  `a_b${L}`, `*${L}`, `${"a".repeat(63)}${L}`, `${"a".repeat(64)}${L}`,
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
