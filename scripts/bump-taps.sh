#!/bin/bash
# =============================================================================
# Bump the Homebrew formula and Scoop manifest to a published oam release.
# =============================================================================
# Called by scripts/release-local.sh after the GitHub Release is cut, and
# runnable standalone to repair taps that drifted:
#
#   ./scripts/bump-taps.sh v0.14.0
#   ./scripts/bump-taps.sh v0.14.0 --dry-run     # rewrite, show diff, no push
#
# WHY THIS EXISTS: it did not, and the taps rotted. `Formula/oam.rb` was hand
# written once at v0.8.0 (homebrew-yaw aa80023, 2026-08-06) and never touched
# again, so `brew install oam` served 0.8.1 while the project shipped 0.14.0 --
# six releases and five weeks stale. The same tap gets an automated "Bump to
# vX" on every Yaw Terminal release, because yaw's release.sh has a step for it
# and oam's did not. A distribution channel nobody updates is worse than one
# that does not exist: it hands users an old binary and tells them it is
# current, and `oam self-update` cannot rescue them because a brew-managed
# prefix is not the installer's per-user dir.
#
# The hashes come from the release's own published SHA256SUMS -- the same
# authority install.sh and install.ps1 verify against -- rather than from a
# re-download. Re-reading the published manifest at THIS step boundary (never a
# value cached earlier in the release run) is deliberate: on a resume, or on a
# standalone repair run, there is no earlier value to cache, and a hash derived
# from a local build directory could describe a binary that is not the one that
# actually shipped.
#
# Fails closed. A missing asset hash, a hash that is not 64 hex characters, or
# a rewrite that leaves a stale version string behind aborts before anything is
# committed: a WRONG published hash is worse than a stale one, because it trains
# people to ignore a mismatch.
#
# Env knobs:
#   OAM_HOMEBREW_DIR   path to the homebrew-yaw checkout (default: search)
#   OAM_SCOOP_DIR      path to the scoop-yaw checkout (default: search)
#   OAM_TAPS_OPTIONAL  1 = warn and exit 0 when a checkout is missing
#                      (release-local.sh sets this: a release is still valid
#                      without a tap bump, and the verification says so loudly)
# =============================================================================

set -euo pipefail

TAG="${1:?usage: bump-taps.sh <tag> [--dry-run]   (e.g. v0.14.0)}"
DRY_RUN=0
[ "${2:-}" = "--dry-run" ] && DRY_RUN=1

REPO="${OAM_REPO:-YawLabs/oam}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

RED='\033[0;31m'; GRN='\033[0;32m'; YEL='\033[1;33m'; CYA='\033[1;36m'; NC='\033[0m'
ok()  { echo -e "${GRN}  [ok]${NC} $*" >&2; }
warn(){ echo -e "${YEL}  [warn]${NC} $*" >&2; }
fail(){ echo -e "${RED}  [fail]${NC} $*" >&2; exit 1; }
step(){ echo -e "\n${CYA}=== $* ===${NC}" >&2; }

case "$TAG" in
  v[0-9]*) ;;
  *) fail "tag must look like v1.2.3 (got '$TAG')" ;;
esac
VERSION="${TAG#v}"

# --- locate the tap checkouts ----------------------------------------------
# The taps are siblings of yaw_terminal, not of oam, because Yaw Terminal
# created them first. Search rather than hardcode so a differently-laid-out
# clone still works, and so the failure names every path that was tried.
find_tap() {
  local name="$1" override="$2" c
  if [ -n "$override" ]; then
    [ -d "$override/.git" ] || fail "$name override '$override' is not a git checkout"
    printf '%s' "$override"; return 0
  fi
  for c in "$REPO_DIR/../$name" "$REPO_DIR/../../$name" "$REPO_DIR/../../yaw_terminal/$name" "$HOME/yaw/$name" "$HOME/yaw/yaw_terminal/$name"; do
    if [ -d "$c/.git" ]; then (cd "$c" && pwd); return 0; fi
  done
  return 1
}

HOMEBREW_DIR="$(find_tap homebrew-yaw "${OAM_HOMEBREW_DIR:-}")" || HOMEBREW_DIR=""
SCOOP_DIR="$(find_tap scoop-yaw "${OAM_SCOOP_DIR:-}")" || SCOOP_DIR=""

if [ -z "$HOMEBREW_DIR" ] || [ -z "$SCOOP_DIR" ]; then
  msg="tap checkout missing (homebrew-yaw='${HOMEBREW_DIR:-NOT FOUND}' scoop-yaw='${SCOOP_DIR:-NOT FOUND}') -- clone them beside yaw_terminal, or set OAM_HOMEBREW_DIR / OAM_SCOOP_DIR"
  if [ "${OAM_TAPS_OPTIONAL:-0}" = "1" ]; then warn "$msg"; warn "taps NOT bumped -- brew/scoop users stay on the previous release"; exit 0; fi
  fail "$msg"
fi

# --- the published SHA256SUMS is the only hash authority -------------------
step "Read the published SHA256SUMS for $TAG"
SUMS_DIR="$(mktemp -d -t oam-taps-XXXXXX)"
trap 'rm -rf "$SUMS_DIR"' EXIT
gh release download "$TAG" --repo "$REPO" --pattern SHA256SUMS --dir "$SUMS_DIR" \
  || fail "no published SHA256SUMS for $TAG -- cut the release before bumping taps"
SUMS="$SUMS_DIR/SHA256SUMS"

# hash_for <asset>: the sha256 for one asset name, or empty. sha256sum's
# binary-mode "*" prefix is stripped so the field matches the plain name.
hash_for() {
  awk -v want="$1" '{ n=$2; sub(/^\*/, "", n); if (n == want) { print $1; exit } }' "$SUMS"
}

# Every asset each tap serves. linux-arm64 is deliberately absent: it has never
# been released (the V8 snapshot forbids cross-compiling), and the formula omits
# the block rather than 404 on a URL that was never published.
BREW_ASSETS=(oam-aarch64-apple-darwin oam-x86_64-apple-darwin oam-x86_64-unknown-linux-gnu)
SCOOP_ASSETS=(oam-x86_64-pc-windows-msvc.exe oam-aarch64-pc-windows-msvc.exe)

declare -A HASH
for a in "${BREW_ASSETS[@]}" "${SCOOP_ASSETS[@]}"; do
  h="$(hash_for "$a")"
  [ -n "$h" ] || fail "SHA256SUMS for $TAG has no entry for $a -- refusing to publish a manifest with a missing hash"
  [[ "$h" =~ ^[0-9a-f]{64}$ ]] || fail "hash for $a is not 64 hex chars (got '$h')"
  HASH["$a"]="$h"
  ok "$a  ${h:0:12}..."
done

# --- helper: pull, rewrite, commit, push -----------------------------------
# Shared by both taps. The rewrite is done by the caller (different formats);
# this owns the git half so the two paths cannot drift.
publish_tap() {
  local dir="$1" file="$2" label="$3"
  if git -C "$dir" diff --quiet -- "$file"; then
    ok "$label already at $VERSION with matching hashes"
    return 0
  fi
  git -C "$dir" --no-pager diff --stat -- "$file" >&2
  if [ "$DRY_RUN" = "1" ]; then
    warn "DRY RUN -- $label rewritten but not committed; run 'git -C $dir checkout -- $file' to discard"
    return 0
  fi
  git -C "$dir" add "$file"
  git -C "$dir" commit -q -m "oam: bump to $TAG" \
    || fail "could not commit $label"
  # Idempotent: the commit is already local, so re-pushing the same ref is a
  # no-op. A blip here leaves this tap on the previous release while everything
  # else shipped, which is exactly the drift this script exists to end -- so it
  # is fatal, with the by-hand recovery printed.
  git -C "$dir" push -q origin HEAD \
    || fail "$label push failed -- finish by hand: git -C $dir push origin HEAD"
  ok "$label bumped to $VERSION and pushed"
}

# A tap that is behind origin cannot push, so sync first -- but these checkouts
# are SHARED. homebrew-yaw carries Casks/yaw.rb for Yaw Terminal, and a release
# of that product (or another agent session) can legitimately have it dirty
# while we are here for Formula/oam.rb. So: never stash, never rebase over
# someone else's uncommitted work, and never touch a file that is not ours.
# Rebase only when actually behind, and only from a clean tree.
sync_tap() {
  local dir="$1" file="$2" label="$3" branch behind dirty
  git -C "$dir" fetch -q origin || fail "could not fetch origin in $dir"
  branch="$(git -C "$dir" symbolic-ref --quiet --short HEAD)" \
    || fail "$label checkout is in a detached HEAD -- resolve by hand, then re-run"

  # Our own file being dirty means someone is mid-edit on exactly what we are
  # about to rewrite. Overwriting that is the one thing we must never do.
  if ! git -C "$dir" diff --quiet -- "$file" || ! git -C "$dir" diff --cached --quiet -- "$file"; then
    fail "$file already has uncommitted changes in $dir -- refusing to overwrite work in progress"
  fi

  behind="$(git -C "$dir" rev-list --count "HEAD..origin/$branch" 2>/dev/null || echo 0)"
  if [ "$behind" = "0" ]; then return 0; fi

  # Behind origin, so a push would be rejected and a rebase is required -- which
  # git refuses with a dirty tree, and which we refuse to force by stashing
  # another session's changes.
  dirty="$(git -C "$dir" status --porcelain | awk '{print $NF}' | tr '\n' ' ')"
  if [ -n "$dirty" ]; then
    fail "$label is $behind commit(s) behind origin/$branch AND has uncommitted changes ($dirty). Not stashing another session's work -- commit or stash them yourself, then re-run."
  fi
  git -C "$dir" rebase -q "origin/$branch" \
    || fail "could not rebase $dir onto origin/$branch -- resolve by hand, then re-run"
  ok "$label fast-forwarded $behind commit(s)"
}
sync_tap "$HOMEBREW_DIR" "Formula/oam.rb" "Homebrew formula"
sync_tap "$SCOOP_DIR" "bucket/oam.json" "Scoop manifest"

# --- Homebrew formula -------------------------------------------------------
# Asset-keyed, never positional: each `url` line names its asset, and the
# `sha256` that follows it belongs to that asset. Pairing by position is how a
# formula ends up serving the mac hash for the linux binary.
step "Rewrite Formula/oam.rb"
BREW_FILE="Formula/oam.rb"
[ -f "$HOMEBREW_DIR/$BREW_FILE" ] || fail "no $BREW_FILE in $HOMEBREW_DIR"
node -e '
  const fs = require("fs");
  const [path, version, tag, hashJson] = process.argv.slice(1);
  const hashes = JSON.parse(hashJson);
  const lines = fs.readFileSync(path, "utf-8").split("\n");
  let pendingAsset = null, seenVersion = false, rewrote = 0;
  for (let i = 0; i < lines.length; i++) {
    const versionLine = lines[i].match(/^(\s*)version\s+"[^"]*"\s*$/);
    if (versionLine && !seenVersion) { lines[i] = `${versionLine[1]}version "${version}"`; seenVersion = true; continue; }
    const url = lines[i].match(/^(\s*)url\s+"(https:\/\/github\.com\/[^"]*\/releases\/download\/)[^\/]+\/([^"]+)"(.*)$/);
    if (url) {
      const asset = url[3];
      if (!(asset in hashes)) throw new Error(`formula references an asset with no hash: ${asset}`);
      lines[i] = `${url[1]}url "${url[2]}${tag}/${asset}"${url[4]}`;
      pendingAsset = asset;
      continue;
    }
    const sha = lines[i].match(/^(\s*)sha256\s+"[^"]*"\s*$/);
    if (sha) {
      if (!pendingAsset) throw new Error(`sha256 at line ${i + 1} has no preceding url line`);
      lines[i] = `${sha[1]}sha256 "${hashes[pendingAsset]}"`;
      pendingAsset = null;
      rewrote++;
    }
  }
  if (!seenVersion) throw new Error("no top-level version line found");
  if (rewrote !== Object.keys(hashes).length) throw new Error(`rewrote ${rewrote} sha256 lines, expected ${Object.keys(hashes).length}`);
  fs.writeFileSync(path, lines.join("\n"));
' "$HOMEBREW_DIR/$BREW_FILE" "$VERSION" "$TAG" \
  "$(printf '{"%s":"%s","%s":"%s","%s":"%s"}' \
      "${BREW_ASSETS[0]}" "${HASH[${BREW_ASSETS[0]}]}" \
      "${BREW_ASSETS[1]}" "${HASH[${BREW_ASSETS[1]}]}" \
      "${BREW_ASSETS[2]}" "${HASH[${BREW_ASSETS[2]}]}")" \
  || fail "could not rewrite $BREW_FILE"

# --- Scoop manifest ---------------------------------------------------------
step "Rewrite bucket/oam.json"
SCOOP_FILE="bucket/oam.json"
[ -f "$SCOOP_DIR/$SCOOP_FILE" ] || fail "no $SCOOP_FILE in $SCOOP_DIR"
node -e '
  const fs = require("fs");
  const [path, version, tag, x64, arm64] = process.argv.slice(1);
  const m = JSON.parse(fs.readFileSync(path, "utf-8"));
  m.version = version;
  const set = (archKey, asset, hash) => {
    const arch = m.architecture && m.architecture[archKey];
    if (!arch) throw new Error(`manifest has no architecture.${archKey}`);
    arch.url = `https://github.com/YawLabs/oam/releases/download/${tag}/${asset}`;
    arch.hash = hash;
  };
  set("64bit", "oam-x86_64-pc-windows-msvc.exe", x64);
  set("arm64", "oam-aarch64-pc-windows-msvc.exe", arm64);
  fs.writeFileSync(path, JSON.stringify(m, null, 2) + "\n");
' "$SCOOP_DIR/$SCOOP_FILE" "$VERSION" "$TAG" \
  "${HASH[oam-x86_64-pc-windows-msvc.exe]}" "${HASH[oam-aarch64-pc-windows-msvc.exe]}" \
  || fail "could not rewrite $SCOOP_FILE"

# --- fail closed on a stale version string ----------------------------------
# The rewrite is asset-keyed and total, so any surviving reference to another
# release means a shape this script did not understand -- e.g. a URL the regex
# missed. Catching it here is the difference between "no bump" and "a manifest
# that mixes two releases".
for pair in "$HOMEBREW_DIR/$BREW_FILE" "$SCOOP_DIR/$SCOOP_FILE"; do
  if grep -oE 'releases/download/v[0-9][^/"]*' "$pair" | grep -qv "releases/download/$TAG"; then
    git -C "$(dirname "$(dirname "$pair")")" checkout -- "$pair" 2>/dev/null || true
    fail "$(basename "$pair") still references a release other than $TAG after rewrite -- reverted, nothing pushed"
  fi
done

step "Publish"
publish_tap "$HOMEBREW_DIR" "$BREW_FILE" "Homebrew formula"
publish_tap "$SCOOP_DIR" "$SCOOP_FILE" "Scoop manifest"

# --- verify what the taps actually serve ------------------------------------
# raw.githubusercontent caches a push for up to ~5 minutes, so a mismatch here
# is usually lag rather than failure. Warn-only: the push already succeeded or
# this script would have exited.
if [ "$DRY_RUN" = "1" ]; then exit 0; fi
step "Verify the taps serve $VERSION"
for spec in "homebrew-yaw/main/Formula/oam.rb" "scoop-yaw/main/bucket/oam.json"; do
  live="$(curl -fsSL --max-time 30 "https://raw.githubusercontent.com/YawLabs/$spec" 2>/dev/null || true)"
  if [ -z "$live" ]; then
    warn "could not fetch $spec to verify"
  elif printf '%s' "$live" | grep -q "\"$VERSION\""; then
    ok "$spec serves $VERSION"
  else
    warn "$spec does not serve $VERSION yet (raw.githubusercontent caches for ~5 min)"
  fi
done
