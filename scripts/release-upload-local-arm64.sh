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
# Signed releases: a release that carries RELEASE-MANIFEST (see
# scripts/lib/signing.sh) has its manifest VERIFIED before anything is patched
# -- patching on top of a manifest that does not verify would launder whatever
# made it fail under a fresh, valid signature -- and regenerated, re-signed and
# re-verified after. That needs the release key (OAM_RELEASE_SIGNING_KEY), so
# its passphrase is asked for up front, before the build. A pre-signing
# release (no manifest) is patched exactly as before, with a warning.
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
# shellcheck source=lib/signing.sh
. "$SCRIPT_DIR/lib/signing.sh"

# ONE EXIT trap: the scratch dir (created after the build) and the private
# signing agent (started before it) both go on every exit, Ctrl-C included.
tmp=""
cleanup() {
  release_agent_stop
  if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
}
trap cleanup EXIT

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

# The release must already exist -- this script patches, it does not cut.
# Checked before the build (it used to come after), so a typo in the tag costs
# seconds, not a release build.
gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1 \
  || { echo "error: release ${TAG} does not exist on ${REPO} -- cut it first with scripts/release-local.sh ${TAG}" >&2; exit 1; }

# Signed or pre-signing? Asked of the release's asset list now, so the key's
# passphrase prompt comes before the build rather than after it. Re-checked
# against what the download actually returns further down.
published_assets="$(gh release view "$TAG" --repo "$REPO" --json assets -q '.assets[].name')" \
  || { echo "error: could not list the assets of ${TAG}" >&2; exit 1; }
has_manifest=0; has_sig=0
grep -qxF RELEASE-MANIFEST <<<"$published_assets" && has_manifest=1
grep -qxF RELEASE-MANIFEST.sig <<<"$published_assets" && has_sig=1
if [ "$has_manifest" != "$has_sig" ]; then
  echo "error: ${TAG} carries RELEASE-MANIFEST=${has_manifest} but RELEASE-MANIFEST.sig=${has_sig} -- a half-signed release is not something to patch on top of; investigate it first" >&2
  exit 1
fi
SIGNED="$has_manifest"
if [ "$SIGNED" = "1" ]; then
  # A signed release can only be re-signed by a committed key: no bootstrap
  # skip applies here, whatever OAM_SIGN_REQUIRED says.
  sign_decision="$(release_signing_decision)"
  [ "$sign_decision" = "sign" ] \
    || { echo "error: ${TAG} is signed but this checkout cannot sign (${sign_decision}) -- cannot re-sign the patched manifest" >&2; exit 1; }
  release_agent_start || { echo "error: could not load the release signing key (OAM_RELEASE_SIGNING_KEY) -- see above" >&2; exit 1; }
  release_signing_preflight "$TAG" || { echo "error: signing preflight failed for ${TAG} -- see above; nothing was built or uploaded" >&2; exit 1; }
else
  echo "  [warn] ${TAG} has no RELEASE-MANIFEST (a pre-signing release) -- SHA256SUMS is patched unsigned, as before" >&2
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
cp target/release/oam.exe "${tmp}/${ASSET}"

# Re-read SHA256SUMS -- and the manifest pair, when there is one -- from the
# release at this step boundary, never cached. All three patterns in one call,
# whatever the up-front probe said, so a manifest that appeared or vanished
# during the build is caught rather than silently ignored or dropped.
gh release download "$TAG" --repo "$REPO" --dir "$tmp" --clobber \
  --pattern SHA256SUMS --pattern RELEASE-MANIFEST --pattern RELEASE-MANIFEST.sig
[ -s "${tmp}/SHA256SUMS" ] || { echo "error: ${TAG} has no SHA256SUMS to patch" >&2; exit 1; }
now_manifest=0
if [ -f "${tmp}/RELEASE-MANIFEST" ] || [ -f "${tmp}/RELEASE-MANIFEST.sig" ]; then now_manifest=1; fi
if [ "$now_manifest" != "$SIGNED" ]; then
  echo "error: ${TAG}'s manifest state changed during the build (signed=${SIGNED} before, ${now_manifest} now) -- re-run" >&2
  exit 1
fi

# Verify BEFORE patching. The patch keeps every other line of SHA256SUMS as
# published, and the re-sign below then vouches for all of them: patching a
# release whose manifest does not verify (a tampered SUMS line, a manifest
# for another tag) would put a fresh, valid signature on exactly what the old
# one refused.
if [ "$SIGNED" = "1" ]; then
  release_verify_manifest "$tmp" "$TAG" \
    || { echo "error: the published RELEASE-MANIFEST of ${TAG} does not verify -- refusing to patch on top of it; nothing was uploaded" >&2; exit 1; }
fi

# Patch SHA256SUMS: drop any prior line for this asset, append ours.
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

upload=("${tmp}/${ASSET}" "${tmp}/SHA256SUMS")
if [ "$SIGNED" = "1" ]; then
  # Same order as release-local.sh: write from the patched SHA256SUMS on disk,
  # sign, then verify what was written before any of it uploads.
  release_write_manifest "$tmp" "$TAG" || { echo "error: could not regenerate RELEASE-MANIFEST -- nothing was uploaded" >&2; exit 1; }
  release_sign_manifest "$tmp" || { echo "error: could not re-sign RELEASE-MANIFEST -- nothing was uploaded" >&2; exit 1; }
  release_verify_manifest "$tmp" "$TAG" || { echo "error: the re-signed RELEASE-MANIFEST does not verify -- nothing was uploaded" >&2; exit 1; }
  release_agent_stop
  upload+=("${tmp}/RELEASE-MANIFEST" "${tmp}/RELEASE-MANIFEST.sig")
fi

# ONE call for the binary, SHA256SUMS and (when signed) the manifest pair.
# GitHub has no transactional multi-asset update, so a window where some
# assets are new and some old remains -- but in one call it is as short as gh
# makes it, and with a signed manifest a mismatched set fails verification
# rather than verifying wrong: installers re-fetch the pair once, and a re-run
# converges (every asset is --clobber-idempotent).
gh release upload "$TAG" --repo "$REPO" "${upload[@]}" --clobber \
  || { echo "error: upload failed part-way -- the release may now mix new and old assets for ${ASSET}; re-run to converge" >&2; exit 1; }
if [ "$SIGNED" = "1" ]; then
  echo "uploaded ${ASSET}, patched SHA256SUMS and re-signed RELEASE-MANIFEST on ${TAG}"
else
  echo "uploaded ${ASSET} and patched SHA256SUMS on ${TAG}"
fi
