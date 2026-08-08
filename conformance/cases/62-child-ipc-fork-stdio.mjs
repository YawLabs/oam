// The two ordinary ways to ask for an IPC channel, neither of which case 58
// covers: a plain `spawn` whose stdio array carries an 'ipc' slot, and a
// `fork` handed an EXPLICIT stdio array instead of the silent/default shorthand.
//
// Case 58 exercises 'ipc' alongside numbered fds above 2 -- the extra-fd spawn
// backend, which is the exotic path (Chromium's CDP pipe). The shape below is
// the one every ordinary supervisor writes: three standard slots plus a
// channel. It routes through a completely different branch of spawn() -- the
// 'ipc' entry is spliced out FIRST, which drops the array back to 3 entries so
// it never reaches the extra-fd backend -- and so it can regress on its own.
//
// What is pinned here:
//   * spawn(..., { stdio: ['pipe','pipe','pipe','ipc'] }) gets a real, working
//     channel: parent -> child and child -> parent, both directions.
//   * child.stdio reads back with the ipc slot as null AT THE INDEX THE CALLER
//     PUT IT. That index used to be hardcoded to 3; it is now derived from
//     where 'ipc' actually appeared. Index 3 IS the answer for this shape, so
//     this case is what proves the derived value did not regress the common
//     case while fixing the extra-fd one.
//   * the piped stdout is still a live stream and still delivers, i.e. adding
//     a channel did not cost the caller its output.
//   * fork(module, args, { stdio: [...] }) with an explicit array: the array
//     wins over fork's inherit-by-default, so 'pipe' at slot 1 captures the
//     child's stdout on child.stdout exactly as `silent: true` would -- while
//     the 'ipc' entry in that same array keeps messaging alive. Case 56
//     already covers fork's DEFAULT (inherit) and `silent: true` shapes; only
//     the explicit-array form is covered here.
//
// Every observable is printed by THIS process from a CAPTURED pipe. Both
// children are 'pipe'd rather than inherited for exactly the reason case 56
// documents: an inherited child writes straight into this process's stdout, so
// its bytes would land at the mercy of scheduling and the compared stdout
// would not be deterministic.
//
// pid and child.stdio[n] are read from the 'spawn' event, never synchronously
// after the call. oam binds the IPC channel on a loopback socket before it
// execs, so an IPC child's handle resolves one tick later than Node's (see
// "child.pid is one tick late for IPC children" in docs/node-divergences.md).
// From 'spawn' onward both runtimes agree -- and reading it there is what
// Node's own docs recommend regardless, so this is not a concession, it is the
// shape that is correct on both.
import { fork, spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

// pid-scoped: the runner executes the node and oam legs back to back, and a
// shared fixed path would let one leg's cleanup delete the other's fixtures.
const dir = path.join(os.tmpdir(), `oam-conf-ipcfork-${process.pid}`);
mkdirSync(dir, { recursive: true });

// Both children follow the same script: announce on stdout, register a
// 'message' listener, then say hello over the channel. The listener is
// registered BEFORE the first send because that registration is what opens the
// child side of the channel -- sending first would queue into a socket nobody
// had connected yet.
//
// The exit is delayed rather than immediate: a live channel keeps the child's
// loop open, so the child has to end itself, and doing so in the same turn as
// the reply would race the reply's flush off the wire.
const childBody = (tag) =>
  `process.stdout.write('CHILD_${tag}_STDOUT\\n');` +
  "process.on('message',(m)=>{process.send({echo:m.ping});setTimeout(()=>process.exit(0),200);});" +
  `process.send({ready:'${tag}'});`;

const spawnChild = path.join(dir, "spawn-child.cjs");
writeFileSync(spawnChild, childBody("A"));

const forkChild = path.join(dir, "fork-child.cjs");
writeFileSync(forkChild, childBody("B"));

// --------------------------------------------------------------------------
// 1. Ordinary spawn with an ipc slot.
// --------------------------------------------------------------------------
{
  const cp = spawn(process.execPath, [spawnChild], {
    // Exactly 4 entries: three standard slots plus the channel. Once 'ipc' is
    // spliced out this is a plain 3-slot spawn, which is the whole point --
    // the channel must survive a path that has no idea it exists.
    stdio: ["pipe", "pipe", "pipe", "ipc"],
  });

  let out = "";
  const seen = [];
  let stdioLen = null;
  let ipcSlotIsNull = null;
  let stdStreamsLive = null;
  let pidIsLive = null;

  cp.on("spawn", () => {
    cp.stdout.on("data", (c) => { out += c.toString(); });
    stdioLen = cp.stdio.length;
    // The ipc slot reads back as null -- at index 3, where the caller put it.
    ipcSlotIsNull = cp.stdio[3] === null;
    // ...and the three real slots next to it are untouched, live streams.
    stdStreamsLive =
      typeof cp.stdio[0]?.write === "function" &&
      typeof cp.stdio[1]?.on === "function" &&
      typeof cp.stdio[2]?.on === "function";
    pidIsLive = typeof cp.pid === "number" && cp.pid > 0;
    // Nothing is ever written to the child's stdin; close it so the child is
    // never left waiting on a writer this process has no intention of being.
    cp.stdin.end();
  });

  cp.on("message", (m) => {
    seen.push(JSON.stringify(m));
    if (m.ready) cp.send({ ping: "PING-A" });
  });

  const code = await new Promise((done) => cp.on("close", done));

  console.log("spawn ipc stdout", JSON.stringify(out.trim()));
  console.log("spawn ipc messages", seen.join(" "));
  console.log("spawn ipc stdio", stdioLen, ipcSlotIsNull, stdStreamsLive);
  console.log("spawn ipc pid live", pidIsLive);
  console.log("spawn ipc exit", code);
}

// --------------------------------------------------------------------------
// 2. fork with an EXPLICIT stdio array.
// --------------------------------------------------------------------------
{
  // An array here overrides fork's inherit default outright, so slot 1 being
  // 'pipe' is what routes the child's stdout onto child.stdout -- the same
  // observable `silent: true` produces, reached by a different option. The
  // 'ipc' entry is mandatory on this form: Node throws if an explicit fork
  // stdio array has no channel in it.
  const cp = fork(forkChild, ["ARG-B"], {
    stdio: ["pipe", "pipe", "pipe", "ipc"],
  });

  let out = "";
  const seen = [];
  let stdoutCaptured = null;
  let stdioLen = null;
  let ipcSlotIsNull = null;
  let pidIsLive = null;

  cp.on("spawn", () => {
    // A stream here -- not null -- is the assertion: the explicit 'pipe'
    // beat the inherit default, so these bytes are ours to read rather than
    // being dumped into this process's own stdout.
    stdoutCaptured = cp.stdout !== null && typeof cp.stdout.on === "function";
    cp.stdout.on("data", (c) => { out += c.toString(); });
    stdioLen = cp.stdio.length;
    ipcSlotIsNull = cp.stdio[3] === null;
    pidIsLive = typeof cp.pid === "number" && cp.pid > 0;
    cp.stdin.end();
  });

  cp.on("message", (m) => {
    seen.push(JSON.stringify(m));
    if (m.ready) cp.send({ ping: "PING-B" });
  });

  const code = await new Promise((done) => cp.on("close", done));

  console.log("fork stdio captured", stdoutCaptured);
  console.log("fork stdio stdout", JSON.stringify(out.trim()));
  console.log("fork stdio messages", seen.join(" "));
  console.log("fork stdio stdio", stdioLen, ipcSlotIsNull);
  console.log("fork stdio pid live", pidIsLive);
  console.log("fork stdio exit", code);
}

rmSync(dir, { recursive: true, force: true });
console.log("exit 0");
