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
# Guarded, and not decoration: every path below is repo-relative, so a failed cd
# would run the whole suite against whatever directory the caller stood in --
# which is a variant of the `cd ""` no-op that let gc-target.sh prune the wrong
# tree for its entire life.
cd "$REPO_DIR" || { echo "cannot cd to $REPO_DIR" >&2; exit 1; }

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

# Same contract as survivors, but paths are relative to the fixture ROOT rather
# than to target/debug/deps -- needed by anything asserting across trees, or on
# incremental/ and the parked copies, none of which live under deps/.
tree_survivors(){
  local bad="" f
  for f in "$@"; do
    case "$f" in
      '!'*) [ -e "$ROOT/${f#!}" ] && bad="$bad ${f#!}(should be gone)" ;;
      *)    [ -e "$ROOT/$f" ]     || bad="$bad $f(missing)" ;;
    esac
  done
  if [ -z "$bad" ]; then pass; else fail "wrong survivors:$bad"; fi
}

# gc() swallows output, which is right for the selection tests and wrong for the
# ones whose whole claim is that the script SAID something -- a degrade path
# that prunes nothing is only distinguishable from a silent no-op by its warning.
gc_out(){ ( cd "$ROOT" && bash scripts/gc-target.sh "$@" 2>&1 ); }

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
plant target/release/deps 0 oam-aaaa000000000001
plant target/release/deps 1 oam-aaaa000000000002
plant target/x64-host/release/deps 0 oam-3333333333333333
plant target/x64-host/release/deps 1 oam-4444444444444444
plant target/x64-host/x86_64-pc-windows-msvc/release/deps 0 oam-bbbb000000000001
plant target/x64-host/x86_64-pc-windows-msvc/release/deps 1 oam-bbbb000000000002
plant target/x64-host/x86_64-apple-darwin/release/deps 0 oam-5555555555555555
plant target/x64-host/x86_64-apple-darwin/release/deps 1 oam-6666666666666666
gc --keep 1

# All FIVE entries in TREES, not three. target/release and the triple-suffixed
# msvc path were the two nobody exercised, and the msvc one exists specifically
# to collect trees left by a pre-0.11 release -- a legacy entry that nothing
# runs against is exactly how a list entry becomes a silent no-op.
it "prunes every configured tree in one run, not just target/debug"
# Labelled rather than a bare count string: the counts are compared as one value
# so a single `it` books a single result, but an unpruned tree has to say WHICH.
eq "debug=$(count_in target/debug/deps) release=$(count_in target/release/deps) win-x64=$(count_in target/x64-host/release/deps) msvc=$(count_in target/x64-host/x86_64-pc-windows-msvc/release/deps) mac-x64=$(count_in target/x64-host/x86_64-apple-darwin/release/deps)" \
   "debug=1 release=1 win-x64=1 msvc=1 mac-x64=1"

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

# The builder`s exact condition: the tree arrives as a tarball of the files git
# tracks, which never carries .git itself, and the caller`s cwd is $HOME. `git
# rev-parse` fails there and `cd ""` is a silent no-op, so the script used to
# prune whatever directory it stood in.
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
group "gc-target.sh -- incremental, parked copies, and argv"
# =============================================================================

# incremental/ is the LARGEST reclaim this script makes -- 53.8GB of the 113GB
# measured on the dev box, against 48.2GB for deps/ -- and nothing exercised it.
# It is also the only reclaim with an opt-out, so a flag whose sense inverted
# would either stop reclaiming the bulk of the tree or destroy a cache the
# caller asked to keep, and every deps/ assertion above would stay green.
reset_tree
plant target/debug/deps 0 oam-cccc000000000001
plant target/debug/deps 1 oam-cccc000000000002
mkdir -p "$ROOT/target/debug/incremental/oam-abc123/s-xyz"
echo cache > "$ROOT/target/debug/incremental/oam-abc123/s-xyz/dep-graph.bin"
gc --keep 1

it "deletes incremental/ wholesale by default"
tree_survivors '!target/debug/incremental' target/debug/deps/oam-cccc000000000002

reset_tree
plant target/debug/deps 0 oam-dddd000000000001
plant target/debug/deps 1 oam-dddd000000000002
mkdir -p "$ROOT/target/debug/incremental/oam-abc123"
echo cache > "$ROOT/target/debug/incremental/oam-abc123/dep-graph.bin"
gc --keep-incremental --keep 1

it "--keep-incremental spares the cache and still prunes deps/"
tree_survivors target/debug/incremental/oam-abc123/dep-graph.bin \
               target/debug/deps/oam-dddd000000000002 \
               '!target/debug/deps/oam-dddd000000000001'

# build-locks.sh parks a file it cannot unlink as <name>.inuse-<pid>. Reaping
# those is one of the three jobs this script's own header claims, and it was the
# only one with no coverage -- a parked copy nothing collects is a slow leak of
# exactly the multi-GB artifacts the script exists to remove. Both fixtures are
# deliberately hash-free, so only the reap can account for their removal; a
# 16-hex name would also be a family-prune candidate and blur which ran.
reset_tree
plant target/debug/deps 0 oam-eeee000000000001
echo parked > "$ROOT/target/debug/oam.exe.inuse-4242"
echo parked > "$ROOT/target/debug/deps/libfoo.rlib.inuse-4242"
gc --keep 1

it "reaps parked .inuse-<pid> copies from both the tree and its deps/"
tree_survivors '!target/debug/oam.exe.inuse-4242' \
               '!target/debug/deps/libfoo.rlib.inuse-4242' \
               target/debug/deps/oam-eeee000000000001

# --- argv guards --------------------------------------------------------------
# Asserted on exit status AND on the tree. A validation that drifted below the
# prune loop would still exit non-zero, having already deleted -- which is the
# failure the exit code alone cannot see.
#
# Each of these re-plants rather than sharing one fixture, and that is worth the
# extra spawns: every assertion here is "nothing was deleted", so the first one
# that DOES delete leaves the rest asserting against a tree it already emptied.
# Removing the keep>=1 guard produced four failures for one bug before this;
# now the failing guard is the only thing that reports.
argv_fixture(){
  reset_tree
  plant target/debug/deps 0 oam-ffff000000000001
  plant target/debug/deps 1 oam-ffff000000000002
}
argv_intact(){ tree_survivors target/debug/deps/oam-ffff000000000001 target/debug/deps/oam-ffff000000000002; }

it "--keep 0 is refused, and nothing is deleted"
argv_fixture
gc --keep 0 && fail "--keep 0 should exit non-zero" || argv_intact

it "a non-numeric --keep is refused, and nothing is deleted"
argv_fixture
gc --keep abc && fail "--keep abc should exit non-zero" || argv_intact

# gc is dispatched REMOTELY by build-remote.sh, so a flag that fell through to a
# prune instead of an error would run against a live builder's tree.
it "an unknown flag is refused, and nothing is deleted"
argv_fixture
gc --bogus && fail "an unknown flag should exit non-zero" || argv_intact

it "-h prints usage, exits 0, and prunes nothing"
argv_fixture
HELP_OUT="$(gc_out -h)"
if [ -n "$HELP_OUT" ] && [ "$(count_in target/debug/deps)" = "2" ]; then pass
else fail "help printed ${#HELP_OUT} chars and left $(count_in target/debug/deps) of 2 files"; fi

# --- the BSD/macOS leg --------------------------------------------------------
# `find -printf` is GNU-only. BSD find yields nothing for it, so without the
# probe the per-family prune would collect zero while reporting a clean run --
# the precise shape of the mawk bug this suite exists for. The contract is to
# degrade LOUDLY: skip the deps prune, say why, and still reclaim incremental/.
it "a find without -printf skips the deps prune loudly, reclaiming the rest"
NOPRINTF="$SUITE_TMP/shim-noprintf"
mkdir -p "$NOPRINTF"
REAL_FIND="$(command -v find)"
{
  echo '#!/bin/sh'
  echo 'for a in "$@"; do [ "$a" = "-printf" ] && exit 1; done'
  printf 'exec %s "$@"\n' "$REAL_FIND"
} > "$NOPRINTF/find"
chmod +x "$NOPRINTF/find"
reset_tree
plant target/debug/deps 0 oam-9999000000000001
plant target/debug/deps 1 oam-9999000000000002
mkdir -p "$ROOT/target/debug/incremental"
echo cache > "$ROOT/target/debug/incremental/dep-graph.bin"
DEGRADE_OUT="$( cd "$ROOT" && PATH="$NOPRINTF:$PATH" bash scripts/gc-target.sh --keep 1 2>&1 )"
if grep -q 'needs GNU find' <<<"$DEGRADE_OUT" \
   && have oam-9999000000000001 && have oam-9999000000000002 \
   && [ ! -e "$ROOT/target/debug/incremental" ]; then pass
else
  fail "warned=$(grep -c 'needs GNU find' <<<"$DEGRADE_OUT") deps=$(count_in target/debug/deps)/2 incremental=$([ -e "$ROOT/target/debug/incremental" ] && echo present || echo reclaimed)"
fi

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

# The same guard on the OTHER parser, which is the authoritative one: a bound
# port is what proves the tunnel is up, so this is the function whose failure
# means "do not proceed". Only picked_port's missing-file path was covered.
it "a missing log file is a clean miss for the listening-port parser too"
iap_parse_listening_port "$SUITE_TMP/nonexistent.log" >/dev/null 2>&1 && fail "parsed a missing file" || pass

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

# The regex carries four alternations and only the Debian one was exercised.
# Portability across distro unit namings is the entire reason the other three
# are there, and narrowing the pattern turns cold-VM boot detection into a
# silent 180s stall on every start rather than a visible failure.
it "matches the RHEL-family sshd.service unit naming"
sshd_banner_seen "systemd[1]: Started sshd.service - OpenSSH server daemon." \
  && pass || fail "did not match the RHEL-family unit line"

it "matches the bare OpenBSD and OpenSSH unit descriptions"
if sshd_banner_seen "systemd[1]: Started OpenBSD Secure Shell server." \
   && sshd_banner_seen "systemd[1]: Started OpenSSH Daemon."; then pass
else fail "a distro naming the regex claims to cover did not match"; fi

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

# Both thresholds are documented env knobs that decide whether a release
# prunes, proceeds, or aborts, and every assertion above runs against the
# hardcoded 20/10 defaults. They are resolved at SOURCE time, so an override
# only takes effect in a fresh shell -- assigning after the fact would silently
# test nothing, which is why these go through `bash -c`.
it "OAM_DISK_RECLAIM_GB moves the reclaim threshold"
eq "$(OAM_DISK_RECLAIM_GB=50 bash -c '. scripts/lib/iap-helpers.sh; disk_needs_reclaim 40 && echo reclaim || echo skip')" \
   "reclaim"

it "OAM_DISK_MIN_GB moves the abort floor"
eq "$(OAM_DISK_MIN_GB=50 bash -c '. scripts/lib/iap-helpers.sh; disk_below_floor 40 && echo abort || echo proceed')" \
   "abort"

# =============================================================================
group "src-sync.sh -- source tarball ceiling"
# =============================================================================
# shellcheck source=lib/src-sync.sh
. scripts/lib/src-sync.sh

# The ceiling exists because on 2026-08-31 a release packed 11.6 GB and pushed
# it over an IAP tunnel for 35 minutes before dying. 200MB default, real tree
# ~1.6MB.
it "passes a realistically-sized tarball"; src_tarball_over_ceiling 2000000 && fail "2MB should pass" || pass
it "passes right at the ceiling";         src_tarball_over_ceiling 209715200 && fail "200MB should pass" || pass
it "trips one byte over the ceiling";     src_tarball_over_ceiling 209715201 && pass || fail "200MB+1 should trip"
it "trips on the incident's 11.6GB";      src_tarball_over_ceiling 11642897270 && pass || fail "11.6GB must trip"

# Same inertness rule as the disk predicates: a failed `wc -c` must not abort a
# release on a number nobody measured.
it "an unreadable size triggers neither an abort nor a false pass"
if ! src_tarball_over_ceiling "" && ! src_tarball_over_ceiling "N/A"; then pass
else fail "empty/non-numeric readings must be inert"; fi

# Resolved at SOURCE time, so an override only takes effect in a fresh shell.
it "OAM_SRC_TARBALL_MAX_MB moves the ceiling"
eq "$(OAM_SRC_TARBALL_MAX_MB=1 bash -c '. scripts/lib/src-sync.sh; src_tarball_over_ceiling 2000000 && echo over || echo under')" \
   "over"

# =============================================================================
group "src-sync.sh -- what the sync actually ships"
# =============================================================================
# Runs against the REAL repo, because the property under test is a fact about
# THIS working tree, and a synthetic fixture would just re-encode the assumption
# that broke. Cheap: one `git ls-files`, no tarball.
SRC_LIST="$SUITE_TMP/src-list.txt"
src_file_names(){ ( cd "$REPO_DIR" && git ls-files -z --cached --others --exclude-standard | tr '\0' '\n' ); }
src_file_names > "$SRC_LIST" 2>/dev/null

it "the file list is non-empty (git answered at all)"
if [ -s "$SRC_LIST" ]; then pass; else fail "git ls-files produced nothing -- every assertion below would vacuously pass"; fi

# THE regression guard. Claude Code's agent worktrees are full checkouts of this
# repo, each carrying its own target/; eight of them is what the anchored
# `--exclude=./target` missed.
it "ships no .claude/ agent worktree, and no build output of any kind"
OFFENDERS="$(grep -nE '(^|/)(target|node_modules|dist)/|^\.claude/|^\.git/' "$SRC_LIST" \
  | grep -v 'conformance/vendor/node/test/fixtures/.*/node_modules/' | head -5)"
if [ -z "$OFFENDERS" ]; then pass; else fail "build output or agent scratch in the sync list: $OFFENDERS"; fi

# The other half of that guard, and the reason the naive fix (dropping the `./`
# anchors) is wrong: an unanchored --exclude=node_modules also deletes these
# four TRACKED fixture files, which the node-suite's warning_node_modules case
# asserts on. Verified on GNU tar 1.35, 2026-08-31.
it "still ships the tracked vendored node_modules conformance fixtures"
eq "$(grep -c 'conformance/vendor/node/test/fixtures/warning_node_modules/node_modules/' "$SRC_LIST")" "4"

# A tree missing any of these does not build on the far side at all.
it "ships the inputs a remote build cannot start without"
MISSING=""
for p in Cargo.toml Cargo.lock rust-toolchain.toml .cargo/config.toml scripts/build-remote.sh; do
  grep -qxF "$p" "$SRC_LIST" || MISSING="$MISSING $p"
done
if [ -z "$MISSING" ]; then pass; else fail "absent from the sync list:$MISSING"; fi

# =============================================================================
group "ci-local.sh -- miri gate verdicts"
# =============================================================================
# The gate's own decision logic, which until now was the one part of ci-local.sh
# nothing exercised -- this suite's header said as much. Step 12 decides from
# (exit status, output text), and every way that decision can rot makes the gate
# QUIETER: libtest exits 0 on a run that executed nothing, and a held_* case
# that dies for an unrelated reason still exits non-zero.
#
# Driven with CAPTURED miri output rather than a real run, on purpose. Requiring
# nightly+miri would mean the logic is checked only on the boxes where step 12
# already runs -- which is the opposite of what is wanted: the classifier is
# exactly what a box WITHOUT miri cannot otherwise verify.
# shellcheck source=lib/miri-gate.sh
. scripts/lib/miri-gate.sh

# Shape captured from `cargo miri test -p oam_aliasing_model`: two summary lines,
# because cargo emits one per test binary plus one for doctests. The count is
# their SUM, so a fixture with only one line would not exercise that.
MIRI_PASS_OUT="$(cat <<'FIXTURE'
   Compiling oam_aliasing_model v0.12.1
running 9 tests
test tests::two_shared_scopes_coexist ... ok
test tests::ref_entry_rejects_a_deleted_handle ... ok

test result: ok. 6 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 41.20s

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
FIXTURE
)"

# What a filter matching NOTHING looks like. Verified against real libtest:
# `cargo test -p oam_aliasing_model -- --ignored no_such_name` prints this and
# exits ZERO. This is the fixture the whole "not merely a non-zero exit"
# argument rests on.
MIRI_EMPTY_OUT="$(cat <<'FIXTURE'
running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.01s
FIXTURE
)"

MIRI_TWO_RAN_OUT="$(cat <<'FIXTURE'
running 2 tests

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 7 filtered out; finished in 3.10s
FIXTURE
)"

MIRI_UB_OUT="$(cat <<'FIXTURE'
running 1 test
error: Undefined Behavior: attempting a read access using <2841> at alloc1[0x0],
 but that tag does not exist in the borrow stack for this location
   = help: this indicates a potential bug in the program
test tests::held_two_exclusive_scopes_is_ub ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 8 filtered out; finished in 1.90s
FIXTURE
)"

# The verdict the exit status alone cannot tell apart from the one above: still
# a failure, but for a reason that has nothing to do with the class it models.
MIRI_PANIC_OUT="$(cat <<'FIXTURE'
running 1 test
thread 'tests::held_two_exclusive_scopes_is_ub' panicked at crates/oam_aliasing_model/src/lib.rs:388:9:
assertion `left == right` failed
test tests::held_two_exclusive_scopes_is_ub ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 8 filtered out; finished in 0.40s
FIXTURE
)"

MIRI_NOSUMMARY_OUT="$(cat <<'FIXTURE'
error: the compiler unexpectedly panicked. this is a bug.
error: could not compile `oam_aliasing_model` (lib test)
FIXTURE
)"

it "the executed-model count sums every libtest summary in a run"
eq "$(miri_executed_count "$MIRI_PASS_OUT")" "6"

it "a run with no libtest summary at all reports no count"
miri_executed_count "$MIRI_NOSUMMARY_OUT" >/dev/null 2>&1 \
  && fail "counted models in output that has no summary line" || pass

it "the full model run passing is the only 'pass' verdict"
eq "$(miri_current_verdict 0 "$MIRI_PASS_OUT")" "pass"

it "a non-zero exit means miri rejected a current-design model"
eq "$(miri_current_verdict 1 "$MIRI_UB_OUT")" "rejected"

# THE vacuous pass. Nothing about the exit status distinguishes an empty run
# from a clean one, so a crate whose models were deleted or wholly #[ignore]d
# would have reported a green gate forever.
it "an exit-0 run that executed NOTHING is not-exercised, not a pass"
eq "$(miri_current_verdict 0 "$MIRI_EMPTY_OUT")" "not-exercised"

it "an exit-0 run below the model floor is not-exercised too"
eq "$(miri_current_verdict 0 "$MIRI_TWO_RAN_OUT")" "not-exercised"

it "an exit-0 run that never reached the tests is unparsable, not a pass"
eq "$(miri_current_verdict 0 "$MIRI_NOSUMMARY_OUT")" "unparsable"

it "a held case failing ON a UB diagnosis is the passing verdict"
eq "$(miri_held_verdict 1 "$MIRI_UB_OUT")" "rejected-ub"

it "a held case miri ACCEPTS is a lost tooth, not a pass"
eq "$(miri_held_verdict 0 "$MIRI_PASS_OUT")" "accepted"

# Renaming a held_* case, deleting it, or dropping its #[ignore] all land here:
# `--ignored <name>` matches nothing and libtest exits 0. Reported apart from
# `accepted` because "miri changed its mind" and "the case is gone" need
# different fixes.
it "a held case whose filter matched nothing is missing, not accepted"
eq "$(miri_held_verdict 0 "$MIRI_EMPTY_OUT")" "missing"

it "a held case failing WITHOUT a UB diagnosis is not a rejection"
eq "$(miri_held_verdict 1 "$MIRI_PANIC_OUT")" "failed-without-ub"

# #95: `printf "$big" | grep -q` returns 141 under pipefail, because grep -q
# exits at its first match and SIGPIPEs the writer -- so the BIGGER the UB
# report, the more likely the gate was to read it as "no UB reported". The
# classifier matches with bash builtins for this reason; this proves it holds at
# a size that would have tripped the old shape.
#
# Padded by DOUBLING rather than appending in a loop: bash string append is
# quadratic, and 4000 rounds of it cost 6s on the dev box -- real money on a
# suite that gates every push. Twelve doublings cost nothing and produce a
# bigger report.
it "a very large UB report is still classified as a rejection (no SIGPIPE)"
UB_PAD="   = note: inside oam_aliasing_model::tests::held_two_exclusive_scopes_is_ub
"
while [ "${#UB_PAD}" -lt 200000 ]; do UB_PAD="$UB_PAD$UB_PAD"; done
BIG_UB="$MIRI_UB_OUT
$UB_PAD"
eq "$(miri_held_verdict 1 "$BIG_UB")" "rejected-ub"

# =============================================================================
group "ci-local.sh -- gate wiring"
# =============================================================================
# Source-level assertions, in the same spirit as the interval-expression guard
# on gc-target.sh: these catch a gate step that stopped covering what its own
# comments claim, on hosts that cannot run the step at all.

MODEL_SRC="crates/oam_aliasing_model/src/lib.rs"

# "<fn name> ignored" / "<fn name> active" for every #[test] in the model crate.
# No interval expressions -- mawk matches nothing for those (see above).
MODEL_INDEX="$(awk '
  /^[[:space:]]*#\[test\]/          { intest = 1; ign = 0; next }
  intest && /^[[:space:]]*#\[ignore/ { ign = 1; next }
  intest && /^[[:space:]]*fn [A-Za-z0-9_]+/ {
    name = $0
    sub(/^[[:space:]]*fn /, "", name)
    sub(/\(.*$/, "", name)
    if (ign) { print name " ignored" } else { print name " active" }
    intest = 0
  }
' "$MODEL_SRC")"

# The loop in step 12 iterates OAM_MIRI_HELD_CASES. A case renamed in the crate
# and not here does not fail loudly where miri runs -- `--ignored <old name>`
# matches nothing, which the classifier now calls `missing` -- and on every box
# without miri it would not be noticed at all. This is the check that runs
# everywhere.
it "every held_* case the gate names exists and is still #[ignore]d"
HELD_BAD=""
for hc in "${OAM_MIRI_HELD_CASES[@]}"; do
  if [[ $'\n'"$MODEL_INDEX"$'\n' == *$'\n'"$hc ignored"$'\n'* ]]; then continue; fi
  if [[ $'\n'"$MODEL_INDEX"$'\n' == *$'\n'"$hc active"$'\n'* ]]; then
    HELD_BAD="$HELD_BAD $hc(no longer #[ignore]d, so --ignored skips it)"
  else
    HELD_BAD="$HELD_BAD $hc(no such #[test] fn in $MODEL_SRC)"
  fi
done
if [ -z "$HELD_BAD" ]; then pass; else fail "held cases out of sync:$HELD_BAD"; fi

# Bidirectional, like the unsafe budget: fewer models than the floor means the
# gate's first half went vacuous, more means models were added and the floor is
# stale-low, and both need a human to look.
it "the model floor matches the number of current-design models in the crate"
ACTIVE_MODELS=0
while IFS= read -r mline; do
  case "$mline" in *" active") ACTIVE_MODELS=$((ACTIVE_MODELS + 1)) ;; esac
done <<< "$MODEL_INDEX"
if [ "$ACTIVE_MODELS" = "$OAM_MIRI_CURRENT_MIN" ]; then pass
else fail "$MODEL_SRC has $ACTIVE_MODELS non-ignored models, OAM_MIRI_CURRENT_MIN is $OAM_MIRI_CURRENT_MIN -- re-bless it in scripts/lib/miri-gate.sh"; fi

# Guards the extraction itself: an inline re-implementation of either verdict
# would be untested again, and this suite would keep reporting the same green.
it "the miri step decides through the verdict functions, not inline"
WIRE_BAD=""
for want in miri_current_verdict miri_held_verdict 'OAM_MIRI_HELD_CASES\[@\]'; do
  grep -q -- "$want" scripts/ci-local.sh || WIRE_BAD="$WIRE_BAD $want"
done
if [ -z "$WIRE_BAD" ]; then pass; else fail "ci-local.sh no longer references:$WIRE_BAD"; fi

# #90 shipped a whole second build configuration -- oam_engine without `napi`,
# oam_cli without its passthrough -- that was verified by hand once and then had
# no coverage anywhere: `no-default-features` appeared in no script, no test and
# no workflow. A #[cfg(feature = "napi")] boundary rots silently, so all three
# verbs have to stay in the gate.
it "the gate builds, lints and tests the --no-default-features configuration"
NDF_MISSING=""
grep -qE 'cargo build .*--no-default-features'  scripts/ci-local.sh || NDF_MISSING="$NDF_MISSING build"
grep -qE 'cargo clippy .*--no-default-features' scripts/ci-local.sh || NDF_MISSING="$NDF_MISSING clippy"
grep -qE 'cargo_test .*--no-default-features'   scripts/ci-local.sh || NDF_MISSING="$NDF_MISSING test"
if [ -z "$NDF_MISSING" ]; then pass; else fail "the napi-off gate no longer covers:$NDF_MISSING"; fi

# ci-local.sh is a pre-push hook: a syntax error in it fails every push with a
# bash parse error rather than a gate verdict, and nothing else here parses it.
it "ci-local.sh and the libs it sources parse"
PARSE_BAD=""
for s in scripts/ci-local.sh scripts/bump-taps.sh scripts/release-local.sh \
         scripts/lib/miri-gate.sh scripts/lib/build-locks.sh \
         scripts/lib/crt-linkage.sh scripts/lib/iap-helpers.sh \
         scripts/lib/tap-verify.sh; do
  bash -n "$s" 2>/dev/null || PARSE_BAD="$PARSE_BAD $s"
done
if [ -z "$PARSE_BAD" ]; then pass; else fail "syntax errors in:$PARSE_BAD"; fi

# --- the npm launcher channel -------------------------------------------------
# npm/ ships the `oamjs` launcher and its five per-platform binary packages, and
# NO gate touched the tree: `sync-packages.mjs --check` and the five node --test
# files were declared in npm/package.json and called by nothing. A green
# ci-local.sh therefore said nothing at all about that channel -- the same shape
# as gc-target.sh reporting success while collecting nothing.

it "the local gate runs both of npm/'s own checks"
NPM_GATE_MISSING=""
grep -q 'node sync-packages.mjs --check'   scripts/ci-local.sh || NPM_GATE_MISSING="$NPM_GATE_MISSING drift-check"
grep -q 'node --test "test/\*.test.mjs"'   scripts/ci-local.sh || NPM_GATE_MISSING="$NPM_GATE_MISSING launcher-tests"
if [ -z "$NPM_GATE_MISSING" ]; then pass; else fail "ci-local.sh no longer gates:$NPM_GATE_MISSING"; fi

it "the checks the gate runs are the ones npm/package.json declares"
# Two spellings of the same commands, in two files. If package.json's scripts
# move and the gate keeps the old wording, the gate stops testing the thing the
# maintainer thinks it tests -- so both have to name the same entry points.
NPM_PKG_MISSING=""
grep -q 'sync-packages.mjs --check' npm/package.json || NPM_PKG_MISSING="$NPM_PKG_MISSING check"
grep -q 'node --test'               npm/package.json || NPM_PKG_MISSING="$NPM_PKG_MISSING test"
if [ -z "$NPM_PKG_MISSING" ]; then pass; else fail "npm/package.json no longer declares:$NPM_PKG_MISSING"; fi

it "the release bump REGENERATES the npm manifests, it does not merely check them"
# The manifests are derived from [workspace.package], so the bump is exactly
# what makes them stale. A bare --check here could only ever fail, aborting the
# release over a regeneration the operator would then have to do by hand on top
# of an already-rewritten Cargo.toml. Sync, prove convergence, stage, commit.
REL_BUMP_MISSING=""
grep -qE '^ *node npm/sync-packages\.mjs >&2'         scripts/release-local.sh || REL_BUMP_MISSING="$REL_BUMP_MISSING sync"
grep -qE '^ *node npm/sync-packages\.mjs --check >&2' scripts/release-local.sh || REL_BUMP_MISSING="$REL_BUMP_MISSING convergence-check"
grep -q  'git add Cargo.toml Cargo.lock npm'          scripts/release-local.sh || REL_BUMP_MISSING="$REL_BUMP_MISSING staging"
if [ -z "$REL_BUMP_MISSING" ]; then pass; else fail "the release bump no longer:$REL_BUMP_MISSING"; fi

it "the bump's footprint guard still refuses a path that is neither Cargo nor npm/"
# Widened from an exact two-path string to a prefix rule, so the guard could
# have been widened into uselessness. It still has to reject anything else.
grep -q 'bump touched unexpected paths' scripts/release-local.sh \
  && grep -qE 'Cargo\.toml \| Cargo\.lock \| npm/\*\)' scripts/release-local.sh \
  && pass || fail "release-local.sh no longer classifies the bump's dirty paths"

it "every ci-local.sh step label agrees on the step count, and none is missing"
# A step inserted mid-file renumbers every label after it. Getting that wrong is
# invisible -- the gate still runs, it just lies about where it is -- and the
# only reader who notices is an operator counting "N/M" in a 15-minute log.
# No interval expressions in the awk (mawk matches nothing for those).
STEP_LABELS="$(awk '/^ *say "/ {
  line = $0
  while (match(line, /[0-9]+\/[0-9]+/)) {
    print substr(line, RSTART, RLENGTH)
    line = substr(line, RSTART + RLENGTH)
  }
}' scripts/ci-local.sh)"
STEP_TOTALS="$(printf '%s\n' "$STEP_LABELS" | sed 's:.*/::' | sort -u | tr '\n' ' ')"
STEP_NUMS="$(printf '%s\n' "$STEP_LABELS" | sed 's:/.*::' | sort -n -u | tr '\n' ' ')"
STEP_TOTAL="${STEP_TOTALS% }"
STEP_WANT=""
i=1
while [ "$i" -le "${STEP_TOTAL:-0}" ] 2>/dev/null; do STEP_WANT="$STEP_WANT$i "; i=$((i + 1)); done
if [ "$STEP_TOTALS" != "$STEP_TOTAL " ]; then
  fail "ci-local.sh step labels disagree on the total: $STEP_TOTALS"
elif [ "$STEP_NUMS" != "$STEP_WANT" ]; then
  fail "ci-local.sh step numbers are '$STEP_NUMS', expected '$STEP_WANT'"
else pass; fi

it "the usage block's step range matches the labels"
ck grep -q "full gate (steps 1-$STEP_TOTAL)" scripts/ci-local.sh

# =============================================================================
group "tap-verify.sh -- what a published tap actually serves"
# =============================================================================
# bump-taps.sh's last step decided "did the push land?" from one grep for the
# VERSION STRING, and had no coverage of any kind. Both of the ways that reports
# green are asserted here, cheaply, with no spawn: a manifest naming the right
# version with somebody else's sha256 (which fails on every brew/scoop install),
# and a refactor that stops passing hashes at all.
# shellcheck source=lib/tap-verify.sh
. scripts/lib/tap-verify.sh

LINUX_SHA="5555555555555555555555555555555555555555555555555555555555555555"
OTHER_SHA="9999999999999999999999999999999999999999999999999999999999999999"
BREW_BODY='class Oam < Formula
  version "0.14.0"
  url "https://github.com/YawLabs/oam/releases/download/v0.14.0/oam-x86_64-unknown-linux-gnu"
  sha256 "5555555555555555555555555555555555555555555555555555555555555555"
end'

it "the version plus every expected hash is the only ok verdict"
eq "$(tap_verify_verdict "$BREW_BODY" 0.14.0 "oam-x86_64-unknown-linux-gnu:$LINUX_SHA")" "ok"

it "the right version with the wrong hash is caught, and the asset is NAMED"
# The defect, exactly: brew installs by checking the manifest's hash, so this
# body is broken for every user while a version grep calls it current. Named
# rather than counted -- "1 checksum differs" leaves the operator diffing three
# sha256 lines by hand.
eq "$(tap_verify_verdict "$BREW_BODY" 0.14.0 "oam-x86_64-unknown-linux-gnu:$OTHER_SHA")" \
   "hash-mismatch oam-x86_64-unknown-linux-gnu"

it "every mismatching asset is named, not just the first"
eq "$(tap_verify_verdict "$BREW_BODY" 0.14.0 "mac:$OTHER_SHA" "win:$OTHER_SHA")" \
   "hash-mismatch mac win"

it "a body on another release is version-stale, decided before any hash"
eq "$(tap_verify_verdict "$BREW_BODY" 0.13.2 "oam-x86_64-unknown-linux-gnu:$LINUX_SHA")" "version-stale"

it "a version PREFIX does not count as serving that version"
# The quotes are what make this a whole-field match: without them a tap still on
# 0.14.0 would confirm a 0.14 release, and one on 0.1 would confirm 0.14.0's
# predecessor by accident.
eq "$(tap_verify_verdict "$BREW_BODY" 0.14 "oam-x86_64-unknown-linux-gnu:$LINUX_SHA")" "version-stale"

it "an empty body is unfetched -- the network, never a pass"
eq "$(tap_verify_verdict "" 0.14.0 "oam-x86_64-unknown-linux-gnu:$LINUX_SHA")" "unfetched"

it "passing no hashes at all is its own verdict, not a silent pass"
# The version-only check that started this. A refactor that drops the pairs must
# be LOUD; degrading quietly back to a version grep is the whole bug.
eq "$(tap_verify_verdict "$BREW_BODY" 0.14.0)" "no-hashes"

it "an empty expected hash is a miss, not a substring match on everything"
# "" is a substring of every body, so accepting one would make the check
# unconditionally green -- a worse failure than the one it replaced.
eq "$(tap_verify_verdict "$BREW_BODY" 0.14.0 "oam-x86_64-unknown-linux-gnu:")" \
   "hash-mismatch oam-x86_64-unknown-linux-gnu"

it "a pair that lost its colon is a miss too, not a match on the asset name"
eq "$(tap_verify_verdict "$BREW_BODY" 0.14.0 "oam-x86_64-unknown-linux-gnu")" \
   "hash-mismatch oam-x86_64-unknown-linux-gnu"

it "the cache-buster appends a query parameter to a bare url"
eq "$(tap_cache_bust "https://raw.githubusercontent.com/YawLabs/homebrew-yaw/main/Formula/oam.rb" 1757000000)" \
   "https://raw.githubusercontent.com/YawLabs/homebrew-yaw/main/Formula/oam.rb?nocache=1757000000"

it "a url that already carries a query gets & rather than a second ?"
eq "$(tap_cache_bust "https://example.test/x?a=1" 42)" "https://example.test/x?a=1&nocache=42"

it "an omitted stamp still produces a busted url"
case "$(tap_cache_bust https://example.test/x)" in
  https://example.test/x\?nocache=[0-9]*) pass ;;
  *) fail "no timestamp was appended" ;;
esac

# =============================================================================
group "bump-taps.sh -- publishing to the package-manager taps"
# =============================================================================
# This script PUSHES to two public repos during a release, and it had no
# coverage of any kind -- it was not even in the parse list above. Every case
# below was a live defect found in review; they are here so they cannot come
# back silently. The failures they encode all shared one shape: the script
# reported success while a tap stayed on the old release.
#
# Real repos, real pushes, real git. Only the two EXTERNAL binaries are stubbed
# -- gh (which would need a published release) and curl (which would need the
# network) -- because everything this script gets wrong, it gets wrong in git,
# and a mocked git would encode the very assumption under test.

TAPS_BIN="$SUITE_TMP/taps-bin"
mkdir -p "$TAPS_BIN"

# `gh release download` -> a SHA256SUMS carrying the five real asset names in
# sha256sum's binary-mode form (the leading `*`), which is what the real file
# uses and what hash_for has to strip. `gh release view` -> the latest tag, so
# the downgrade guard has something to compare against.
cat > "$TAPS_BIN/gh" <<'GHSTUB'
#!/bin/bash
if [ "$1" = "release" ] && [ "$2" = "download" ]; then
  d=""; want=0
  for a in "$@"; do
    if [ "$want" = "1" ]; then d="$a"; want=0; fi
    [ "$a" = "--dir" ] && want=1
  done
  [ -n "$d" ] || exit 1
  cat > "$d/SHA256SUMS" <<'SUMS'
1111111111111111111111111111111111111111111111111111111111111111 *oam-aarch64-apple-darwin
2222222222222222222222222222222222222222222222222222222222222222 *oam-aarch64-pc-windows-msvc.exe
3333333333333333333333333333333333333333333333333333333333333333 *oam-x86_64-apple-darwin
4444444444444444444444444444444444444444444444444444444444444444 *oam-x86_64-pc-windows-msvc.exe
5555555555555555555555555555555555555555555555555555555555555555 *oam-x86_64-unknown-linux-gnu
SUMS
  exit 0
fi
if [ "$1" = "release" ] && [ "$2" = "view" ]; then echo "${STUB_LATEST:-v0.14.0}"; exit 0; fi
exit 0
GHSTUB
chmod +x "$TAPS_BIN/gh"
# The verify step is warn-only and network-bound; failing the fetch exercises
# its "could not fetch" branch without pretending to know what GitHub serves.
printf '#!/bin/bash\nexit 1\n' > "$TAPS_BIN/curl"
chmod +x "$TAPS_BIN/curl"

# taps_fixture -- two bare origins plus working clones, both taps at 0.13.2,
# with the real manifest shapes: the formula's per-arch url/sha256 pairs
# (including `using: :nounzip` and the deliberately absent linux-arm64 block)
# and the manifest's autoupdate block, whose `v$version` must NOT trip the
# stale-version guard.
#
# A FRESH directory per case, never a reset of a shared one. These cases push,
# rewind refs and dirty the tree; reusing one directory would let a failure in
# an early case masquerade as a defect in a later one. `taps_dir` names the
# fixture belonging to the case that just called it.
TAPS_N=0
taps_dir=""
taps_fixture(){
  TAPS_N=$((TAPS_N + 1))
  taps_dir="$SUITE_TMP/taps-$TAPS_N"
  mkdir -p "$taps_dir"
  ( cd "$taps_dir"
    git init -q --bare homebrew-yaw.git
    git init -q --bare scoop-yaw.git
    git init -q -b main homebrew-yaw
    mkdir -p homebrew-yaw/Formula homebrew-yaw/Casks
    cat > homebrew-yaw/Formula/oam.rb <<'RB'
class Oam < Formula
  desc "d"
  version "0.13.2"
  on_macos do
    on_arm do
      url "https://github.com/YawLabs/oam/releases/download/v0.13.2/oam-aarch64-apple-darwin", using: :nounzip
      sha256 "aaaa111111111111111111111111111111111111111111111111111111111111"
    end
    on_intel do
      url "https://github.com/YawLabs/oam/releases/download/v0.13.2/oam-x86_64-apple-darwin", using: :nounzip
      sha256 "bbbb222222222222222222222222222222222222222222222222222222222222"
    end
  end
  on_linux do
    on_intel do
      url "https://github.com/YawLabs/oam/releases/download/v0.13.2/oam-x86_64-unknown-linux-gnu", using: :nounzip
      sha256 "cccc333333333333333333333333333333333333333333333333333333333333"
    end
  end
end
RB
    # Yaw Terminal's cask lives in this same repo. It is what makes the tap a
    # SHARED checkout, and it is the passenger the index-sweep case looks for.
    echo "cask placeholder" > homebrew-yaw/Casks/yaw.rb
    ( cd homebrew-yaw
      git add -A
      git -c user.email=t@t -c user.name=t commit -qm init
      git remote add origin "$taps_dir/homebrew-yaw.git"
      git push -q origin main
      git branch -q --set-upstream-to=origin/main main )
    git init -q -b main scoop-yaw
    mkdir -p scoop-yaw/bucket
    cat > scoop-yaw/bucket/oam.json <<'JS'
{
  "version": "0.13.2",
  "architecture": {
    "64bit": { "url": "https://github.com/YawLabs/oam/releases/download/v0.13.2/oam-x86_64-pc-windows-msvc.exe", "hash": "dddd444444444444444444444444444444444444444444444444444444444444" },
    "arm64": { "url": "https://github.com/YawLabs/oam/releases/download/v0.13.2/oam-aarch64-pc-windows-msvc.exe", "hash": "eeee555555555555555555555555555555555555555555555555555555555555" }
  },
  "autoupdate": { "architecture": { "64bit": { "url": "https://github.com/YawLabs/oam/releases/download/v$version/oam-x86_64-pc-windows-msvc.exe" } } }
}
JS
    ( cd scoop-yaw
      git add -A
      git -c user.email=t@t -c user.name=t commit -qm init
      git remote add origin "$taps_dir/scoop-yaw.git"
      git push -q origin main
      git branch -q --set-upstream-to=origin/main main ) ) >/dev/null 2>&1
}

# run_taps <args...> -- the real script against the current fixture, with only
# gh and curl stubbed.
run_taps(){
  PATH="$TAPS_BIN:$PATH" \
  OAM_HOMEBREW_DIR="$taps_dir/homebrew-yaw" OAM_SCOOP_DIR="$taps_dir/scoop-yaw" \
  STUB_LATEST="${STUB_LATEST:-v0.14.0}" \
  bash "$REPO_DIR/scripts/bump-taps.sh" "$@" 2>&1
}

# What origin actually SERVES -- the only question that matters here. Every
# defect below reported success while this stayed on the old release.
served(){ git -C "$taps_dir/$1.git" show "refs/heads/main:$2" 2>/dev/null; }

if ! command -v git >/dev/null 2>&1; then
  it "bump-taps.sh cases"; skip "git not on PATH"
else
  it "the happy path publishes both taps, each hash in its own arch block"
  taps_fixture
  OUT="$(run_taps v0.14.0)"
  BREW="$(served homebrew-yaw Formula/oam.rb)"
  SCOOP="$(served scoop-yaw bucket/oam.json)"
  # Asset-keyed, not positional: pairing a url with the sha256 that FOLLOWS it
  # is what keeps the mac hash off the linux binary.
  if printf '%s' "$BREW" | grep -q 'version "0.14.0"' \
     && printf '%s' "$BREW" | grep -A1 'oam-aarch64-apple-darwin' | grep -q '1111111111' \
     && printf '%s' "$BREW" | grep -A1 'oam-x86_64-unknown-linux-gnu' | grep -q '5555555555' \
     && printf '%s' "$SCOOP" | grep -q '"version": "0.14.0"' \
     && printf '%s' "$SCOOP" | grep -q '4444444444'; then pass
  else fail "taps did not serve 0.14.0 with per-asset hashes: $OUT"; fi

  it "a second run is a no-op rather than an empty commit"
  OUT="$(run_taps v0.14.0)"
  if printf '%s' "$OUT" | grep -q "already at 0.14.0"; then pass
  else fail "a second run did not report the taps as already current: $OUT"; fi

  it "the autoupdate block does not trip the stale-version guard"
  # `v[0-9]` must not match the literal `v$version`, or every scoop bump aborts.
  if printf '%s' "$(served scoop-yaw bucket/oam.json)" | grep -q 'download/v\$version/'; then pass
  else fail "the autoupdate template was rewritten, or the guard tripped on it"; fi

  it "a commit that never reached origin is pushed, not called current"
  # The defect: the idempotence check asked only whether the WORKING TREE
  # differed. After a failed push the commit is local and the file is clean, so
  # the documented repair re-run printed "already at ..." and returned PAST the
  # push -- brew served the old release forever, under a green checkmark.
  taps_fixture
  run_taps v0.14.0 >/dev/null
  git -C "$taps_dir/homebrew-yaw.git" update-ref refs/heads/main \
    "$(git -C "$taps_dir/homebrew-yaw" rev-parse HEAD~1)"
  git -C "$taps_dir/homebrew-yaw" fetch -q origin
  OUT="$(run_taps v0.14.0)"
  if printf '%s' "$(served homebrew-yaw Formula/oam.rb)" | grep -q 'version "0.14.0"'; then pass
  else fail "an unpushed commit was not pushed on the repair run: $OUT"; fi

  it "another session's staged file is not swept into our commit"
  # homebrew-yaw carries Casks/yaw.rb for Yaw Terminal. A bare `git commit`
  # commits the whole INDEX, so a teammate's mid-edit file was committed and
  # pushed to the live tap -- invisibly, because the printed diffstat is scoped
  # to our own file.
  taps_fixture
  echo "work in progress" >> "$taps_dir/homebrew-yaw/Casks/yaw.rb"
  git -C "$taps_dir/homebrew-yaw" add Casks/yaw.rb
  run_taps v0.14.0 >/dev/null
  TOUCHED="$(git -C "$taps_dir/homebrew-yaw" show --stat --name-only --format= HEAD | tr -d '\r' | tr '\n' ' ')"
  case "$TOUCHED" in
    *Casks/yaw.rb*) fail "the commit swept in a foreign staged file: $TOUCHED" ;;
    *Formula/oam.rb*) pass ;;
    *) fail "unexpected commit contents: $TOUCHED" ;;
  esac

  it "a tap on a local-only branch is refused, and gains no branch"
  # `rev-list --count HEAD..origin/<branch>` fell back to 0 when the upstream
  # did not exist, so `push origin HEAD` CREATED that branch on the tap instead
  # of updating the one people install from -- and reported success.
  taps_fixture
  git -C "$taps_dir/homebrew-yaw" checkout -q -b yaw-bump-2.1.4
  OUT="$(run_taps v0.14.0)"
  REFS="$(git -C "$taps_dir/homebrew-yaw.git" branch --format='%(refname:short)' | tr '\n' ' ')"
  if printf '%s' "$OUT" | grep -q "has no origin/yaw-bump-2.1.4"; then
    case "$REFS" in
      *yaw-bump*) fail "a junk branch was created on the tap: $REFS" ;;
      *) pass ;;
    esac
  else fail "a local-only branch was not refused: $OUT"; fi

  it "an older tag is refused rather than silently downgrading both taps"
  # Nothing compared the requested tag against the newest release, and the
  # verify step confirms whatever it was told -- so a stale tag pasted from an
  # old release log downgraded every brew and scoop user, reporting success.
  taps_fixture
  run_taps v0.14.0 >/dev/null
  OUT="$(run_taps v0.13.2)"
  if printf '%s' "$OUT" | grep -q "older than the latest published release" \
     && printf '%s' "$(served homebrew-yaw Formula/oam.rb)" | grep -q 'version "0.14.0"'; then pass
  else fail "a downgrade was not refused: $OUT"; fi

  it "a downgrade proceeds when it is asked for explicitly"
  OUT="$(OAM_ALLOW_DOWNGRADE=1 PATH="$TAPS_BIN:$PATH" \
        OAM_HOMEBREW_DIR="$taps_dir/homebrew-yaw" OAM_SCOOP_DIR="$taps_dir/scoop-yaw" \
        bash "$REPO_DIR/scripts/bump-taps.sh" v0.13.2 2>&1)"
  if printf '%s' "$(served homebrew-yaw Formula/oam.rb)" | grep -q 'version "0.13.2"'; then pass
  else fail "an explicitly authorized downgrade did not land: $OUT"; fi

  it "an untracked file does not block a tap that is behind origin"
  # `git rebase` does not care about untracked files, and the advice the refusal
  # printed could not work anyway: `git stash` without -u leaves them in place,
  # so an operator who followed it hit the identical failure.
  taps_fixture
  git -C "$taps_dir/homebrew-yaw" commit -q --allow-empty -m "another session's cask bump"
  git -C "$taps_dir/homebrew-yaw" push -q origin main
  git -C "$taps_dir/homebrew-yaw" reset -q --hard HEAD~1
  : > "$taps_dir/homebrew-yaw/oam.rb.bak"
  OUT="$(run_taps v0.14.0)"
  if printf '%s' "$(served homebrew-yaw Formula/oam.rb)" | grep -q 'version "0.14.0"'; then pass
  else fail "an untracked file blocked the bump: $OUT"; fi

  it "a behind tap with real uncommitted work is refused, never stashed"
  # The other half of the same guard: the checkout is shared, so rebasing over
  # someone's tracked changes -- or stashing them -- is never ours to do.
  taps_fixture
  git -C "$taps_dir/homebrew-yaw" commit -q --allow-empty -m "another session's cask bump"
  git -C "$taps_dir/homebrew-yaw" push -q origin main
  git -C "$taps_dir/homebrew-yaw" reset -q --hard HEAD~1
  echo "someone is editing this" >> "$taps_dir/homebrew-yaw/Casks/yaw.rb"
  OUT="$(run_taps v0.14.0)"
  if ! printf '%s' "$OUT" | grep -q "Not stashing another session"; then
    fail "a dirty shared checkout was not refused: $OUT"
  elif git -C "$taps_dir/homebrew-yaw" diff --quiet -- Casks/yaw.rb; then
    fail "the foreign edit was discarded"
  else pass; fi

  it "an abort reverts every un-published rewrite, in both taps"
  # Both files are rewritten before either is published, so an abort in between
  # left the OTHER tap rewritten and uncommitted in a shared checkout -- and the
  # next run then refused to proceed, blaming the operator for the script's own
  # leftover. Tripped here by a url the JSON rewriter does not touch.
  taps_fixture
  node -e 'const fs=require("fs"),p=process.argv[1];const d=JSON.parse(fs.readFileSync(p,"utf8"));d.notes_url="https://github.com/YawLabs/oam/releases/download/v0.9.9/NOTES";fs.writeFileSync(p,JSON.stringify(d,null,2)+"\n");' "$taps_dir/scoop-yaw/bucket/oam.json"
  ( cd "$taps_dir/scoop-yaw" && git commit -qam "a url the rewriter does not reach" \
    && git push -q origin main ) >/dev/null 2>&1
  OUT="$(run_taps v0.14.0)"
  BREW_DIRTY="$(git -C "$taps_dir/homebrew-yaw" status --porcelain --untracked-files=no | tr -d ' \r\n')"
  SCOOP_DIRTY="$(git -C "$taps_dir/scoop-yaw" status --porcelain --untracked-files=no | tr -d ' \r\n')"
  if printf '%s' "$OUT" | grep -q "still references a release other than v0.14.0" \
     && [ -z "$BREW_DIRTY" ] && [ -z "$SCOOP_DIRTY" ]; then pass
  else fail "an abort left a rewrite behind (brew=$BREW_DIRTY scoop=$SCOOP_DIRTY): $OUT"; fi

  it "a missing asset hash fails closed rather than publishing a bad manifest"
  # A wrong published hash is worse than a stale one: it teaches people to
  # ignore a mismatch.
  taps_fixture
  BAD_BIN="$SUITE_TMP/taps-bin-bad"; mkdir -p "$BAD_BIN"
  grep -v '^4444444444' "$TAPS_BIN/gh" > "$BAD_BIN/gh"
  chmod +x "$BAD_BIN/gh"; cp "$TAPS_BIN/curl" "$BAD_BIN/curl"
  OUT="$(PATH="$BAD_BIN:$PATH" OAM_HOMEBREW_DIR="$taps_dir/homebrew-yaw" \
        OAM_SCOOP_DIR="$taps_dir/scoop-yaw" \
        bash "$REPO_DIR/scripts/bump-taps.sh" v0.14.0 2>&1)"
  if printf '%s' "$OUT" | grep -q "no entry for oam-x86_64-pc-windows-msvc.exe" \
     && printf '%s' "$(served scoop-yaw bucket/oam.json)" | grep -q '"version": "0.13.2"'; then pass
  else fail "a missing hash did not fail closed: $OUT"; fi

  it "--dry-run publishes nothing"
  taps_fixture
  run_taps v0.14.0 --dry-run >/dev/null
  if printf '%s' "$(served homebrew-yaw Formula/oam.rb)" | grep -q 'version "0.13.2"'; then pass
  else fail "--dry-run published to the tap"; fi

  it "a missing tap checkout exits 3, distinct from a successful bump"
  # release-local.sh prints "taps are current" for exit 0. Conflating "skipped"
  # with "done" put that green line directly under the `taps NOT bumped`
  # warning, and the green one is what an operator skimming a long log keeps.
  RC=0
  PATH="$TAPS_BIN:$PATH" OAM_TAPS_OPTIONAL=1 \
    OAM_HOMEBREW_DIR="$SUITE_TMP/nope-brew" OAM_SCOOP_DIR="$SUITE_TMP/nope-scoop" \
    bash "$REPO_DIR/scripts/bump-taps.sh" v0.14.0 >/dev/null 2>&1 || RC=$?
  eq "$RC" "3"

  it "release-local.sh maps that skip code to a warning, not to a success line"
  ck grep -q '3) warn "taps SKIPPED' "$REPO_DIR/scripts/release-local.sh"

  # --- what the verify step proves, end to end ------------------------------
  # Substring assertions via `case`, not `printf ... | grep -q`: this file runs
  # under `set -o pipefail` and grep -q exits at its FIRST match, so a large
  # enough body SIGPIPEs the writer and a HIT reads as a miss (the #95 shape
  # lib/tap-verify.sh's header describes).
  has_all(){
    local hay="$1" n; shift
    for n in "$@"; do
      case "$hay" in *"$n"*) ;; *) fail "missing '$n' in: $hay"; return 0 ;; esac
    done
    pass
  }

  # One publish, then re-runs whose publish half is a no-op and whose verify
  # half is the thing under test. The stub serves whatever origin actually holds,
  # so the happy case cannot pass by agreeing with a fixture nobody published.
  taps_fixture
  run_taps v0.14.0 >/dev/null
  SERVE="$SUITE_TMP/taps-serve"; mkdir -p "$SERVE"
  served homebrew-yaw Formula/oam.rb > "$SERVE/brew"
  served scoop-yaw   bucket/oam.json > "$SERVE/scoop"
  SBIN="$SUITE_TMP/taps-bin-serve"; mkdir -p "$SBIN"; cp "$TAPS_BIN/gh" "$SBIN/gh"
  cat > "$SBIN/curl" <<'CURLSTUB'
#!/bin/bash
printf '%s\n' "$*" >> "$TAP_CURL_LOG"
for a in "$@"; do
  case "$a" in
    *homebrew-yaw*) cat "$TAP_SERVE/brew"; exit 0 ;;
    *scoop-yaw*)    cat "$TAP_SERVE/scoop"; exit 0 ;;
  esac
done
exit 1
CURLSTUB
  chmod +x "$SBIN/curl"
  CURL_LOG="$SUITE_TMP/taps-curl.log"
  verify_run(){
    TAP_CURL_LOG="$CURL_LOG" TAP_SERVE="$SERVE" PATH="$SBIN:$PATH" \
    OAM_HOMEBREW_DIR="$taps_dir/homebrew-yaw" OAM_SCOOP_DIR="$taps_dir/scoop-yaw" \
    STUB_LATEST=v0.14.0 \
    bash "$REPO_DIR/scripts/bump-taps.sh" v0.14.0 2>&1
  }

  it "a tap serving this release with the published hashes verifies clean"
  : > "$CURL_LOG"
  OUT="$(verify_run)"
  has_all "$OUT" \
    "homebrew-yaw/main/Formula/oam.rb serves 0.14.0 with matching hashes" \
    "scoop-yaw/main/bucket/oam.json serves 0.14.0 with matching hashes"

  it "the verify fetch reads origin, not whatever the CDN has cached"
  # The documented repair path re-pushes a CORRECTED HASH UNDER THE SAME
  # VERSION. Without a cache-buster the ~5 min raw.githubusercontent copy still
  # carries that version, so the run whose whole job was to prove the fix landed
  # confirmed the body it was sent to replace.
  has_all "$(cat "$CURL_LOG")" "nocache=" "Cache-Control: no-cache"

  it "a right-version wrong-hash tap is caught, and the asset is named"
  # What a version grep called green: brew resolves the url, hashes the binary
  # and compares it to this field, so every install of 0.14.0 fails while the
  # release log says the taps are current.
  node -e 'const fs=require("fs"),p=process.argv[1];fs.writeFileSync(p,fs.readFileSync(p,"utf8").replace(/5555555555/,"9999999999"));' "$SERVE/brew"
  OUT="$(verify_run)"
  has_all "$OUT" \
    "serves a DIFFERENT hash for: oam-x86_64-unknown-linux-gnu" \
    "worse than none" \
    "scoop-yaw/main/bucket/oam.json serves 0.14.0 with matching hashes"

  it "an unreachable tap is reported as unfetched, never as verified"
  OUT="$(TAP_CURL_LOG="$CURL_LOG" TAP_SERVE="$SUITE_TMP/no-such-serve" PATH="$SBIN:$PATH" \
        OAM_HOMEBREW_DIR="$taps_dir/homebrew-yaw" OAM_SCOOP_DIR="$taps_dir/scoop-yaw" \
        STUB_LATEST=v0.14.0 \
        bash "$REPO_DIR/scripts/bump-taps.sh" v0.14.0 2>&1)"
  has_all "$OUT" "could not fetch homebrew-yaw/main/Formula/oam.rb to verify"
fi

# =============================================================================
# The front-page conformance figures are a RECEIPT, and README.md says two
# lines above them that receipts are "never hand-edited". They drifted anyway:
# the docs claimed 429/431 with "both remaining failures" long after the
# generated scorecard had moved to 439/442 with three. Nothing compared them,
# so the wrong number read as authoritative precisely because of the sentence
# promising it could not be.
#
# Parsed out of the JSON twin rather than the markdown, because that file is
# the machine artifact the generator writes first.
it "the README and why-oam conformance figures match the generated scorecard"
CONF_JSON="conformance/node-suite-scorecard.json"
if [ ! -f "$CONF_JSON" ]; then
  skip "no scorecard at $CONF_JSON"
else
  # FIRST match, not the last: `byModule` repeats "pass" per module, and a
  # greedy sed happily returns the tally of whichever module sorts last --
  # which is how the first draft of this gate reported 22/442.
  # No jq dependency: it is not guaranteed on a contributor's box.
  SC_PASS=$(grep -o '"pass"[[:space:]]*:[[:space:]]*[0-9]*' "$CONF_JSON" | head -1 | grep -o '[0-9]*$')
  SC_RUNNABLE=$(grep -o '"runnable"[[:space:]]*:[[:space:]]*[0-9]*' "$CONF_JSON" | head -1 | grep -o '[0-9]*$')
  SC_RATIO="${SC_PASS}/${SC_RUNNABLE}"
  CONF_STALE=""
  for doc in README.md docs/why-oam.md; do
    grep -q "$SC_RATIO" "$doc" || CONF_STALE="$CONF_STALE $doc"
  done
  if [ -z "$CONF_STALE" ]; then
    pass
  else
    fail "scorecard says $SC_RATIO; not found in:$CONF_STALE"
  fi
fi

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
