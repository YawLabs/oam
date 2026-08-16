// kill() signal validation -- the convertToValidSignal half of the contract.
// Validation happens BEFORE any signal is delivered, so this runs on every
// platform; only the delivered-kill shape at the end depends on the host, and
// both runtimes are asked on the same host.
//
// Pinned here, none of which had coverage:
//   1. An unknown NAME throws ERR_UNKNOWN_SIGNAL (a TypeError, node's exact
//      message), leaves `killed` false, and the child runs on.
//   2. An unknown NUMBER does the same. (oam regression: numbers were passed
//      through to the native layer, which stringified them into names it did
//      not recognize -- so kill(9) delivered SIGTERM and kill(987654) threw
//      nothing.)
//   3. Names are case-insensitive ('sigterm' is accepted, node uppercases
//      before lookup) and the accepted kill reports node's close shape.
//   4. Validation runs before any handle-state branching: a kill('SIGWRONG')
//      issued the instant an ipc-bound spawn returns -- while oam's native
//      handle can still be null (case 65's window) -- must THROW, not park in
//      the pending-kill slot.
//
// Deliberately NOT asserted: kill()'s return value on an already-dead child
// (node returns false via ESRCH; oam returns true -- a known divergence, out
// of scope here).
import { spawn } from "node:child_process";

const sleepy = () => spawn(process.execPath, ["-e", "setTimeout(()=>{}, 8000)"]);

// 1. Unknown name.
const child = sleepy();
try {
  child.kill("SIGWRONG");
  console.log("name no-throw");
} catch (e) {
  console.log(
    "name",
    e.code,
    e instanceof TypeError,
    JSON.stringify(e.message),
    "killed",
    child.killed,
  );
}

// 2. Unknown number.
try {
  child.kill(987654);
  console.log("number no-throw");
} catch (e) {
  console.log("number", e.code, JSON.stringify(e.message), "killed", child.killed);
}

// 3. Case-insensitive name, then the real kill shape.
const shape = await new Promise((done) => {
  child.on("close", (code, signal) => done(`close ${code} ${signal}`));
  let accepted;
  try {
    child.kill("sigterm");
    accepted = true;
  } catch {
    accepted = false;
  }
  console.log("lowercase accepted", accepted, "killed", child.killed);
  setTimeout(() => done("close TIMEOUT"), 8000);
});
console.log(shape);

// 4. Pre-handle validation on an ipc-bound child.
const ipc = spawn(process.execPath, ["-e", "setTimeout(()=>{}, 8000)"], {
  stdio: ["ignore", "ignore", "ignore", "ipc"],
});
let preHandle;
try {
  ipc.kill("SIGWRONG");
  preHandle = "no-throw";
} catch (e) {
  preHandle = e.code;
}
console.log("pre-handle", preHandle);
// Clean up with a VALID kill -- which may itself land in the pending-kill
// window; 'close' firing is the proof it was delivered, not dropped.
await new Promise((done) => {
  ipc.on("close", () => done());
  ipc.kill();
  setTimeout(done, 8000);
});
console.log("ipc cleaned up");
