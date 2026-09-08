#!/usr/bin/env node
// Derive every npm package manifest under npm/ from the two things that
// actually decide their contents: the workspace version in Cargo.toml, and the
// platform table the launcher resolves against (npm/oamjs/lib/targets.js).
//
// Hand-maintaining six version fields plus five exact-pinned optionalDependency
// ranges is the failure this exists to prevent: a launcher pinned to a version
// nobody published resolves nothing, and the error the user sees is the
// "optional dependency was skipped" message -- which points at their install
// rather than at ours. Nothing here is generated at pack time, so the manifests
// are readable in the tree; --check is what makes a stale one loud.
//
//   node npm/sync-packages.mjs            rewrite the manifests from Cargo.toml
//   node npm/sync-packages.mjs --check    exit 1 on any drift, write nothing
//   node npm/sync-packages.mjs --root D   operate on a copy of the repo at D
//
// --root exists for the tests: drift has to be provable, and the only honest
// way to prove it is to perturb a real tree and watch --check fail.

import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const REPO_ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

export function workspaceVersion(cargoToml) {
  // Scoped to the [workspace.package] table on purpose. A bare
  // /^version = "(.+)"/ over the whole file matches whichever table comes
  // first, and Cargo.toml opens with [workspace] followed by [workspace.package]
  // -- so the loose form works today and silently starts reading a dependency's
  // version the day a table is inserted above it.
  const section = cargoToml.split(/^\[/m).find((s) => s.startsWith('workspace.package]'));
  if (!section) throw new Error('no [workspace.package] table in Cargo.toml');
  const m = section.match(/^version\s*=\s*"([^"]+)"/m);
  if (!m) throw new Error('no version key in [workspace.package]');
  return m[1];
}

// The launcher's own table, loaded rather than duplicated: these manifests and
// the runtime resolution must agree on the package names and the os/cpu pairs,
// and two lists that "should" match are two lists that will not.
async function loadTargets(root) {
  const mod = await import(pathToFileURL(join(root, 'npm/oamjs/lib/targets.js')).href);
  return mod.default.SUPPORTED;
}

function platformManifest(key, target, version) {
  const [os, arch] = key.split(' ');
  return {
    name: target.pkg,
    version,
    // The triple is the point of the description: it is the string that ties
    // this package to a named release asset in install/README.md.
    description: `The oam binary for ${os} ${arch} (${target.triple}). Installed as an optional dependency of oamjs; not useful on its own.`,
    license: 'Apache-2.0',
    homepage: 'https://oamjs.org',
    repository: {
      type: 'git',
      url: 'git+https://github.com/YawLabs/oam.git',
      directory: `npm/${target.pkg}`,
    },
    os: [os],
    cpu: [arch],
    // No "exports": the launcher resolves the binary by subpath
    // (`require.resolve('oamjs-linux-x64/bin/oam')`), and an exports map with
    // no entry for it turns that into ERR_PACKAGE_PATH_NOT_EXPORTED.
    files: ['bin/', 'LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.md'],
    // Yarn Berry zips a package unless told otherwise, and a zipped executable
    // cannot be exec'd.
    preferUnplugged: true,
  };
}

// npm renders whatever README it finds, and with none it renders the package
// name over an empty page -- which is how a support-bearing binary package ends
// up looking abandoned. Generated rather than committed for the same reason the
// manifests are: five near-identical files drift.
function platformReadme(key, target) {
  const [os, arch] = key.split(' ');
  return [
    `# ${target.pkg}`,
    '',
    `The oam binary for ${os} ${arch} (\`${target.triple}\`).`,
    '',
    'You do not install this directly. It is an optional dependency of',
    '[oamjs](https://www.npmjs.com/package/oamjs), which npm installs only on a',
    'matching host, and which resolves and exec\'s the binary in here.',
    '',
    'No install scripts. The binary is in the tarball.',
    '',
    'Apache-2.0. `LICENSE`, `NOTICE` and `THIRD_PARTY_LICENSES.md` ship beside the',
    'binary: it statically links V8, ICU, a port of Node\'s streams and around 380',
    'Rust crates, and those notices travel with every copy.',
    '',
    'Home: <https://oamjs.org> -- <https://github.com/YawLabs/oam>',
    '',
  ].join('\n');
}

function launcherPatch(pkg, version, supported) {
  const optional = {};
  // Exact pins, not ^: the launcher and the binary are one artifact cut from
  // one commit, and a caret range would let npm satisfy a 0.14.0 launcher with
  // a 0.14.1 binary whose CLI surface has moved.
  for (const target of Object.values(supported)) optional[target.pkg] = version;
  return { ...pkg, version, optionalDependencies: optional };
}

function readJson(p) {
  return JSON.parse(readFileSync(p, 'utf8'));
}

// Two spaces + trailing newline is what `npm init` and `npm version` write, so
// a manifest this script rewrites does not fight the next npm command.
function serialize(obj) {
  return `${JSON.stringify(obj, null, 2)}\n`;
}

export async function sync(root, { check }) {
  const version = workspaceVersion(readFileSync(join(root, 'Cargo.toml'), 'utf8'));
  const supported = await loadTargets(root);
  const drift = [];

  const writeOrCheck = (relPath, contents) => {
    const abs = join(root, relPath);
    const current = existsSync(abs) ? readFileSync(abs, 'utf8') : null;
    if (current === contents) return;
    if (check) {
      drift.push(current === null ? `${relPath} is missing` : `${relPath} is out of date`);
      return;
    }
    mkdirSync(dirname(abs), { recursive: true });
    writeFileSync(abs, contents);
    process.stdout.write(`wrote ${relPath}\n`);
  };

  for (const [key, target] of Object.entries(supported)) {
    writeOrCheck(
      `npm/${target.pkg}/package.json`,
      serialize(platformManifest(key, target, version)),
    );
    writeOrCheck(`npm/${target.pkg}/README.md`, platformReadme(key, target));
  }

  const launcherPath = 'npm/oamjs/package.json';
  const launcher = readJson(join(root, launcherPath));
  writeOrCheck(launcherPath, serialize(launcherPatch(launcher, version, supported)));

  return { version, drift };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const check = process.argv.includes('--check');
  const rootFlag = process.argv.indexOf('--root');
  const root = rootFlag === -1 ? REPO_ROOT : process.argv[rootFlag + 1];
  const { version, drift } = await sync(root, { check });
  if (drift.length > 0) {
    for (const d of drift) process.stderr.write(`  ${d}\n`);
    process.stderr.write(
      `npm/ manifests do not match Cargo.toml (${version}) -- run: node npm/sync-packages.mjs\n`,
    );
    process.exitCode = 1;
  } else if (check) {
    process.stdout.write(`npm/ manifests match Cargo.toml (${version})\n`);
  }
}
