import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  mkdtempSync, mkdirSync, cpSync, readFileSync, writeFileSync, rmSync,
  symlinkSync, unlinkSync, realpathSync,
} from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { workspaceVersion, sync, isMainModule } from '../sync-packages.mjs';

const REPO = fileURLToPath(new URL('../..', import.meta.url));

// The manifests and the launcher (whose lib/targets.js the sync loads) copied
// under a scratch root, so a case can perturb or relocate a real tree without
// touching the checkout. Only the two generated files per platform package are
// taken: a tree staged for publish also holds a ~60 MB binary in each.
function copyNpmTree(root) {
  mkdirSync(join(root, 'npm'), { recursive: true });
  cpSync(join(REPO, 'npm/oamjs'), join(root, 'npm/oamjs'), { recursive: true });
  for (const pkg of ['oamjs-win32-x64', 'oamjs-win32-arm64', 'oamjs-darwin-x64',
    'oamjs-darwin-arm64', 'oamjs-linux-x64']) {
    mkdirSync(join(root, 'npm', pkg), { recursive: true });
    for (const f of ['package.json', 'README.md']) {
      cpSync(join(REPO, 'npm', pkg, f), join(root, 'npm', pkg, f));
    }
  }
}

test('the script runs its body when it is reached through a linked directory', (t) => {
  // Node realpath's the entry script for import.meta.url but not for
  // process.argv[1]. A main-module guard comparing the two unresolved skipped
  // the whole body through a junction or symlink: no output, exit 0 -- which
  // both gates read as "manifests match". So the positive line is what is
  // asserted here, not merely the exit code a skipped body also produces.
  const root = mkdtempSync(join(tmpdir(), 'oamnpm-link-'));
  const link = join(root, 'linked');
  t.after(() => {
    // The link first and by unlink, so nothing ever recurses through it.
    try { unlinkSync(link); } catch { /* never created */ }
    rmSync(root, { recursive: true, force: true });
  });

  copyNpmTree(join(root, 'real'));
  cpSync(join(REPO, 'Cargo.toml'), join(root, 'real', 'Cargo.toml'));
  cpSync(join(REPO, 'npm/sync-packages.mjs'), join(root, 'real', 'npm', 'sync-packages.mjs'));
  try {
    // 'junction' needs no privilege on Windows and is ignored elsewhere, where
    // this is an ordinary directory symlink.
    symlinkSync(join(root, 'real', 'npm'), link, 'junction');
  } catch (e) {
    t.skip(`cannot create a directory link here (${e.code})`);
    return;
  }

  const viaLink = join(link, 'sync-packages.mjs');
  const r = spawnSync(process.execPath, [viaLink, '--check'], { encoding: 'utf8' });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /npm\/ manifests match Cargo\.toml \(/);

  // The predicate on its own: a linked spelling of this very module is main,
  // an unrelated path is not, and an absent argv[1] (node -e, the REPL) is not.
  const real = join(root, 'real', 'npm', 'sync-packages.mjs');
  const realUrl = pathToFileURL(realpathSync(real)).href;
  assert.equal(isMainModule(realUrl, viaLink), true);
  assert.equal(isMainModule(realUrl, join(REPO, 'Cargo.toml')), false);
  assert.equal(isMainModule(realUrl, undefined), false);
});

test('--root with no directory is refused by name', () => {
  const r = spawnSync(
    process.execPath,
    [join(REPO, 'npm/sync-packages.mjs'), '--check', '--root'],
    { encoding: 'utf8' },
  );
  assert.equal(r.status, 1);
  assert.match(r.stderr, /--root needs a directory/);
});

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

  copyNpmTree(root);
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
