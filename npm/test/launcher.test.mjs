import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const LAUNCHER = fileURLToPath(new URL('../oamjs/bin/oamjs.js', import.meta.url));
const CHILD = fileURLToPath(new URL('./fixtures/child.mjs', import.meta.url));

// The launcher exec's whatever OAMJS_BINARY names, so pointing it at the
// running Node and passing the fixture as the first argument exercises the real
// spawn, the real inherited descriptors and the real exit status -- without
// needing a published per-platform package on the box running the tests.
function runLauncher(args, { input, env } = {}) {
  const child = spawn(process.execPath, [LAUNCHER, CHILD, ...args], {
    env: { ...process.env, OAMJS_BINARY: process.execPath, ...env },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const out = [];
  const err = [];
  child.stdout.on('data', (c) => out.push(c));
  child.stderr.on('data', (c) => err.push(c));
  child.stdin.end(input === undefined ? '' : input);
  return new Promise((resolve) => {
    child.on('close', (code, signal) => {
      resolve({ code, signal, stdout: Buffer.concat(out), stderr: Buffer.concat(err) });
    });
  });
}

test('the child exit code is the launcher exit code', async () => {
  for (const code of [0, 1, 7, 42]) {
    const r = await runLauncher(['exit', String(code)]);
    assert.equal(r.code, code, `expected exit ${code}, got ${r.code}`);
  }
});

test('argv survives the round trip, quoting and all', async () => {
  // Every entry here is something a naive wrapper loses: a shell would eat the
  // quotes and the backslash, a join/split round trip would split on the space,
  // and an empty argument disappears entirely from a re-quoted command line.
  const args = ['--flag', 'a b', 'has "quotes"', 'back\\slash', '', '-pe', '2+2', '--'];
  const r = await runLauncher(['argv', ...args]);
  assert.equal(r.code, 0);
  assert.deepEqual(JSON.parse(r.stdout.toString()), args);
});

test('stdio is byte-exact in both directions -- the MCP stdio case', async () => {
  // 256 KiB, well past one pipe buffer, so a wrapper that relayed through a
  // stream would have to have got its backpressure right too. The payload
  // covers every byte value, which includes the ones a text-mode or
  // line-oriented relay mangles: NUL (U+0000, a C-string terminator), SUB
  // (U+001A, which ends a Windows console read in text mode), and bare CR and
  // LF, which such a relay normalises.
  const payload = Buffer.alloc(256 * 1024);
  for (let i = 0; i < payload.length; i += 1) payload[i] = i % 256;
  payload.write('\r\n {"jsonrpc":"2.0"}\r\n', 0, 'binary');

  const r = await runLauncher(['echo'], { input: payload });
  assert.equal(r.code, 0);
  assert.equal(r.stdout.length, payload.length);
  assert.ok(r.stdout.equals(payload), 'stdout differs from stdin');
  // Separate descriptors, still: an MCP client parsing stdout must not see the
  // server's diagnostics interleaved into its framing.
  assert.equal(r.stderr.toString(), 'child-stderr-marker');
});

test('a missing OAMJS_BINARY is reported, not exec\'d', async () => {
  const r = await runLauncher(['exit', '0'], { env: { OAMJS_BINARY: '/no/such/oam' } });
  assert.equal(r.code, 1);
  assert.match(r.stderr.toString(), /^oamjs: error: OAMJS_BINARY is set to /);
});

test('linux arm64 gets install.sh\'s answer, not a module-resolution stack', async () => {
  // The whole point of the honest-error path, driven through the real bin
  // script on a box that is not Linux ARM: redefine the two properties the
  // resolver reads, then require the launcher exactly as the shim does.
  const preload = [
    "Object.defineProperty(process,'platform',{value:'linux'});",
    "Object.defineProperty(process,'arch',{value:'arm64'});",
    `require(${JSON.stringify(LAUNCHER.replace(/\\/g, '/'))});`,
  ].join('');
  const env = { ...process.env };
  delete env.OAMJS_BINARY;

  const r = await new Promise((resolve) => {
    const child = spawn(process.execPath, ['-e', preload], {
      env,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    const out = [];
    const err = [];
    child.stdout.on('data', (c) => out.push(c));
    child.stderr.on('data', (c) => err.push(c));
    child.on('close', (code) => resolve({
      code,
      stdout: Buffer.concat(out).toString(),
      stderr: Buffer.concat(err).toString(),
    }));
  });

  assert.equal(r.code, 1);
  assert.equal(
    r.stderr.trim(),
    'oamjs: error: no published oam binary for Linux arm64 yet '
    + '(aarch64-unknown-linux-gnu is unreleased; use an x86_64 host or build from source)',
  );
  assert.equal(r.stdout, '');
});

test('posix: SIGTERM to the launcher stops the child and the launcher dies of it', async (t) => {
  if (process.platform === 'win32') {
    // Loud rather than silent: this is the case the Windows branch cannot
    // implement at all (TerminateProcess is not interceptable), so a green run
    // on this box has NOT covered it.
    t.skip('no POSIX signals on win32 -- signals.test.mjs covers the branch with a stub process');
    return;
  }

  const child = spawn(process.execPath, [LAUNCHER, CHILD, 'signal'], {
    env: { ...process.env, OAMJS_BINARY: process.execPath },
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  await new Promise((resolve) => child.stdout.once('data', resolve));

  // Directed at the launcher's PID only, which is what a supervisor does. A
  // process-group kill would reach the fixture directly and prove nothing.
  child.kill('SIGTERM');
  const { code, signal } = await new Promise((resolve) => {
    child.on('close', (c, s) => resolve({ code: c, signal: s }));
  });

  assert.equal(signal, 'SIGTERM', 'the launcher did not die of the signal it was sent');
  assert.equal(code, null);
  // The fixture would have run for 30s; if the forward had not landed it would
  // still be running now, reparented to init.
  await delay(50);
});
