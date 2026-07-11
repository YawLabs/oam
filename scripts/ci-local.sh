#!/bin/bash
# =============================================================================
# Local CI gate -- replaces .github/workflows/ci.yml (removed b2f8e24)
# =============================================================================
# Runs the same checks ci.yml used to run on every push to main / PR, on the
# local platform (the daily-driver dev box is win-arm64, the one target the
# hosted runners never covered anyway):
#   1. cargo fmt --all --check
#   2. cargo clippy --workspace --all-targets -- -D warnings
#   3. cargo build --workspace
#   4. cargo test --workspace          (15-min ceiling where `timeout` exists)
#   5. smoke: ./target/debug/oam run   (expects "ci smoke 42", byte-identical)
#   6. cargo run -p xtask -- conformance    (node-differential gate; GATING)
#   7. cargo run -p xtask -- node-suite     (skip-ratchet gate; pass-rate is
#                                            advisory, only a ratchet violation
#                                            fails -- node-compat.yml parity)
#   8. unsafe-mention count per crate       (ADVISORY, never fails)
#
# Cross-platform coverage moved to the remote legs: scripts/release-local.sh
# gates a release on this script PLUS gate+test+conformance on the GCP Linux
# VM and the tailnet Mac (see scripts/build-remote.sh). For an on-demand
# cross-platform sweep outside a release, use scripts/node-compat-measure.sh.
#
# Usage:
#   ./scripts/ci-local.sh              # full gate (steps 1-8)
#   ./scripts/ci-local.sh --fast       # skip conformance + node-suite (6-7)
#   ./scripts/ci-local.sh --no-tests   # skip the cargo test run (4)
#
# Install as a pre-push hook (so you can't push without it passing). A
# wrapper, NOT `ln -s`: MSYS/Git Bash `ln -s` silently COPIES the file, and
# a copy goes stale the next time this script changes:
#   printf '#!/bin/sh\nexec bash scripts/ci-local.sh\n' > .git/hooks/pre-push
#   chmod +x .git/hooks/pre-push
# =============================================================================

set -e
set -o pipefail

# Run from repo root regardless of how this script is invoked (direct call,
# symlinked .git/hooks/pre-push, etc.). $0 under a symlinked hook resolves
# to the hook path, not the script -- git rev-parse is the robust answer.
cd "$(git rev-parse --show-toplevel)"

SKIP_TESTS=0
FAST=0
for arg in "$@"; do
  case "$arg" in
    --no-tests) SKIP_TESTS=1 ;;
    --fast) FAST=1 ;;
    -h|--help)
      # Print the leading comment block (after the shebang) with the `# `
      # prefix stripped. Stops at the first non-comment line.
      awk 'NR==1{next} !/^#/{exit} {sub(/^# ?/, ""); print}' "$0"
      exit 0
      ;;
  esac
done

RED='\033[0;31m'; GRN='\033[0;32m'; YEL='\033[1;33m'; CYA='\033[0;36m'; NC='\033[0m'
say() { echo -e "${CYA}=== $* ===${NC}"; }
ok()  { echo -e "${GRN}  [ok]${NC} $*"; }
warn(){ echo -e "${YEL}  [warn]${NC} $*" >&2; }
ko()  { echo -e "${RED}  [fail]${NC} $*" >&2; exit 1; }

say "1/8 Format (cargo fmt --check)"
if cargo fmt --all --check; then
  ok "fmt clean"
else
  ko "fmt diffs above -- run 'cargo fmt --all' and re-stage"
fi

say "2/8 Clippy (-D warnings)"
# -D warnings via clippy args, NOT RUSTFLAGS: a global RUSTFLAGS would
# fingerprint-poison the cargo cache against the plain build/test steps.
if cargo clippy --workspace --all-targets -- -D warnings; then
  ok "clippy clean"
else
  ko "clippy warnings above"
fi

say "3/8 Build (cargo build --workspace)"
if cargo build --workspace; then
  ok "build ok"
else
  ko "build failed"
fi

if [ "$SKIP_TESTS" -eq 0 ]; then
  say "4/8 Tests (cargo test --workspace)"
  # ci.yml installed tsgo per matrix leg (continue-on-error); locally just
  # surface the gap -- the oam-check differential tests self-skip without it.
  command -v tsgo >/dev/null 2>&1 \
    || warn "tsgo not on PATH -- oam-check tests will self-skip (npm install -g @typescript/native-preview)"
  # ci.yml bounded a hung test at 15 min (the deadlocked/lingering test
  # class). Git Bash ships coreutils timeout; macOS needs brew coreutils
  # (gtimeout) -- unbounded as the last resort.
  if command -v timeout >/dev/null 2>&1; then
    timeout 900 cargo test --workspace || ko "tests failed (or hit the 15-min hang ceiling)"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout 900 cargo test --workspace || ko "tests failed (or hit the 15-min hang ceiling)"
  else
    cargo test --workspace || ko "tests failed"
  fi
  ok "tests passed"
else
  say "4/8 Tests SKIPPED (--no-tests)"
fi

say "5/8 Smoke (oam run)"
SMOKE_DIR="$(mktemp -d)"
trap 'rm -rf "$SMOKE_DIR"' EXIT
echo "console.log('ci smoke', 6 * 7)" > "$SMOKE_DIR/smoke.js"
# Guarded capture (a crash inside $() under set -e dies without a message)
# + ci.yml's 5-min smoke ceiling where a timeout tool exists.
if command -v timeout >/dev/null 2>&1; then
  out=$(timeout 300 ./target/debug/oam run "$SMOKE_DIR/smoke.js") || ko "smoke run failed (crash or 5-min hang)"
else
  out=$(./target/debug/oam run "$SMOKE_DIR/smoke.js") || ko "smoke run failed (crashed)"
fi
if [ "$out" = "ci smoke 42" ]; then
  ok "smoke ok"
else
  ko "smoke output unexpected: '$out'"
fi

if [ "$FAST" -eq 0 ]; then
  say "6/8 Conformance (node-differential gate)"
  command -v node >/dev/null 2>&1 || ko "conformance needs node on PATH"
  if cargo run -p xtask -- conformance; then
    ok "conformance clean"
  else
    ko "conformance diverged from Node -- see output above / conformance/scorecard.json"
  fi

  say "7/8 Node-suite (skip-ratchet + pass-floor gate)"
  if cargo run -p xtask -- node-suite; then
    ok "node-suite gate ok (pass-rate in CONFORMANCE-NODE.md)"
  else
    ko "node-suite gate failed (skip-ratchet or pass-floor violation -- see output above)"
  fi
else
  say "6/8 + 7/8 Conformance + node-suite SKIPPED (--fast)"
fi

say "8/8 Unsafe budget (advisory)"
# Advisory for now; becomes a ratchet gate with the SAFETY-comment checker
# (AI-POLICY.md gate 5). oam_engine is the quarantined FFI boundary.
for crate in crates/*/; do
  n=$(grep -rn 'unsafe' "$crate/src" --include='*.rs' 2>/dev/null | wc -l || true)
  echo "  $crate: $n unsafe mentions"
done
ok "unsafe audit reported (advisory only)"

echo ""
ok "All local CI gates passed"
