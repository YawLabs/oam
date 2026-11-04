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
import http from "node:http";
import net from "node:net";
import os from "node:os";
import { Worker, isMainThread } from "node:worker_threads";

const scenario = isMainThread ? process.argv[2] : "in-worker";

const families = (answer) => answer.map((a) => a.family).join("");
// ipv4first / ipv6first of `verbatim`, by the definition node implements.
const partition = (verbatim, first) => [
  ...verbatim.filter((a) => a.family === first),
  ...verbatim.filter((a) => a.family !== first),
];
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

if (scenario === "in-worker") {
  console.log("worker ran", dns.getDefaultResultOrder());
} else if (scenario === "report") {
  console.log("default", dns.getDefaultResultOrder(), dns.promises.getDefaultResultOrder());
} else if (scenario === "worker") {
  // A Worker's own --dns-result-order: a bad one fails the worker as it
  // starts (exit 1, no 'error' event); a good one is its default.
  const start = (order) => new Promise((resolve) => {
    const worker = new Worker(new URL(import.meta.url), { execArgv: [`--dns-result-order=${order}`] });
    worker.on("error", (e) => console.log("worker error event", e.name, e.code));
    worker.on("exit", (code) => { console.log(`worker with ${order} exit`, code); resolve(); });
  });
  await start("ipv6first");
  await start("bogus");
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

  // http.get over the stock agent follows the order too, to a server on
  // both families: under ipv4first it arrives from 127.0.0.1.
  const both = http.createServer((req, res) => res.end(req.socket.remoteAddress)).listen(0, "::");
  await new Promise((resolve) => both.once("listening", resolve));
  const get = () => new Promise((resolve) => {
    http.get({ host: "localhost", port: both.address().port, agent: false }, (res) => {
      let body = "";
      res.on("data", (chunk) => (body += chunk));
      res.on("end", () => resolve(body));
    }).on("error", (e) => resolve(e.code));
  });
  for (const order of ["ipv4first", "ipv6first", "verbatim"]) {
    dns.setDefaultResultOrder(order);
    const from = await get();
    const v4 = from === "::ffff:127.0.0.1" || from === "127.0.0.1";
    const expect = order === "ipv4first"
      ? true
      : order === "ipv6first"
        ? !localhost.some((a) => a.family === 6)
        : localhost[0]?.family === 4;
    console.log(`http.get under ${order} from IPv4`, v4 === expect);
  }
  dns.setDefaultResultOrder("verbatim");
  both.close();

  // The flag.
  const run = (flags, env = {}) => {
    const child = spawnSync(process.execPath, [...flags, import.meta.filename, "report"], {
      encoding: "utf8",
      env: { ...process.env, ...env },
    });
    // The error's own line (the report's source frame names a builtin of
    // each runtime's), and whether the report carries the error's code.
    const lines = child.stderr.split(/\r?\n/);
    const err = lines.find((l) => l.startsWith("TypeError") || l.includes("requires an argument")) ?? "";
    const coded = lines.some((l) => l.trim() === "code: 'ERR_INVALID_ARG_VALUE'");
    console.log(`## ${[...flags, ...Object.entries(env).map(([k, v]) => `${k}=${v}`)].join(" ")}`,
      "exit", child.status, child.stdout.trim(),
      // The executable's path heads node's option-parser line; it is not compared.
      err.trim().replace(/^.*?: (--dns-result-order)/, "$1"), coded ? "{ code }" : "");
  };
  run(["--dns-result-order=ipv4first"]);
  run(["--dns-result-order", "ipv6first"]);
  run([], { NODE_OPTIONS: "--dns-result-order=ipv4first" });
  run(["--dns-result-order=ipv6first"], { NODE_OPTIONS: "--dns-result-order=ipv4first" });
  run(["--dns-result-order=bogus"]);
  run([], { NODE_OPTIONS: "--dns-result-order=bogus" });
  // No value: node's option parser refuses it, exit 9.
  run(["--dns-result-order="]);
  run(["--dns-result-order", "--no-warnings"]);
  run([], { NODE_OPTIONS: "--dns-result-order" });
  run([], { NODE_OPTIONS: "--dns-result-order --no-warnings" });
  run([], { NODE_OPTIONS: "--dns-result-order ipv6first" });

  const worker = spawnSync(process.execPath, [import.meta.filename, "worker"], { encoding: "utf8" });
  console.log("## worker", "exit", worker.status);
  for (const line of worker.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
}
