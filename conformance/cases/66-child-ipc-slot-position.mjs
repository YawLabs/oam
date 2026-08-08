// WHERE an 'ipc' slot sits in the stdio array, and what that position means for
// every OTHER slot.
//
// Case 62 pins the one canonical shape (['pipe','pipe','pipe','ipc']) and case
// 58 pins 'ipc' alongside two extra fds. Neither proves the slot's position is
// HONORED rather than merely tolerated: every array in both is all-'pipe'
// below the channel, so a null at the ipc index is indistinguishable from a
// null anywhere else, and sliding the three real dispositions by one would not
// move a single observable.
//
// What makes position observable is a NON-piped slot underneath the channel.
// With 'ignore' at index 0 (leg 1) and at index 1 (leg 2), child.stdio carries
// nulls at TWO indices for two unrelated reasons, and the captured bytes prove
// each disposition landed on the fd the caller actually named -- an off-by-one
// in the slots below the channel flips these lines.
//
// Background, oam-side, deliberately NOT asserted here (divergence 20 in
// docs/node-divergences.md): oam carries the IPC channel over a loopback socket
// rather than an fd, so it removes the 'ipc' entry from the array before it
// spawns. That removal is a no-op only when 'ipc' is the LAST entry at index 3
// or above; anywhere else it renumbers the real fds after it, so oam refuses
// those shapes with ERR_INVALID_ARG_VALUE. Node ACCEPTS them -- it makes the
// caller's own slot the channel fd, so ['pipe','ipc','pipe'] means the child's
// stdout IS the channel -- which is why the refusal cannot live in a
// differential case: the runtimes genuinely disagree there. Pinned here is the
// other half of that same rule, the set of positions both runtimes support.
//
// Every observable is printed by THIS process from a CAPTURED pipe; no child is
// ever 'inherit'ed, for the ordering reason case 56 documents. child.stdio is
// read from the 'spawn' event because oam binds the channel before it execs and
// so resolves an IPC child one tick later than Node (divergence 6) -- from
// 'spawn' onward both agree, and Node's own docs recommend reading it there.
import { spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

// pid-scoped: the runner executes the node and oam legs back to back, and a
// shared fixed path would let one leg's cleanup delete the other's fixtures.
const dir = path.join(os.tmpdir(), `oam-conf-ipcslot-${process.pid}`);
mkdirSync(dir, { recursive: true });

// One child script for every leg: it marks BOTH standard output fds so the
// parent can tell which of them survived its disposition, then round-trips a
// message. The 'message' listener is registered before the first send because
// that registration is what opens the child side of the channel; the exit is
// delayed so ending the process cannot race the reply off the wire.
const childSrc = path.join(dir, "child.cjs");
writeFileSync(
  childSrc,
  "process.stdout.write('OUT\\n');" +
    "process.stderr.write('ERR\\n');" +
    "process.on('message',(m)=>{process.send({echo:m.ping});setTimeout(()=>process.exit(0),200);});" +
    "process.send({ready:true});",
);

// Runs one shape and returns everything the parent can observe about it. Only
// slots the caller asked to 'pipe' are drained -- reading a null slot is the
// bug this case is looking for, not something to paper over.
const probe = (stdio, ping) =>
  new Promise((done) => {
    const cp = spawn(process.execPath, [childSrc], { stdio });
    let out = "";
    let err = "";
    const seen = [];
    let shape = null;

    cp.on("spawn", () => {
      if (cp.stdout) cp.stdout.on("data", (c) => { out += c.toString(); });
      if (cp.stderr) cp.stderr.on("data", (c) => { err += c.toString(); });
      // The nullness PATTERN across the whole array is the assertion: which
      // indices are null, and which still hold a live stream.
      shape = cp.stdio.map((s) => (s === null ? "null" : "stream"));
      // Nothing is ever written to the child's stdin; close it so the child is
      // never left waiting on a writer this process has no intention of being.
      if (cp.stdin) cp.stdin.end();
    });

    cp.on("message", (m) => {
      seen.push(JSON.stringify(m));
      if (m.ready) cp.send({ ping });
    });

    cp.on("close", (code) => {
      done({
        len: cp.stdio.length,
        shape: shape.join(","),
        // cp.stdout/cp.stderr are the same objects as stdio[1]/stdio[2]; that
        // they agree is itself part of the contract.
        aliased:
          cp.stdout === cp.stdio[1] && cp.stderr === cp.stdio[2] && cp.stdin === cp.stdio[0],
        out: out.trim(),
        err: err.trim(),
        seen: seen.join(" "),
        code,
      });
    });
  });

// --------------------------------------------------------------------------
// 1. 'ignore' UNDER the channel, at index 0.
//
// Two nulls in child.stdio, at 0 and 3, meaning two different things: an
// ignored stdin and a socket-backed channel. cp.stdin is null with it, so a
// supervisor cannot write to a child it declined to give a stdin -- and the
// child's own stdout/stderr both still reach this process, which is what says
// the channel took index 3 and did not displace them.
// --------------------------------------------------------------------------
{
  const r = await probe(["ignore", "pipe", "pipe", "ipc"], "PING-1");
  console.log("ignore-stdin len", r.len);
  console.log("ignore-stdin shape", r.shape);
  console.log("ignore-stdin aliased", r.aliased);
  console.log("ignore-stdin out", JSON.stringify(r.out));
  console.log("ignore-stdin err", JSON.stringify(r.err));
  console.log("ignore-stdin messages", r.seen);
  console.log("ignore-stdin exit", r.code);
}

// --------------------------------------------------------------------------
// 2. 'ignore' in the MIDDLE, at index 1.
//
// The sharpest position probe available on the ordinary (no extra fd) path.
// The child writes the same two lines as before, but only ERR can arrive: OUT
// went to the null device because index 1 said 'ignore'. Slide the array by
// one in either direction and this flips -- OUT would arrive and ERR would
// vanish -- so these two lines together are what pin the dispositions to their
// fds rather than to their ordinal position in a post-splice array.
// --------------------------------------------------------------------------
{
  const r = await probe(["pipe", "ignore", "pipe", "ipc"], "PING-2");
  console.log("ignore-stdout len", r.len);
  console.log("ignore-stdout shape", r.shape);
  console.log("ignore-stdout aliased", r.aliased);
  console.log("ignore-stdout out", JSON.stringify(r.out));
  console.log("ignore-stdout err", JSON.stringify(r.err));
  console.log("ignore-stdout messages", r.seen);
  console.log("ignore-stdout exit", r.code);
}

// --------------------------------------------------------------------------
// 3. The channel at index 4, one extra fd below it.
//
// Proves the reported ipc index is DERIVED from the array and not the constant
// 3 the ordinary shape would let it be mistaken for. Case 58 makes the same
// point at index 5 with two extra fds; the single-extra-fd array here is the
// smallest one that can make it, and it is the shape that fails if the derived
// index is ever re-hardcoded to "length - 1" of the wrong array.
//
// Platform-gated exactly as case 58 is: >3 entries routes to the raw extra-fd
// spawn backend, which exists on win32/linux/darwin only. Elsewhere the
// expected lines are printed verbatim so the differential still compares.
// --------------------------------------------------------------------------
{
  const supported =
    process.platform === "win32" ||
    process.platform === "linux" ||
    process.platform === "darwin";

  if (!supported) {
    console.log("extra-fd len 5");
    console.log("extra-fd shape stream,stream,stream,stream,null");
    console.log('extra-fd out "OUT"');
    console.log('extra-fd err "ERR"');
    console.log('extra-fd messages {"ready":true} {"echo":"PING-3"}');
    console.log("extra-fd exit 0");
  } else {
    const r = await probe(["pipe", "pipe", "pipe", "pipe", "ipc"], "PING-3");
    console.log("extra-fd len", r.len);
    console.log("extra-fd shape", r.shape);
    console.log("extra-fd out", JSON.stringify(r.out));
    console.log("extra-fd err", JSON.stringify(r.err));
    console.log("extra-fd messages", r.seen);
    console.log("extra-fd exit", r.code);
  }
}

rmSync(dir, { recursive: true, force: true });
console.log("exit 0");
