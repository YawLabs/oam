// Review-followup fixes (the round after 66e3a94): timer.refresh() re-arms
// after a one-shot FIRES but is a no-op after an explicit clearTimeout;
// write(null) throws ERR_STREAM_NULL_VALUES in object mode too (not just byte
// mode); the default byte-mode highWaterMark equals getDefaultHighWaterMark
// (asserted as a RELATION, not an absolute: Node raised the byte default from
// 16384 to 65536 in v22.23, so the absolute value drifts with the runner's
// node patch version and would violate the corpus determinism rule).
// emitWarning's stderr default-handler is guarded by the vendored
// test-process-warning node-suite case (its output carries a varying pid, so it
// is not byte-comparable here).
import { Writable, Readable, getDefaultHighWaterMark } from "node:stream";

// timer.refresh(): a self-rescheduling one-shot fires repeatedly (Node keeps
// _destroyed false during the callback so an in-callback refresh() re-arms).
await new Promise((resolve) => {
  let n = 0;
  const t = setTimeout(function poll() {
    n++;
    if (n < 3) t.refresh();
    else {
      console.log("self-reschedule=" + n);
      resolve();
    }
  }, 2);
});

// refresh() after an explicit clearTimeout is terminal -- a no-op.
await new Promise((resolve) => {
  let fired = 0;
  const t = setTimeout(() => {
    fired++;
  }, 2);
  clearTimeout(t);
  t.refresh();
  setTimeout(() => {
    console.log("refresh-after-clear=" + fired);
    resolve();
  }, 25);
});

// write(null) throws ERR_STREAM_NULL_VALUES in BOTH byte and object mode.
const wbyte = new Writable({ write(_c, _e, cb) { cb(); } });
try {
  wbyte.write(null);
  console.log("byte-null=nothrow");
} catch (e) {
  console.log("byte-null=" + e.code);
}
const wobj = new Writable({ objectMode: true, write(_c, _e, cb) { cb(); } });
try {
  wobj.write(null);
  console.log("obj-null=nothrow");
} catch (e) {
  console.log("obj-null=" + e.code);
}

// default byte-mode highWaterMark, now observable via the exposed state --
// asserted against getDefaultHighWaterMark so the case is byte-identical on
// every node version regardless of what the default happens to be.
const defaultHwm = getDefaultHighWaterMark(false);
console.log(
  "w.hwm-is-default=" +
    (new Writable({ write() {} })._writableState.highWaterMark === defaultHwm),
);
console.log(
  "r.hwm-is-default=" +
    (new Readable({ read() {} })._readableState.highWaterMark === defaultHwm),
);
