#!/usr/bin/env bash
# Upload the locally-built win-arm64 oam binary to an existing GitHub Release
# and patch SHA256SUMS to cover it.
#
# Why this still exists: scripts/release-local.sh ships win-arm64 as a
# first-class asset now (GitHub Actions was removed, b2f8e24), so a normal
# release never needs this. It remains the patch-up path for an EXISTING
# release: one cut with a skip flag, or a win-arm64 asset that needs
# rebuilding without re-cutting the whole release.
#
# Usage (from the repo root, with HEAD on the tag and the release already cut):
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

# Local tag must match the remote tag -- otherwise this clobbers a good asset
# with a binary built from a commit users' tag will never resolve to.
remote_tag_obj="$(gh api "repos/${REPO}/git/ref/tags/${TAG}" --jq .object.sha 2>/dev/null)" \
  || { echo "error: tag ${TAG} is not on the remote -- push it first" >&2; exit 1; }
local_tag_obj="$(git rev-parse "$TAG")"
if [ "$remote_tag_obj" != "$local_tag_obj" ]; then
  echo "error: remote tag ${TAG} (${remote_tag_obj}) != local tag (${local_tag_obj}) -- local and origin disagree" >&2
  exit 1
fi

# The resident type-check daemon holds oam.exe open; kill before rebuild.
taskkill //F //IM oam.exe 2>/dev/null || true
cargo build --release -p oam_cli

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cp target/release/oam.exe "${tmp}/${ASSET}"

# The release must already exist -- this script patches, it does not cut.
gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1 \
  || { echo "error: release ${TAG} does not exist on ${REPO} -- cut it first with scripts/release-local.sh ${TAG}" >&2; exit 1; }

# Patch SHA256SUMS: drop any prior line for this asset, append ours. The
# manifest is re-read from the release at this step boundary, never cached.
gh release download "$TAG" --repo "$REPO" --pattern SHA256SUMS \
  --output "${tmp}/SHA256SUMS" --clobber
# Match field 2 exactly, stripping sha256sum's binary-mode "*" -- the manifest
# is written as "<hash> *<asset>", so `grep -v " <asset>$"` drops NOTHING and a
# re-run appends a SECOND line for this asset. Installers take the first match,
# which would then be the stale hash, so the download fails checksum verify.
# No `|| true`: awk exits 0 when nothing matches, so the only way this fails is
# a genuinely unreadable manifest -- which must abort under `set -e` rather than
# silently produce a SHA256SUMS containing just this one asset.
awk -v a="$ASSET" '{ f = $2; sub(/^\*/, "", f); if (f != a) print }' \
  "${tmp}/SHA256SUMS" > "${tmp}/SHA256SUMS.new"
(cd "$tmp" && sha256sum "$ASSET" >> SHA256SUMS.new && mv SHA256SUMS.new SHA256SUMS)

# Two calls, binary first. GitHub has no transactional multi-asset update,
# so a stale-sums window exists either way -- but split calls make a partial
# failure visible with exact remediation, and both are --clobber-idempotent.
gh release upload "$TAG" --repo "$REPO" "${tmp}/${ASSET}" --clobber \
  || { echo "error: binary upload failed -- release unchanged; re-run to retry" >&2; exit 1; }
gh release upload "$TAG" --repo "$REPO" "${tmp}/SHA256SUMS" --clobber \
  || { echo "error: SHA256SUMS upload failed AFTER the binary landed -- the live SHA256SUMS is now stale for ${ASSET}; re-run to converge" >&2; exit 1; }
echo "uploaded ${ASSET} and patched SHA256SUMS on ${TAG}"
