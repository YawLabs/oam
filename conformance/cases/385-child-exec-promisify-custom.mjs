// util.promisify(exec) and util.promisify(execFile), as node v22's
// lib/child_process.js pins them through util.promisify.custom:
//
//   * the promise RESOLVES { stdout, stderr } -- both callback values, not
//     the first one alone;
//   * it REJECTS with the callback's error, with err.stdout and err.stderr
//     stamped on it (in that key order, after code/killed/signal/cmd);
//   * it carries the spawned ChildProcess as `promise.child`;
//   * `encoding` reaches both outcomes: 'buffer' keeps Buffers.
//
// oam had no promisify.custom on either, so the generic promisify resolved
// the stdout STRING (`const { stdout } = await ...` gave undefined, and the
// usual `.trim()` threw) and rejected an error with no output on it. (#302)
import { exec, execFile, ChildProcess } from "node:child_process";
import util, { promisify } from "node:util";

const NODE = process.execPath;
const OK = "process.stdout.write('out'); process.stderr.write('err')";
const FAIL = "process.stdout.write('o'); process.stderr.write('e'); process.exit(3)";

// The custom form itself: a function, and what util.promisify hands back.
for (const [label, fn] of [["exec", exec], ["execFile", execFile]]) {
  const custom = fn[util.promisify.custom];
  const desc = Object.getOwnPropertyDescriptor(fn, promisify.custom);
  console.log(
    label,
    typeof custom === "function",
    promisify(fn) === custom,
    custom.name,
    custom.length,
    desc.enumerable,
    desc.writable,
    desc.configurable,
    Object.keys(fn).length,
    Object.getOwnPropertySymbols(fn).length,
  );
}
console.log(typeof execFile[util.promisify.custom] === "function");

const pExecFile = promisify(execFile);
const pExec = promisify(exec);

function show(label, value) {
  console.log(label, JSON.stringify(Object.keys(value)), JSON.stringify(value.stdout), JSON.stringify(value.stderr));
}

// execFile: resolve shape, and the promise's .child.
{
  const p = pExecFile(NODE, ["-e", OK]);
  console.log("execFile child", p.child instanceof ChildProcess, typeof p.child.pid);
  show("execFile resolve", await p);
}

// execFile: reject shape.
try {
  await pExecFile(NODE, ["-e", FAIL]);
  console.log("execFile reject: resolved");
} catch (err) {
  show("execFile reject", err);
  console.log("execFile reject code", err.code, err.killed, err.signal);
}

// execFile with options and encoding: 'buffer' -- Buffers on both outcomes.
{
  const r = await pExecFile(NODE, ["-e", OK], { encoding: "buffer" });
  console.log("execFile buffer resolve", Buffer.isBuffer(r.stdout), Buffer.isBuffer(r.stderr), r.stdout.toString(), r.stderr.toString());
}
try {
  await pExecFile(NODE, ["-e", FAIL], { encoding: "buffer" });
  console.log("execFile buffer reject: resolved");
} catch (err) {
  console.log("execFile buffer reject", Buffer.isBuffer(err.stdout), Buffer.isBuffer(err.stderr), err.stdout.toString(), err.stderr.toString());
}

// A binary that never starts still rejects with the output fields present.
try {
  await pExecFile("definitely-not-a-real-binary-xyz", []);
  console.log("execFile enoent: resolved");
} catch (err) {
  console.log("execFile enoent", err.code, JSON.stringify(err.stdout), JSON.stringify(err.stderr));
}

// exec: the same contract through a shell. The script has no quotes of its
// own so the command line is the same under cmd.exe and sh.
const cmd = (code) => `"${NODE}" -e "${code}"`;
{
  const p = pExec(cmd("process.stdout.write(String(6*7)); process.stderr.write(String(1+1))"));
  console.log("exec child", p.child instanceof ChildProcess, typeof p.child.pid);
  show("exec resolve", await p);
}
try {
  await pExec(cmd("process.stdout.write(String(4)); process.stderr.write(String(5)); process.exit(3)"));
  console.log("exec reject: resolved");
} catch (err) {
  show("exec reject", err);
  console.log("exec reject code", err.code, err.killed, err.signal);
}
{
  const r = await pExec(cmd("process.stdout.write(String(7))"), { encoding: "buffer" });
  console.log("exec buffer resolve", Buffer.isBuffer(r.stdout), Buffer.isBuffer(r.stderr), r.stdout.toString(), r.stderr.length);
}
