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

// Parse the flag AND its value out before picking the positional. Taking the
// first non-flag argument meant `--only <pkg> <dir>` swallowed <pkg> as the
// release dir and reported "no such release dir: oamjs-win32-arm64" -- naming
// something the operator never passed as a directory. A valueless `--only` used
// to yield [undefined], match no target, stage nothing, and exit 0.
const args = process.argv.slice(2);
const only = [];
const positional = [];
for (let i = 0; i < args.length; i += 1) {
  if (args[i] === '--only') {
    if (i + 1 >= args.length) diePreflight('--only needs a package name');
    only.push(args[i + 1]);
    i += 1;
  } else if (args[i].startsWith('--')) {
    diePreflight(`unknown flag: ${args[i]}`);
  } else {
    positional.push(args[i]);
  }
}
const releaseDir = positional[0];

// `die` is defined below and this parse runs above it; a second tiny exit keeps
// the parse where it reads naturally instead of hoisting the whole block.
function diePreflight(message) {
  process.stderr.write(`oam-npm-stage: error: ${message}
`);
  process.exit(1);
}

function die(message) {
  process.stderr.write(`oam-npm-stage: error: ${message}\n`);
  process.exit(1);
}

if (!releaseDir) die('usage: node npm/stage.mjs <release-dir> [--only <package>]');
// SUPPORTED is keyed by "<platform> <arch>"; --only names the PACKAGE, which is
// the `pkg` field. Validating against the wrong set is why a correct package
// name was rejected here a moment ago.
const PACKAGE_NAMES = Object.values(SUPPORTED).map((t) => t.pkg);
for (const name of only) {
  if (!PACKAGE_NAMES.includes(name)) {
    die(`--only ${name} is not a platform package. One of: ${PACKAGE_NAMES.join(', ')}`);
  }
}
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
  // npm packs the mode it finds on disk, and a binary that arrives 0644 is a
  // permission-denied on the user's first run.
  //
  // chmod cannot deliver that mode from Windows: libuv's uv_fs_chmod only
  // toggles FILE_ATTRIBUTE_READONLY, so the execute bit is a silent no-op and
  // the tarball ships 0644. Measured on this box -- staging the linux binary
  // and packing produced `-rw-r--r--  package/bin/oam`. The release box IS
  // Windows (scripts/release-local.sh runs there), so the default path would
  // have published three broken POSIX packages, and npm forbids re-publishing a
  // version: the only fix after the fact is a new release.
  //
  // So refuse rather than warn. The .exe targets are unaffected -- Windows has
  // no execute bit and the suffix is what makes them runnable -- which is why
  // this gate is per-target and not a blanket "do not stage on Windows".
  chmodSync(dest, 0o755);
  if (process.platform === 'win32' && !target.bin.endsWith('.exe')) {
    die(`cannot stage ${target.pkg} from Windows: chmod cannot set the execute bit here, `
      + `so the packed binary would be mode 0644 and every user of that platform would get a `
      + `permission denied. Stage and publish the POSIX packages from the mac or linux release `
      + `leg. (Set OAM_NPM_ALLOW_UNEXECUTABLE=1 to override, for a local pack you will not publish.)`);
  }
  for (const f of ATTRIBUTION) copyFileSync(join(releaseDir, f), join(pkgDir, f));

  process.stdout.write(`staged ${asset} -> npm/${target.pkg}/bin/${target.bin} `
    + `(${(statSync(dest).size / 1024 / 1024).toFixed(1)} MB)\n`);
  staged += 1;
}

// The launcher ships no binary, but it is still a distribution of oam's own
// Apache-2.0 code and npm shows the license file on the package page.
copyFileSync(join(releaseDir, 'LICENSE'), join(REPO_ROOT, 'npm/oamjs/LICENSE'));

// Fail, do not warn. A release script runs under `set -e` and sees exit 0, and
// a warning in a long release log is not a gate: the outcome is a published
// platform package with no binary, which npm does not allow un-publishing after
// 72 hours. --only is a local-verification tool, so it refuses here too rather
// than let a partially staged tree reach `npm publish`.
if (staged !== Object.keys(SUPPORTED).length) {
  die(`staged ${staged} of ${Object.keys(SUPPORTED).length} platforms. This tree is now `
    + `PARTIALLY staged and must NOT be published -- the unstaged packages would reach the `
    + `registry with no binary, and npm does not allow re-publishing a version. `
    + `A release stages every platform; --only is for local verification only.`);
}
