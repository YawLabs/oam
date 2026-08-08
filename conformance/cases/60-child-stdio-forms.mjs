// child_process stdio SPELLINGS. Case 56 pins the three dispositions
// themselves (inherit / ignore / pipe); this one pins the alternate spellings
// that all REDUCE to those three. Each reduction is its own place for a
// reimplementation to disagree while the plain spellings still look right:
//
//   * a bare integer names "the parent's fd of that number", so [0,1,2] is
//     long-hand 'inherit' -- a launcher written that way must hand its own fds
//     down exactly as `stdio:'inherit'` would, and expose no pipes to hold;
//   * a slot that is MISSING (array shorter than 3, or the array empty), or
//     present but null/undefined, falls back to the DEFAULT, which is 'pipe'.
//     Reading "unspecified" as "closed"/'ignore' is the tempting bug, and it is
//     invisible from the parent's side: the child boots, writes, exits 0, and
//     the bytes go nowhere;
//   * 'overlapped' is a Windows I/O-MODE hint (open the pipe
//     FILE_FLAG_OVERLAPPED) and nothing more. The slot is still an ordinary
//     pipe, on every platform, so it must be neither rejected nor quietly
//     degraded to 'ignore' off-Windows;
//   * spawnSync reports a never-piped slot as null rather than as a zero-length
//     Buffer -- the distinction a caller doing `r.stdout.toString()` depends on;
//   * spawnSync's `input` fills slot 0 only when slot 0 is PIPE-SHAPED. See the
//     long note at probe 9: the docs say `input` "overrides stdio[0]", the
//     implementation does not, and that gap is a live divergence.
//
// Ordering hazard (inherited from 56): a child holding an inherited stdout
// writes DIRECTLY into this process's stdout, racing whatever this process has
// buffered but not yet flushed. So nothing observable here is ever produced by
// an inheriting child -- every child below is either silent (shape-only probes)
// or has its stdout on a pipe this process drains and then prints itself.
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

// pid-scoped: the runner executes the node and oam legs back to back, and a
// shared fixed path would let one leg's cleanup delete the other's fixtures.
const dir = path.join(os.tmpdir(), `oam-conf-stdio-forms-${process.pid}`);
mkdirSync(dir, { recursive: true });

// Silent: the only child safe to hand an INHERITED stdout, since it contributes
// nothing that could interleave with this process's own writes.
const silent = path.join(dir, "silent.cjs");
writeFileSync(silent, "process.exitCode = 0;");

// Noisy: only ever spawned with slots 1/2 piped (or discarded), so its bytes
// reach the comparison through a drain this process controls.
const noisy = path.join(dir, "noisy.cjs");
writeFileSync(
  noisy,
  "process.stdout.write('OUT\\n');process.stderr.write('ERR\\n');",
);

// Echo: proves data really FLOWS on a slot rather than merely that a stream
// object exists. Reads stdin to EOF, then reports it on stdout and its length on
// stderr, so one child exercises all three slots at once. Never given an
// inherited stdin -- whether that EOFs depends on how the harness was invoked.
const echo = path.join(dir, "echo.cjs");
writeFileSync(
  echo,
  "let d='';process.stdin.setEncoding('utf8');" +
    "process.stdin.on('data',(c)=>{d+=c;});" +
    "process.stdin.on('end',()=>{" +
    "process.stdout.write('ECHO:'+d+'\\n');" +
    "process.stderr.write('LEN:'+d.length+'\\n');});",
);

const drain = (cp) =>
  new Promise((done) => {
    let out = "";
    let err = "";
    cp.stdout.on("data", (d) => { out += d; });
    cp.stderr.on("data", (d) => { err += d; });
    cp.on("close", (code) => done({ out: out.trim(), err: err.trim(), code }));
  });

// 1. Numeric fd form, all three slots. [0,1,2] is 'inherit' spelled out: the
//    child gets this process's own fds, so -- exactly as for 'inherit' in case
//    56 -- there is no pipe for the parent to hold and node reports null in
//    every slot, on the child object and in child.stdio alike. The child is
//    silent precisely BECAUSE its stdout is ours.
const numeric = spawn(process.execPath, [silent], { stdio: [0, 1, 2] });
console.log(
  "numeric shape",
  numeric.stdin === null,
  numeric.stdout === null,
  numeric.stderr === null,
);
console.log(
  "numeric stdio",
  numeric.stdio.length,
  numeric.stdio[0] === null,
  numeric.stdio[1] === null,
  numeric.stdio[2] === null,
);
console.log("numeric exit", await new Promise((r) => numeric.on("close", r)));

// 2. Numeric fd form, SHORT. [0] pins the two rules at once: slot 0 is the
//    parent's fd 0 (so null), and the absent slots 1/2 default to 'pipe' rather
//    than being dropped. A runtime that truncated at the array's length would
//    give this child no stdout at all.
const numericShort = spawn(process.execPath, [noisy], { stdio: [0] });
console.log(
  "numeric short shape",
  numericShort.stdin === null,
  numericShort.stdout !== null,
  numericShort.stderr !== null,
  numericShort.stdio.length,
);
const numericShortIo = await drain(numericShort);
console.log(
  "numeric short flow",
  JSON.stringify(numericShortIo.out),
  JSON.stringify(numericShortIo.err),
  numericShortIo.code,
);

// 3. Short array, string spelling. Only slot 0 is given; 1 and 2 fall back to
//    the default 'pipe' -- NOT 'ignore'. Shape alone would still pass if the
//    slots were piped but never wired through to the child, so the bytes are
//    read back: OUT/ERR arriving is the real assertion.
const sparse = spawn(process.execPath, [noisy], { stdio: ["inherit"] });
console.log(
  "sparse shape",
  sparse.stdin === null,
  sparse.stdout !== null,
  sparse.stderr !== null,
  sparse.stdio.length,
);
const sparseIo = await drain(sparse);
console.log(
  "sparse flow",
  JSON.stringify(sparseIo.out),
  JSON.stringify(sparseIo.err),
  sparseIo.code,
);

// 4. The degenerate end of the same rule: an EMPTY array specifies nothing, so
//    all three slots take the default and the whole thing is equivalent to
//    'pipe'. Pinned separately because "shorter than 3" and "length 0" are easy
//    to normalize down different code paths.
const empty = spawn(process.execPath, [echo], { stdio: [] });
console.log(
  "empty shape",
  empty.stdin !== null,
  empty.stdout !== null,
  empty.stderr !== null,
  empty.stdio.length,
);
const emptyIo = drain(empty);
empty.stdin.end("VIA-EMPTY");
const emptyDone = await emptyIo;
console.log(
  "empty flow",
  JSON.stringify(emptyDone.out),
  JSON.stringify(emptyDone.err),
  emptyDone.code,
);

// 5. 'overlapped' as the whole-stdio shorthand. It is a pipe carrying a Windows
//    handle-mode hint, so it must behave as 'pipe' EVERYWHERE: all three streams
//    present, and payload moving in BOTH directions (write in, read out).
const overlapped = spawn(process.execPath, [echo], { stdio: "overlapped" });
console.log(
  "overlapped shape",
  overlapped.stdin !== null,
  overlapped.stdout !== null,
  overlapped.stderr !== null,
  overlapped.stdio.length,
);
const overlappedIo = drain(overlapped);
overlapped.stdin.end("PING");
const ov = await overlappedIo;
console.log(
  "overlapped flow",
  JSON.stringify(ov.out),
  JSON.stringify(ov.err),
  ov.code,
);

// 6. 'overlapped' per-slot, and mixed with the other dispositions -- the form a
//    real caller reaches for when only one slot needs the flag. 'ignore' in slot
//    0 still nulls that slot, and the overlapped slot behaves as any pipe.
//    (Deliberately spawn(), never spawnSync(): Node 22 has no 'overlapped' arm
//    in spawn_sync.cc's ParseStdioOption and ABORTS the process on
//    "Unreachable code reached" rather than throwing, so a spawnSync probe here
//    would kill the node leg outright.)
const perSlot = spawn(process.execPath, [noisy], {
  stdio: ["ignore", "overlapped", "pipe"],
});
console.log(
  "per-slot shape",
  perSlot.stdin === null,
  perSlot.stdout !== null,
  perSlot.stderr !== null,
  perSlot.stdio.length,
);
const perSlotIo = await drain(perSlot);
console.log(
  "per-slot flow",
  JSON.stringify(perSlotIo.out),
  JSON.stringify(perSlotIo.err),
  perSlotIo.code,
);

// 7. spawnSync + 'ignore'. A slot that was never piped has no captured bytes at
//    all, so node reports null -- NOT a zero-length Buffer, which is what a
//    "captured nothing" implementation hands back and what a caller doing
//    `r.stdout.toString()` would silently accept. Run it inside a launcher whose
//    own stdout/stderr this process drains: that turns "the grandchild's output
//    was discarded" into a positive, ORDERED observation (the launcher's line is
//    the only thing on the pipe) instead of an inherited-write race.
const syncIgnore = path.join(dir, "sync-ignore.cjs");
writeFileSync(
  syncIgnore,
  "const {spawnSync}=require('child_process');" +
    `const r=spawnSync(process.execPath,[${JSON.stringify(noisy)}],{stdio:'ignore'});` +
    "process.stdout.write('status='+r.status+' outNull='+(r.stdout===null)+" +
    "' errNull='+(r.stderr===null)+' outputLen='+r.output.length+" +
    "' output0Null='+(r.output[0]===null)+' output1Null='+(r.output[1]===null)+" +
    "' output2Null='+(r.output[2]===null)+'\\n');",
);
const ignoreIo = await drain(
  spawn(process.execPath, [syncIgnore], { stdio: ["ignore", "pipe", "pipe"] }),
);
console.log("spawnSync ignore", JSON.stringify(ignoreIo.out));
// Empty: the grandchild's OUT/ERR went to the null device. Had 'ignore' been
// mis-reduced to 'inherit', they would have surfaced on the launcher's pipes.
console.log(
  "spawnSync ignore leak",
  JSON.stringify(ignoreIo.err),
  ignoreIo.code,
);

// 8. spawnSync `input` reaches the child for every PIPE-SHAPED spelling of slot
//    0 -- omitted entirely, the 'pipe' shorthand, an explicit 'pipe', and an
//    explicit null (which is just the default again, i.e. probe 4's rule seen
//    from spawnSync's side). All four must land the payload identically.
for (const [label, extra] of [
  ["omitted", {}],
  ["'pipe'", { stdio: "pipe" }],
  ["explicit pipes", { stdio: ["pipe", "pipe", "pipe"] }],
  ["null slot0", { stdio: [null, "pipe", "pipe"] }],
]) {
  const r = spawnSync(process.execPath, [echo], {
    input: "HELLO",
    encoding: "utf8",
    ...extra,
  });
  console.log(
    "spawnSync input",
    label,
    r.status,
    JSON.stringify(r.stdout),
    JSON.stringify(r.stderr),
  );
}

// 9. `input` together with an explicit 'ignore' in slot 0.
//
//    THE DOCS ARE WRONG HERE, and this probe pins the implementation. Node's
//    documented rule is "Supplying this value will override stdio[0]", but v22
//    does not implement that sentence: spawnSync attaches `input` to the slot-0
//    descriptor and leaves its TYPE alone, and the sync spawn only pumps input
//    into slots already typed 'pipe'. So an explicit 'ignore' (or 'inherit')
//    WINS and the payload is silently dropped -- the child observes an
//    immediately-EOF stdin and echoes nothing back.
//
//    oam originally followed the documentation and delivered the bytes anyway,
//    which reads like the more correct behavior and is exactly why it is pinned
//    here: this suite diffs against real node, so "more correct than node" is
//    still a divergence. The echoed payload below is the assertion that catches
//    a re-regression in either direction.
const conflict = spawnSync(process.execPath, [echo], {
  input: "HELLO",
  stdio: ["ignore", "pipe", "pipe"],
  encoding: "utf8",
});
console.log(
  "spawnSync input+ignore",
  conflict.status,
  conflict.error === undefined,
  conflict.output.length,
  conflict.output[0] === null,
  typeof conflict.stdout,
  typeof conflict.stderr,
);
// The byte-level half: 'ignore' beat `input`, so the child read nothing.
// Contrast with probe 8, where the same `input` IS delivered to a pipe-shaped
// slot 0 -- together they pin which side wins.
console.log("spawnSync input+ignore echoed", JSON.stringify(conflict.stdout));

rmSync(dir, { recursive: true, force: true });
console.log("exit 0");
