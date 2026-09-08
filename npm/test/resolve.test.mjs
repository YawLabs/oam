import { test } from 'node:test';
import assert from 'node:assert/strict';

import resolveModule from '../oamjs/lib/resolve.js';
import targetsModule from '../oamjs/lib/targets.js';

const { resolveBinary } = resolveModule;
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
