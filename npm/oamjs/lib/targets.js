'use strict';

// The npm side of install/install.sh's uname -> target-triple table. Both files
// have to name the SAME five assets scripts/release-local.sh uploads, so the
// triple is spelled out here rather than reconstructed from the npm package
// name: a reader diffing this against install.sh should see the same strings,
// not two encodings of them.
//
// Keyed by `${process.platform} ${process.arch}` -- Node's names, not uname's,
// because that is what the launcher has at runtime and what npm matches the
// per-platform packages on (their "os" / "cpu" fields).
const SUPPORTED = {
  'win32 x64': { pkg: 'oamjs-win32-x64', triple: 'x86_64-pc-windows-msvc', bin: 'oam.exe' },
  'win32 arm64': { pkg: 'oamjs-win32-arm64', triple: 'aarch64-pc-windows-msvc', bin: 'oam.exe' },
  'darwin x64': { pkg: 'oamjs-darwin-x64', triple: 'x86_64-apple-darwin', bin: 'oam' },
  'darwin arm64': { pkg: 'oamjs-darwin-arm64', triple: 'aarch64-apple-darwin', bin: 'oam' },
  'linux x64': { pkg: 'oamjs-linux-x64', triple: 'x86_64-unknown-linux-gnu', bin: 'oam' },
};

// Platforms someone will plausibly be standing on where "nothing is published
// for this, and here is why" IS the answer. Without an entry the generic
// message below sends them hunting for a broken install instead of telling them
// the binary was never built. Wording tracks install/install.sh's die() for the
// same case, deliberately: the two channels must not disagree about why a
// Linux ARM box gets nothing.
const KNOWN_UNSUPPORTED = {
  'linux arm64':
    'no published oam binary for Linux arm64 yet (aarch64-unknown-linux-gnu is unreleased; '
    + 'use an x86_64 host or build from source)',
};

module.exports = { SUPPORTED, KNOWN_UNSUPPORTED };
