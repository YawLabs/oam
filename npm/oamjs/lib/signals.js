'use strict';

const os = require('node:os');

// SIGKILL and SIGSTOP are absent because they cannot be caught: a SIGKILL to
// the launcher orphans the child, and no wrapper written in Node can prevent
// that. SIGBREAK exists only on Windows and is ignored by the POSIX branch,
// which is fine -- registering it there throws.
const POSIX_FORWARDED = ['SIGINT', 'SIGTERM', 'SIGHUP', 'SIGQUIT', 'SIGUSR1', 'SIGUSR2'];
const WINDOWS_HELD = ['SIGINT', 'SIGBREAK'];

// Keep the launcher's signal disposition honest for as long as the child runs.
//
// The reason this is not spawnSync + process.exit: an MCP host starts its
// servers and stops them by killing the PID it spawned, which is THIS process,
// not oam. spawnSync blocks the loop, so a JS handler cannot run during it, and
// the default SIGTERM disposition would kill the launcher and leave oam running
// with the host's stdio pipes still open -- a hung server the host thinks it
// stopped.
//
// Returns a function that undoes every listener it installed; the caller must
// run it before exiting, or the re-raise in exitLike() below hits our own
// handler instead of the default disposition.
function forwardSignals(child, opts = {}) {
  const proc = opts.process || process;
  const platform = opts.platform || proc.platform;

  const installed = [];
  const listen = (sig, fn) => {
    proc.on(sig, fn);
    installed.push([sig, fn]);
  };

  if (platform === 'win32') {
    // Windows has no signals: the console delivers Ctrl+C and Ctrl+Break to
    // EVERY process attached to it, so the child already has the event before
    // we could forward anything, and child.kill() here is TerminateProcess --
    // it would turn the child's graceful shutdown into a hard kill mid-write.
    // So hold the event and do nothing with it: without a listener the launcher
    // dies first and the shell reports the launcher's exit code while oam is
    // still flushing.
    for (const sig of WINDOWS_HELD) listen(sig, () => {});
  } else {
    for (const sig of POSIX_FORWARDED) {
      listen(sig, () => {
        // A terminal-generated signal already reached the child through the
        // foreground process group, so this copy is redundant there. It is not
        // redundant for a PID-directed kill from a supervisor, which is the
        // case that matters. Delivering a second signal to a process that has
        // already exited is a no-op -- Node swallows the ESRCH.
        child.kill(sig);
      });
    }
  }

  return () => {
    for (const [sig, fn] of installed) proc.removeListener(sig, fn);
  };
}

// Exit the way the child exited, so `$?` and WIFSIGNALED() say what they would
// have said had the caller run oam directly. A child killed by SIGTERM must not
// look like a clean `exit 0` to the shell that is waiting on us.
function exitLike(code, signal, opts = {}) {
  const proc = opts.process || process;
  const platform = opts.platform || proc.platform;

  if (signal && platform !== 'win32') {
    // The listeners are gone by now (stopForwarding ran), so the default
    // disposition applies and this actually kills us with the right status.
    proc.kill(proc.pid, signal);
    // Reached only if the signal was somehow blocked or ignored. 128+n is what
    // a POSIX shell reports for it, so say that rather than inventing a code.
    const n = os.constants.signals[signal];
    proc.exitCode = n ? 128 + n : 1;
    return proc.exitCode;
  }

  // A null code with no signal means the child never started, which the caller
  // reports separately; 1 is the safe floor rather than a fabricated success.
  proc.exitCode = code === null || code === undefined ? 1 : code;
  return proc.exitCode;
}

module.exports = { forwardSignals, exitLike, POSIX_FORWARDED, WINDOWS_HELD };
