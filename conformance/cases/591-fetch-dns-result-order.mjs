// fetch() follows dns's result order (dns.setDefaultResultOrder(),
// --dns-result-order), as node's does: undici connects through net, whose
// dns.lookup applies it. oam's own transport dialled getaddrinfo's order
// whatever the default said. A connection already in the pool is reused
// whatever order opened it, as undici's origin-keyed pool reuses it
// (measured on node v22.22.2).
//
// Each order runs in a fresh process, since the pool would otherwise hand
// the next fetch the first one's connection. The server listens on both
// families; what is compared is whether a fetch arrived over IPv4, against
// what the order and this host's own answer for localhost predict.
import { execFile } from "node:child_process";
import dns from "node:dns";
import http from "node:http";

const scenario = process.argv[2];

if (scenario === "client") {
  const [port, ...orders] = process.argv.slice(3);
  const seen = [];
  for (const order of orders) {
    if (order !== "-") dns.setDefaultResultOrder(order);
    const from = await (await fetch(`http://localhost:${port}/`)).text();
    seen.push(from === "::ffff:127.0.0.1" || from === "127.0.0.1" ? 4 : 6);
  }
  console.log(JSON.stringify(seen));
} else {
  const server = http.createServer((req, res) => res.end(req.socket.remoteAddress)).listen(0, "::");
  await new Promise((resolve) => server.once("listening", resolve));
  const port = server.address().port;
  const localhost = await dns.promises.lookup("localhost", { all: true, order: "verbatim" });
  const first = localhost[0]?.family;
  const has6 = localhost.some((a) => a.family === 6);
  const has4 = localhost.some((a) => a.family === 4);
  const run = (flags, orders) => new Promise((resolve) => {
    execFile(process.execPath, [...flags, import.meta.filename, "client", String(port), ...orders],
      (err, stdout) => resolve(err ? `error ${err.code}` : JSON.parse(stdout)));
  });
  const expect = (order) => (order === "ipv4first" ? (has4 ? 4 : 6)
    : order === "ipv6first" ? (has6 ? 6 : 4)
      : first);
  for (const order of ["verbatim", "ipv4first", "ipv6first"]) {
    const [family] = await run([], [order]);
    console.log(`fetch under ${order} as predicted`, family === expect(order));
  }
  const [flagged] = await run(["--dns-result-order=ipv4first"], ["-"]);
  console.log("fetch under --dns-result-order=ipv4first as predicted", flagged === expect("ipv4first"));
  // One process: the second order's fetches reuse the first one's pooled
  // connections, so every fetch arrives over the first order's family.
  const reused = await run([], ["ipv4first", "ipv4first", "ipv6first", "ipv6first"]);
  console.log("an order change reuses the pooled connections",
    Array.isArray(reused) && reused.every((family) => family === expect("ipv4first")));
  server.close();
}
