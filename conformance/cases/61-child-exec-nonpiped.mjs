// exec()/execSync() when stdio is NOT piped. Both APIs are built around owning
// the child's pipes -- exec wires data collectors onto child.stdout/child.stderr,
// execSync interpolates the captured stderr into its failure message -- but a
// caller may pass stdio:'inherit'/'ignore' straight through, and then those
// slots are null. Every assertion below is on the CALLBACK/THROW contract, which
// is what ecosystem code branches on.
//
// Regression guard, two null-dereferences that each failed worse than the thing
// they were meant to report:
//   - exec attached its collectors unguarded (cp.stdout.on('data', ...)), so
//     exec(cmd, {stdio:'inherit'}, cb) threw "Cannot read properties of null"
//     from inside the spawn handler. Thrown out of an event callback, no caller
//     try/catch could catch it, and the callback never fired -- so the child's
//     exit status was lost entirely.
//   - execSync built `Command failed: ${cmd}\n${result.stderr}` with no null
//     coalesce, so the same non-piped call produced the literal text
//     "Command failed: ...\nnull". The operator got the word "null" where the
//     diagnostic should be, while the child's real error had already gone to
//     the inherited terminal.
//
// Ordering hazard (case 56 covers the general form): an inherited child writes
// DIRECTLY into this process's stdout, interleaving with the compared output.
// So every child used in an assertion here is SILENT, and the one child that
// does print is read back through a real pipe -- which doubles as the control
// proving these paths are specific to the non-piped case.
//
// Third guard, and the subtle one: async exec must IGNORE a caller's `stdio`.
// node's exec/execFile hand spawn an explicit option whitelist (cwd, env, gid,
// shell, signal, uid, windowsHide, windowsVerbatimArguments) with `stdio`
// deliberately absent -- exec owns the pipes, because collecting output for the
// callback is its entire contract. Forwarding `stdio` through to spawn returns a
// ChildProcess whose stdout/stderr are null where node's are real streams. Note
// this is asymmetric on purpose: execSync/spawnSync DO honor stdio, since
// `execSync(cmd, {stdio:'inherit'})` is the normal way to stream a build's
// output to the terminal. Both halves are asserted below.
import { exec, execSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

// pid-scoped: the runner executes the node and oam legs back to back, and a
// shared fixed path would let one leg's cleanup delete the other's fixtures.
const dir = path.join(os.tmpdir(), `oam-conf-execnp-${process.pid}`);
mkdirSync(dir, { recursive: true });

// Silent on BOTH streams. A single byte written here would land in the compared
// stdout under 'inherit' and desync the two legs.
const fail = path.join(dir, "fail.cjs");
writeFileSync(fail, "process.exit(7);");

const ok = path.join(dir, "ok.cjs");
writeFileSync(ok, "process.exit(0);");

// The only child that speaks, and only ever through a captured pipe.
const talker = path.join(dir, "talker.cjs");
writeFileSync(talker, "process.stdout.write('PIPED_OUT\\n');");

// Quote the exe: on Windows it lives under a path with spaces.
const cmdFor = (script) => `"${process.execPath}" ${JSON.stringify(script)}`;

const runExec = (script, stdio) =>
  new Promise((done) => {
    exec(cmdFor(script), { stdio }, (err, stdout, stderr) => {
      done({
        code: err === null ? null : err.code,
        out: stdout,
        err: stderr,
      });
    });
  });

// 1. stdio:'inherit'. exec must survive wiring collectors onto null slots, and
//    the callback must still report the child's exit status -- the whole point
//    of the guard is that a non-piped exec degrades to "no output captured"
//    rather than to "no callback at all".
const inheritFail = await runExec(fail, "inherit");
console.log("inherit fail code", inheritFail.code);
console.log(
  "inherit fail empty",
  inheritFail.out === "",
  inheritFail.err === "",
);

const inheritOk = await runExec(ok, "inherit");
console.log("inherit ok code", inheritOk.code);

// 2. stdio:'ignore'. Same null slots by a different route; the callback's
//    stdout/stderr arguments are the empty string, never null or undefined.
const ignoreFail = await runExec(fail, "ignore");
console.log("ignore fail code", ignoreFail.code);
console.log("ignore fail empty", ignoreFail.out === "", ignoreFail.err === "");
console.log(
  "ignore fail types",
  typeof ignoreFail.out,
  typeof ignoreFail.err,
);

const ignoreOk = await runExec(ok, "ignore");
console.log("ignore ok code", ignoreOk.code);

// 2b. exec IGNORES the stdio it was handed: the returned ChildProcess still has
//     real, collectable pipes. This is the assertion that catches `stdio` being
//     forwarded to spawn -- which nulls these slots and, before the guards
//     above existed, took the callback down with them.
const shaped = exec(cmdFor(ok), { stdio: "inherit" }, () => {});
console.log(
  "exec keeps pipes",
  shaped.stdout !== null,
  shaped.stderr !== null,
  shaped.stdin !== null,
);
await new Promise((r) => shaped.on("close", r));

// 3. execSync + non-piped + non-zero exit: the throw path that used to stringify
//    a null stderr. The message embeds the command, which holds each runtime's
//    own execPath, so it cannot be compared verbatim across legs -- what IS
//    comparable, and what actually regressed, is that the word "null" never
//    appears in it.
let threw = false;
let status = null;
let hasNull = null;
let stdoutNull = null;
try {
  execSync(cmdFor(fail), { stdio: "inherit" });
} catch (err) {
  threw = true;
  status = err.status;
  hasNull = String(err.message).includes("null");
  // node assigns the spawnSync result onto the error, so the non-piped slot
  // reads back as null here too rather than as an empty buffer.
  stdoutNull = err.stdout === null;
}
console.log("execSync inherit threw", threw);
console.log("execSync inherit status", status);
console.log("execSync inherit message has null", hasNull);
console.log("execSync inherit err.stdout null", stdoutNull);

// A non-piped execSync that SUCCEEDS returns null, not an empty buffer: there
// was no pipe to read, and callers that test the return value for content must
// see the difference.
const syncInheritOk = execSync(cmdFor(ok), { stdio: "inherit" });
console.log("execSync inherit ok returns null", syncInheritOk === null);

// 4. Control: default (piped) stdio still hands back the child's real output as
//    a Buffer. Without this the three cases above would also pass if exec and
//    execSync had simply stopped capturing anything at all.
const piped = execSync(cmdFor(talker));
console.log("execSync piped isBuffer", Buffer.isBuffer(piped));
console.log("execSync piped text", JSON.stringify(piped.toString().trim()));

rmSync(dir, { recursive: true, force: true });
console.log("exit 0");
