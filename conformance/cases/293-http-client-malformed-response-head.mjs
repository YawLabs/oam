// A malformed response head is the parser's error, as node reports it:
// http.request emits llhttp's HPE_INVALID_STATUS / HPE_INVALID_CONSTANT
// `Parse Error` -- on oam's own transport (no agent of its own), through a
// lookup and through createConnection alike -- and fetch fails with
// undici's HTTPParserError as its cause. On its own transport oam reported
// ECONNRESET `socket hang up` and fetch a code-less `error sending
// request`, and every path worded a head that is not HTTP as `Expected
// HTTP/`. Measured on node v22.22.2. A status over 999 is worded
// differently by node (docs/node-divergences.md entry 38), so it is not
// here.
import http from "node:http";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const CRLF = "\r\n";
// [head, whether fetch is asked too]: undici 6.29.0 fails an assertion of
// its own on a status under 100, which oam does not reproduce.
const heads = {
  "letter in the status": ["HTTP/1.1 2x0 OK" + CRLF + CRLF, true],
  "two-digit status": ["HTTP/1.1 99 OK" + CRLF + CRLF, false],
  "two spaces before the status": ["HTTP/1.1  200 OK" + CRLF + CRLF, false],
  "not HTTP at all": ["NOT HTTP AT ALL" + CRLF + CRLF, true],
};
const lookup = (host, options, cb) =>
  options && options.all ? cb(null, [{ address: "127.0.0.1", family: 4 }]) : cb(null, "127.0.0.1", 4);

for (const [label, [head, viaFetch]] of Object.entries(heads)) {
  for (const close of [false, true]) {
    const server = net.createServer((s) => {
      s.on("error", () => {});
      s.once("data", () => (close ? s.end(head) : s.write(head)));
    });
    await new Promise((r) => server.listen(0, "127.0.0.1", r));
    const port = server.address().port;
    const tail = `${label}${close ? ", then a FIN" : ""}`;
    for (const [path, options] of [
      ["own transport", { host: "127.0.0.1", port }],
      ["lookup", { host: "head.test", port, lookup }],
      ["createConnection", { host: "127.0.0.1", port, createConnection: () => net.connect(port, "127.0.0.1") }],
    ]) {
      const got = await new Promise((done) => {
        const req = http.get(options, (res) => {
          done(`response ${res.statusCode}`);
          res.resume();
        });
        req.on("error", (e) => done(`error ${e.code} | ${e.message} | reason ${e.reason}`));
      });
      console.log(`http.get (${path}), ${tail}: ${got}`);
    }
    if (viaFetch) {
      const fetched = await fetch(`http://127.0.0.1:${port}/`).then(
        (r) => `status ${r.status}`,
        (e) => `${e.message} | ${e.cause?.name} ${e.cause?.code} | ${e.cause?.message}`,
      );
      console.log(`fetch, ${tail}: ${fetched}`);
    }
    server.close();
  }
}
