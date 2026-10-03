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
# its passphrase is asked for up front, before the build. The trust root for
# all of it is release-keys/ as committed on origin/main, NOT the tag's own
# copy: see "trust root" below. A release with no manifest is patched as
# before, with a warning -- but only if its tag predates every key range; a
# tag from the signing era with no manifest has lost it, and is refused. And
# never a release pinned in release-keys/presigning-sums (every published
# pre-signing release is): installers verify those by the hash of their
# SHA256SUMS, which a patch would change.
#
# Run THIS copy -- main's -- whatever the tag. The binary is built from the
# tag's commit in a throwaway git worktree, so HEAD stays where it is. Never
# `git checkout <old tag>` and run the copy that tree carries: a tag's tree
# holds this script as it was at that tag, and the copy in every pinned
# release's tree (v0.17.1 and older) patches SHA256SUMS with none of the
# checks above. Nothing on main can stop an old copy; only running main's
# does.
#
# Usage (from a main checkout, with the release already cut):
#   scripts/release-upload-local-arm64.sh v0.18.0
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

# ONE EXIT trap: the scratch dirs (the published asset's signature check, the
# staging dir created after the build) and the private signing agent (started
# before it) all go on every exit, Ctrl-C included.
tmp=""
trust_dir=""
wt=""
prior_dir=""
cleanup() {
  release_agent_stop
  if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
  if [ -n "$prior_dir" ]; then rm -rf "$prior_dir"; fi
  if [ -n "$trust_dir" ]; then rm -rf "$trust_dir"; fi
  if [ -n "$wt" ]; then
    git worktree remove --force "$wt" >/dev/null 2>&1 || true
    rm -rf "$wt"
    git worktree prune >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

# The uploaded binary must be built from the tag's commit, not whatever the
# working tree happens to hold: it is, in a worktree of that commit (below),
# which is clean by construction. This checkout supplies only the script, its
# libs and the target/ cache.
tag_sha="$(git rev-parse --verify -q "${TAG}^{commit}")" \
  || { echo "error: no tag ${TAG} in this clone -- fetch it first (git fetch origin tag ${TAG})" >&2; exit 1; }
REPO_ROOT="$(pwd)"

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

# The trust root: release-keys/ as committed on origin/main right now -- not
# this checkout's copy (which may lag origin), and never the tag's, which is
# frozen at the tag: a key whose range was closed since (the
# README's compromise step 1) would still verify there, and a key rotated in
# since would not. Verifying an attacker's v0.18.5 manifest, signed with a
# key retired at v0.18.3, against v0.18.5's own ranges would pass -- and the
# re-sign below would launder it. So the keys come from main's tip, read
# through git's transport like the tag above: ls-remote names the commit,
# fetch brings it, and the files are read from that exact commit (never
# FETCH_HEAD, which a concurrent fetch in a shared checkout can move).
if ! main_ls="$(git ls-remote origin refs/heads/main 2>/dev/null)"; then
  echo "error: could not reach origin to read main (the signing trust root)" >&2; exit 1
fi
main_sha="$(printf '%s\n' "$main_ls" | awk '$2 == "refs/heads/main" {print $1; exit}')"
[ -n "$main_sha" ] || { echo "error: origin has no main branch to read release-keys/ from" >&2; exit 1; }
git fetch -q origin main || { echo "error: could not fetch origin/main (the signing trust root)" >&2; exit 1; }
trust_dir="$(mktemp -d)"
release_keys_from_commit "$main_sha" "$trust_dir" \
  || { echo "error: could not read release-keys/ from origin/main ($main_sha) -- see above" >&2; exit 1; }
echo "  [ok] signing trust root: release-keys/ at origin/main ${main_sha}" >&2

# A pre-signing release is never patched. The installers (and `oam
# self-update`) verify a release cut before signing existed by the SHA-256 of
# its published SHA256SUMS, pinned in release-keys/presigning-sums and frozen
# into every installer and binary already out there. Patching its SHA256SUMS
# changes that hash, and from then on every one of them refuses the release --
# with no way to re-pin the copies people already have. Read from origin/main,
# like the keys: the tag's own tree predates the table. (This is also why the
# build happens in a worktree: from a checkout of the tag, the copy of this
# script that runs is the tag's, which has no such check.)
if ! pinned_sums="$(git show "${main_sha}:release-keys/presigning-sums" 2>/dev/null)"; then
  echo "error: release-keys/presigning-sums does not exist at origin/main (${main_sha}) -- cannot tell whether ${TAG} is a pinned pre-signing release; nothing was built or uploaded" >&2
  exit 1
fi
if awk -v t="$TAG" '!/^[[:space:]]*(#|$)/ && $1 == t { found = 1 } END { exit !found }' <<<"$pinned_sums"; then
  echo "error: ${TAG} is a pre-signing release pinned in release-keys/presigning-sums: install.sh, install.ps1 and oam self-update accept it only while its SHA256SUMS hashes to the pinned digest, so patching that file would break every install of ${TAG}. Ship the binary in a new release instead; nothing was built or uploaded" >&2
  exit 1
fi

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
sign_decision="$(release_signing_decision)"
case "$sign_decision" in
  fail:*) echo "error: ${sign_decision#fail:}" >&2; exit 1 ;;
esac
if [ "$SIGNED" = "1" ]; then
  # A signed release can only be re-signed by a committed key: no bootstrap
  # skip applies here, whatever OAM_SIGN_REQUIRED says.
  [ "$sign_decision" = "sign" ] \
    || { echo "error: ${TAG} is signed but origin/main's release-keys/ cannot sign (${sign_decision}) -- cannot re-sign the patched manifest" >&2; exit 1; }
  release_agent_start || { echo "error: could not load the release signing key (OAM_RELEASE_SIGNING_KEY) -- see above" >&2; exit 1; }
  release_signing_preflight "$TAG" || { echo "error: signing preflight failed for ${TAG} -- see above; nothing was built or uploaded" >&2; exit 1; }
else
  # No manifest. Benign only for a release cut before signing existed. Asked
  # of the committed ranges, never of the asset list alone: the asset list is
  # exactly what an attacker with upload access controls, and deleting the
  # pair is how a signed release would be passed off as an unsigned one --
  # this script would then patch on top of whatever SHA256SUMS they left.
  if [ "$sign_decision" = "sign" ] && ! release_tag_predates_signing "$TAG"; then
    echo "error: ${TAG} is at or after the start of a release-keys/ranges window, so it was cut in the signing era, but it carries no RELEASE-MANIFEST -- the manifest pair was removed (or the release was cut unsigned). Investigate before patching anything; nothing was built or uploaded" >&2
    exit 1
  fi
  echo "  [warn] ${TAG} has no RELEASE-MANIFEST (a pre-signing release) -- SHA256SUMS is patched unsigned, as before" >&2
fi

# Windows Authenticode for the patched-in binary: same decision, knobs and
# preflight as release-local.sh (OAM_WIN_SIGN_METADATA / _PUBLISHER,
# OAM_SKIP_WIN_SIGN, OAM_SIGN_REQUIRED), proven before the build so a lapsed
# az session costs seconds. Signed whatever the release's other assets carry:
# a signed binary is never worse than the unsigned one it replaces.
#
# The other direction is not symmetric. A run that will NOT sign (bootstrap:
# a shell without the knobs) must not --clobber a published asset that IS
# signed -- SmartScreen and Smart App Control would start blocking a binary
# they trusted, under a freshly re-signed manifest. So the current asset is
# fetched and its PE certificate table read; a signed one is replaced unsigned
# only on an explicit OAM_SKIP_WIN_SIGN=1, and "cannot tell" counts as signed.
# Asked twice: before the build (so a refusal costs seconds), and again right
# before the --clobber upload, from a fresh asset list -- a signed asset
# published by another run during the build must not be overwritten either.
#
# guard_signed_asset <what was not done> -- exit 1 when this run would replace
# a signed (or unreadable) published asset with an unsigned one.
guard_signed_asset() {
  local assets state=unknown
  [ "$WIN_SIGNING" = "0" ] || return 0
  assets="$(gh release view "$TAG" --repo "$REPO" --json assets -q '.assets[].name')" \
    || { echo "error: could not list ${TAG}'s assets to check whether ${ASSET} is Authenticode-signed; $1" >&2; exit 1; }
  grep -qxF "$ASSET" <<<"$assets" || return 0
  prior_dir="$(mktemp -d)"
  if gh release download "$TAG" --repo "$REPO" --dir "$prior_dir" --pattern "$ASSET"; then
    state="$(win_pe_signature_state "${prior_dir}/${ASSET}")"
  fi
  rm -rf "$prior_dir"
  prior_dir=""
  [ "$state" != "unsigned" ] || return 0
  if [ "${OAM_SKIP_WIN_SIGN:-0}" = "1" ]; then
    echo "  [warn] ${TAG}'s published ${ASSET} is Authenticode-signed (${state}); OAM_SKIP_WIN_SIGN=1 replaces it with an UNSIGNED build" >&2
    return 0
  fi
  echo "error: ${TAG}'s published ${ASSET} is Authenticode-signed (or could not be read: ${state}), and this run would replace it with an UNSIGNED build -- set OAM_WIN_SIGN_METADATA and OAM_WIN_SIGN_PUBLISHER (release-keys/README.md), or OAM_SKIP_WIN_SIGN=1 to downgrade it deliberately; $1" >&2
  exit 1
}
WIN_SIGNING=0
win_decision="$(win_sign_decision "${OAM_SKIP_WIN_SIGN:-0}")"
case "$win_decision" in
  sign) win_sign_preflight || { echo "error: Windows signing preflight failed -- see above; nothing was built or uploaded" >&2; exit 1; }
        WIN_SIGNING=1 ;;
  skip:*) echo "  [warn] ${win_decision#skip:}" >&2 ;;
  *) echo "error: ${win_decision#fail:}" >&2; exit 1 ;;
esac
guard_signed_asset "nothing was built or uploaded"

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
# The tag's tree, checked out beside this one: its Cargo.lock,
# .cargo/config.toml and rust-toolchain.toml (cargo and rustup read the last
# two from the cwd) are the ones that built the rest of the release. Output
# goes to THIS checkout's target/ -- the dirs parked above, and a warm cache.
wt="$(mktemp -d)"
git worktree add -q --detach "$wt" "$tag_sha" \
  || { echo "error: could not check ${TAG} out into a worktree at $wt" >&2; exit 1; }
[ "$(git -C "$wt" rev-parse HEAD)" = "$tag_sha" ] \
  || { echo "error: the worktree at $wt is not on ${TAG} ($tag_sha)" >&2; exit 1; }
(cd "$wt" && cargo build --release -p oam_cli --target-dir "$REPO_ROOT/target")

tmp="$(mktemp -d)"
cp target/release/oam.exe "${tmp}/${ASSET}"
# Sign the staged copy (never target/release/oam.exe), and prove it from disk,
# before anything below hashes it into SHA256SUMS.
if [ "$WIN_SIGNING" = "1" ]; then
  win_sign "${tmp}/${ASSET}" || { echo "error: Authenticode signing failed for ${ASSET} -- nothing was uploaded" >&2; exit 1; }
  win_verify "${tmp}/${ASSET}" || { echo "error: ${ASSET} does not verify after signing -- nothing was uploaded" >&2; exit 1; }
fi

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
# converges (every asset is --clobber-idempotent). Immediately before it, the
# signed-asset check once more, on the release as it is NOW, not as it was
# before the build.
guard_signed_asset "nothing was uploaded"
gh release upload "$TAG" --repo "$REPO" "${upload[@]}" --clobber \
  || { echo "error: upload failed part-way -- the release may now mix new and old assets for ${ASSET}; re-run to converge" >&2; exit 1; }
if [ "$SIGNED" = "1" ]; then
  echo "uploaded ${ASSET}, patched SHA256SUMS and re-signed RELEASE-MANIFEST on ${TAG}"
else
  echo "uploaded ${ASSET} and patched SHA256SUMS on ${TAG}"
fi
