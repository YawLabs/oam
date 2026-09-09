import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const STAGE = join(here, '..', 'stage.mjs');
const ASSETS = [
  'oam-x86_64-pc-windows-msvc.exe',
  'oam-aarch64-pc-windows-msvc.exe',
  'oam-x86_64-apple-darwin',
  'oam-aarch64-apple-darwin',
  'oam-x86_64-unknown-linux-gnu',
];

// A release directory as scripts/release-local.sh assembles it: the five
// binaries plus the attribution files that travel with every copy.
function releaseDir() {
  const d = mkdtempSync(join(tmpdir(), 'oam-stage-'));
  for (const a of ASSETS) writeFileSync(join(d, a), 'binary');
  for (const f of ['LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.md']) {
    writeFileSync(join(d, f), 'attribution');
  }
  return d;
}

// stage.mjs writes into the repo's own npm/ tree, so every case cleans up the
// bin/ directories it created. Staging is idempotent, and a leftover binary
// would make a later case pass for the wrong reason.
function cleanStaged() {
  for (const pkg of ['oamjs-win32-x64', 'oamjs-win32-arm64', 'oamjs-darwin-x64',
    'oamjs-darwin-arm64', 'oamjs-linux-x64']) {
    rmSync(join(here, '..', pkg, 'bin'), { recursive: true, force: true });
  }
}

// Run stage.mjs and return {code, stderr}. It exits non-zero on refusal, which
// is the property most of these cases are about, so a throw is an expected
// outcome rather than a test failure.
function stage(args, env = {}) {
  try {
    execFileSync(process.execPath, [STAGE, ...args], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, ...env },
    });
    return { code: 0, stderr: '' };
  } catch (e) {
    return { code: e.status ?? 1, stderr: String(e.stderr ?? '') };
  }
}

test('a full stage from a complete release directory succeeds', () => {
  const d = releaseDir();
  try {
    // On Windows the POSIX targets are refused by design (see the next case),
    // so the all-platforms path is only assertable where chmod works.
    const r = stage([d], process.platform === 'win32'
      ? { OAM_NPM_ALLOW_UNEXECUTABLE: '1' }
      : {});
    assert.equal(r.code, 0, r.stderr);
    assert.ok(existsSync(join(here, '..', 'oamjs-linux-x64', 'bin', 'oam')));
    assert.ok(existsSync(join(here, '..', 'oamjs-win32-x64', 'bin', 'oam.exe')));
  } finally {
    cleanStaged();
    rmSync(d, { recursive: true, force: true });
  }
});

// The one that would have shipped three broken packages. chmod cannot set the
// execute bit on Windows -- libuv's uv_fs_chmod only toggles the read-only
// attribute -- so the packed tarball carries mode 0644 and every user of that
// platform gets a permission denied. The release box IS Windows, and npm does
// not allow re-publishing a version, so the only fix after the fact is a new
// release.
test('staging a POSIX binary from Windows is refused, not silently mis-moded', (t) => {
  if (process.platform !== 'win32') {
    t.skip('the guard is Windows-specific; chmod works here');
    return;
  }
  const d = releaseDir();
  try {
    const r = stage([d, '--only', 'oamjs-linux-x64']);
    assert.notEqual(r.code, 0);
    assert.match(r.stderr, /cannot stage oamjs-linux-x64 from Windows/);
    assert.match(r.stderr, /permission denied/);
  } finally {
    cleanStaged();
    rmSync(d, { recursive: true, force: true });
  }
});

// The .exe targets are unaffected: Windows has no execute bit and the suffix is
// what makes them runnable, which is why the guard is per-target rather than a
// blanket refusal to stage on Windows at all.
test('the Windows targets still stage from Windows', (t) => {
  if (process.platform !== 'win32') {
    t.skip('nothing to distinguish off Windows');
    return;
  }
  const d = releaseDir();
  try {
    const r = stage([d, '--only', 'oamjs-win32-arm64']);
    // Exits non-zero only because ONE platform was staged (the partial guard
    // below), never because the target itself was refused.
    assert.doesNotMatch(r.stderr, /cannot stage oamjs-win32-arm64 from Windows/);
    assert.ok(existsSync(join(here, '..', 'oamjs-win32-arm64', 'bin', 'oam.exe')));
  } finally {
    cleanStaged();
    rmSync(d, { recursive: true, force: true });
  }
});

// A warning here was not a gate: a release script under `set -e` sees exit 0
// and proceeds to publish, and the outcome is a platform package on the
// registry with no binary in it.
test('a partial stage fails rather than warning', () => {
  const d = releaseDir();
  try {
    const r = stage([d, '--only', 'oamjs-win32-arm64'],
      { OAM_NPM_ALLOW_UNEXECUTABLE: '1' });
    assert.notEqual(r.code, 0, 'a partially staged tree must not report success');
    assert.match(r.stderr, /PARTIALLY staged and must NOT be published/);
  } finally {
    cleanStaged();
    rmSync(d, { recursive: true, force: true });
  }
});

// `args.find(a => !a.startsWith('--'))` took the first non-flag argument as the
// release dir, so --only's VALUE was swallowed and the error named a directory
// the operator never passed.
test('--only before the directory does not consume the directory', () => {
  const d = releaseDir();
  try {
    const r = stage(['--only', 'oamjs-win32-arm64', d],
      { OAM_NPM_ALLOW_UNEXECUTABLE: '1' });
    assert.doesNotMatch(r.stderr, /no such release dir: oamjs-win32-arm64/);
  } finally {
    cleanStaged();
    rmSync(d, { recursive: true, force: true });
  }
});

test('a valueless --only is refused rather than staging nothing', () => {
  const d = releaseDir();
  try {
    const r = stage([d, '--only']);
    assert.notEqual(r.code, 0);
    assert.match(r.stderr, /--only needs a package name/);
  } finally {
    rmSync(d, { recursive: true, force: true });
  }
});

// A typo used to match no target, stage zero files, and exit 0.
test('an unknown --only package is refused and lists the real ones', () => {
  const d = releaseDir();
  try {
    const r = stage([d, '--only', 'oamjs-linux-arm64']);
    assert.notEqual(r.code, 0);
    assert.match(r.stderr, /not a platform package/);
    assert.match(r.stderr, /oamjs-linux-x64/);
  } finally {
    rmSync(d, { recursive: true, force: true });
  }
});

test('a release directory missing an attribution file is refused', () => {
  // Each binary is a redistribution of V8, ICU, the Node streams port and ~380
  // Rust crates, whose licenses require the notices travel with the copy. An
  // npm install is a copy.
  const d = releaseDir();
  rmSync(join(d, 'NOTICE'));
  try {
    const r = stage([d], { OAM_NPM_ALLOW_UNEXECUTABLE: '1' });
    assert.notEqual(r.code, 0);
    assert.match(r.stderr, /NOTICE/);
  } finally {
    cleanStaged();
    rmSync(d, { recursive: true, force: true });
  }
});

test('a non-existent release directory is named in the error', () => {
  const r = stage([join(tmpdir(), 'oam-stage-does-not-exist')]);
  assert.notEqual(r.code, 0);
  assert.match(r.stderr, /no such release dir/);
});

test('a release directory missing a binary is refused', () => {
  const d = releaseDir();
  rmSync(join(d, 'oam-x86_64-unknown-linux-gnu'));
  try {
    const r = stage([d], { OAM_NPM_ALLOW_UNEXECUTABLE: '1' });
    assert.notEqual(r.code, 0);
    assert.match(r.stderr, /oam-x86_64-unknown-linux-gnu/);
  } finally {
    cleanStaged();
    rmSync(d, { recursive: true, force: true });
  }
});
