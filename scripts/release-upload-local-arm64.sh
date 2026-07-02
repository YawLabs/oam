#!/usr/bin/env bash
# Upload the locally-built win-arm64 oam binary to an existing GitHub Release
# and patch SHA256SUMS to cover it.
#
# Why this exists: GitHub's hosted ARM runners (windows-11-arm,
# ubuntu-24.04-arm) are public-repo-only, so while YawLabs/oam is private the
# release workflow ships only the four core targets (see release.yml
# build-arm). The dev box IS win-arm64; this script fills that gap until the
# repo goes public — then build-arm takes over and this script retires.
#
# Usage (from the repo root, after pushing the tag):
#   scripts/release-upload-local-arm64.sh v0.6.1
set -euo pipefail

TAG="${1:?usage: release-upload-local-arm64.sh <tag>}"
REPO="YawLabs/oam"
TARGET="aarch64-pc-windows-msvc"
ASSET="oam-${TARGET}.exe"

cd "$(dirname "$0")/.."

# The uploaded binary must be built from the tag's commit, not whatever the
# working tree happens to hold.
tag_sha="$(git rev-parse "${TAG}^{commit}")"
head_sha="$(git rev-parse HEAD)"
if [ "$tag_sha" != "$head_sha" ]; then
  echo "error: HEAD ($head_sha) is not the tag commit ($tag_sha); checkout the tag first" >&2
  exit 1
fi
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "error: working tree is dirty; release binaries build from clean trees" >&2
  exit 1
fi

# The resident type-check daemon holds oam.exe open; kill before rebuild.
taskkill //F //IM oam.exe 2>/dev/null || true
cargo build --release -p oam_cli

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cp target/release/oam.exe "${tmp}/${ASSET}"

# Wait for the release to exist (the tag push runs release.yml; the release
# job cuts it at the end, ~15 min after push).
for _ in $(seq 1 60); do
  if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
    break
  fi
  echo "waiting for release ${TAG} to exist..."
  sleep 30
done
gh release view "$TAG" --repo "$REPO" >/dev/null

# Patch SHA256SUMS: drop any prior line for this asset, append ours. The
# manifest is re-read from the release at this step boundary, never cached.
gh release download "$TAG" --repo "$REPO" --pattern SHA256SUMS \
  --output "${tmp}/SHA256SUMS" --clobber
grep -v " ${ASSET}\$" "${tmp}/SHA256SUMS" > "${tmp}/SHA256SUMS.new" || true
(cd "$tmp" && sha256sum "$ASSET" >> SHA256SUMS.new && mv SHA256SUMS.new SHA256SUMS)

gh release upload "$TAG" --repo "$REPO" "${tmp}/${ASSET}" "${tmp}/SHA256SUMS" --clobber
echo "uploaded ${ASSET} and patched SHA256SUMS on ${TAG}"
