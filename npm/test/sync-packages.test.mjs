import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, cpSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

import { workspaceVersion, sync } from '../sync-packages.mjs';

const REPO = fileURLToPath(new URL('../..', import.meta.url));

test('the committed manifests match Cargo.toml', async () => {
  // The gate itself, run against the real tree: this is what fails in CI when
  // release-local.sh bumps the workspace version and nobody re-ran the sync.
  const { version, drift } = await sync(REPO, { check: true });
  assert.deepEqual(drift, [], `npm/ manifests are stale for ${version}`);
});

test('the launcher pins the workspace version exactly', async () => {
  const version = workspaceVersion(readFileSync(join(REPO, 'Cargo.toml'), 'utf8'));
  const launcher = JSON.parse(readFileSync(join(REPO, 'npm/oamjs/package.json'), 'utf8'));
  assert.equal(launcher.version, version);
  for (const [pkg, range] of Object.entries(launcher.optionalDependencies)) {
    // A caret here would let npm satisfy this launcher with a later binary.
    assert.equal(range, version, `${pkg} is not pinned to ${version}`);
  }
});

test('the version is read from [workspace.package], not from whatever table comes first', () => {
  const toml = [
    '[workspace]',
    'members = ["crates/oam_cli"]',
    '',
    '[workspace.package]',
    'version = "1.2.3"',
    '',
    '[workspace.dependencies]',
    'v8 = "150"',
    '',
    '[package]',
    'version = "9.9.9"',
  ].join('\n');
  assert.equal(workspaceVersion(toml), '1.2.3');
});

test('a bumped workspace version is drift until the manifests are regenerated', async (t) => {
  // Proving the gate has teeth needs a real perturbed tree, not a claim: the
  // failure being prevented is exactly "Cargo.toml moved and npm/ did not".
  const root = mkdtempSync(join(tmpdir(), 'oamnpm-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));

  mkdirSync(join(root, 'npm'), { recursive: true });
  cpSync(join(REPO, 'npm/oamjs'), join(root, 'npm/oamjs'), { recursive: true });
  for (const pkg of ['oamjs-win32-x64', 'oamjs-win32-arm64', 'oamjs-darwin-x64',
    'oamjs-darwin-arm64', 'oamjs-linux-x64']) {
    cpSync(join(REPO, 'npm', pkg), join(root, 'npm', pkg), { recursive: true });
  }
  const cargo = readFileSync(join(REPO, 'Cargo.toml'), 'utf8');
  const bumped = workspaceVersion(cargo)
    .replace(/^(\d+)\.(\d+)\.(\d+)/, (_, a, b, c) => `${a}.${b}.${Number(c) + 1}`);
  writeFileSync(
    join(root, 'Cargo.toml'),
    cargo.replace(`version = "${workspaceVersion(cargo)}"`, `version = "${bumped}"`),
  );

  const stale = await sync(root, { check: true });
  assert.equal(stale.version, bumped);
  assert.equal(stale.drift.length, 6, 'all six manifests should report drift');

  await sync(root, { check: false });
  const fixed = await sync(root, { check: true });
  assert.deepEqual(fixed.drift, []);
  const launcher = JSON.parse(readFileSync(join(root, 'npm/oamjs/package.json'), 'utf8'));
  assert.equal(launcher.version, bumped);
  assert.equal(launcher.optionalDependencies['oamjs-linux-x64'], bumped);
});
