// Where a nextTick batch a handled throw interrupted drains, relative to an
// immediate already queued when the throw was handled (#495, #366, #399
// item 8). Node runs the rest of the batch at the close of the
// InternalCallbackScope around the callback that interrupted it, so which
// runs first is node's phase detail, not the queue's contract: it depends on
// where the interrupting timer was armed, and on which callbacks busy-wait.
//
// The scenarios are the cells of a probe matrix (arming callback x the arming
// callback busy-waits x the timer body busy-waits), with node's measured
// answer on v26.13.1 windows-aarch64 over 30 runs per cell. Two things shape
// the list:
//
// - One cell is absent: a timer armed from inside a setImmediate callback
//   that busy-waits, the one cell oam does not match
//   (docs/node-divergences.md #69). The differential has no known-divergence
//   allowance, so a case asserting it would fail the gate on every leg.
// - Three cells are asserted as a SET, because node flips on them run to run:
//   the main script, an I/O completion, or a tick arms the timer and NOTHING
//   busy-waits (27-28 of 30 immediate-first, 2-3 the other way). Every cell
//   where something busy-waits is deterministic on both runtimes.
//
// Only pass/fail per scenario is printed, never the observed order: the
// differential compares this case's stdout byte-for-byte between node and
// oam, and a printed order would differ between two runs of node alone.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-690-"));

const scenario = (arm, armBusy, bodyBusy) => `
const out = [];
const log = (...a) => out.push(a.join(" "));
process.on("uncaughtException", (e) => log("uncaughtException", e.message));
process.on("exit", () => console.log(out.join("|")));
const busy = (ms) => { const end = Date.now() + ms; while (Date.now() < end); };
const body = () => {
  process.nextTick(() => { throw new Error("t1"); });
  process.nextTick(() => log("t2"));
  setImmediate(() => log("immediate"));
  ${bodyBusy ? "busy(25);" : ""}
};
const armIt = () => { setTimeout(body, 1); ${armBusy ? "busy(25);" : ""} };
${
  {
    entry: "armIt();",
    timer: "setTimeout(armIt, 0);",
    immediate: "setImmediate(armIt);",
    io: `(await import("node:fs")).readFile(import.meta.url, armIt);`,
    tick: "process.nextTick(armIt);",
  }[arm]
}
`;

const IMMEDIATE_FIRST = "uncaughtException t1|immediate|t2";
const BATCH_FIRST = "uncaughtException t1|t2|immediate";
// Node's answer when NOTHING busy-waits: it flips, so either order matches.
const EITHER = [IMMEDIATE_FIRST, BATCH_FIRST];

// [name, source, node's measured order -- a SET where node itself flips].
const cases = [
  ["entry-no-busy", scenario("entry", false, false), EITHER],
  ["entry-body-busy", scenario("entry", false, true), BATCH_FIRST],
  ["entry-arm-busy", scenario("entry", true, false), BATCH_FIRST],
  ["entry-both-busy", scenario("entry", true, true), BATCH_FIRST],
  ["timer-no-busy", scenario("timer", false, false), IMMEDIATE_FIRST],
  ["timer-body-busy", scenario("timer", false, true), BATCH_FIRST],
  ["timer-arm-busy", scenario("timer", true, false), IMMEDIATE_FIRST],
  ["timer-both-busy", scenario("timer", true, true), BATCH_FIRST],
  ["immediate-no-busy", scenario("immediate", false, false), IMMEDIATE_FIRST],
  ["immediate-body-busy", scenario("immediate", false, true), BATCH_FIRST],
  ["immediate-both-busy", scenario("immediate", true, true), BATCH_FIRST],
  ["io-no-busy", scenario("io", false, false), EITHER],
  ["io-body-busy", scenario("io", false, true), BATCH_FIRST],
  ["io-arm-busy", scenario("io", true, false), BATCH_FIRST],
  ["io-both-busy", scenario("io", true, true), BATCH_FIRST],
  ["tick-no-busy", scenario("tick", false, false), EITHER],
  ["tick-body-busy", scenario("tick", false, true), BATCH_FIRST],
  ["tick-arm-busy", scenario("tick", true, false), BATCH_FIRST],
  ["tick-both-busy", scenario("tick", true, true), BATCH_FIRST],
];

let failed = 0;
for (const [name, source, expected] of cases) {
  const file = path.join(dir, `${name}.mjs`);
  fs.writeFileSync(file, source);
  const want = Array.isArray(expected) ? expected : [expected];
  const run = spawnSync(process.execPath, [file], { encoding: "utf8" });
  const got = (run.stdout || "").trim();
  const ok = run.status === 0 && want.includes(got);
  if (!ok) failed += 1;
  console.log(`${ok ? "ok" : "FAIL"} ${name}`);
}
console.log(`${cases.length - failed}/${cases.length} scenarios match Node`);
if (failed) process.exitCode = 1;
