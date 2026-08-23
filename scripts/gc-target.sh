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
#   ./scripts/gc-target.sh --keep-incremental   # leave incremental/ alone
# =============================================================================

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
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

# target/x64-host/release is the win-x64 leg: release-local.sh builds it
# WITHOUT --target (see the comment there), so its output is not under a
# triple subdirectory. The triple-suffixed path stays listed so a tree left
# behind by a pre-0.11 release still gets collected.
TREES=(target/debug target/release target/x64-host/release target/x64-host/x86_64-pc-windows-msvc/release)

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
        {
          path = $3
          n = split(path, seg, "/")
          base = seg[n]
          # <name>-<16 hex>.<ext> is cargo`s hash-named output shape. Strip the
          # hash to get the family; anything that does not match that shape
          # (build scripts` output, .d files cargo rewrites in place) is left
          # entirely alone.
          if (match(base, /-[0-9a-f]{16}\./)) {
            family = substr(base, 1, RSTART - 1) substr(base, RSTART + 18)
            if (++seen[family] > keep) print $2 " " path
          }
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
