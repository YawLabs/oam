'use strict';

const fs = require('node:fs');
const { SUPPORTED, KNOWN_UNSUPPORTED } = require('./targets.js');

// Reads the glibc version the running Node is linked against. On a musl host
// (Alpine) the field is absent, which is the only reliable in-process signal:
// the x86_64-unknown-linux-gnu binary IS installed and executable there, and
// exec'ing it fails in the dynamic loader with a bare "No such file or
// directory" that names the loader, not the missing libc. Catching it here
// turns that into a sentence someone can act on.
// Callers distinguish three answers only: null (musl -- no glibc), a non-null
// value (glibc present), and undefined (could not tell). The VALUE of a
// non-null answer is never read, which is what lets the cheap probe below
// return a marker instead of a version.
//
// The cheap probe exists because this runs before every Linux launch, on the
// happy path, and `process.report.getReport()` is not cheap: it walks the
// native stack, the libuv handles and the whole environment to build a full
// diagnostic report, all of it discarded except one header field. On a runtime
// whose pitch is cold start, that is milliseconds per launch to answer a
// question that only matters when the exec is about to fail. An `existsSync`
// on the dynamic loader answers it in microseconds for both of the common
// cases, and getReport stays as the fallback for anything unrecognized.
function detectGlibc() {
  try {
    const arch = process.arch === 'arm64' ? 'aarch64' : 'x86_64';
    if (fs.existsSync(`/lib/ld-musl-${arch}.so.1`)) return null;
    for (const loader of ['/lib64/ld-linux-x86-64.so.2', '/lib/ld-linux-aarch64.so.1',
      '/lib/x86_64-linux-gnu/libc.so.6', '/lib/aarch64-linux-gnu/libc.so.6']) {
      if (fs.existsSync(loader)) return 'present';
    }
  } catch {
    // Fall through to the authoritative probe rather than guessing from an
    // fs error -- a sandbox that hides /lib is not a musl signal.
  }
  try {
    return process.report.getReport().header.glibcVersionRuntime || null;
  } catch {
    // Not a musl signal: an old Node, or one built without process.report.
    // Returning undefined means "unknown", and callers must not guess from it.
    return undefined;
  }
}

// Resolve the oam binary for this host, or throw an Error whose message is the
// whole explanation. Every argument is injectable so the unsupported-platform
// paths -- the ones that only exist on hardware we are not standing on -- are
// testable from any box.
function resolveBinary(opts = {}) {
  const platform = opts.platform || process.platform;
  const arch = opts.arch || process.arch;
  const env = opts.env || process.env;
  const resolve = opts.resolve || require.resolve;
  const exists = opts.exists || fs.existsSync;

  // Escape hatch, same spirit as install.sh's OAM_INSTALL_DIR: point the
  // launcher at a locally built target/release/oam without publishing
  // anything. Also what the launcher's own tests drive, so the tested code
  // path is the shipped one.
  if (env.OAMJS_BINARY) {
    if (!exists(env.OAMJS_BINARY)) {
      throw new Error(`OAMJS_BINARY is set to ${env.OAMJS_BINARY}, which does not exist`);
    }
    return env.OAMJS_BINARY;
  }

  const key = `${platform} ${arch}`;
  const target = SUPPORTED[key];

  if (!target) {
    if (KNOWN_UNSUPPORTED[key]) throw new Error(KNOWN_UNSUPPORTED[key]);
    throw new Error(
      `unsupported platform: ${platform} ${arch} (oam publishes binaries for `
      + `${Object.keys(SUPPORTED).join(', ')}; build from source for anything else)`,
    );
  }

  if (platform === 'linux') {
    // Keyed on the property being PRESENT, not on its value: undefined is a
    // meaningful injected value here ("the probe ran and could not answer"),
    // and a `!== undefined` guard would silently re-run the real probe for it.
    const glibc = Object.prototype.hasOwnProperty.call(opts, 'glibcVersionRuntime')
      ? opts.glibcVersionRuntime
      : detectGlibc();
    // Strictly null, never undefined: undefined means the probe could not run,
    // and a musl claim we cannot substantiate is worse than the loader error
    // it would replace.
    if (glibc === null) {
      throw new Error(
        'the only published Linux binary is x86_64-unknown-linux-gnu and this host has no '
        + 'glibc (musl -- Alpine and friends); build oam from source, or use a glibc image',
      );
    }
  }

  const request = `${target.pkg}/bin/${target.bin}`;
  try {
    return resolve(request);
  } catch (err) {
    if (err && err.code === 'MODULE_NOT_FOUND') {
      // The expected failure, not a corrupt tree: the per-platform packages are
      // optionalDependencies (that is what keeps `npm install oamjs` from
      // failing outright on a platform we do not publish), and npm skips an
      // optional dependency SILENTLY on a download failure or under
      // --no-optional / --omit=optional. So a missing package here says nothing
      // about the platform -- it says the install was partial.
      // Two different states reach this branch and they need opposite advice.
      // If the package resolves but its binary does not, the tarball shipped
      // without one -- telling that user to "reinstall with optional
      // dependencies enabled" sends them round a loop that reproduces the same
      // state forever, because the install already succeeded.
      let packagePresent = false;
      try {
        // The INJECTED resolver, not require.resolve: this is the same seam the
        // lookup above uses, so the tests drive the shipped code path rather
        // than a parallel one that only runs in production.
        resolve(`${target.pkg}/package.json`);
        packagePresent = true;
      } catch {
        packagePresent = false;
      }
      if (packagePresent) {
        throw new Error(
          `${target.pkg} is installed but does not contain bin/${target.bin}. That package was `
          + 'published without its binary, so reinstalling will not fix it -- please report it at '
          + 'https://github.com/YawLabs/oam/issues. To keep working now, set OAMJS_BINARY to an '
          + 'oam binary on this machine.',
        );
      }
      throw new Error(
        `${target.pkg} is not installed. oam's per-platform binaries are optionalDependencies, `
        + 'which npm skips silently when a download fails or when installing with --no-optional. '
        + `Reinstall with optional dependencies enabled, or set OAMJS_BINARY to an oam binary `
        + 'on this machine.',
      );
    }
    throw err;
  }
}

module.exports = { resolveBinary, detectGlibc };
