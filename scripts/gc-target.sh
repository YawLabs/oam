#!/bin/bash
# =============================================================================
# Prune stale cargo build outputs from target/.
# =============================================================================
# cargo never garbage-collects: every rebuild writes a NEW hash-named artifact
# and leaves every previous one behind forever. Measured on the dev box
# 2026-08-06, mid-v0.8.2-release:
#
#   target/debug/incremental   53.8 GB
#   target/debug/deps          48.2 GB   (e2e-<hash>.exe copies back to Jun 17)
#   target/release              6.6 GB
#   target/x64-host             4.9 GB
#
# What this does NOT do is prune by age. A flat `find -mtime +N -delete` over
# deps/ deletes rlibs that are OLD BUT STILL CURRENT -- a dependency compiled
# in June and untouched since is still the one being linked -- and for this
# workspace that means re-running the V8 build. So:
#
#   incremental/  deleted wholesale. Pure cache, regenerated on demand; the
#                 only cost is a slower next build.
#   deps/         per-FAMILY prune. Files are grouped by name-with-the-hash-
#                 stripped plus extension, sorted newest-first, and only copies
#                 beyond --keep are removed. A crate with exactly one artifact
#                 keeps it no matter how old it is, so nothing that is still
#                 linked can be collected.
#   *.inuse-*     parked copies from scripts/lib/build-locks.sh whose holder
#                 has since exited.
#
# A file some process still has mapped simply fails to unlink and is left; that
# is not an error here.
#
# Usage:
#   ./scripts/gc-target.sh                 # prune, keeping 1 copy per family
#   ./scripts/gc-target.sh --dry-run       # report only, delete nothing
#   ./scripts/gc-target.sh --keep 2        # keep the 2 newest per family
#
# On --keep: cargo emits a bin and that bin`s test harness under the SAME family
# name (both `oam-<hash>`, or `oam-<hash>.exe` on Windows), so the default keep=1
# can evict the current half of the pair and force a rebuild of it. That is a
# time cost, never a correctness one -- everything under deps/ is regenerable --
# but it is why the remote `gc` dispatch in build-remote.sh passes --keep 2.
#   ./scripts/gc-target.sh --keep-incremental   # leave incremental/ alone
# =============================================================================

set -euo pipefail

# Repo root from THIS script location, NOT `git rev-parse`: the remote build
# hosts receive the tree as a tarball of the files git tracks (the orchestrators
# via scripts/lib/src-sync.sh), which never includes .git itself, so there is no
# git dir there. rev-parse then fails, and
# because `cd ""` is a silent no-op the script would carry on against whatever
# directory the caller happened to be in -- pruning the wrong tree, or nothing,
# and reporting success either way.
cd "$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=lib/build-locks.sh
. scripts/lib/build-locks.sh

GRN='\033[0;32m'; YEL='\033[1;33m'; CYA='\033[0;36m'; NC='\033[0m'
say() { echo -e "${CYA}=== $* ===${NC}"; }
ok()  { echo -e "${GRN}  [ok]${NC} $*"; }
warn(){ echo -e "${YEL}  [warn]${NC} $*" >&2; }

DRY_RUN=0
KEEP=1
KEEP_INCREMENTAL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --keep) KEEP="${2:?--keep needs a count}"; shift ;;
    --keep-incremental) KEEP_INCREMENTAL=1 ;;
    -h|--help) awk 'NR==1{next} !/^#/{exit} {sub(/^# ?/, ""); print}' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 1 ;;
  esac
  shift
done
case "$KEEP" in ''|*[!0-9]*) echo "--keep needs a number, got '$KEEP'" >&2; exit 1 ;; esac
[ "$KEEP" -ge 1 ] || { echo "--keep must be >= 1 (0 would delete the current build)" >&2; exit 1; }

# One x64-host tree per emulated platform, not just Windows.
#
# target/x64-host/release is the win-x64 leg: release-local.sh builds it WITHOUT
# --target (see the comment there), so its output is not under a triple
# subdirectory. The triple-suffixed Windows path stays listed so a tree left
# behind by a pre-0.11 release still gets collected.
TREES=(
  target/debug
  target/release
  target/x64-host/release
  target/x64-host/x86_64-pc-windows-msvc/release
  target/x64-host/x86_64-apple-darwin/release
)

# deps/ pruning needs GNU find (-printf). macOS BSD find yields nothing and the
# prune silently reports zero candidates -- degrade LOUDLY instead.
GNU_FIND=1
find . -maxdepth 0 -printf '' >/dev/null 2>&1 || GNU_FIND=0

before_gb="$(oam_dir_gb target)"
say "target/ is ${before_gb}GB before pruning (keep=$KEEP, dry-run=$DRY_RUN)"

# rm that never fails the script: a running image cannot be unlinked on
# Windows, and that is an expected outcome here, not an error.
gc_rm() {
  if [ "$DRY_RUN" = "1" ]; then return 0; fi
  rm -f "$@" 2>/dev/null || true
}

for tree in "${TREES[@]}"; do
  [ -d "$tree" ] || continue

  if [ "$KEEP_INCREMENTAL" = "0" ] && [ -d "$tree/incremental" ]; then
    inc_gb="$(oam_dir_gb "$tree/incremental")"
    if [ "$DRY_RUN" = "1" ]; then
      warn "would delete $tree/incremental (${inc_gb}GB)"
    else
      rm -rf "$tree/incremental" 2>/dev/null || true
      ok "deleted $tree/incremental (${inc_gb}GB)"
    fi
  fi

  [ -d "$tree/deps" ] || continue
  if [ "$GNU_FIND" = "0" ]; then
    warn "$tree/deps: per-family prune skipped -- needs GNU find -printf (BSD find here); incremental/ was still reclaimed"
    oam_reap_parked "$tree"; oam_reap_parked "$tree/deps"
    continue
  fi
  # Newest-first globally, so the FIRST time awk sees a family it is that
  # family's current artifact. Anything past --keep in the same family is a
  # superseded copy. NUL-delimited: nothing here has spaces today, but a path
  # that grows one should not silently delete the wrong file.
  stale_count=0
  stale_kb=0
  while IFS= read -r -d '' line; do
    kb="${line%% *}"; path="${line#* }"
    stale_count=$((stale_count + 1))
    stale_kb=$((stale_kb + kb))
    gc_rm "$path"
  done < <(
    find "$tree/deps" -maxdepth 1 -type f -printf '%T@\t%k\t%p\0' 2>/dev/null |
      sort -zrn |
      awk -v keep="$KEEP" -v RS='\0' -v ORS='\0' -F'\t' '
        BEGIN {
          # Interval expressions ({16}) are NOT portable. mawk is Ubuntu`s
          # default awk -- and so the GCP builder`s -- and mawk 1.3.4 matches
          # NOTHING for /-[0-9a-f]{16}/ rather than erroring, so this whole
          # prune was a silent no-op on Linux: it reported a clean deps/ while
          # collecting zero of 2535 files (21GB). A dynamic string regex
          # behaves identically on mawk, gawk and BSD awk.
          hex = ""
          for (i = 0; i < 16; i++) hex = hex "[0-9a-f]"
          bare   = "-" hex "$"
          dotted = "-" hex "\\."
        }
        {
          path = $3
          n = split(path, seg, "/")
          base = seg[n]
          # cargo hash-names outputs in TWO shapes and both must be handled:
          #   <name>-<16 hex>.<ext>   rlib / rmeta / d, and .exe on Windows
          #   <name>-<16 hex>         bare test + bin executables on unix
          # Matching only the dotted shape blinded this prune to exactly the
          # files that dominate the tree: on the Linux builder 2026-08-22,
          # 107 extensionless binaries held 16GB of a 21GB deps/.
          if (match(base, bare)) {
            family = substr(base, 1, RSTART - 1)
          } else if (match(base, dotted)) {
            family = substr(base, 1, RSTART - 1) substr(base, RSTART + RLENGTH)
          } else {
            next
          }
          if (++seen[family] > keep) print $2 " " path
        }
      '
  )
  if [ "$stale_count" -gt 0 ]; then
    if [ "$DRY_RUN" = "1" ]; then
      warn "would prune $stale_count superseded artifacts from $tree/deps ($((stale_kb / 1024))MB)"
    else
      ok "pruned $stale_count superseded artifacts from $tree/deps ($((stale_kb / 1024))MB)"
    fi
  fi

  oam_reap_parked "$tree"
  oam_reap_parked "$tree/deps"
done

after_gb="$(oam_dir_gb target)"
if [ "$DRY_RUN" = "1" ]; then
  say "dry run -- nothing deleted (target/ still ${after_gb}GB)"
else
  say "target/ is ${after_gb}GB (was ${before_gb}GB)"
fi
