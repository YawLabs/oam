#!/usr/bin/env bash
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
# Not a copy of the @yawlabs/*-mcp house script (scripts/update-manifests.mjs,
# shared verbatim across seven MCP-server repos), because both of that script's
# load-bearing mechanisms assume a shape oam does not have. It derives the
# command name, repo slug, license and description from package.json -- oam is a
# Rust workspace with no package.json at its root -- and it reads hashes from
# per-asset `.sha256` sidecars, which an oam release does not publish: the
# authority here is the single combined SHA256SUMS that install.sh and
# install.ps1 already verify against. What IS shared is the model (manifest
# repos checked out as siblings, pushed with the gh_woods key, no cross-repo CI
# token) and the env-var names below.
#
# Env knobs:
#   OAM_HOMEBREW_DIR   path to the homebrew-yaw checkout (default: search)
#   OAM_SCOOP_DIR      path to the scoop-yaw checkout (default: search)
#   YAW_HOMEBREW_DIR / YAW_SCOOP_DIR
#                      the org-wide spellings the @yawlabs/*-mcp scripts read,
#                      honored as a fallback so one export works everywhere.
#                      The OAM_-prefixed names win when both are set.
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
    # Absolutized, because a RELATIVE override silently broke the stale-version
    # guard's revert: its pathspec is resolved relative to the directory `git -C`
    # switched into, so `./homebrew-yaw` became `<dir>/<dir>/...`, checkout
    # errored, and the error was swallowed while the message claimed the file
    # had been reverted. The house script (mcp_servers/*/scripts/
    # update-manifests.mjs) resolves these the same way.
    (cd "$override" && pwd); return 0
  fi
  for c in "$REPO_DIR/../$name" "$REPO_DIR/../../$name" "$REPO_DIR/../../yaw_terminal/$name" "$HOME/yaw/$name" "$HOME/yaw/yaw_terminal/$name"; do
    if [ -d "$c/.git" ]; then (cd "$c" && pwd); return 0; fi
  done
  return 1
}

HOMEBREW_DIR="$(find_tap homebrew-yaw "${OAM_HOMEBREW_DIR:-${YAW_HOMEBREW_DIR:-}}")" || HOMEBREW_DIR=""
SCOOP_DIR="$(find_tap scoop-yaw "${OAM_SCOOP_DIR:-${YAW_SCOOP_DIR:-}}")" || SCOOP_DIR=""

if [ -z "$HOMEBREW_DIR" ] || [ -z "$SCOOP_DIR" ]; then
  msg="tap checkout missing (homebrew-yaw='${HOMEBREW_DIR:-NOT FOUND}' scoop-yaw='${SCOOP_DIR:-NOT FOUND}') -- clone them beside yaw_terminal, or set OAM_HOMEBREW_DIR / OAM_SCOOP_DIR"
  # Exit 3, not 0: the caller must be able to tell "skipped" from "done", or it
  # prints a success line directly under this warning.
  if [ "${OAM_TAPS_OPTIONAL:-0}" = "1" ]; then warn "$msg"; warn "taps NOT bumped -- brew/scoop users stay on the previous release"; exit 3; fi
  fail "$msg"
fi

# --- the published SHA256SUMS is the only hash authority -------------------
step "Read the published SHA256SUMS for $TAG"
SUMS_DIR="$(mktemp -d -t oam-taps-XXXXXX)"

# Which files this run has rewritten, and which have been published. A rewrite
# that never got published must not be left behind in a SHARED checkout: the
# next run's sync_tap refuses to touch a dirty file and blames the operator for
# this script's own leftover, so the documented repair path is dead until a
# human works out that the dirt is ours and discards it by hand.
#
# An EXIT trap rather than a revert at each failure site, because the failure
# sites are the problem: `fail` is called from eight places and from inside
# publish_tap, and every one of them that forgets to revert reintroduces this.
# The trap cannot be forgotten. Publishing is one-way, so a published tap is
# recorded and never reverted -- only the un-published remainder is undone.
BREW_REWRITTEN=0; BREW_PUBLISHED=0
SCOOP_REWRITTEN=0; SCOOP_PUBLISHED=0

cleanup() {
  local rc=$?
  rm -rf "$SUMS_DIR"
  # A clean exit leaves the tree as the run intended -- including --dry-run,
  # which deliberately keeps the rewrite and prints how to discard it.
  if [ "$rc" -eq 0 ]; then return; fi
  revert_unpublished "${HOMEBREW_DIR:-}" "$BREW_FILE" "$BREW_REWRITTEN" "$BREW_PUBLISHED"
  revert_unpublished "${SCOOP_DIR:-}" "$SCOOP_FILE" "$SCOOP_REWRITTEN" "$SCOOP_PUBLISHED"
}

# Undo one un-published rewrite, and say so only if there was something to undo.
# `git checkout --` on an already-committed file is a silent no-op, so an
# unconditional "reverted ..." would claim work that did not happen -- and here
# that matters: a committed-but-unpushed file is the recoverable state the next
# run pushes, not a leftover, and telling the operator it was reverted would
# send them looking for work that is still pending.
revert_unpublished() {
  local dir="$1" file="$2" rewritten="$3" published="$4"
  [ "$rewritten" = "1" ] && [ "$published" = "0" ] && [ -n "$dir" ] || return 0
  git -C "$dir" diff --quiet -- "$file" && return 0
  if git -C "$dir" checkout -- "$file" 2>/dev/null; then
    warn "reverted the un-published rewrite of $file"
  else
    warn "could not revert $file in $dir -- discard it by hand before re-running"
  fi
}
trap cleanup EXIT
gh release download "$TAG" --repo "$REPO" --pattern SHA256SUMS --dir "$SUMS_DIR" \
  || fail "no published SHA256SUMS for $TAG -- cut the release before bumping taps"
SUMS="$SUMS_DIR/SHA256SUMS"

# A tag older than the newest published release would rewrite BOTH public taps
# downward, push, and report success -- the verify step confirms it, because it
# only greps for the version it was told. That is a silent downgrade for every
# brew and scoop user, and the standalone repair path in the header (paste a tag
# from an older release log) is exactly how someone gets there by accident.
LATEST="$(gh release view --repo "$REPO" --json tagName -q .tagName 2>/dev/null || true)"
if [ -n "$LATEST" ] && [ "$LATEST" != "$TAG" ]; then
  newest="$(printf '%s
%s
' "${LATEST#v}" "${VERSION}" | sort -V | tail -1)"
  if [ "$newest" != "$VERSION" ]; then
    if [ "${OAM_ALLOW_DOWNGRADE:-0}" = "1" ]; then
      warn "$TAG is OLDER than the latest release $LATEST -- proceeding because OAM_ALLOW_DOWNGRADE=1"
    else
      fail "$TAG is older than the latest published release ($LATEST). Bumping the taps to it would DOWNGRADE every brew and scoop user, and the verify step would report success. Re-run with OAM_ALLOW_DOWNGRADE=1 if that is genuinely what you want."
    fi
  fi
fi

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

# Deliberately NOT an associative array: `declare -A` is bash 4, and macOS
# ships bash 3.2.57. The tailnet Mac is a release host and the header advertises
# this script as the standalone repair path, so a bash-4 feature would make that
# path exist only on this Windows box. Two parallel indexed arrays work
# everywhere, and `hash_of` is the lookup.
HASH_KEYS=()
HASH_VALS=()
hash_of() {
  local want="$1" i=0
  while [ "$i" -lt "${#HASH_KEYS[@]}" ]; do
    if [ "${HASH_KEYS[$i]}" = "$want" ]; then printf '%s' "${HASH_VALS[$i]}"; return 0; fi
    i=$((i + 1))
  done
  return 1
}
for a in "${BREW_ASSETS[@]}" "${SCOOP_ASSETS[@]}"; do
  h="$(hash_for "$a")"
  [ -n "$h" ] || fail "SHA256SUMS for $TAG has no entry for $a -- refusing to publish a manifest with a missing hash"
  case "$h" in
    *[!0-9a-f]* | "") fail "hash for $a is not 64 lowercase hex chars (got '$h')" ;;
  esac
  [ "${#h}" -eq 64 ] || fail "hash for $a is not 64 hex chars (got '$h')"
  HASH_KEYS+=("$a")
  HASH_VALS+=("$h")
  ok "$a  $(printf '%.12s' "$h")..."
done

# --- helper: pull, rewrite, commit, push -----------------------------------
# Shared by both taps. The rewrite is done by the caller (different formats);
# this owns the git half so the two paths cannot drift.
publish_tap() {
  local dir="$1" file="$2" label="$3" branch unpushed
  branch="$(git -C "$dir" symbolic-ref --quiet --short HEAD)" || return 1
  unpushed="$(git -C "$dir" rev-list --count "origin/$branch..HEAD" 2>/dev/null || echo 0)"
  # "Already current" means current AT ORIGIN, not merely in the worktree. A
  # previous run whose push failed leaves the commit local and the file clean,
  # so a worktree-only check reported success and early-returned PAST the push:
  # the documented repair re-run exited 0 while brew kept serving the old
  # release. That is the exact drift this script exists to end, with a green
  # checkmark on top. Ahead of origin now falls through to the push.
  if [ "$unpushed" = "0" ] && git -C "$dir" diff --quiet -- "$file"; then
    ok "$label already at $VERSION with matching hashes"
    return 0
  fi
  # Clean tree + unpushed commits = an earlier run committed and then failed to
  # push. There is nothing to commit, so go straight to the push; falling
  # through to `git commit` here fails with "nothing to commit" and turns a
  # recoverable state into a hard error on the documented repair path.
  if [ "$unpushed" != "0" ] && git -C "$dir" diff --quiet -- "$file"; then
    warn "$label: $unpushed commit(s) never reached origin (an earlier push failed) -- pushing now"
    git -C "$dir" push -q origin HEAD       || fail "$label push failed -- finish by hand: git -C $dir push origin HEAD"
    ok "$label pushed (already committed by an earlier run)"
    return 0
  fi
  git -C "$dir" --no-pager diff --stat -- "$file" >&2
  if [ "$DRY_RUN" = "1" ]; then
    warn "DRY RUN -- $label rewritten but not committed; run 'git -C $dir checkout -- $file' to discard"
    return 0
  fi
  # Pathspec form, and NO `git add`: a bare `git commit` commits the whole
  # INDEX, and these checkouts are SHARED -- homebrew-yaw also carries
  # Casks/yaw.rb for Yaw Terminal. Another session with an unrelated file
  # staged mid-edit would have had it swept into oam's commit and pushed to
  # the live tap, invisibly: the diffstat printed above is scoped to our
  # file, so it would not have shown the passenger. Committing the path
  # directly ignores the rest of the index, which is what this script's own
  # header promises and what the old shape quietly broke.
  git -C "$dir" commit -q -m "oam: bump to $TAG" -- "$file" \
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

  # An absent origin/<branch> must NOT read as "not behind". A shared checkout
  # parked on a local-only branch sailed through, and `push origin HEAD` then
  # CREATED that branch on the tap instead of updating the one people install
  # from -- exit 0, "bumped and pushed", origin/main still on the old release.
  # The verify step hardcodes main, which is the tell that this whole script
  # assumes it.
  if ! git -C "$dir" rev-parse --verify --quiet "origin/$branch" >/dev/null; then
    fail "$label checkout is on '$branch', which has no origin/$branch -- pushing would create a new branch on the tap instead of updating the one users install from. Switch it to the tap's default branch, then re-run."
  fi
  behind="$(git -C "$dir" rev-list --count "HEAD..origin/$branch")"
  if [ "$behind" = "0" ]; then return 0; fi

  # Behind origin, so a push would be rejected and a rebase is required -- which
  # git refuses with a dirty tree, and which we refuse to force by stashing
  # another session's changes.
  # --untracked-files=no: `git rebase` does not care about untracked files, so
  # a stray .bak or editor scratch file must not abort a release's tap bump.
  # The advice this printed was wrong for that case too -- `git stash` without
  # -u leaves untracked files exactly where they were, so an operator who
  # followed the instruction hit the identical failure on the re-run.
  dirty="$(git -C "$dir" status --porcelain --untracked-files=no | awk '{print $NF}' | tr '\n' ' ')"
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
      "${BREW_ASSETS[0]}" "$(hash_of "${BREW_ASSETS[0]}")" \
      "${BREW_ASSETS[1]}" "$(hash_of "${BREW_ASSETS[1]}")" \
      "${BREW_ASSETS[2]}" "$(hash_of "${BREW_ASSETS[2]}")")" \
  || fail "could not rewrite $BREW_FILE"
BREW_REWRITTEN=1

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
  "$(hash_of oam-x86_64-pc-windows-msvc.exe)" "$(hash_of oam-aarch64-pc-windows-msvc.exe)" \
  || fail "could not rewrite $SCOOP_FILE"
SCOOP_REWRITTEN=1

# --- fail closed on a stale version string ----------------------------------
# The rewrite is asset-keyed and total, so any surviving reference to another
# release means a shape this script did not understand -- e.g. a URL the regex
# missed. Catching it here is the difference between "no bump" and "a manifest
# that mixes two releases".
# Both files are rewritten before either is published, so an abort here must
# undo every un-published rewrite -- which the EXIT trap above now does for
# EVERY abort path, not just this one. Reverting only the file that tripped left
# the other rewritten and uncommitted in a SHARED checkout, and sync_tap then
# refused the documented repair run, blaming the operator for this script's own
# leftover.
for spec in "$HOMEBREW_DIR/$BREW_FILE" "$SCOOP_DIR/$SCOOP_FILE"; do
  if grep -oE 'releases/download/v[0-9][^/"]*' "$spec" | grep -qv "releases/download/$TAG"; then
    fail "$(basename "$spec") still references a release other than $TAG after rewrite -- nothing pushed, and every un-published rewrite is reverted"
  fi
done

step "Publish"
publish_tap "$HOMEBREW_DIR" "$BREW_FILE" "Homebrew formula"
BREW_PUBLISHED=1
publish_tap "$SCOOP_DIR" "$SCOOP_FILE" "Scoop manifest"
SCOOP_PUBLISHED=1

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
    # CDN lag is ONE cause and was previously reported as the only one, which
    # sent operators off to wait out a cache that would never change while the
    # real reason -- an unpushed commit, or a push that landed on a non-default
    # branch -- sat one free git command away. Check the local facts first.
    case "$spec" in
      homebrew-yaw/*) vdir="$HOMEBREW_DIR" ;;
      *)              vdir="$SCOOP_DIR" ;;
    esac
    vbranch="$(git -C "$vdir" symbolic-ref --quiet --short HEAD || echo '?')"
    vahead="$(git -C "$vdir" rev-list --count "origin/$vbranch..HEAD" 2>/dev/null || echo '?')"
    if [ "$vahead" != "0" ] && [ "$vahead" != "?" ]; then
      warn "$spec does not serve $VERSION: $vahead commit(s) in $vdir never reached origin/$vbranch. Push them: git -C $vdir push origin $vbranch"
    elif [ "$vbranch" != "main" ]; then
      warn "$spec does not serve $VERSION: $vdir is on '$vbranch', not main -- the push went to the wrong branch."
    else
      warn "$spec does not serve $VERSION yet; local state looks correct, so this is most likely raw.githubusercontent's cache (~5 min)."
    fi
  fi
done
