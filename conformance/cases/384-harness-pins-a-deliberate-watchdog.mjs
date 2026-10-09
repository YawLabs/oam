// The differential harness scores a case both runtimes cut short with their
// own watchdog (#211): it is `watchdog`, not `pass` -- the rest of the case
// was never run -- and never `fail` on whichever side got further. Every
// other watchdog case arms one to catch a hang, on top of a workload whose
// handles keep the loop alive; this one fires it ON PURPOSE, on both
// runtimes, so the scorecard has a case whose `watchdog` entry is a real
// receipt of the scoring, not a slow host's accident.
//
// The timer is ref'd on purpose: there is no other handle, and an unref'd
// one would let the loop drain and the process exit 0 before it fired (what
// cases 104-106 pin). The output before the cut is one deterministic line,
// and 1 s keeps the harness's 90 s ceiling out of the way. What the harness
// must NOT do is read `WATCHDOG` as just another stdout line: if this case
// ever scores `pass`, the verifier ran without the fix, or with `verdict()`
// broken.
console.log("before the cut");
setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 1000);