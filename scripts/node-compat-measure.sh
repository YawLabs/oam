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
#   linux-x64 (Lima VM)   scripts/build-platforms-mac-linux.sh --mode=measure
#                         (an x86_64 container under Rosetta in a VM on the
#                         same Mac, run after the mac leg -- the two do not fit
#                         in its memory together)
#
# The per-PR RATCHET gate (the only gating part of node-compat.yml) lives in
# scripts/ci-local.sh step 9 (node-suite; step 8 is the conformance gate) --
# this script is purely measurement.
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
# OAM_MAC_USER, OAM_MAC_KEY, OAM_LIMA_INSTANCE, OAM_KEEP_VM).
#
# NOTE: the local leg runs the two xtask suites in this working tree, and they
# rewrite the committed receipts (CONFORMANCE.md, CONFORMANCE-NODE.md and the
# two scorecards under conformance/) when results changed; review the diff
# before committing. A suite that failed without writing its receipts stages
# nothing: the committed ones are not this run's numbers.
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
    mkdir -p "$STAGE_DIR/local"
    # A suite's receipts are staged only when THIS run produced them. Both
    # suites gate after writing (a divergence or a ratchet violation exits
    # non-zero with the receipts current), so a failure alone does not say
    # which: a run refused before measuring (a non-pinned node, an xtask that
    # does not compile) leaves the committed receipts in place, and copying
    # those would report them as this run's local numbers -- the "green leg
    # measuring nothing" case the summary below rejects for the remote legs,
    # which it cannot detect here because the host field matches. The tell is
    # the line each suite prints once its receipts are settled ("wrote ... +
    # <scorecard> (results changed)" or "... + <scorecard> unchanged"); if
    # that wording moves, a failed run stages nothing, which is the safe side.
    # $1 = xtask subcommand, $2 = scorecard path as the suite prints it,
    # $3 = its markdown twin, $4 = what a non-zero exit means when it measured.
    stage_local_suite() {
      local log="$STAGE_DIR/local/$1.log" rc=0
      # pipefail: the pipeline's status is cargo's (tee does not fail).
      cargo run -p xtask -- "$1" 2>&1 | tee "$log" >&2 || rc=$?
      if [ "$rc" -ne 0 ]; then
        if grep -qF -e "+ $2 (results changed)" -e "+ $2 unchanged" "$log"; then
          warn "local $1: $4 (measurement continues)"
        else
          warn "local $1 failed before it measured anything (exit $rc; log: $log) -- its receipts in the tree are the COMMITTED ones, not this run's, so they are not staged"
          return 0
        fi
      fi
      cp "$2" "$3" "$STAGE_DIR/local/" 2>/dev/null \
        || warn "local $1: receipts missing ($2, $3)"
    }
    stage_local_suite conformance conformance/scorecard.json CONFORMANCE.md "diverged"
    stage_local_suite node-suite conformance/node-suite-scorecard.json CONFORMANCE-NODE.md "ratchet violated"
    ok "local leg done"
  else
    warn "local build failed -- local leg dropped"
  fi
fi

# --- remote legs (advisory: warn-and-continue on failure) -------------------------
# A linux-leg VM left running would hold the build Mac's memory through the
# mac leg. The linux leg's preflight stops one it finds Running with no live
# lock, as release-local does before its mac legs. A VM an earlier run left
# with OAM_KEEP_VM=1 comes with that run's Mac build lock still held: this
# preflight then warns BUSY and stops nothing, and this run's linux leg is
# BUSY too -- clear it by hand first (the BUSY message prints the limactl
# stop and rm -rf ~/.lima-build.lock commands). The preflight runs with
# OAM_KEEP_VM=0 whatever is exported here: under OAM_KEEP_VM=1 the preflight
# keeps the Mac build lock (protocol v1.2, every mode), and this run's own
# linux leg would then find it BUSY. The knob still applies to that leg.
if [ "${OAM_SKIP_MAC:-0}" != "1" ] && [ "${OAM_SKIP_LINUX:-0}" != "1" ]; then
  OAM_KEEP_VM=0 bash "$SCRIPT_DIR/build-platforms-mac-linux.sh" --preflight-only \
    || warn "linux leg preflight failed (see above) -- a VM it could not stop, or one an OAM_KEEP_VM=1 run left with the Mac build lock held (BUSY), would slow the mac leg; continuing"
fi

if [ "${OAM_SKIP_MAC:-0}" = "1" ]; then
  warn "OAM_SKIP_MAC=1 -- mac leg skipped"
else
  step "Mac leg (tailnet, --mode=measure)"
  if MAC_ART=$(bash "$SCRIPT_DIR/build-platforms-tailnet.sh" --mode=measure); then
    MAC_ART="${MAC_ART##*$'\n'}"   # contract: artifact dir = LAST stdout line
    # `&& ok || warn`: under set -e a bare cp would end the sweep here, with
    # no summary and no stage-dir line, when the leg handed back no such dir.
    cp -r "$MAC_ART/mac-arm64" "$STAGE_DIR/" && ok "mac leg done" \
      || warn "mac leg exited 0 but handed back no mac-arm64 dir under '${MAC_ART}' (measurement continues without it)"
  else
    warn "mac leg failed (measurement continues without it)"
  fi
fi

if [ "${OAM_SKIP_LINUX:-0}" = "1" ]; then
  warn "OAM_SKIP_LINUX=1 -- linux leg skipped"
else
  step "Linux leg (Lima VM on the build Mac, --mode=measure)"
  if LINUX_ART=$(bash "$SCRIPT_DIR/build-platforms-mac-linux.sh" --mode=measure); then
    LINUX_ART="${LINUX_ART##*$'\n'}"   # contract: artifact dir = LAST stdout line
    cp -r "$LINUX_ART/linux-x64" "$STAGE_DIR/" && ok "linux leg done" \
      || warn "linux leg exited 0 but handed back no linux-x64 dir under '${LINUX_ART}' (measurement continues without it)"
  else
    warn "linux leg failed (measurement continues without it)"
  fi
fi

# --- summary ----------------------------------------------------------------------
# The committed scorecard travels to each remote in the source tarball, so a
# leg whose node-suite never wrote one pulls THAT file back and reports the
# local numbers under a remote platform's name -- a green leg measuring
# nothing. The host field is the receipt: it must name the platform the leg
# ran on, or the result is this box's and gets rejected.
expected_host_for() {
  case "$1" in
    linux-x64)   echo "linux-x86_64" ;;
    mac-arm64)   echo "macos-aarch64" ;;
    local)       echo "" ;;
    *)           echo "" ;;
  esac
}

step "Node-suite pass rates"
found=0
stale=0
for sc in "$STAGE_DIR"/*/node-suite-scorecard.json; do
  [ -f "$sc" ] || continue
  leg="$(basename "$(dirname "$sc")")"
  want="$(expected_host_for "$leg")"
  got="$(node -p 'require(process.argv[1]).host' "$sc" 2>/dev/null || echo "")"
  if [ -n "$want" ] && [ "$want" != "$got" ]; then
    warn "$leg staged a scorecard for host '$got', not '$want' -- its node-suite did not write one, so this is the LOCAL scorecard round-tripping. Discarding."
    stale=1
    continue
  fi
  found=1
  node -e '
    const s = require(process.argv[1]);
    console.log(`  ${s.host}: ${s.pass}/${s.runnable} runnable (${s.passOverRunnable} over runnable, ${s.passOverTotal} over total) -- node ${s.nodeVersion}, ${s.oamVersion}, ${s.commit}`);
  ' "$sc" >&2 || warn "could not summarize $sc"
done
[ "$found" = "1" ] || warn "no scorecards staged -- every leg failed or was skipped"
[ "$stale" = "0" ] || warn "at least one leg reported success while measuring nothing -- treat its floor as UNCHANGED"

ok "scorecards staged under $STAGE_DIR"
# LAST stdout line = the stage dir.
echo "$STAGE_DIR"
