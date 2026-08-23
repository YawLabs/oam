#!/bin/bash
# =============================================================================
# Tests for the release-orchestration shell scripts.
# =============================================================================
# scripts/ ships the binaries but had no automated verification of any kind:
# ci-local.sh`s Rust gates never looked at it, and nothing in the workspace
# referenced these files. That is how gc-target.sh spent its whole life
# collecting NOTHING on Linux while reporting success -- three silent failures
# at once (mawk interval expressions, a dot-requiring family regex, and a
# `cd ""` no-op), none of which any gate could have caught.
#
# Everything here runs the REAL scripts against real directories. No mocks: the
# bugs that actually bit were external behaviour (mawk`s regex dialect, gcloud`s
# output buffering) differing from what the code assumed, which is precisely
# what a mock would have encoded wrong.
#
# Cost matters, because this is a gate on every push and process spawn on the
# Windows dev box runs ~1s. So: ONE temp root for the whole suite, ONE repo
# skeleton shared by every fixture, and assertions grouped so a single
# gc-target.sh run can answer several questions.
#
# Usage:
#   ./scripts/test-scripts.sh            # run all, non-zero exit on any failure
#   ./scripts/test-scripts.sh -v         # also print each passing assertion
# =============================================================================

set -uo pipefail

VERBOSE=0
[ "${1:-}" = "-v" ] && VERBOSE=1

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

# One temp root, removed on exit. Every fixture used to call mktemp and nothing
# ever cleaned up: a single run leaked 14 directories, and once this became a
# pre-push gate that grew without bound (149 had piled up on the dev box before
# anyone looked). Same shape as ci-local.sh`s CLEANUP_PATHS + EXIT trap.
SUITE_TMP="$(mktemp -d -t oamtest-XXXXXX)"
trap 'rm -rf "$SUITE_TMP"' EXIT

RED='\033[0;31m'; GRN='\033[0;32m'; YEL='\033[0;33m'; CYA='\033[0;36m'; NC='\033[0m'
PASS=0; FAIL=0; SKIP=0; CURRENT=""
group(){ echo -e "\n${CYA}== $* ==${NC}"; }
it(){ CURRENT="$1"; }
pass(){ PASS=$((PASS + 1)); [ "$VERBOSE" = "1" ] && echo -e "  ${GRN}ok${NC} $CURRENT"; return 0; }
fail(){ FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC} $CURRENT"; echo "       $*"; return 0; }
# A third outcome, because "this host cannot run that check" is neither a pass
# nor a failure. Counting it as a pass is how a suite reports coverage it does
# not have; counting it as a failure would block every push on a box that is
# simply missing an optional tool. It prints unconditionally -- a skip nobody
# sees is the same as a green one.
skip(){ SKIP=$((SKIP + 1)); echo -e "  ${YEL}SKIP${NC} $CURRENT"; echo "       $*"; return 0; }
eq(){ [ "$1" = "$2" ] && pass || fail "expected '$2', got '$1'"; }
# Echo the command on failure. Without it every ck failure read "assertion
# failed" with no indication of which predicate in the chain gave way, on a
# suite whose entire purpose is catching silent ones.
ck(){ if "$@"; then pass; else fail "assertion failed: $*"; fi; }

# ONE repo skeleton. gc-target.sh resolves its root from its own location, so a
# fixture needs scripts/ beside target/ -- but it does not need a PRIVATE copy,
# so every fixture shares this one and just resets target/ in between.
ROOT="$SUITE_TMP/repo"
mkdir -p "$ROOT/scripts/lib"
cp "$REPO_DIR/scripts/gc-target.sh"       "$ROOT/scripts/"
cp "$REPO_DIR/scripts/lib/build-locks.sh" "$ROOT/scripts/lib/"

reset_tree(){ rm -rf "$ROOT/target"; }

# plant <tree-relative-dir> <generation> <file>...
# Every file in one call shares a generation timestamp; a higher generation is
# newer. One touch per generation rather than per file, since touch is a spawn.
plant(){
  local dir="$ROOT/$1" gen="$2"; shift 2
  mkdir -p "$dir"
  local f
  for f in "$@"; do echo "artifact" > "$dir/$f"; done
  ( cd "$dir" && touch -t "2026080112$(printf '%02d' "$gen")" "$@" )
}

gc(){ ( cd "$ROOT" && bash scripts/gc-target.sh "$@" >/dev/null 2>&1 ); }
have(){ [ -e "$ROOT/target/debug/deps/$1" ]; }
gone(){ [ ! -e "$ROOT/target/debug/deps/$1" ]; }
count_in(){ ls "$ROOT/$1" 2>/dev/null | wc -l | tr -d ' '; }

# survivors <name>... -- a bare name must SURVIVE, a !-prefixed one must be GONE.
#
# Replaces `ck test -n "$(have X && gone Y && echo y)"`. That idiom collapsed
# the whole chain to "y" or "" BEFORE the assertion ran, so every failure read
# `assertion failed: test -n` -- it could not say which file was wrong, or even
# whether gc-target.sh had run at all. This checks each name and names every one
# that came out on the wrong side, which is the entire point of a suite written
# to catch failures that report success.
survivors(){
  local bad="" f
  for f in "$@"; do
    case "$f" in
      '!'*) have "${f#!}" && bad="$bad ${f#!}(should be gone)" ;;
      *)    have "$f"     || bad="$bad $f(missing)" ;;
    esac
  done
  if [ -z "$bad" ]; then pass; else fail "wrong survivors:$bad"; fi
}

# =============================================================================
group "gc-target.sh -- artifact selection"
# =============================================================================
# One fixture, one gc run, four independent questions. The families are chosen
# so none of them can mask another:
#   oam-<hash>          extensionless unix executables -- the 16GB blind spot
#   grp-<hash> + .d     same stem with and without an extension, which must NOT
#                       share a keep budget
#   libgrp-<hash>.rlib  a third extension
#   mt-<hash>           newest is alphabetically FIRST, oldest LAST, so name
#                       order and mtime order disagree
#   plus four files carrying no 16-hex hash at all
reset_tree
plant target/debug/deps 0 \
  oam-aaaaaaaaaaaaaaa1 grp-bbbbbbbbbbbbbbb1 grp-bbbbbbbbbbbbbbb1.d \
  libgrp-ccccccccccccccc1.rlib mt-fffffffffffffff9
plant target/debug/deps 1 \
  oam-aaaaaaaaaaaaaaa2 grp-bbbbbbbbbbbbbbb2 grp-bbbbbbbbbbbbbbb2.d \
  libgrp-ccccccccccccccc2.rlib mt-000000000000000a
plant target/debug/deps 2 oam-aaaaaaaaaaaaaaa3
plant target/debug/deps 3 \
  build_script_build-notahash.txt README libfoo.rlib oam-short123
gc --keep 1

# The bug that hid for the script`s entire life: no extension, so a family regex
# requiring a dot after the hash could not see these at all.
it "collects extensionless unix executables"
survivors oam-aaaaaaaaaaaaaaa3 '!oam-aaaaaaaaaaaaaaa1' '!oam-aaaaaaaaaaaaaaa2'

# Each extension is its own family. If they merged, keep=1 would leave only one
# of these three rather than all three.
it "groups dotted artifacts per-extension, separately from the bare executable"
survivors grp-bbbbbbbbbbbbbbb2 grp-bbbbbbbbbbbbbbb2.d libgrp-ccccccccccccccc2.rlib \
          '!grp-bbbbbbbbbbbbbbb1' '!grp-bbbbbbbbbbbbbbb1.d'

# The safety invariant of a delete tool: a loosened regex would start removing
# real files and the success message would look identical.
it "never touches files without a 16-hex hash"
survivors build_script_build-notahash.txt README libfoo.rlib oam-short123

# The whole safety argument rests on newest-first ordering. mt-000...a is newer
# but sorts FIRST by name, so a name-ordered prune keeps the wrong one.
it "survivors are the newest by mtime, not by name"
survivors mt-000000000000000a '!mt-fffffffffffffff9'

# --- keep boundaries, one fixture and one run ---------------------------------
reset_tree
plant target/debug/deps 0 xtask-ccccccccccccccc1 e2e-ddddddddddddddd1
plant target/debug/deps 1 xtask-ccccccccccccccc2 e2e-ddddddddddddddd2
plant target/debug/deps 2 xtask-ccccccccccccccc3
plant target/debug/deps 3 xtask-ccccccccccccccc4
gc --keep 2

it "keeps exactly --keep per family and drops the oldest beyond it"
survivors xtask-ccccccccccccccc3 xtask-ccccccccccccccc4 \
          '!xtask-ccccccccccccccc1' '!xtask-ccccccccccccccc2'

it "a family at exactly --keep loses nothing"
survivors e2e-ddddddddddddddd1 e2e-ddddddddddddddd2

# --- every configured tree is collected, not just target/debug ----------------
# TREES carries the win-x64 leg (built WITHOUT --target, so no triple
# subdirectory), the triple-suffixed Windows path, and the mac x64 path. A tree
# added to that list but never pruned is a silent no-op, which is the same class
# of failure as the mawk gap -- so assert more than one tree in a single run.
reset_tree
plant target/debug/deps 0 oam-1111111111111111
plant target/debug/deps 1 oam-2222222222222222
plant target/x64-host/release/deps 0 oam-3333333333333333
plant target/x64-host/release/deps 1 oam-4444444444444444
plant target/x64-host/x86_64-apple-darwin/release/deps 0 oam-5555555555555555
plant target/x64-host/x86_64-apple-darwin/release/deps 1 oam-6666666666666666
gc --keep 1

it "prunes every configured tree in one run, not just target/debug"
# Labelled rather than a bare "111": the counts are compared as one value so a
# single `it` books a single result, but an unpruned tree has to say WHICH tree.
eq "debug=$(count_in target/debug/deps) win-x64=$(count_in target/x64-host/release/deps) mac-x64=$(count_in target/x64-host/x86_64-apple-darwin/release/deps)" \
   "debug=1 win-x64=1 mac-x64=1"

# =============================================================================
group "gc-target.sh -- portability and safety"
# =============================================================================

# THE regression guard. mawk -- Ubuntu`s default awk, and the GCP builder`s --
# matches NOTHING for /-[0-9a-f]{16}/ rather than erroring, so reintroducing an
# interval expression silently returns Linux collection to zero with a green
# run. Asserted on the source because the dev box has no mawk to run under.
# Comments are stripped first: the prose above the awk block necessarily quotes
# the very pattern it warns against.
it "the awk program uses no interval expressions"
INTERVALS="$(sed 's/#.*//' scripts/gc-target.sh | grep -nE '\{[0-9]+\}' || true)"
if [ -n "$INTERVALS" ]; then
  fail "interval expression in live code -- mawk matches nothing for it: $INTERVALS"
else pass; fi

# Actually FORCE each awk via a PATH shim. Looping over awk names without one
# just runs whatever `awk` already resolves to, N times, and proves nothing.
it "selection is identical under every awk installed here"
AWK_TESTED=""
AWK_BAD=0
for AWKBIN in mawk gawk original-awk busybox; do
  AWKPATH="$(command -v "$AWKBIN" 2>/dev/null)" || continue
  [ -n "$AWKPATH" ] || continue
  SHIM="$SUITE_TMP/shim-$AWKBIN"
  mkdir -p "$SHIM"
  # busybox is a multi-call dispatcher: it behaves as awk only when invoked AS
  # awk. `exec busybox "$@"` hands it the gc-target argv with argv[0]=busybox,
  # so it prints its own usage and the leg fails for a reason that has nothing
  # to do with awk dialects. Every other candidate is already an awk.
  case "$AWKBIN" in
    busybox) printf '#!/bin/sh\nexec %s awk "$@"\n' "$AWKPATH" > "$SHIM/awk" ;;
    *)       printf '#!/bin/sh\nexec %s "$@"\n'     "$AWKPATH" > "$SHIM/awk" ;;
  esac
  chmod +x "$SHIM/awk"
  reset_tree
  plant target/debug/deps 0 oam-eeeeeeeeeeeeeee1
  plant target/debug/deps 1 oam-eeeeeeeeeeeeeee2
  ( cd "$ROOT" && PATH="$SHIM:$PATH" bash scripts/gc-target.sh --keep 1 >/dev/null 2>&1 )
  AWK_TESTED="$AWK_TESTED $AWKBIN"
  have oam-eeeeeeeeeeeeeee2 && gone oam-eeeeeeeeeeeeeee1 \
    || { AWK_BAD=1; fail "$AWKBIN selected wrongly: $(ls "$ROOT/target/debug/deps" | tr '\n' ' ')"; }
done
# A wrong selection has already been reported per-awk above; recording a pass
# on top of it booked one `it` as both a failure and a success.
if [ "$AWK_BAD" = "1" ]; then :
elif [ -n "$AWK_TESTED" ]; then pass
else fail "no awk implementation found to test with"; fi

# mawk is the entire reason the guard above exists -- it is Ubuntu's default
# and the GCP builder's, and it matches NOTHING for an interval expression
# rather than erroring. The loop above silently covers whatever happens to be
# installed, so on a host without mawk it reports a pass having never run the
# implementation the 16GB bug came from. Name that gap instead of hiding it.
it "mawk itself was available to test against"
case " $AWK_TESTED " in
  *" mawk "*) pass ;;
  *) skip "mawk is not installed on this host, so the dialect that caused the original bug went untested; the source-level interval check above is the only mawk protection here" ;;
esac

it "--dry-run deletes nothing"
reset_tree
plant target/debug/deps 0 oam-999999999999999a
plant target/debug/deps 1 oam-999999999999999b
plant target/debug/deps 2 oam-999999999999999c
gc --dry-run --keep 1
eq "$(count_in target/debug/deps)" "3"

# The builder`s exact condition: the tree arrives by tar with --exclude=./.git
# and the caller`s cwd is $HOME. `git rev-parse` fails there and `cd ""` is a
# silent no-op, so the script used to prune whatever directory it stood in.
it "resolves its own root from a foreign cwd with no .git"
reset_tree
plant target/debug/deps 0 oam-7777777777777771
plant target/debug/deps 1 oam-7777777777777772
[ -d "$ROOT/.git" ] && fail "fixture unexpectedly has .git"
( cd "$HOME" && bash "$ROOT/scripts/gc-target.sh" --keep 1 >/dev/null 2>&1 )
survivors oam-7777777777777772 '!oam-7777777777777771'

# A freshly imaged builder has no target/ at all; cleanup must not fail a run.
it "an absent target/ is a clean no-op, not an error"
reset_tree
gc --keep 1
eq "$?" "0"

it "an absent deps/ is a clean no-op, not an error"
reset_tree
mkdir -p "$ROOT/target/debug"
gc --keep 1
eq "$?" "0"

# =============================================================================
group "build-remote.sh -- gc dispatch"
# =============================================================================

it "gc is a routable dispatch"
# Capture, then match. Piping build-remote.sh into grep looks right and is not:
# the script exits non-zero on a missing dispatch arg, and under `pipefail` that
# fails the whole pipeline even when grep matched -- the same trap this suite
# checks for in the orchestrator.
USAGE="$(bash scripts/build-remote.sh 2>&1 || true)"
grep -q '| gc |' <<<"$USAGE" && pass || fail "gc missing from the dispatch usage line"

# Cleanup must never fail a run whose artifacts are already built and pulled.
it "run_gc warns and succeeds when gc-target.sh is absent"
BR="$SUITE_TMP/bare/scripts"
mkdir -p "$BR"
cp scripts/build-remote.sh "$BR/"
OUT="$(cd "$SUITE_TMP/bare" && bash scripts/build-remote.sh gc 2>&1)"; RC=$?
if [ "$RC" = "0" ] && grep -q 'skipping target/ reclaim' <<<"$OUT"; then pass
else fail "rc=$RC out=$OUT"; fi

# =============================================================================
group "iap-helpers.sh -- tunnel log parsing"
# =============================================================================
# shellcheck source=lib/iap-helpers.sh
. scripts/lib/iap-helpers.sh

# Captured verbatim from gcloud 577 on 2026-08-22. Note what is NOT here:
# "Listening on port" never arrived, because it is the only line gcloud writes
# to stdout and redirecting stdout to a file block-buffers it in Python. The
# tunnel was fully functional and served SSH for 160s+ regardless.
LOG="$SUITE_TMP/tunnel.log"
cat > "$LOG" <<'FIXTURE'
Picking local unused port [52528].
WARNING:
To increase the performance of the tunnel, consider installing NumPy.
Testing if tunnel connection works.
FIXTURE

it "picked-port parses from a self-test-in-progress log"
eq "$(iap_parse_picked_port "$LOG")" "52528"

it "listening-port is absent while gcloud is still self-testing"
iap_parse_listening_port "$LOG" >/dev/null 2>&1 && fail "reported a bound port that was never logged" || pass

printf 'Listening on port [52528].\n' >> "$LOG"
it "listening-port parses once gcloud has bound the socket"
eq "$(iap_parse_listening_port "$LOG")" "52528"

it "a missing log file is a clean miss, not a crash"
iap_parse_picked_port "$SUITE_TMP/nonexistent.log" >/dev/null 2>&1 && fail "parsed a missing file" || pass

# =============================================================================
group "iap-helpers.sh -- guest boot detection"
# =============================================================================

# Captured verbatim from yaw-linux-builder`s serial console, 2026-08-22.
it "detects the Debian/Ubuntu systemd sshd banner"
sshd_banner_seen "2026-08-22T22:40:44 yaw-linux-builder systemd[1]: Started ssh.service - OpenBSD Secure Shell server." \
  && pass || fail "did not match the real builder banner"

it "does not fire on the pre-sshd portion of a boot"
sshd_banner_seen "systemd[1]: Starting ssh.service - OpenBSD Secure Shell server..." \
  && fail "matched 'Starting', which precedes sshd actually accepting" || pass

it "does not fire on unrelated ssh chatter"
sshd_banner_seen "tailscaled[417]: pm: using backend prefs ssh=true routes=[]" \
  && fail "matched unrelated output containing 'ssh'" || pass

# =============================================================================
group "iap-helpers.sh -- disk headroom thresholds"
# =============================================================================

it "reclaims below the reclaim threshold"; disk_needs_reclaim 14 && pass || fail "14GB should reclaim"
it "does not reclaim at the threshold";    disk_needs_reclaim 20 && fail "20GB should not reclaim" || pass
it "does not reclaim well above it";       disk_needs_reclaim 40 && fail "40GB should not reclaim" || pass
it "aborts below the floor";               disk_below_floor 9 && pass || fail "9GB should abort"
it "proceeds at the floor";                disk_below_floor 10 && fail "10GB should proceed" || pass

# The builder`s real reading the day this was written: low enough to prune, high
# enough to still run. Both predicates must agree on that.
it "10GB free reclaims but still builds"
if disk_needs_reclaim 10 && ! disk_below_floor 10; then pass; else fail "10GB should reclaim and proceed"; fi

# df failing over a flaky tunnel must not silently trigger a prune, and must not
# abort a release either.
it "an unreadable df result triggers neither a prune nor an abort"
if ! disk_needs_reclaim "" && ! disk_below_floor "" \
   && ! disk_needs_reclaim "N/A" && ! disk_below_floor "N/A"; then pass
else fail "empty/non-numeric readings must be inert"; fi

# =============================================================================
echo
# A skip is carried into the summary rather than swallowed: "all N passed" on a
# run that quietly skipped the mawk leg is the same overclaim the suite exists
# to prevent. Skips do not fail the run -- a missing optional tool is not a
# regression -- but they are never invisible.
SKIP_NOTE=""
[ "$SKIP" -gt 0 ] && SKIP_NOTE=", ${SKIP} skipped"
if [ "$FAIL" -gt 0 ]; then
  echo -e "${RED}$FAIL failed${NC}, $PASS passed${SKIP_NOTE}"
  exit 1
fi
if [ "$SKIP" -gt 0 ]; then
  echo -e "${GRN}$PASS passed${NC}${YEL}${SKIP_NOTE}${NC}"
else
  echo -e "${GRN}all $PASS passed${NC}"
fi
