#!/bin/bash
# =============================================================================
# Cross-platform benchmark sweep -- replaces bench.yml (removed b2f8e24).
# Advisory, on-demand, NOT a gate (same contract as the old workflow_dispatch
# job). Produces per-platform oam-vs-node-vs-bun numbers; the committed
# BENCHMARKS.md stays the windows-aarch64 dev-host reference, this gives the
# cross-platform directional picture.
# =============================================================================
# What it runs, per platform (a failed leg warns and the sweep continues):
#   local (this box)      cargo run -p xtask -- bench --release --compare
#   mac-arm64 (tailnet)   scripts/build-platforms-tailnet.sh --mode=bench
#   linux-x64 (GCP IAP)   scripts/build-platforms-gcp-iap.sh --mode=bench
#                         (includes the Linux-only io_uring same-binary A/B --
#                         bench.yml's io-uring-ab job)
#
# Runtime discovery is PATH-based: a missing bun just drops that column
# (build-remote.sh attempts a best-effort bun install on the remotes).
# Dedicated hosts beat GitHub's noisy shared runners, but treat cross-host
# numbers as directional, not publication-grade.
#
# Output: a stage dir with per-platform BENCHMARKS.md + results.json.
# LAST stdout line = the stage dir.
#
# Usage:
#   ./scripts/bench-platforms.sh                 # all three platforms
#   OAM_SKIP_MAC=1 ./scripts/bench-platforms.sh  # drop the mac leg
#   OAM_SKIP_LINUX=1 ...                         # drop the linux leg
#   OAM_SKIP_LOCAL=1 ...                         # drop the local leg
# Remote host config: same env as the two platform scripts (OAM_MAC_HOST,
# OAM_MAC_USER, OAM_GCP_*, OAM_KEEP_VM).
#
# NOTE: the local leg overwrites the committed BENCHMARKS.md +
# bench/results.json in the working tree (that is how the dev-host reference
# gets refreshed); review the diff before committing.
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_DIR"

RED='\033[0;31m'; GRN='\033[0;32m'; YEL='\033[1;33m'; CYA='\033[1;36m'; NC='\033[0m'
ok()  { echo -e "${GRN}  [ok]${NC} $*" >&2; }
warn(){ echo -e "${YEL}  [warn]${NC} $*" >&2; }
step(){ echo -e "\n${CYA}=== $* ===${NC}" >&2; }

case "${1:-}" in
  -h|--help) awk 'NR==1{next} !/^#/{exit} {sub(/^# ?/, ""); print}' "$0"; exit 0 ;;
esac

STAGE_DIR="$(mktemp -d -t oam-bench-XXXXXX)"

# --- local leg ------------------------------------------------------------------
if [ "${OAM_SKIP_LOCAL:-0}" = "1" ]; then
  warn "OAM_SKIP_LOCAL=1 -- local leg skipped"
else
  step "Local leg (bench --release --compare)"
  node --version >&2 2>/dev/null || warn "node not on PATH -- comparison columns will be thin"
  bun --version >&2 2>/dev/null || warn "bun not on PATH -- bun column dropped"
  if cargo run -p xtask -- bench --release --compare >&2; then
    mkdir -p "$STAGE_DIR/local"
    cp BENCHMARKS.md bench/results.json "$STAGE_DIR/local/" 2>/dev/null \
      || warn "some local bench outputs missing"
    ok "local leg done"
  else
    warn "local bench failed (sweep continues without it)"
  fi
fi

# --- remote legs (advisory: warn-and-continue on failure) -------------------------
if [ "${OAM_SKIP_MAC:-0}" = "1" ]; then
  warn "OAM_SKIP_MAC=1 -- mac leg skipped"
else
  step "Mac leg (tailnet, --mode=bench)"
  if MAC_ART=$(bash "$SCRIPT_DIR/build-platforms-tailnet.sh" --mode=bench); then
    MAC_ART="${MAC_ART##*$'\n'}"   # contract: artifact dir = LAST stdout line
    cp -r "$MAC_ART/mac-arm64" "$STAGE_DIR/"
    ok "mac leg done"
  else
    warn "mac leg failed (sweep continues without it)"
  fi
fi

if [ "${OAM_SKIP_LINUX:-0}" = "1" ]; then
  warn "OAM_SKIP_LINUX=1 -- linux leg skipped"
else
  step "Linux leg (GCP IAP, --mode=bench, incl. io_uring A/B)"
  if LINUX_ART=$(bash "$SCRIPT_DIR/build-platforms-gcp-iap.sh" --mode=bench); then
    LINUX_ART="${LINUX_ART##*$'\n'}"   # contract: artifact dir = LAST stdout line
    cp -r "$LINUX_ART/linux-x64" "$STAGE_DIR/"
    ok "linux leg done"
  else
    warn "linux leg failed (sweep continues without it)"
  fi
fi

# --- summary ----------------------------------------------------------------------
step "Staged bench results"
find "$STAGE_DIR" -type f -exec ls -lh {} \; >&2
ls "$STAGE_DIR"/*/results.json >/dev/null 2>&1 || warn "no results staged -- every leg failed or was skipped"

ok "bench results staged under $STAGE_DIR"
# LAST stdout line = the stage dir.
echo "$STAGE_DIR"
