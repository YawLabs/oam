#!/bin/bash
# =============================================================================
# Cross-platform node-compat measurement -- replaces node-compat.yml's daily
# 4-platform "measure" job (removed b2f8e24). On-demand instead of cron: run
# it when you want the real per-platform >85%-gate numbers.
# =============================================================================
# What it runs, per platform (advisory -- a failed leg warns and the sweep
# continues, matching the old workflow's continue-on-error):
#   local (this box)      cargo run -p xtask -- conformance + node-suite
#   mac-arm64 (tailnet)   scripts/build-platforms-tailnet.sh --mode=measure
#   linux-x64 (GCP IAP)   scripts/build-platforms-gcp-iap.sh --mode=measure
#
# The per-PR RATCHET gate (the only gating part of node-compat.yml) lives in
# scripts/ci-local.sh step 7 -- this script is purely measurement.
#
# Output: a stage dir with per-platform scorecards + a pass-rate summary
# table on stderr. LAST stdout line = the stage dir.
#
# Usage:
#   ./scripts/node-compat-measure.sh                 # all three platforms
#   OAM_SKIP_MAC=1 ./scripts/node-compat-measure.sh  # drop the mac leg
#   OAM_SKIP_LINUX=1 ...                             # drop the linux leg
#   OAM_SKIP_LOCAL=1 ...                             # drop the local leg
# Remote host config: same env as the two platform scripts (OAM_MAC_HOST,
# OAM_MAC_USER, OAM_GCP_*, OAM_KEEP_VM).
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

STAGE_DIR="$(mktemp -d -t oam-node-compat-XXXXXX)"

# --- local leg ------------------------------------------------------------------
if [ "${OAM_SKIP_LOCAL:-0}" = "1" ]; then
  warn "OAM_SKIP_LOCAL=1 -- local leg skipped"
else
  step "Local leg (conformance + node-suite)"
  LOCAL_OK=1
  cargo build -p oam_cli >&2 || LOCAL_OK=0
  if [ "$LOCAL_OK" = "1" ]; then
    cargo run -p xtask -- conformance >&2 || warn "local conformance diverged (measurement continues)"
    cargo run -p xtask -- node-suite  >&2 || warn "local node-suite ratchet violated (measurement continues)"
    mkdir -p "$STAGE_DIR/local"
    cp conformance/scorecard.json conformance/node-suite-scorecard.json \
       CONFORMANCE.md CONFORMANCE-NODE.md "$STAGE_DIR/local/" 2>/dev/null \
      || warn "some local scorecards missing"
    ok "local leg done"
  else
    warn "local build failed -- local leg dropped"
  fi
fi

# --- remote legs (advisory: warn-and-continue on failure) -------------------------
if [ "${OAM_SKIP_MAC:-0}" = "1" ]; then
  warn "OAM_SKIP_MAC=1 -- mac leg skipped"
else
  step "Mac leg (tailnet, --mode=measure)"
  if MAC_ART=$(bash "$SCRIPT_DIR/build-platforms-tailnet.sh" --mode=measure); then
    MAC_ART="${MAC_ART##*$'\n'}"   # contract: artifact dir = LAST stdout line
    cp -r "$MAC_ART/mac-arm64" "$STAGE_DIR/"
    ok "mac leg done"
  else
    warn "mac leg failed (measurement continues without it)"
  fi
fi

if [ "${OAM_SKIP_LINUX:-0}" = "1" ]; then
  warn "OAM_SKIP_LINUX=1 -- linux leg skipped"
else
  step "Linux leg (GCP IAP, --mode=measure)"
  if LINUX_ART=$(bash "$SCRIPT_DIR/build-platforms-gcp-iap.sh" --mode=measure); then
    LINUX_ART="${LINUX_ART##*$'\n'}"   # contract: artifact dir = LAST stdout line
    cp -r "$LINUX_ART/linux-x64" "$STAGE_DIR/"
    ok "linux leg done"
  else
    warn "linux leg failed (measurement continues without it)"
  fi
fi

# --- summary ----------------------------------------------------------------------
step "Node-suite pass rates"
found=0
for sc in "$STAGE_DIR"/*/node-suite-scorecard.json; do
  [ -f "$sc" ] || continue
  found=1
  node -e '
    const s = require(process.argv[1]);
    console.log(`  ${s.host}: ${s.pass}/${s.runnable} runnable (${s.passOverRunnable} over runnable, ${s.passOverTotal} over total) -- node ${s.nodeVersion}, ${s.oamVersion}, ${s.commit}`);
  ' "$sc" >&2 || warn "could not summarize $sc"
done
[ "$found" = "1" ] || warn "no scorecards staged -- every leg failed or was skipped"

ok "scorecards staged under $STAGE_DIR"
# LAST stdout line = the stage dir.
echo "$STAGE_DIR"
