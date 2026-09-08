#!/usr/bin/env node
// Copy a cut release's binaries and its attribution files into the npm package
// directories, so `npm pack` / `npm publish` has something to ship.
//
//   node npm/stage.mjs <release-dir>
//   node npm/stage.mjs <release-dir> --only oamjs-win32-arm64   (see below)
//
// <release-dir> is scripts/release-local.sh's $RELEASE_DIR: the temp directory
// holding oam-<triple>[.exe] for all five targets plus LICENSE, NOTICE and
// THIRD_PARTY_LICENSES.md. The asset names are the contract documented in
// install/README.md; this script reads exactly those names and nothing else.
//
// The binaries are NOT committed -- npm/.gitignore keeps them out -- because a
// 60 MB artifact per target per release is not what git is for, and because a
// binary in the tree would be a second source of truth about what shipped.
//
// --only stages a subset. It exists for local verification (pack the one
// platform this box can actually execute) and MUST NOT be used for a release:
// a partially staged publish puts empty per-platform packages on the registry,
// and npm cannot unpublish them after 72 hours.
//
// The attribution files travel with every binary for the same reason
// scripts/release-local.sh stages them beside the release assets: each oam
// binary is a binary redistribution of V8, ICU, the Node streams port and ~380
// Rust crates, and their licenses require the notices travel with the copy.
// An npm install is a copy.

import { copyFileSync, mkdirSync, chmodSync, existsSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const ATTRIBUTION = ['LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.md'];

const { SUPPORTED } = (await import(
  new URL('./oamjs/lib/targets.js', import.meta.url).href
)).default;

const args = process.argv.slice(2);
const releaseDir = args.find((a) => !a.startsWith('--'));
const only = args.reduce((acc, a, i) => (a === '--only' ? [...acc, args[i + 1]] : acc), []);

function die(message) {
  process.stderr.write(`oam-npm-stage: error: ${message}\n`);
  process.exit(1);
}

if (!releaseDir) die('usage: node npm/stage.mjs <release-dir> [--only <package>]');
if (!existsSync(releaseDir)) die(`no such release dir: ${releaseDir}`);

for (const f of ATTRIBUTION) {
  if (!existsSync(join(releaseDir, f))) {
    die(`${f} missing from ${releaseDir} -- release-local.sh stages it beside the binaries`);
  }
}

let staged = 0;
for (const target of Object.values(SUPPORTED)) {
  if (only.length > 0 && !only.includes(target.pkg)) continue;

  const asset = `oam-${target.triple}${target.bin.endsWith('.exe') ? '.exe' : ''}`;
  const src = join(releaseDir, asset);
  if (!existsSync(src)) die(`${asset} missing from ${releaseDir}`);

  const pkgDir = join(REPO_ROOT, 'npm', target.pkg);
  if (!existsSync(pkgDir)) die(`${target.pkg} has no package dir -- run node npm/sync-packages.mjs`);

  mkdirSync(join(pkgDir, 'bin'), { recursive: true });
  const dest = join(pkgDir, 'bin', target.bin);
  copyFileSync(src, dest);
  // npm preserves the mode it finds in the tarball, and a binary that arrives
  // 0644 is a permission-denied on first run. Harmless on Windows, where the
  // mode is ignored and the .exe suffix is what makes it executable.
  chmodSync(dest, 0o755);
  for (const f of ATTRIBUTION) copyFileSync(join(releaseDir, f), join(pkgDir, f));

  process.stdout.write(`staged ${asset} -> npm/${target.pkg}/bin/${target.bin} `
    + `(${(statSync(dest).size / 1024 / 1024).toFixed(1)} MB)\n`);
  staged += 1;
}

// The launcher ships no binary, but it is still a distribution of oam's own
// Apache-2.0 code and npm shows the license file on the package page.
copyFileSync(join(releaseDir, 'LICENSE'), join(REPO_ROOT, 'npm/oamjs/LICENSE'));

if (staged !== Object.keys(SUPPORTED).length) {
  process.stderr.write(
    `oam-npm-stage: warning: staged ${staged} of ${Object.keys(SUPPORTED).length} platforms `
    + '-- publishing now would put empty packages on the registry\n',
  );
}
