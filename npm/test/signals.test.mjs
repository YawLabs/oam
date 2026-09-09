import { test } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import os from 'node:os';

import signalsModule from '../oamjs/lib/signals.js';

const { forwardSignals, exitLike, POSIX_FORWARDED, WINDOWS_HELD } = signalsModule;

// A stub process, because the real one cannot be driven both ways from one box:
// the POSIX branch is the one that matters for an MCP host, and the Windows
// branch is the one this repo's dev box runs on. Injecting the platform is what
// makes both testable everywhere; the end-to-end SIGTERM case in
// launcher.test.mjs then proves the POSIX branch against the real kernel where
// there is one.
function stubProcess(platform) {
  const proc = new EventEmitter();
  proc.platform = platform;
  proc.pid = 4242;
  proc.killed = [];
  proc.kill = (pid, sig) => proc.killed.push([pid, sig]);
  return proc;
}

function stubChild() {
  const child = { killed: [] };
  child.kill = (sig) => child.killed.push(sig);
  return child;
}

test('posix: every forwarded signal reaches the child', () => {
  const proc = stubProcess('linux');
  const child = stubChild();
  forwardSignals(child, { process: proc, platform: 'linux' });

  for (const sig of POSIX_FORWARDED) proc.emit(sig);
  assert.deepEqual(child.killed, POSIX_FORWARDED);
});

test('posix: SIGTERM specifically is forwarded -- the MCP host stop path', () => {
  // Called out on its own because this is the signal a supervisor sends to the
  // PID it spawned, which is the launcher and not oam. If only this one
  // regressed, the suite above would still be green for five other signals
  // while every `mcp stop` left an orphaned runtime holding the host's pipes.
  const proc = stubProcess('darwin');
  const child = stubChild();
  forwardSignals(child, { process: proc, platform: 'darwin' });

  proc.emit('SIGTERM');
  assert.deepEqual(child.killed, ['SIGTERM']);
});

test('windows: console events are held, never re-sent to the child', () => {
  const proc = stubProcess('win32');
  const child = stubChild();
  forwardSignals(child, { process: proc, platform: 'win32' });

  for (const sig of WINDOWS_HELD) proc.emit(sig);
  // The console already delivered Ctrl+C to every process attached to it, and
  // child.kill() on Windows is TerminateProcess -- forwarding would convert a
  // graceful shutdown into a hard kill mid-write.
  assert.deepEqual(child.killed, []);
  // Held, though: without a listener the launcher dies on Ctrl+C and the shell
  // reports the launcher's status while oam is still flushing.
  assert.equal(proc.listenerCount('SIGINT'), 1);
});

test('the launcher stops holding signals once the child is gone', () => {
  const proc = stubProcess('linux');
  const stop = forwardSignals(stubChild(), { process: proc, platform: 'linux' });
  for (const sig of POSIX_FORWARDED) assert.equal(proc.listenerCount(sig), 1);

  stop();
  // Not hygiene: exitLike re-raises the child's signal on this process, and a
  // surviving listener would swallow it into a clean exit 0.
  for (const sig of POSIX_FORWARDED) assert.equal(proc.listenerCount(sig), 0);
});

test('exit status mirrors the child', () => {
  for (const code of [0, 1, 7, 42]) {
    const proc = stubProcess('linux');
    exitLike(code, null, { process: proc, platform: 'linux' });
    assert.equal(proc.exitCode, code);
  }
});

test('a child that never started does not report success', () => {
  const proc = stubProcess('linux');
  exitLike(null, null, { process: proc, platform: 'linux' });
  assert.equal(proc.exitCode, 1);
});

test('posix: a signalled child re-raises the same signal on the launcher', () => {
  const proc = stubProcess('linux');
  exitLike(null, 'SIGTERM', { process: proc, platform: 'linux' });
  // WIFSIGNALED() for the caller, not a synthesized exit code: `oamjs` under a
  // shell must die the way oam died.
  assert.deepEqual(proc.killed, [[4242, 'SIGTERM']]);
  // The stub does not actually die, so the 128+n fallback shows here. On a real
  // process the kill above has already ended it.
  assert.equal(proc.exitCode, 128 + os.constants.signals.SIGTERM);
});

test('windows: a signalled child is not re-raised', () => {
  const proc = stubProcess('win32');
  exitLike(null, 'SIGTERM', { process: proc, platform: 'win32' });
  // Windows has no signal to re-raise; process.kill(pid, 'SIGTERM') there is
  // TerminateProcess, which would replace the child's status with our own.
  assert.deepEqual(proc.killed, []);
  assert.equal(proc.exitCode, 1);
});
