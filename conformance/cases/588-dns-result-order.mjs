// dns's result order, as node v22.22.2 has it:
//
// * dns.getDefaultResultOrder() is "verbatim"; setDefaultResultOrder()
//   validates node's three names and sets the one default dns and
//   dns.promises share (the same two functions on both).
// * a lookup's order is the default, then `verbatim` (true verbatim, false
//   ipv4first), then `order`, which wins; ipv4first / ipv6first are a stable
//   partition by family of the resolver's answer.
// * --dns-result-order (both forms) and NODE_OPTIONS set the initial
//   default, argv winning; a bad value exits 1 with ERR_INVALID_ARG_VALUE.
// * net.connect follows the default too: with autoSelectFamily off, a
//   localhost connect under ipv4first dials 127.0.0.1 first.
//
// oam validated order / verbatim but ignored them, had a no-op
// setDefaultResultOrder and no getDefaultResultOrder, and refused the flag.
//
// The answers are the host's own (localhost and os.hostname()), so what is
// printed is invariants, not address lists.
import { spawnSync } from "node:child_process";
import dns from "node:dns";
import net from "node:net";
import os from "node:os";

const scenario = process.argv[2];

const families = (answer) => answer.map((a) => a.family).join("");
// ipv4first / ipv6first of `verbatim`, by the definition node implements.
const partition = (verbatim, first) => [
  ...verbatim.filter((a) => a.family === first),
  ...verbatim.filter((a) => a.family !== first),
];
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

if (scenario === "report") {
  console.log("default", dns.getDefaultResultOrder(), dns.promises.getDefaultResultOrder());
} else {
  console.log("default", dns.getDefaultResultOrder(), dns.promises.getDefaultResultOrder());
  console.log("same functions", dns.getDefaultResultOrder === dns.promises.getDefaultResultOrder,
    dns.setDefaultResultOrder === dns.promises.setDefaultResultOrder,
    dns.getDefaultResultOrder.length, dns.setDefaultResultOrder.length);
  for (const bad of [undefined, "IPV4FIRST", 4, null, ""]) {
    try {
      dns.setDefaultResultOrder(bad);
      console.log("set", JSON.stringify(bad), "accepted");
    } catch (e) {
      console.log("set", String(bad), e.name, e.code, e.message);
    }
  }
  console.log("set returns", dns.setDefaultResultOrder("verbatim"));

  for (const host of ["localhost", os.hostname()]) {
    const label = host === "localhost" ? "localhost" : "hostname";
    const verbatim = await dns.promises.lookup(host, { all: true, order: "verbatim" });
    const checks = [];
    for (const [opts, expect] of [
      [{ order: "ipv4first" }, partition(verbatim, 4)],
      [{ order: "ipv6first" }, partition(verbatim, 6)],
      [{ verbatim: true }, verbatim],
      [{ verbatim: false }, partition(verbatim, 4)],
      [{ verbatim: true, order: "ipv4first" }, partition(verbatim, 4)],
      [{ verbatim: false, order: "ipv6first" }, partition(verbatim, 6)],
    ]) {
      const got = await dns.promises.lookup(host, { all: true, ...opts });
      const one = await dns.promises.lookup(host, opts);
      const cb = await new Promise((resolve) => dns.lookup(host, { all: true, ...opts }, (e, a) => resolve(a)));
      checks.push(`${JSON.stringify(opts)}=${same(got, expect) && same(cb, expect) && same(one, expect[0])}`);
    }
    for (const [order, expect] of [["ipv4first", partition(verbatim, 4)], ["ipv6first", partition(verbatim, 6)]]) {
      dns.promises.setDefaultResultOrder(order);
      const got = await dns.promises.lookup(host, { all: true });
      const cb = await new Promise((resolve) => dns.lookup(host, { all: true }, (e, a) => resolve(a)));
      checks.push(`default ${order} ${dns.getDefaultResultOrder()}=${same(got, expect) && same(cb, expect)}`);
    }
    dns.setDefaultResultOrder("verbatim");
    console.log(label, "ordering", checks.join(" "));
    console.log(label, "families present", [...new Set(families(verbatim))].sort().join(""));
  }

  // net.connect under ipv4first, to a server on 127.0.0.1 only.
  const localhost = await dns.promises.lookup("localhost", { all: true, order: "verbatim" });
  const server = net.createServer((s) => s.end()).listen(0, "127.0.0.1");
  await new Promise((resolve) => server.once("listening", resolve));
  const port = server.address().port;
  const connect = (order) => new Promise((resolve) => {
    dns.setDefaultResultOrder(order);
    const s = net.connect({ host: "localhost", port, autoSelectFamily: false });
    s.on("connect", () => { resolve(`connected ${s.remoteAddress}`); s.destroy(); });
    s.on("error", (e) => resolve(`error ${e.code}`));
  });
  console.log("net ipv4first", await connect("ipv4first"));
  // Under verbatim the answer decides: whichever family the host lists
  // first is dialled, which is ::1 (refused) where it lists ::1 first.
  const firstFamily = localhost[0]?.family;
  const verbatimOutcome = await connect("verbatim");
  console.log("net verbatim follows the resolver",
    firstFamily === 4 ? verbatimOutcome === "connected 127.0.0.1" : verbatimOutcome === "error ECONNREFUSED");
  dns.setDefaultResultOrder("verbatim");
  server.close();

  // The flag.
  const run = (flags, env = {}) => {
    const child = spawnSync(process.execPath, [...flags, import.meta.filename, "report"], {
      encoding: "utf8",
      env: { ...process.env, ...env },
    });
    const err = child.stderr.split(/\r?\n/).find((l) => l.includes("ERR_INVALID_ARG_VALUE")) ?? "";
    console.log(`## ${[...flags, ...Object.entries(env).map(([k, v]) => `${k}=${v}`)].join(" ")}`,
      "exit", child.status, child.stdout.trim(), err.trim().replace(/^.*TypeError/, "TypeError"));
  };
  run(["--dns-result-order=ipv4first"]);
  run(["--dns-result-order", "ipv6first"]);
  run([], { NODE_OPTIONS: "--dns-result-order=ipv4first" });
  run(["--dns-result-order=ipv6first"], { NODE_OPTIONS: "--dns-result-order=ipv4first" });
  run(["--dns-result-order=bogus"]);
  run([], { NODE_OPTIONS: "--dns-result-order=bogus" });
}
