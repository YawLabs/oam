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

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."
# shellcheck source=lib/build-locks.sh
. "$SCRIPT_DIR/lib/build-locks.sh"

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
#
# Read through git's transport, not `gh api .../git/ref/tags/<tag>`: the REST
# read path lags a push and 404s on a tag that is demonstrably on origin (see
# origin_tag_object in release-local.sh for the run that cost). ls-remote also
# separates "no such tag" (exit 0, empty) from "origin unreachable" (non-zero),
# which the API form collapsed into one misleading "push it first".
if ! remote_ls="$(git ls-remote --tags origin "refs/tags/${TAG}" 2>/dev/null)"; then
  echo "error: could not reach origin to read tag ${TAG}" >&2; exit 1
fi
remote_tag_obj="$(printf '%s\n' "$remote_ls" | awk -v r="refs/tags/${TAG}" '$2 == r {print $1; exit}')"
if [ -z "$remote_tag_obj" ]; then
  echo "error: tag ${TAG} is not on the remote -- push it first" >&2; exit 1
fi
local_tag_obj="$(git rev-parse "$TAG")"
if [ "$remote_tag_obj" != "$local_tag_obj" ]; then
  echo "error: remote tag ${TAG} (${remote_tag_obj}) != local tag (${local_tag_obj}) -- local and origin disagree" >&2
  exit 1
fi

# Live typed-cli sessions run this exact file. `taskkill //F //IM oam.exe`
# (what this used to do) killed the operator's other agent panes AND made the
# failure MORE likely: every killed session restarts on --resume, and a process
# mid-launch is precisely what denies the link step this path. Renaming frees
# it without touching a single process -- see scripts/lib/build-locks.sh.
#
# BOTH paths get parked: cargo's link writes deps/oam.exe FIRST and only promotes
# it to release/oam.exe on success, so the LNK1104 deny window lands on the
# deps-stage file. An orphan from an earlier aborted build is the usual holder
# there; rename frees it the same way it frees the promoted binary.
oam_reap_parked target/release
oam_reap_parked target/release/deps
if ! oam_park_file target/release/oam.exe; then
  echo "error: target/release/oam.exe is locked and a rename could not free it" >&2
  exit 1
fi
if ! oam_park_file target/release/deps/oam.exe; then
  echo "error: target/release/deps/oam.exe is locked and a rename could not free it" >&2
  exit 1
fi
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

# Self-check the assembled manifest BEFORE anything is uploaded. A bad manifest
# is worse than a failed upload: it publishes a release whose binary cannot be
# verified, and the installers fail closed on a checksum mismatch.
#
# 1. Exactly one entry for this asset. The dedupe above is the only thing
#    standing between a re-run and a duplicate pair, and installers take the
#    FIRST match -- which on a duplicate is the STALE hash. This is the exact
#    bug the old `grep -v " <asset>$"` pattern shipped.
entries="$(awk -v a="$ASSET" '{ f = $2; sub(/^\*/, "", f); if (f == a) n++ } END { print n+0 }' "${tmp}/SHA256SUMS")"
if [ "$entries" -ne 1 ]; then
  echo "error: SHA256SUMS has ${entries} entries for ${ASSET} (expected exactly 1) -- refusing to upload a manifest installers would resolve to the wrong hash" >&2
  exit 1
fi

# 2. The recorded hash actually matches the binary about to be uploaded.
#    --ignore-missing: the manifest covers all six targets, only ours is here.
(cd "$tmp" && sha256sum -c --ignore-missing SHA256SUMS >/dev/null) \
  || { echo "error: SHA256SUMS does not verify against ${ASSET} -- refusing to upload" >&2; exit 1; }
echo "  [ok] manifest self-check: 1 entry for ${ASSET}, hash verifies"

# Two calls, binary first. GitHub has no transactional multi-asset update,
# so a stale-sums window exists either way -- but split calls make a partial
# failure visible with exact remediation, and both are --clobber-idempotent.
gh release upload "$TAG" --repo "$REPO" "${tmp}/${ASSET}" --clobber \
  || { echo "error: binary upload failed -- release unchanged; re-run to retry" >&2; exit 1; }
gh release upload "$TAG" --repo "$REPO" "${tmp}/SHA256SUMS" --clobber \
  || { echo "error: SHA256SUMS upload failed AFTER the binary landed -- the live SHA256SUMS is now stale for ${ASSET}; re-run to converge" >&2; exit 1; }
echo "uploaded ${ASSET} and patched SHA256SUMS on ${TAG}"
