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
#   6. cargo run -p xtask -- conformance    (node-differential gate + builtin
#                                            export-parity ratchet; GATING)
#   7. cargo run -p xtask -- node-suite     (skip-ratchet gate; pass-rate is
#                                            advisory, only a ratchet violation
#                                            fails -- node-compat.yml parity)
#   8. THIRD_PARTY_LICENSES.md drift        (cargo-about; GATING when the tool
#                                            is installed -- see below)
#   9. unsafe budget bidirectional ratchet  (GATING; AI-POLICY.md gate 5 --
#                                            real unsafe_count ceiling per crate
#                                            + // SAFETY: documented_count floor)
#
# Cross-platform coverage moved to the remote legs: scripts/release-local.sh
# gates a release on this script PLUS gate+test+conformance on the GCP Linux
# VM and the tailnet Mac (see scripts/build-remote.sh). For an on-demand
# cross-platform sweep outside a release, use scripts/node-compat-measure.sh.
#
# Usage:
#   ./scripts/ci-local.sh              # full gate (steps 1-9)
#   ./scripts/ci-local.sh --fast       # skip conformance + node-suite + attribution (6-8)
#   ./scripts/ci-local.sh --no-tests   # skip the cargo test run (4)
#
# Env:
#   OAM_SKIP_ATTRIBUTION=1   downgrade step 8 to a warning. Step 8 fails CLOSED,
#                            so an offline box (cargo-about resolves some license
#                            texts over the network) cannot push without this.
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

# A build output cannot be replaced while a process is mid-launch from it, and
# oam leaves detached self-spawned daemons behind that keep re-entering that
# window (mechanism + measurements in the lib's header).
# shellcheck source=lib/build-locks.sh
. scripts/lib/build-locks.sh

# Leftovers under target/debug are, by definition, orphans of an earlier run:
# nothing a human uses long-term runs out of the debug tree. Clearing them
# before a step that relinks is what keeps LNK1104 from surfacing at the worst
# possible moment (mid-release). Scoped to target/debug BY PATH, so a live
# `target/release/oam.exe` typed-cli session is out of reach by construction.
clear_debug_holders() {
  oam_kill_under "$PWD/target/debug"
  oam_reap_parked target/debug
  oam_reap_parked target/debug/deps
}

# ONE EXIT trap for the whole script: a second `trap ... EXIT` silently REPLACES
# the first, so every temp path registers here instead of installing its own.
CLEANUP_PATHS=()
cleanup() {
  if [ ${#CLEANUP_PATHS[@]} -gt 0 ]; then rm -rf "${CLEANUP_PATHS[@]}"; fi
  return 0
}
trap cleanup EXIT

say "1/9 Format (cargo fmt --check)"
if cargo fmt --all --check; then
  ok "fmt clean"
else
  ko "fmt diffs above -- run 'cargo fmt --all' and re-stage"
fi

say "2/9 Clippy (-D warnings)"
# -D warnings via clippy args, NOT RUSTFLAGS: a global RUSTFLAGS would
# fingerprint-poison the cargo cache against the plain build/test steps.
if cargo clippy --workspace --all-targets -- -D warnings; then
  ok "clippy clean"
else
  ko "clippy warnings above"
fi

say "3/9 Build (cargo build --workspace)"
clear_debug_holders
# Fallback for a holder the kill could not reach (elevated process, AV handle):
# renaming works where deleting does not. Parking the deps-stage file is NOT
# free, though: deleting deps/oam.exe dirties cargo's bin-unit fingerprint, so
# the build below relinks the binary even on an otherwise-fresh tree (measured
# ~7.6s vs a 0.68s no-op in debug; the release scripts pay more). That cost is
# accepted -- a locked binary fails the link outright, which is worse than a
# relink.
#
# BOTH paths get parked: cargo's link writes deps/oam.exe FIRST and only
# promotes it to debug/oam.exe on success, so the LNK1104 deny window lands
# on the deps-stage file. An orphan test binary (a previous run's `cargo
# test` left mapped) is the usual holder there -- same deny window, same
# fix.
oam_park_file target/debug/oam.exe \
  || ko "target/debug/oam.exe is locked and could not be parked -- find the holder with: Get-CimInstance Win32_Process | Where-Object { \$_.ExecutablePath -like '*target*' } | Select-Object ProcessId,ExecutablePath"
oam_park_file target/debug/deps/oam.exe \
  || ko "target/debug/deps/oam.exe is locked and could not be parked -- find the holder with: Get-CimInstance Win32_Process | Where-Object { \$_.ExecutablePath -like '*target*' } | Select-Object ProcessId,ExecutablePath"
if cargo build --workspace; then
  ok "build ok"
else
  ko "build failed"
fi

if [ "$SKIP_TESTS" -eq 0 ]; then
  say "4/9 Tests (cargo test --workspace)"
  # ci.yml installed tsgo per matrix leg (continue-on-error); locally just
  # surface the gap -- the oam-check differential tests self-skip without it.
  command -v tsgo >/dev/null 2>&1 \
    || warn "tsgo not on PATH -- oam-check tests will self-skip (npm install -g @typescript/native-preview)"
  # The harnesses cargo is about to relink are exactly the images a previous
  # run's orphans are still executing.
  clear_debug_holders
  # ci.yml bounded a hung test at 15 min (the deadlocked/lingering test
  # class). Git Bash ships coreutils timeout; macOS needs brew coreutils
  # (gtimeout) -- unbounded as the last resort.
  #
  # -k 30 matters as much as the ceiling: plain `timeout` TERMs CARGO only.
  # The test binaries cargo already launched, and the detached daemons THOSE
  # spawned, sit in other process groups and survive -- which is how one hung
  # run poisons the NEXT run's link step. KILL after 30s, then reap by image
  # path below whatever the outcome was.
  test_status=0
  if command -v timeout >/dev/null 2>&1; then
    timeout -k 30 900 cargo test --workspace || test_status=$?
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout -k 30 900 cargo test --workspace || test_status=$?
  else
    cargo test --workspace || test_status=$?
  fi
  # Unconditional, and BEFORE the failure exit: a failed or timed-out run leaks
  # more orphans than a clean one, and the smoke step below executes
  # target/debug/oam.
  clear_debug_holders
  [ "$test_status" -eq 0 ] \
    || ko "tests failed (status $test_status -- 124 means it hit the 15-min hang ceiling)"
  ok "tests passed"
else
  say "4/9 Tests SKIPPED (--no-tests)"
fi

say "5/9 Smoke (oam run)"
SMOKE_DIR="$(mktemp -d)"
CLEANUP_PATHS+=("$SMOKE_DIR")
echo "console.log('ci smoke', 6 * 7)" > "$SMOKE_DIR/smoke.js"
# Guarded capture (a crash inside $() under set -e dies without a message)
# + ci.yml's 5-min smoke ceiling where a timeout tool exists.
#
# OAM_DAEMON_IDLE_MS: without it the type-check daemon this run may spawn idles
# for THIRTY MINUTES (crates/oam_ts/src/daemon.rs), detached, holding
# target/debug/oam.exe -- long enough to break the next gate's link step. The
# e2e suite already pins 45s for the same reason; the smoke step never needs a
# warm daemon at all.
if command -v timeout >/dev/null 2>&1; then
  out=$(OAM_DAEMON_IDLE_MS=1500 timeout 300 ./target/debug/oam run "$SMOKE_DIR/smoke.js") || ko "smoke run failed (crash or 5-min hang)"
else
  out=$(OAM_DAEMON_IDLE_MS=1500 ./target/debug/oam run "$SMOKE_DIR/smoke.js") || ko "smoke run failed (crashed)"
fi
if [ "$out" = "ci smoke 42" ]; then
  ok "smoke ok"
else
  ko "smoke output unexpected: '$out'"
fi

if [ "$FAST" -eq 0 ]; then
  say "6/9 Conformance (node-differential gate)"
  command -v node >/dev/null 2>&1 || ko "conformance needs node on PATH"
  if cargo run -p xtask -- conformance; then
    ok "conformance clean"
  else
    ko "conformance diverged from Node -- see output above / conformance/scorecard.json"
  fi

  say "7/9 Node-suite (skip-ratchet + pass-floor gate)"
  if cargo run -p xtask -- node-suite; then
    ok "node-suite gate ok (pass-rate in CONFORMANCE-NODE.md)"
  else
    ko "node-suite gate failed (skip-ratchet or pass-floor violation -- see output above)"
  fi

  say "8/9 Attribution (THIRD_PARTY_LICENSES drift)"
  # Every released binary statically links ~380 crates, so their notices have to
  # travel with it. Cargo.lock changes silently invalidate the checked-in file;
  # this catches that.
  #
  # Generate to a TEMP file OUTSIDE the repo and diff -- never regenerate in
  # place. The release preflight demands a porcelain-clean tree, so a gate that
  # rewrites (or drops an untracked file next to) a tracked artifact mid-run
  # makes the NEXT release fail its clean-tree check. That is exactly the
  # self-dirtying trap the conformance stamps hit, and it needed a restore step
  # in release-local.sh to dig out of.
  if ! command -v cargo-about >/dev/null 2>&1; then
    warn "cargo-about not installed -- attribution drift unchecked (cargo install cargo-about --locked --features cli)"
  elif [ "${OAM_SKIP_ATTRIBUTION:-}" = "1" ]; then
    warn "OAM_SKIP_ATTRIBUTION=1 -- attribution drift unchecked"
  else
    attr_tmp="$(mktemp)"; CLEANUP_PATHS+=("$attr_tmp")
    attr_err="$(mktemp)"; CLEANUP_PATHS+=("$attr_err")
    if cargo about generate about.hbs -o "$attr_tmp" 2>"$attr_err" >/dev/null; then
      # Compare CR-insensitively. .gitattributes marks this file `-text` so the
      # stored bytes match a fresh generate, but that only holds for checkouts
      # made after that rule landed -- an older clone, or a stray core.autocrlf,
      # would otherwise fail the gate over line endings that carry no meaning.
      if diff -q <(tr -d '\r' <"$attr_tmp") <(tr -d '\r' <THIRD_PARTY_LICENSES.md) >/dev/null 2>&1; then
        ok "THIRD_PARTY_LICENSES.md matches the dependency graph"
      else
        ko "THIRD_PARTY_LICENSES.md is stale -- regenerate with: cargo about generate about.hbs -o THIRD_PARTY_LICENSES.md"
      fi
    else
      # FAIL CLOSED. A non-zero exit is NOT necessarily infrastructure: an
      # unaccepted or unresolvable license exits 1 too, and that is precisely
      # the compliance violation this gate exists to catch (verified: dropping
      # "ISC" from about.toml's accepted list -- which makes ring's conjunctive
      # "Apache-2.0 AND ISC" unsatisfiable -- exits 1). Treating every non-zero
      # as a warning would wave that through. Offline boxes use the env escape.
      sed 's/^/      /' "$attr_err" >&2
      ko "cargo about generate failed (see above) -- a rejected license fails here too; if this is only network, re-run or set OAM_SKIP_ATTRIBUTION=1"
    fi
  fi
else
  say "6/9 + 7/9 + 8/9 Conformance + node-suite + attribution SKIPPED (--fast)"
fi

say "9/9 Unsafe budget (bidirectional ratchet -- AI-POLICY.md gate 5)"
# GATING. Replaces the old advisory `grep -c unsafe` loop: xtask lexes out the
# noise that grep counted (#[unsafe(...)] attributes, // SAFETY: comments, and
# unsafe extern "C" fn(...) POINTER TYPES) and ratchets the real count. A crate
# above its unsafe_count ceiling, or below its documented_count floor -- OR any
# count strictly BETTER than the committed baseline (which must be re-blessed) --
# fails here. Baseline: conformance/unsafe-budget.json; regen with
# `cargo run -p xtask -- unsafe-budget --regen`.
if cargo run -p xtask -- unsafe-budget; then
  ok "unsafe budget within ceilings / at-or-above floors"
else
  ko "unsafe-budget ratchet violated (see above) -- fix, or re-bless with 'cargo run -p xtask -- unsafe-budget --regen'"
fi

echo ""
ok "All local CI gates passed"
