import { test } from 'node:test';
import assert from 'node:assert/strict';

import resolveModule from '../oamjs/lib/resolve.js';
import targetsModule from '../oamjs/lib/targets.js';

const { resolveBinary, detectGlibc } = resolveModule;
const { SUPPORTED } = targetsModule;

// Every platform path below is injected rather than probed, because four of the
// five are hardware this suite will never run on -- and the unsupported ones
// are hardware nobody will run it on deliberately.
const stub = (extra = {}) => ({
  env: {},
  resolve: (request) => `/resolved/${request}`,
  exists: () => true,
  ...extra,
});

test('resolves the per-platform package for every published target', () => {
  for (const [key, target] of Object.entries(SUPPORTED)) {
    const [platform, arch] = key.split(' ');
    assert.equal(
      resolveBinary(stub({ platform, arch, glibcVersionRuntime: '2.39' })),
      `/resolved/${target.pkg}/bin/${target.bin}`,
    );
  }
});

test('linux arm64 says the binary was never released, not that something is broken', () => {
  assert.throws(
    () => resolveBinary(stub({ platform: 'linux', arch: 'arm64' })),
    (err) => {
      // The three facts install/install.sh gives the same user: which triple,
      // that it is unreleased, and what to do instead.
      assert.match(err.message, /aarch64-unknown-linux-gnu/);
      assert.match(err.message, /unreleased/);
      assert.match(err.message, /build from source/);
      // A resolution failure would be indistinguishable from a broken install.
      assert.notEqual(err.code, 'MODULE_NOT_FOUND');
      return true;
    },
  );
});

test('an unpublished platform names what is published instead of failing blank', () => {
  assert.throws(
    () => resolveBinary(stub({ platform: 'freebsd', arch: 'x64' })),
    /unsupported platform: freebsd x64 .*win32 x64.*linux x64/s,
  );
});

test('a musl host is told it is a musl host', () => {
  // glibcVersionRuntime absent is the only in-process musl signal; without this
  // branch the gnu binary exec's and the dynamic loader reports "No such file
  // or directory" about a path that plainly exists.
  assert.throws(
    () => resolveBinary(stub({ platform: 'linux', arch: 'x64', glibcVersionRuntime: null })),
    /no glibc \(musl/,
  );
});

// detectGlibc itself, with the filesystem injected: every case above hands
// resolveBinary a ready-made answer, so the probe that produces the answer on a
// real Linux host was otherwise unexercised.
const MUSL_X64 = '/lib/ld-musl-x86_64.so.1';
const GLIBC_X64 = '/lib64/ld-linux-x86-64.so.2';
const probe = (present, extra = {}) => detectGlibc({
  arch: 'x64',
  exists: (p) => present.includes(p),
  report: () => { throw new Error('the report fallback must not run for this case'); },
  ...extra,
});

test('a glibc host that also has the musl loader installed is a glibc host', () => {
  // Debian/Ubuntu's `musl` package puts the musl loader on a glibc system. The
  // gnu binary runs there, so the musl loader alone must not decide.
  assert.equal(probe([MUSL_X64, GLIBC_X64]), 'present');
  assert.equal(probe([MUSL_X64, '/lib/x86_64-linux-gnu/libc.so.6']), 'present');
  // And resolveBinary, fed that answer, resolves instead of refusing.
  assert.equal(
    resolveBinary(stub({
      platform: 'linux', arch: 'x64', glibcVersionRuntime: probe([MUSL_X64, GLIBC_X64]),
    })),
    '/resolved/oamjs-linux-x64/bin/oam',
  );
});

test('the loader probe answers the two plain hosts without the report', () => {
  assert.equal(probe([GLIBC_X64]), 'present');
  assert.equal(probe([MUSL_X64]), null);
  assert.equal(
    detectGlibc({
      arch: 'arm64',
      exists: (p) => p === '/lib/ld-linux-aarch64.so.1',
      report: () => { throw new Error('unused'); },
    }),
    'present',
  );
  assert.equal(
    detectGlibc({
      arch: 'arm64',
      exists: (p) => p === '/lib/ld-musl-aarch64.so.1',
      report: () => { throw new Error('unused'); },
    }),
    null,
  );
});

test('an unrecognized layout falls back to the report, and an unusable report is "unknown"', () => {
  const none = () => false;
  assert.equal(
    detectGlibc({ arch: 'x64', exists: none, report: () => ({ header: { glibcVersionRuntime: '2.39' } }) }),
    '2.39',
  );
  assert.equal(detectGlibc({ arch: 'x64', exists: none, report: () => ({ header: {} }) }), null);
  // undefined, not null: a probe that could not run is not a musl claim.
  assert.equal(
    detectGlibc({ arch: 'x64', exists: none, report: () => { throw new Error('no report'); } }),
    undefined,
  );
  // An fs error is not a musl signal either; the report still decides.
  assert.equal(
    detectGlibc({
      arch: 'x64',
      exists: () => { throw new Error('EACCES'); },
      report: () => ({ header: { glibcVersionRuntime: '2.39' } }),
    }),
    '2.39',
  );
});

test('an unreadable glibc probe does not become a musl claim', () => {
  // undefined means the probe could not run (an old Node, one built without
  // process.report). Guessing musl there would send a glibc user to build from
  // source over a question we never actually answered.
  assert.equal(
    resolveBinary(stub({ platform: 'linux', arch: 'x64', glibcVersionRuntime: undefined })),
    '/resolved/oamjs-linux-x64/bin/oam',
  );
});

test('a skipped optional dependency blames the install, not the platform', () => {
  const notFound = () => {
    const err = new Error("Cannot find module 'oamjs-linux-x64/bin/oam'");
    err.code = 'MODULE_NOT_FOUND';
    throw err;
  };
  assert.throws(
    () => resolveBinary(stub({
      platform: 'linux',
      arch: 'x64',
      glibcVersionRuntime: '2.39',
      resolve: notFound,
    })),
    (err) => {
      assert.match(err.message, /oamjs-linux-x64 is not installed/);
      assert.match(err.message, /optionalDependencies/);
      assert.match(err.message, /--no-optional/);
      return true;
    },
  );
});

test('a resolution error that is not MODULE_NOT_FOUND is not swallowed', () => {
  // Hiding, say, an ERR_PACKAGE_PATH_NOT_EXPORTED behind the reinstall advice
  // above would send someone to reinstall a package that is already there.
  const boom = () => {
    const err = new Error('exports blocked the subpath');
    err.code = 'ERR_PACKAGE_PATH_NOT_EXPORTED';
    throw err;
  };
  assert.throws(
    () => resolveBinary(stub({
      platform: 'linux', arch: 'x64', glibcVersionRuntime: '2.39', resolve: boom,
    })),
    /exports blocked the subpath/,
  );
});

test('OAMJS_BINARY wins over the packages, and is checked before it is exec\'d', () => {
  assert.equal(
    resolveBinary(stub({ platform: 'linux', arch: 'arm64', env: { OAMJS_BINARY: '/tmp/oam' } })),
    '/tmp/oam',
  );
  assert.throws(
    () => resolveBinary(stub({ env: { OAMJS_BINARY: '/tmp/gone' }, exists: () => false })),
    /OAMJS_BINARY is set to \/tmp\/gone, which does not exist/,
  );
});

// A package that IS installed but shipped without its binary must not be
// reported as "not installed". That advice sends the user round a reinstall
// loop which reproduces the same state forever, because the install already
// succeeded -- the two states need opposite instructions.
test('an installed package missing its binary is not blamed on the install', () => {
  assert.throws(
    () => resolveBinary(stub({
      platform: 'linux',
      arch: 'x64',
      glibcVersionRuntime: '2.39',
      // package.json resolves (the package IS installed); the bin request does
      // not (the tarball shipped without it).
      resolve: (request) => {
        if (request.endsWith('/package.json')) return '/nm/oamjs-linux-x64/package.json';
        const e = new Error('not found');
        e.code = 'MODULE_NOT_FOUND';
        throw e;
      },
    })),
    (err) => {
      assert.match(err.message, /does not contain bin\//);
      assert.doesNotMatch(err.message, /Reinstall with optional dependencies/);
      return true;
    },
  );
});

// ...and the genuinely-skipped optional dependency keeps the advice that does
// help it.
test('a skipped optional dependency still says to reinstall with optionals', () => {
  assert.throws(
    () => resolveBinary(stub({
      platform: 'linux',
      arch: 'x64',
      glibcVersionRuntime: '2.39',
      resolve: () => {
        const e = new Error('not found');
        e.code = 'MODULE_NOT_FOUND';
        throw e;
      },
    })),
    /Reinstall with optional dependencies/,
  );
});
