# shellcheck shell=bash
# =============================================================================
# What ships to a remote builder, and a ceiling on how much.
# =============================================================================
# Both platform orchestrators (build-platforms-gcp-iap.sh, the linux leg, and
# build-platforms-tailnet.sh, the mac leg) send the WORKING TREE to their
# builder as a tarball. They used to do it with a hand-maintained deny-list,
# duplicated verbatim in both files:
#
#   TAR_EXCLUDES=(--exclude=./.git --exclude=./target --exclude=./node_modules
#                 --exclude=./dist --exclude='*.log')
#   ( cd "$REPO_DIR" && tar czf "$t" "${TAR_EXCLUDES[@]}" . )
#
# Every one of those patterns is `./`-anchored, so each matched only the
# TOP-LEVEL copy. On 2026-08-31 that shipped an 11.6 GB tarball (run
# 20260831-155830): Claude Code had accumulated eight agent git worktrees under
# .claude/worktrees/, each a full checkout of this repo carrying its OWN
# target/ -- one of them 4.9 GB. None were excluded, because none of them are
# `./target`. The legitimate tree is ~1.6 MB. The release spent 35 minutes
# packing and pushing build junk over an IAP tunnel and then died, and because
# sync_src had no error handling it died SILENTLY: the only thing the operator
# saw was the EXIT trap's "stopping VM" warning and release-local.sh's generic
# "linux leg failed -- see its log output above", above which there was nothing
# to see.
#
# The fix is to stop maintaining a list of what the working tree is NOT and ask
# git what it IS. `git ls-files --cached --others --exclude-standard` is exactly
# "tracked files, plus untracked files git would not ignore" -- it needs no
# anchoring, it picks up new junk directories for free as they appear, and it
# cannot drift out of sync with .gitignore the way two copies of a hand-written
# array did.
#
# Do NOT be tempted back to tar with the anchors merely removed. Verified
# 2026-08-31 on GNU tar 1.35: an unanchored `--exclude=node_modules` does drop
# the nested worktree copies, but it ALSO drops
# conformance/vendor/node/test/fixtures/warning_node_modules/node_modules/**,
# four files that are deliberately TRACKED (.gitignore re-includes them) and
# that the node-suite's warning_node_modules case asserts on. That trades a
# loud failure for a silent conformance regression. The git-driven list keeps
# them, because git knows they are tracked.
#
# Same convention as lib/iap-helpers.sh and lib/build-locks.sh: sourced, never
# executed. Functions RETURN status; the caller owns fail()/warn().
# =============================================================================

# --- what ships ---------------------------------------------------------------

# write_src_tarball <repo-dir> <out-tar-gz>
# Packs the working tree as git sees it. Non-zero on any failure in the chain.
#
# `set -o pipefail` is set INSIDE the subshell rather than relied upon from the
# caller: without it a git that dies mid-list still yields a tar that exits 0,
# and the leg would ship a silently truncated tree. The mac leg does not set
# pipefail globally, so inheriting it is not a safe assumption.
#
# --no-recursion is load-bearing: git already names every file individually, and
# without it tar would descend into any directory it was handed and re-collect
# the very trees this exists to leave behind.
#
# --ignore-failed-read covers the one benign race -- a tracked file deleted in
# the working tree, which `--cached` still lists. tar warns on stderr and exits
# 0 rather than aborting a release over a file the local build does not have
# either.
write_src_tarball() {
  local repo="$1" out="$2"
  (
    set -o pipefail
    cd "$repo" || exit 1
    git ls-files -z --cached --others --exclude-standard \
      | tar czf "$out" --null --no-recursion --ignore-failed-read -T -
  )
}

# --- the size ceiling ----------------------------------------------------------
#
# The backstop, and deliberately not specific to the worktree bug above: it
# catches ANY future reason the tree balloons, including ones nobody has thought
# of yet. The real tarball is ~1.6 MB (that already includes the whole vendored
# node conformance corpus, which is the largest legitimate thing in here), so
# 200 MB is ~120x headroom -- far too loose to ever fire on honest growth, far
# too tight to let 11.6 GB through.
OAM_SRC_TARBALL_MAX_MB="${OAM_SRC_TARBALL_MAX_MB:-200}"

# src_tarball_over_ceiling <bytes>  -- 0 when the archive is implausibly large.
# An empty or non-numeric reading is NOT treated as oversized: a failed `wc -c`
# must not abort a release on a number nobody actually measured. Same inertness
# rule as disk_needs_reclaim/disk_below_floor in lib/iap-helpers.sh.
src_tarball_over_ceiling() {
  case "$1" in '' | *[!0-9]*) return 1 ;; esac
  [ "$1" -gt "$((OAM_SRC_TARBALL_MAX_MB * 1048576))" ]
}

# src_largest_paths <repo-dir> [n]
# The n biggest files the sync WOULD ship, as "<bytes>\t<path>", largest first.
# Diagnosis only, printed when the ceiling trips -- a bare "too big" number
# leaves the operator to go find the offender by hand, which is most of the cost
# of the incident this guards.
#
# `du --files0-from=-` is GNU-only. Both legs pack the tarball LOCALLY (the mac
# leg tars on this box and scp's the result), so that holds today; if it ever
# does not, du prints nothing and the caller still reports the size and the
# ceiling. A missing diagnosis must not swallow the failure itself.
src_largest_paths() {
  (
    cd "$1" || exit 1
    git ls-files -z --cached --others --exclude-standard \
      | du --files0-from=- -b 2>/dev/null | sort -rn | head -n "${2:-10}"
  )
}
