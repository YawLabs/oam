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

# The operator's release knobs, scrubbed before any fixture runs. This suite is
# release-local.sh's own local gate (via ci-local.sh), so it runs inside a
# release's environment, and that environment is exactly where the recovery
# knobs get exported: a failed mac gate's documented way out is a re-run with
# OAM_SKIP_MAC_SIGN=1, and before this scrub that knob reached 23 mac-signing
# verdicts here and killed the re-run at "local CI gate failed". Every case
# that needs a knob sets it on its own call. The host knobs are here for a
# second reason: no fixture may ever reach the real build Mac.
#
# A guard in the "mac-signing.sh" group fails when a release script starts
# reading a signing, skip or mac-host knob that this list does not carry.
OPERATOR_KNOBS="OAM_SIGN_REQUIRED OAM_RELEASE_SIGNING_KEY
  OAM_SKIP_MAC_SIGN OAM_SKIP_MAC_X64 OAM_SKIP_MAC OAM_SIGNING_DIR
  OAM_MAC_HOST OAM_MAC_USER OAM_MAC_KEY
  OAM_WIN_SIGN_METADATA OAM_WIN_SIGN_PUBLISHER OAM_SKIP_WIN_SIGN OAM_WIN_SIGN_TIMEOUT
  OAM_SKIP_WIN_X64 OAM_SKIP_LINUX OAM_SKIP_LOCAL_GATE
  OAM_DRY_RUN OAM_NO_AUTO_BUMP OAM_NO_AUTO_ATTRIBUTION
  OAM_INSECURE_SKIP_SIGNATURE"
scrub_operator_knobs(){
  local k
  for k in $OPERATOR_KNOBS; do unset "$k"; done
}
scrub_operator_knobs

# One temp root, removed on exit. Every fixture used to call mktemp and nothing
# ever cleaned up: a single run leaked 14 directories, and once this became a
# pre-push gate that grew without bound (149 had piled up on the dev box before
# anyone looked). Same shape as ci-local.sh`s CLEANUP_PATHS + EXIT trap.
SUITE_TMP="$(mktemp -d -t oamtest-XXXXXX)"
# One EXIT trap: the temp root goes, and a console code page an install.ps1
# case switched (in_ps_cp, below) is put back when the suite dies mid-case --
# chcp is per console and sticks, and the console is the operator's.
suite_exit(){
  rm -rf "$SUITE_TMP"
  if [ -n "${IN_CP_ORIG:-}" ] && [ -x "${IN_CHCP:-}" ]; then "$IN_CHCP" "$IN_CP_ORIG" >/dev/null 2>&1 || true; fi
}
trap suite_exit EXIT

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

# --- codegen-unit objects follow their unit's dep-info ------------------------
# macOS leaves each unit's <name>-<hash>.<cgu>.rcgu.o in deps/ (its debug info
# lives there), and every build names its codegen units afresh -- so keyed like
# any other dotted file, each object was a family of one and never collected:
# 57 generations of oam_engine on the Air, most of a 49GB deps/ (2026-09-25).
reset_tree
plant target/debug/deps 0 eng-0000000000000001.d eng-0000000000000001.a1.rcgu.o eng-0000000000000001.b1.rcgu.o
plant target/debug/deps 1 eng-0000000000000002.d eng-0000000000000002.a2.rcgu.o
plant target/debug/deps 2 eng-0000000000000003.d eng-0000000000000003.a3.rcgu.o eng-0000000000000003.b3.rcgu.o
gc --keep 2

it "a codegen-unit object goes once neither its unit's .d nor its executable survives"
survivors eng-0000000000000002.d eng-0000000000000002.a2.rcgu.o \
          eng-0000000000000003.d eng-0000000000000003.a3.rcgu.o eng-0000000000000003.b3.rcgu.o \
          '!eng-0000000000000001.d' '!eng-0000000000000001.a1.rcgu.o' '!eng-0000000000000001.b1.rcgu.o'

# Neither an executable nor a .d names this unit. An hour-old one is a build
# that died, and goes; a fresh one may be a build still writing it -- deleting
# its objects would break its link -- so it stays.
it "an unclaimed object goes once it is an hour old, and stays while fresh"
reset_tree
plant target/debug/deps 0 dead-00000000000000fe.x.rcgu.o
echo artifact > "$ROOT/target/debug/deps/wip-00000000000000ff.x.rcgu.o"
gc --keep 2
survivors wip-00000000000000ff.x.rcgu.o '!dead-00000000000000fe.x.rcgu.o'

# The leak the first version had: a harness loses its .d (check .d files outrank
# it) while the harness itself survives, then a later run evicts the harness.
# The objects must go in THAT run, not be orphaned because this run did not
# prune the .d itself.
it "an executable's objects go in the run that evicts it, after its .d went earlier"
reset_tree
plant target/debug/deps 0 hx-b000000000000001 hx-b000000000000001.d hx-b000000000000001.x.rcgu.o
plant target/debug/deps 1 hx-b000000000000002.d
plant target/debug/deps 2 hx-b000000000000003.d
gc --keep 2
HX_RUN1="$([ -e "$ROOT/target/debug/deps/hx-b000000000000001.x.rcgu.o" ] && echo kept || echo gone)"
plant target/debug/deps 3 hx-b000000000000004
plant target/debug/deps 4 hx-b000000000000005
gc --keep 2
if [ "$HX_RUN1" = "kept" ]; then
  survivors '!hx-b000000000000001' '!hx-b000000000000001.x.rcgu.o'
else fail "run 1 already took the objects of the executable it kept"; fi

# One crate version has several live .rmeta files (a check-mode one per target
# beside the build's), and three versions of one crate can be live at once;
# evicting any forces a re-check or rebuild on every run. .rmeta is metadata
# only, so it gets four times the keep.
it ".rmeta keeps four times --keep per family"
reset_tree
for g in 1 2 3 4 5 6 7 8 9; do plant target/debug/deps "$g" "librm-c00000000000000$g.rmeta"; done
gc --keep 2
survivors librm-c000000000000009.rmeta librm-c000000000000002.rmeta '!librm-c000000000000001.rmeta'

# The real tree's shape: clippy and cargo check write .d files (with no
# executable) into the same family, so they outrank a real build's .d. On the
# Air that pruned the .d of the xtask binary still in use, and its 236 debug
# objects went with it. An executable that survives keeps its own objects.
reset_tree
plant target/debug/deps 0 app-a000000000000001 app-a000000000000001.d app-a000000000000001.x.rcgu.o
plant target/debug/deps 1 app-a000000000000002.d
plant target/debug/deps 2 app-a000000000000003.d
gc --keep 2

it "a surviving executable keeps its objects when check-only .d files outrank its .d"
survivors app-a000000000000001 app-a000000000000001.x.rcgu.o '!app-a000000000000001.d'

# =============================================================================
group "gc-target.sh -- portability and safety"
# =============================================================================

# macOS has BSD find (no -printf), so there the listing comes from BSD stat -f.
# That path is what the Air runs, and nothing else here reaches it: on a host
# with real BSD stat it runs as-is; elsewhere a shim hides find -printf and
# answers `stat -f` the BSD way through GNU stat, so the dev box proves it too.
it "without find -printf, BSD stat -f still drives the per-family prune"
BSDLIST="$SUITE_TMP/shim-bsdlist"
mkdir -p "$BSDLIST"
REAL_FIND="$(command -v find)"
REAL_STAT="$(command -v stat)"
{
  echo '#!/bin/sh'
  echo 'for a in "$@"; do [ "$a" = "-printf" ] && exit 1; done'
  printf 'exec "%s" "$@"\n' "$REAL_FIND"
} > "$BSDLIST/find"
if ! stat -f '%m' . >/dev/null 2>&1; then
  # GNU stat: map the three BSD conversions gc-target.sh uses (%m mtime, %b
  # 512-byte blocks, %N name) and its %t tab onto --printf.
  {
    echo '#!/bin/sh'
    printf '[ "$1" = "-f" ] || exec "%s" "$@"\n' "$REAL_STAT"
    echo "fmt=\$(printf '%s' \"\$2\" | sed 's/%m/%Y/g; s/%N/%n/g; s/%t/\\\\t/g'); shift 2"
    printf 'exec "%s" --printf="$fmt\\n" "$@"\n' "$REAL_STAT"
  } > "$BSDLIST/stat"
fi
chmod +x "$BSDLIST"/*
reset_tree
plant target/debug/deps 0 oam-bbbd000000000001 lib-bbbd000000000001.d lib-bbbd000000000001.q.rcgu.o
plant target/debug/deps 1 oam-bbbd000000000002 lib-bbbd000000000002.d
BSD_OUT="$( cd "$ROOT" && PATH="$BSDLIST:$PATH" bash scripts/gc-target.sh --keep 1 2>&1 )"
if grep -q 'prune skipped' <<<"$BSD_OUT"; then
  fail "the BSD path degraded instead of pruning: $BSD_OUT"
else
  survivors oam-bbbd000000000002 lib-bbbd000000000002.d \
            '!oam-bbbd000000000001' '!lib-bbbd000000000001.d' '!lib-bbbd000000000001.q.rcgu.o'
fi

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
# Plain `awk` only when no named one exists: macOS ships its BSD awk (the one
# true awk) under that name alone, so without it this case found nothing to run
# there -- on exactly the host whose awk differs most.
for AWKBIN in mawk gawk original-awk busybox awk; do
  [ "$AWKBIN" = "awk" ] && [ -n "$AWK_TESTED" ] && continue
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

# --- a host with neither lister -----------------------------------------------
# `find -printf` is GNU-only and `stat -f` is BSD-only. With neither, the
# per-family prune would collect zero while reporting a clean run -- the precise
# shape of the mawk bug this suite exists for. The contract is to degrade
# LOUDLY: skip the deps prune, say why, and still reclaim incremental/. Both
# are shimmed away, so this holds on macOS too, where real stat -f exists.
it "with neither find -printf nor BSD stat, the deps prune is skipped loudly, reclaiming the rest"
NOPRINTF="$SUITE_TMP/shim-noprintf"
mkdir -p "$NOPRINTF"
REAL_FIND="$(command -v find)"
REAL_STAT="$(command -v stat)"
{
  echo '#!/bin/sh'
  echo 'for a in "$@"; do [ "$a" = "-printf" ] && exit 1; done'
  printf 'exec %s "$@"\n' "$REAL_FIND"
} > "$NOPRINTF/find"
{
  echo '#!/bin/sh'
  echo '[ "$1" = "-f" ] && exit 1'
  printf 'exec "%s" "$@"\n' "$REAL_STAT"
} > "$NOPRINTF/stat"
chmod +x "$NOPRINTF/find" "$NOPRINTF/stat"
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
# #210: the linux leg's probe could answer nothing with exit 0 (df's failure
# hidden behind a trailing `tr`), and the check then skipped itself in silence.
# Every reading is now either a number or a reason.
it "a df answer in GB reads as its number"
eq "$(disk_free_reading 0 "$(printf 'Avail\n  37G\n')")" "37"
it "a df answer that is not a size gives a reason, not a number"
out="$(disk_free_reading 0 "Avail")" && fail "a header-only answer must not read as a size: $out" \
  || { case "$out" in *"not a size"*) pass ;; *) fail "no reason given: '$out'" ;; esac; }
it "a df probe that printed nothing gives a reason"
out="$(disk_free_reading 0 "")" && fail "an empty answer must not read as a size: $out" \
  || { case "$out" in *"printed nothing"*) pass ;; *) fail "no reason given: '$out'" ;; esac; }
it "a failed probe (a dead tunnel's 255) gives a reason naming its status"
out="$(disk_free_reading 255 "")" && fail "a failed probe must not read as a size: $out" \
  || { case "$out" in *"exit 255"*) pass ;; *) fail "no reason given: '$out'" ;; esac; }
it "a failed df with stray digits on stdout is still a failure"
out="$(disk_free_reading 1 "  12G")" && fail "status 1 must not read as a size: $out" || pass
it "the linux leg's probe keeps df's status and stderr, and says when it did not run"
GCPLEG_CODE="$(sed 's/#.*//' scripts/build-platforms-gcp-iap.sh)"
if grep -qE "df -BG[^\"]*\|" <<<"$GCPLEG_CODE" || grep -qE 'df -BG.*2>/dev/null' <<<"$GCPLEG_CODE"; then
  fail "the df probe pipes or discards stderr again"
elif ! grep -q 'disk headroom NOT CHECKED' <<<"$GCPLEG_CODE"; then
  fail "an unreadable builder disk no longer warns"
else pass; fi

it "OAM_DISK_RECLAIM_GB moves the reclaim threshold"
eq "$(OAM_DISK_RECLAIM_GB=50 bash -c '. scripts/lib/iap-helpers.sh; disk_needs_reclaim 40 && echo reclaim || echo skip')" \
   "reclaim"

it "OAM_DISK_MIN_GB moves the abort floor"
eq "$(OAM_DISK_MIN_GB=50 bash -c '. scripts/lib/iap-helpers.sh; disk_below_floor 40 && echo abort || echo proceed')" \
   "abort"

# =============================================================================
group "iap-helpers.sh -- instance schedules + ssh transport drops"
# =============================================================================
# The reading gcloud gave for yaw-linux-builder on 2026-09-25. gcloud on
# Windows terminates value() output with \r\n, so every parser here is fed the
# \r too: a policy name carrying one would make `resource-policies describe`
# look up a policy that does not exist, and the detach would silently skip.
POLICY_URL='https://www.googleapis.com/compute/v1/projects/yaw-labs-prod/regions/us-west1/resourcePolicies/yaw-linux-builder-autostop'

it "policy name comes off the selfLink"
eq "$(iap_policy_name "$POLICY_URL")" "yaw-linux-builder-autostop"

it "policy name survives a CRLF-terminated reading"
eq "$(iap_policy_name "$POLICY_URL"$'\r')" "yaw-linux-builder-autostop"

it "a bare policy name passes through as itself"
eq "$(iap_policy_name "yaw-linux-builder-autostop")" "yaw-linux-builder-autostop"

it "an empty reading is a clean miss, not an empty name"
iap_policy_name "" >/dev/null 2>&1 && fail "named an empty policy" || pass

it "policy region comes off the selfLink"
eq "$(iap_policy_region "$POLICY_URL"$'\r')" "us-west1"

it "a bare policy name has no region to read"
iap_policy_region "yaw-linux-builder-autostop" >/dev/null 2>&1 && fail "invented a region" || pass

it "two attached policies split into two URLs"
eq "$(iap_policy_urls "$POLICY_URL;${POLICY_URL%autostop}snap"$'\r' | wc -l | tr -d ' ')" "2"

it "the split URLs carry no CR"
eq "$(iap_policy_urls "$POLICY_URL"$'\r' | tr -d '\n')" "$POLICY_URL"

it "no attached policies yields no output at all"
eq "$(iap_policy_urls "" | wc -c | tr -d ' ')" "0"

# The last two lines of the 2026-09-25 node-suite log, verbatim.
it "the scheduled-stop signature reads as a transport drop"
if ssh_transport_dropped $'  PASS   test-stream-pipeline-with-empty-string.js\nConnection to localhost closed by remote host.'; then pass
else fail "missed OpenSSH's closed-by-remote-host line"; fi

it "a remote command that merely failed is not a transport drop"
if ssh_transport_dropped $'error: test failed, to rerun pass ...\n[remote] node-suite FAILED'; then fail "a remote exit code is not a transport drop"
else pass; fi

it "empty text is not a transport drop"
ssh_transport_dropped "" && fail "matched nothing" || pass

# A remote step is run again only when the TRANSPORT dropped under it, the VM
# is still RUNNING and attempts remain -- never for a command that exited.
VMS_DROP=$'  PASS   test-stream-pipeline-with-empty-string.js\nConnection to localhost closed by remote host.'
it "a transport drop (ssh exit 255) under a RUNNING VM, with attempts left, is run again"
if remote_step_should_retry 1 3 255 "$VMS_DROP" RUNNING; then pass; else fail "not retried"; fi
it "the last attempt is not run again"
remote_step_should_retry 3 3 255 "$VMS_DROP" RUNNING && fail "retried past the last attempt" || pass
it "a VM that is not RUNNING is not run again -- the postmortem says who stopped it"
remote_step_should_retry 1 3 255 "$VMS_DROP" TERMINATED && fail "retried against a stopped VM" || pass
it "a remote command that exited non-zero is never run again"
remote_step_should_retry 1 3 1 $'error: test failed, to rerun pass ...\n[remote] test FAILED' RUNNING && fail "retried a real failure" || pass
it "a remote command that exited 1 with a log that mentions a reset is a real failure, not a drop"
remote_step_should_retry 1 3 1 "$VMS_DROP" RUNNING && fail "retried on the log's words alone" || pass
it "a blank attempt count is not an invitation"
remote_step_should_retry "" 3 255 "$VMS_DROP" RUNNING && fail "retried on a blank attempt" || pass

# The same decision as the verdict word remote_step_run acts on. A VM that is
# TERMINATED or STOPPING behind a drop was stopped under the step, and is
# started again rather than left to the postmortem alone -- the 2026-09-25
# schedule stop would have cost one restart, not the release.
it "the verdict for a drop under a RUNNING VM is rerun, which is what should_retry answers to"
eq "$(remote_step_verdict 1 3 255 "$VMS_DROP" RUNNING)" "rerun"
it "a drop under a TERMINATED or STOPPING VM is restart: the VM was stopped under the step"
eq "$(remote_step_verdict 1 3 255 "$VMS_DROP" TERMINATED)-$(remote_step_verdict 2 3 255 "$VMS_DROP" STOPPING)" "restart-restart"
it "a stopped VM on the last attempt is no, as is one in a state the restart does not know"
eq "$(remote_step_verdict 3 3 255 "$VMS_DROP" TERMINATED)-$(remote_step_verdict 1 3 255 "$VMS_DROP" STAGING)-$(remote_step_verdict 1 3 255 "$VMS_DROP" UNKNOWN)" "no-no-no"
it "a command that exited against a stopped VM is no: the exit, not the stop, is the verdict"
eq "$(remote_step_verdict 1 3 1 "$VMS_DROP" TERMINATED)-$(remote_step_verdict 1 3 1 $'[remote] test FAILED' TERMINATED)" "no-no"
it "a blank attempt count is no, whatever the VM is doing"
eq "$(remote_step_verdict "" 3 255 "$VMS_DROP" TERMINATED)" "no"

# =============================================================================
group "iap-helpers.sh -- ssh transport: direct first, IAP tunnel fallback"
# =============================================================================
# On 2026-09-25 the builder answered plain OpenSSH on its external IP in ~1s
# while every IAP probe took 11-57s, and a release died after 1269s of them
# reporting only "(empty stderr)". The orchestrator now goes direct and keeps
# the tunnel as the fallback; these are the decisions that choose between them
# and what the operator is told when the direct path does not answer.

it "the three documented OAM_IAP_SSH_MODE values are accepted"
if ssh_mode_valid auto && ssh_mode_valid direct && ssh_mode_valid tunnel; then pass
else fail "a documented mode was rejected"; fi

it "a typo, a wrong case, padding, an empty value and 'iap' are rejected"
SSH_MODE_BAD=""
for m in "" DIRECT Auto "auto " iap dirct; do
  ssh_mode_valid "$m" && SSH_MODE_BAD="$SSH_MODE_BAD '$m'"
done
if [ -z "$SSH_MODE_BAD" ]; then pass; else fail "accepted:$SSH_MODE_BAD"; fi

it "auto goes direct when the direct probe answered"
eq "$(ssh_transport_pick auto 1)" "direct"

it "auto falls back to the tunnel when it did not"
eq "$(ssh_transport_pick auto 0)" "tunnel"

it "forced direct uses direct when it answered"
eq "$(ssh_transport_pick direct 1)" "direct"

# The one outcome that must NOT degrade into a tunnel: an operator who forced
# direct asked for the run to stop rather than crawl through the relay.
it "forced direct picks nothing when direct did not answer"
SSH_PICK_OUT="$(ssh_transport_pick direct 0)"; SSH_PICK_RC=$?
if [ "$SSH_PICK_RC" != "0" ] && [ -z "$SSH_PICK_OUT" ]; then pass
else fail "rc=$SSH_PICK_RC out='$SSH_PICK_OUT' -- forced direct fell back"; fi

it "forced tunnel stays on the tunnel even when direct would answer"
eq "$(ssh_transport_pick tunnel 1)" "tunnel"

it "an invalid mode picks no transport"
ssh_transport_pick bogus 1 >/dev/null 2>&1 && fail "picked a transport for an invalid mode" || pass

# Captured 2026-09-25 from this orchestrator's own ssh (OpenSSH_10.2p1, Git
# Bash) against yaw-linux-builder, with the CRLF endings Windows tools write.
it "a refused key reads back as ssh's own line, CR stripped"
eq "$(last_nonblank_line $'nosuchuser@34.83.189.193: Permission denied (publickey).\r\n')" \
   "nosuchuser@34.83.189.193: Permission denied (publickey)."

it "trailing blank and whitespace-only lines are skipped"
eq "$(last_nonblank_line $'Warning: Permanently added \'34.83.189.193\' (ED25519) to the list of known hosts.\r\nssh: connect to host 34.83.189.193 port 22: Connection timed out\r\n\r\n \t \r\n')" \
   "ssh: connect to host 34.83.189.193 port 22: Connection timed out"

it "a last line with no newline after it still counts"
eq "$(last_nonblank_line $'first\nsecond')" "second"

# The 2026-09-25 failure mode: the caller must be told there is NOTHING, so it
# says what that means (killed by the timeout, or the exit code) instead of
# printing "(empty stderr)".
it "empty or whitespace-only stderr is a miss, not a blank line"
if ! last_nonblank_line "" >/dev/null && ! last_nonblank_line $' \r\n\t\n' >/dev/null \
   && [ -z "$(last_nonblank_line $'\r\n' || true)" ]; then pass
else fail "reported a line for text that has none"; fi

it "a changed host key is permanent -- polling cannot fix it"
if ssh_error_is_permanent $'@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nHost key verification failed.' \
   && ssh_error_is_permanent 'Host key verification failed.'; then pass
else fail "a changed host key would be retried for the whole boot budget"; fi

it "the failures a booting guest gives are retried"
SSH_PERM_BAD=""
for e in "jeff@34.83.189.193: Permission denied (publickey)." \
         "ssh: connect to host 34.83.189.193 port 22: Connection refused" \
         "ssh: connect to host 34.83.189.193 port 22: Connection timed out" \
         "kex_exchange_identification: read: Connection reset by peer" ""; do
  ssh_error_is_permanent "$e" && SSH_PERM_BAD="$SSH_PERM_BAD [$e]"
done
if [ -z "$SSH_PERM_BAD" ]; then pass; else fail "treated as permanent:$SSH_PERM_BAD"; fi

it "a firewall-shaped failure names tcp:22 and the address"
SSH_HINT="$(direct_ssh_hint 'ssh: connect to host 34.83.189.193 port 22: Connection timed out' 34.83.189.193)"
grep -qF 'tcp:22 on 34.83.189.193' <<<"$SSH_HINT" && pass || fail "hint: '$SSH_HINT'"

it "a changed host key gets the exact command that clears it"
SSH_HINT="$(direct_ssh_hint 'Host key verification failed.' 34.83.189.193)"
grep -qF 'ssh-keygen -R 34.83.189.193 -f ~/.ssh/google_compute_known_hosts' <<<"$SSH_HINT" && pass || fail "hint: '$SSH_HINT'"

it "a refused key points at the key, not the firewall"
SSH_HINT="$(direct_ssh_hint 'jeff@34.83.189.193: Permission denied (publickey).' 34.83.189.193)"
if grep -qF 'google_compute_engine' <<<"$SSH_HINT" && ! grep -qF 'tcp:22' <<<"$SSH_HINT"; then pass
else fail "hint: '$SSH_HINT'"; fi

it "an unrecognised failure gets no hint rather than a wrong one"
eq "$(direct_ssh_hint 'kex_exchange_identification: read: Connection reset by peer' 34.83.189.193)" ""

# Nothing executes the orchestrator (it runs against live GCP), so a syntax
# error in it would first show up as a dead release leg.
it "the linux-leg orchestrator parses"
bash -n scripts/build-platforms-gcp-iap.sh 2>/dev/null && pass || fail "bash -n rejects scripts/build-platforms-gcp-iap.sh"

it "the mac-leg orchestrator parses"
bash -n scripts/build-platforms-tailnet.sh 2>/dev/null && pass || fail "bash -n rejects scripts/build-platforms-tailnet.sh"

# The mac leg never reclaimed at all, so target/ on the Air grew to 93GB with
# nothing to catch it (2026-09-25). Like the linux leg it must prune BEFORE
# building when short (after the sync, so the gc dispatch is this run's) and
# AFTER every build. Asserted on the source: the leg only runs against the Air.
it "the mac leg reclaims before building when short, and after every build"
MAC_BUILD="$(sed -n '/^build_mac(){/,/^}/p' scripts/build-platforms-tailnet.sh | sed 's/#.*//')"
MAC_SYNC="$(grep -n 'sync_src "\$hp"' <<<"$MAC_BUILD" | head -1 | cut -d: -f1)"
MAC_PRE="$(grep -n 'disk_needs_reclaim' <<<"$MAC_BUILD" | head -1 | cut -d: -f1)"
MAC_ESAC="$(grep -n '^  esac' <<<"$MAC_BUILD" | tail -1 | cut -d: -f1)"
MAC_POST="$(grep -n 'mac_reclaim "\$hp"' <<<"$MAC_BUILD" | tail -1 | cut -d: -f1)"
# The reclaim's own body, comments stripped: the comment above it names the
# dispatch too, so a file-wide grep passed with the ssh line gone.
MAC_RECLAIM="$(sed -n '/^mac_reclaim(){/,/^}/p' scripts/build-platforms-tailnet.sh | sed 's/#.*//')"
if [ -n "$MAC_SYNC" ] && [ -n "$MAC_PRE" ] && [ -n "$MAC_ESAC" ] && [ -n "$MAC_POST" ] \
   && [ "$MAC_SYNC" -lt "$MAC_PRE" ] && [ "$MAC_POST" -gt "$MAC_ESAC" ] \
   && grep -q 'ssh .*build-remote.sh gc' <<<"$MAC_RECLAIM"; then pass
else fail "sync@${MAC_SYNC:-none} pre-check@${MAC_PRE:-none} esac@${MAC_ESAC:-none} post-reclaim@${MAC_POST:-none}"; fi

# =============================================================================
group "iap-helpers.sh -- background process-tree reaping"
# =============================================================================

it "an empty pid is a no-op that succeeds"
kill_proc_tree "" && pass || fail "rc=$? for an empty pid"

# 0 or 1 would have the tree walk return nearly every process on the host, and
# -1 is `kill -KILL -1`. Run against stubs, never the real kill: a regression
# here must not take the suite's own host down with it.
it "a pid that names no job (0, 1, -1, garbage) signals nothing"
KPT_SIGNALLED="$(
  kill(){ echo "kill $*"; }
  taskkill(){ echo "taskkill $*"; }
  ps(){ printf '%s\n' '    5     1' '    6     5'; }
  for p in 0 1 -1 abc '5;6'; do kill_proc_tree "$p"; done
)"
eq "$KPT_SIGNALLED" ""

it "a plain background job is killed and reaped"
sleep 60 & KPT_PID=$!
kill_proc_tree "$KPT_PID"; KPT_RC=$?
if [ "$KPT_RC" = "0" ] && ! kill -0 "$KPT_PID" 2>/dev/null; then pass
else fail "rc=$KPT_RC, pid $KPT_PID still alive"; kill "$KPT_PID" 2>/dev/null; fi

# The tunnel retry path reaps a gcloud that has usually exited already (a 4003
# from a still-booting guest), so a dead pid must be a quiet success.
it "a job that has already exited is not an error"
true & KPT_PID=$!
sleep 1
kill_proc_tree "$KPT_PID" && pass || fail "rc=$? for an exited job"

# The real shape of the leak: the scoop gcloud shim is a /bin/sh script
# running `cmd.exe /C gcloud.cmd`, and $! is that sh. This shim is built the
# same way, and its cmd.exe leaf appends to a file about once a second -- so
# the leaf is alive exactly as long as the file keeps growing. A plain `kill`
# of the sh leaves it growing (the python.exe tunnels of the 2026-09-25 03:52
# and 03:56 runs were still alive ten hours later). The loop is bounded, so a
# regression leaks a process for a minute, not forever.
it "a scoop-shaped shim tree (sh -> cmd.exe -> native leaf) is reaped whole"
if [ -r "/proc/$$/winpid" ] && command -v cmd.exe >/dev/null 2>&1 \
   && command -v taskkill >/dev/null 2>&1 && command -v cygpath >/dev/null 2>&1; then
  KPT_DIR="$SUITE_TMP/kpt"
  mkdir -p "$KPT_DIR"
  printf '@for /L %%%%i in (1,1,60) do @(echo x>>"%%~dp0beat.txt" & ping -n 2 127.0.0.1 >nul)\r\n' \
    > "$KPT_DIR/leaf.cmd"
  printf '#!/bin/sh\nMSYS2_ARG_CONV_EXCL=/C cmd.exe /C "%s" "$@"\n' "$(cygpath -w "$KPT_DIR/leaf.cmd")" \
    > "$KPT_DIR/shim"
  chmod +x "$KPT_DIR/shim"
  "$KPT_DIR/shim" >/dev/null 2>&1 &
  KPT_PID=$!
  for _ in $(seq 1 120); do [ -s "$KPT_DIR/beat.txt" ] && break; sleep 0.25; done
  if [ ! -s "$KPT_DIR/beat.txt" ]; then
    kill_proc_tree "$KPT_PID"
    fail "the cmd.exe leaf never wrote its first beat within 30s -- the fixture did not start"
  else
    kill_proc_tree "$KPT_PID"
    sleep 0.5
    KPT_B1="$(wc -c <"$KPT_DIR/beat.txt" | tr -d ' ')"
    sleep 3
    KPT_B2="$(wc -c <"$KPT_DIR/beat.txt" | tr -d ' ')"
    if [ "$KPT_B1" = "$KPT_B2" ]; then pass
    else fail "the cmd.exe under the shim kept running after kill_proc_tree ($KPT_B1 -> $KPT_B2 bytes)"; fi
  fi
else
  skip "no MSYS/Cygwin process tree here (the POSIX tree case below covers Linux/macOS)"
fi

# The same guarantee off Windows, where there is no winpid and the tree comes
# from a ps snapshot. A real tree, not `exec sleep`: exec leaves ONE process,
# which even a plain kill of the job reaches -- the shape under which the old
# kill passed while orphaning everything below the job. Each level records its
# own pid (the leaf's exec keeps the one it wrote) and the trailing `:` keeps
# each sh alive as its child's parent. Survivors are counted by pid rather than
# by name: a multicall sleep (uutils, busybox) picks its utility from argv[0],
# and pgrep is not on every host.
it "a sh -> sh -> sleep tree is reaped whole off Windows"
if [ ! -r "/proc/$$/winpid" ]; then
  KPT_DIR="$SUITE_TMP/kpt-posix"
  mkdir -p "$KPT_DIR"
  printf '#!/bin/sh\necho $$ >>"$(dirname "$0")/pids"\n"$(dirname "$0")/mid"\n:\n' > "$KPT_DIR/shim"
  printf '#!/bin/sh\necho $$ >>"$(dirname "$0")/pids"\nsh -c '"'"'echo $$ >>"$1"; exec sleep 60'"'"' sh "$(dirname "$0")/pids"\n:\n' > "$KPT_DIR/mid"
  chmod +x "$KPT_DIR/shim" "$KPT_DIR/mid"
  : >"$KPT_DIR/pids"
  "$KPT_DIR/shim" >/dev/null 2>&1 &
  KPT_PID=$!
  for _ in $(seq 1 40); do [ "$(wc -l <"$KPT_DIR/pids" 2>/dev/null | tr -d ' ')" = "3" ] && break; sleep 0.25; done
  KPT_TREE="$(wc -l <"$KPT_DIR/pids" 2>/dev/null | tr -d ' ')"
  kill_proc_tree "$KPT_PID"
  # Signals land asynchronously; a real survivor (a 60s sleep) is still there.
  KPT_LEFT=""
  for _ in 1 2 3 4 5; do
    KPT_LEFT=""
    for p in $(cat "$KPT_DIR/pids"); do kill -0 "$p" 2>/dev/null && KPT_LEFT="$KPT_LEFT $p"; done
    [ -z "$KPT_LEFT" ] && break
    sleep 1
  done
  if [ "$KPT_TREE" = "3" ] && [ -z "$KPT_LEFT" ]; then pass
  else fail "tree of ${KPT_TREE:-0}/3 started; still alive:${KPT_LEFT:- none}"; for p in $KPT_LEFT; do kill -KILL "$p" 2>/dev/null; done; fi
else
  skip "MSYS/Cygwin: the scoop-shaped case above covers this host"
fi

# =============================================================================
group "iap-helpers.sh -- VM start: zone capacity, quota, fallback machine types"
# =============================================================================
# gcloud's stderr for the start that failed six times on 2026-09-30, verbatim,
# and fed CRLF-terminated as gcloud on Windows prints it. The v0.17.1 release
# stopped on this with the text thrown away and the log guessing.
VMS_EXHAUSTED="$(cat <<'EOF'
Starting instance(s) yaw-linux-builder...
................................failed.
ERROR: (gcloud.compute.instances.start) ---
code: ZONE_RESOURCE_POOL_EXHAUSTED
errorDetails:
- help:
    links:
    - description: Troubleshooting documentation
      url: https://cloud.google.com/compute/docs/resource-error
- localizedMessage:
    locale: en-US
    message: A e2-highmem-4 VM instance is currently unavailable in the us-west1-b
      zone. Alternatively, you can try your request again with a different VM hardware
      configuration or at a later time. For more information, see the troubleshooting
      documentation.
- errorInfo:
    domain: compute.googleapis.com
    metadatas:
      attachment: ''
      vmType: e2-highmem-4
      zone: us-west1-b
      zonesAvailable: ''
    reason: resource_availability
message: The zone 'projects/yaw-labs-prod/zones/us-west1-b' does not have enough resources
  available to fulfill the request.  Try a different zone, or try again later.
EOF
)"
VMS_EXHAUSTED_CR="$(sed 's/$/\r/' <<<"$VMS_EXHAUSTED")"
# The other capacity code, with a bare (unquoted) zonesAvailable: the body of
# the 2026-09-28 13:48 operation in this project's operations log, laid out as
# gcloud printed today's.
VMS_DETAILS="$(cat <<'EOF'
Starting instance(s) typed-arm-probe...
................................failed.
ERROR: (gcloud.compute.instances.start) ---
code: ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS
errorDetails:
- help:
    links:
    - description: Troubleshooting documentation
      url: https://cloud.google.com/compute/docs/resource-error
- localizedMessage:
    locale: en-US
    message: A t2a-standard-2 VM instance is currently unavailable in the us-central1-f
      zone. Consider trying your request in the us-central1-b, us-central1-a zone(s),
      which currently has capacity to accommodate your request. Alternatively,
      you can try your request again with a different VM hardware configuration
      or at a later time. For more information, see the troubleshooting documentation.
- errorInfo:
    domain: compute.googleapis.com
    metadatas:
      attachment: ''
      vmType: t2a-standard-2
      zone: us-central1-f
      zonesAvailable: us-central1-b,us-central1-a
    reason: resource_availability
message: The zone 'projects/yaw-labs-prod/zones/us-central1-f' does not have enough
  resources available to fulfill the request.  'NULL:0/NULL:0/NULL:0 (state:STOCKOUT,
  sub-state:STOCKOUT, resource type:compute)'.
EOF
)"
VMS_DETAILS_CR="$(sed 's/$/\r/' <<<"$VMS_DETAILS")"
# The HTTP-error shape gcloud prints for a synchronous refusal, with the
# trailing blank line it ends on. Provenance, since the group's premise is
# gcloud's own words: NOTFOUND is verbatim off a describe on this box
# (2026-09-30); PERM is that text with start's verb (the wrapper is the same);
# HTTP_CAPACITY is today's operation message in that wrapper; QUOTA, BADTYPE
# and BACKEND are constructed from the documented API messages (none has ever
# been logged here, and us-west1's CPU quotas leave no way to provoke one);
# AUTH is the SDK's own text (core/credentials/store.py, exceptions.py:126);
# INTERRUPTED is core/util/keyboard_interrupt.py:36.
VMS_HTTP_CAPACITY=$'ERROR: (gcloud.compute.instances.start) Could not fetch resource:\r\n - The zone \'projects/yaw-labs-prod/zones/us-central1-a\' does not have enough resources available to fulfill the request.  Try a different zone, or try again later.\r\n\r\n'
VMS_QUOTA=$'ERROR: (gcloud.compute.instances.start) Could not fetch resource:\r\n - Quota \'N2_CPUS\' exceeded.  Limit: 100.0 in region us-west1.\r\n\r\n'
VMS_PERM=$'ERROR: (gcloud.compute.instances.start) Could not fetch resource:\r\n - Required \'compute.instances.start\' permission for \'projects/yaw-labs-prod/zones/us-west1-b/instances/yaw-linux-builder\'\r\n\r\n'
VMS_NOTFOUND=$'ERROR: (gcloud.compute.instances.describe) Could not fetch resource:\r\n - The resource \'projects/yaw-labs-prod/zones/us-west1-b/instances/no-such-vm\' was not found\r\n\r\n'
VMS_AUTH=$'ERROR: (gcloud.compute.instances.start) There was a problem refreshing your current auth tokens: (\'invalid_grant: Token has been expired or revoked.\', {\'error\': \'invalid_grant\'})\r\nPlease run:\r\n\r\n  $ gcloud auth login\r\n\r\nto obtain new access credentials.\r\n'
VMS_BADTYPE=$'ERROR: (gcloud.compute.instances.set-machine-type) Could not fetch resource:\r\n - Invalid value for field \'resource.machineType\': \'zones/us-west1-b/machineTypes/c3-highmem-4\'. Machine type with name \'c3-highmem-4\' does not exist in zone \'us-west1-b\'.\r\n\r\n'
# What gcloud prints on Windows for a Ctrl-C (core/util/keyboard_interrupt.py),
# where it then exits 2 instead of dying of the signal.
VMS_INTERRUPTED=$'Starting instance(s) yaw-linux-builder...\r\n\r\n\r\nCommand killed by keyboard interrupt\r\n'
VMS_BACKEND=$'ERROR: (gcloud.compute.instances.start) Could not fetch resource:\r\n - Internal error. Please try again or contact support.\r\n\r\n'

it "today's exhausted-zone stderr, CRs and all, reads as capacity"
eq "$(vm_start_verdict "$VMS_EXHAUSTED_CR")" "capacity"

it "the _WITH_DETAILS exhaustion of 2026-09-28 reads as capacity too"
eq "$(vm_start_verdict "$VMS_DETAILS_CR")" "capacity"

it "the HTTP-error shape of the capacity message reads as capacity"
eq "$(vm_start_verdict "$VMS_HTTP_CAPACITY")" "capacity"

it "a CPU-family quota reads as quota, not capacity"
eq "$(vm_start_verdict "$VMS_QUOTA")" "quota"

it "a missing permission and a missing instance are permanent, in the shape gcloud prints them"
eq "$(vm_start_verdict "$VMS_PERM")-$(vm_start_verdict "$VMS_NOTFOUND")" "permanent-permanent"

it "expired credentials are permanent -- no retry refreshes them"
eq "$(vm_start_verdict "$VMS_AUTH")" "permanent"

it "a machine type the API rejects is unsupported: out for the run, not the end of it"
eq "$(vm_start_verdict "$VMS_BADTYPE")" "unsupported"

it "a Ctrl-C that gcloud turned into an exit reads as interrupted, above every other verdict"
eq "$(vm_start_verdict "$VMS_INTERRUPTED")-$(vm_start_verdict "$VMS_INTERRUPTED$VMS_EXHAUSTED_CR")" "interrupted-interrupted"

it "a backend error, a bare failed line and empty text are unknown -- retried, never fatal"
# A variable, not an inline $'...': beside the '' below, inside one "$(...)",
# bash mis-nests the quotes and skips the whole line with a parse error.
VMS_FAILED_LINE=$'................failed.\r\n'
eq "$(vm_start_verdict "$VMS_BACKEND")-$(vm_start_verdict "$VMS_FAILED_LINE")-$(vm_start_verdict "")" "unknown-unknown-unknown"

it "the operation error's folded message comes back as one CR-free line, not the localized one"
eq "$(gcloud_error_message "$VMS_EXHAUSTED_CR")" \
   "The zone 'projects/yaw-labs-prod/zones/us-west1-b' does not have enough resources available to fulfill the request.  Try a different zone, or try again later."

it "a message folded over three lines joins the same way"
eq "$(gcloud_error_message "$VMS_DETAILS_CR")" \
   "The zone 'projects/yaw-labs-prod/zones/us-central1-f' does not have enough resources available to fulfill the request.  'NULL:0/NULL:0/NULL:0 (state:STOCKOUT, sub-state:STOCKOUT, resource type:compute)'."

it "the HTTP-error shape yields its bullet, trailing blank line and all"
eq "$(gcloud_error_message "$VMS_NOTFOUND")" \
   "The resource 'projects/yaw-labs-prod/zones/us-west1-b/instances/no-such-vm' was not found"

it "text with neither shape yields its last non-blank line, CR stripped"
eq "$(gcloud_error_message $'ERROR: something else\r\n\r\n')" "ERROR: something else"

it "empty text yields no message, non-zero"
gcloud_error_message "" >/dev/null 2>&1 && fail "invented a message" || pass

it "a quoted, empty zonesAvailable is a clean miss"
vm_start_zones_available "$VMS_EXHAUSTED_CR" >/dev/null 2>&1 && fail "read zones off an empty field" || pass

it "a bare zonesAvailable list comes back as it is, CR-free"
eq "$(vm_start_zones_available "$VMS_DETAILS_CR")" "us-central1-b,us-central1-a"

it "a quoted zonesAvailable list loses its quotes"
eq "$(vm_start_zones_available $'    metadatas:\r\n      zonesAvailable: \'us-west1-a,us-west1-c\'\r\n    reason: resource_availability\r')" "us-west1-a,us-west1-c"

it "the VM's own type is walked first, then the fallbacks in order"
eq "$(vm_start_types $'e2-highmem-4\r' "n2-highmem-4 n2d-highmem-4" | tr '\n' ' ')" "e2-highmem-4 n2-highmem-4 n2d-highmem-4 "

it "a fallback that repeats the VM's type, or another fallback, is walked once"
eq "$(vm_start_types e2-highmem-4 "n2-highmem-4 e2-highmem-4 n2-highmem-4 n1-highmem-4" | tr '\n' ' ')" "e2-highmem-4 n2-highmem-4 n1-highmem-4 "

it "an empty fallback list walks the VM's own type alone"
eq "$(vm_start_types e2-highmem-4 "" | tr '\n' ' ')" "e2-highmem-4 "

it "a blank current type is refused -- there would be nothing to restore to"
vm_start_types "" "n2-highmem-4" >/dev/null 2>&1 && fail "walked types with no original" || pass

# --- zone fallback: where a builder whose zone stayed out of capacity goes ----
# A `zones list --format='value(name,status,region.basename())'` reading for
# us-west1, tab-separated and CRLF-terminated as gcloud on Windows prints it,
# with one zone DOWN: a clone must never be tried there.
VMS_ZONES=$'us-west1-a\tUP\tus-west1\r\nus-west1-b\tUP\tus-west1\r\nus-west1-c\tUP\tus-west1\r\nus-west1-d\tDOWN\tus-west1\r\n'

it "the region's other UP zones are the candidates, in list order, never the builder's own"
eq "$(zone_fallback_candidates us-west1-b "" us-west1 "$VMS_ZONES" | tr '\n' ' ')" "us-west1-a us-west1-c "

it "a zone of another region in the reading is not a candidate"
eq "$(zone_fallback_candidates us-west1-b "" us-west1 "$VMS_ZONES"$'us-east1-b\tUP\tus-east1\r\n' | tr '\n' ' ')" "us-west1-a us-west1-c "

it "an operator's OAM_GCP_FALLBACK_ZONES wins over the reading, minus the builder's zone and repeats"
eq "$(zone_fallback_candidates us-west1-b "us-west1-c us-west1-b us-west1-a us-west1-c" us-west1 "$VMS_ZONES" | tr '\n' ' ')" "us-west1-c us-west1-a "

it "no candidate at all is a clean miss, non-zero"
zone_fallback_candidates us-west1-b "" us-west1 $'us-west1-b\tUP\tus-west1\r\nus-west1-c\tDOWN\tus-west1\r\n' >/dev/null 2>&1 && fail "invented a zone" || pass

it "zone names are checked by shape, so a typo in OAM_GCP_FALLBACK_ZONES fails before any VM is touched"
VMS_ZONE_SHAPES=""
for z in us-west1-c northamerica-northeast1-a $'us-west1-c\r'; do gce_zone_name_valid "$z" || VMS_ZONE_SHAPES="$VMS_ZONE_SHAPES refused:${z//$'\r'/CR}"; done
for z in us-west1 US-WEST1-C 'us-west1-c;rm' 'us-west1-c us-west1-a' ''; do gce_zone_name_valid "$z" && VMS_ZONE_SHAPES="$VMS_ZONE_SHAPES accepted:'$z'"; done
if [ -z "$VMS_ZONE_SHAPES" ]; then pass; else fail "wrong verdicts:$VMS_ZONE_SHAPES"; fi

# --- wiring: the orchestrator must decide through the lib ----------------------
# Comment-stripped: the block above the loop describes the old loop, and a
# negative grep must not match a description.
VMS_SRC="$(sed 's/#.*//' scripts/build-platforms-gcp-iap.sh)"

it "build-platforms-gcp-iap.sh starts the VM through the lib's verdicts and type walk"
VMS_WIRE=""
for want in 'vm_start_types "$ORIGINAL_MACHINE_TYPE"' 'vm_start_verdict "$start_err"' \
            'vm_start_verdict "$set_err"' 'gcloud_error_message "$start_err"' \
            'vm_start_zones_available "$start_err"' \
            '${OAM_GCP_FALLBACK_MACHINE_TYPES-' 'OAM_VM_START_BUDGET_S:-1800' \
            'remote_step_verdict "$attempt" "$REMOTE_STEP_ATTEMPTS"' 'reconnect_builder' \
            'start_vm_walk "$VM_STATUS"' 'restart_builder "$status"' 'OAM_GCP_ZONE_FALLBACK:-1' \
            'gce_zone_name_valid "$fz"' 'zone_fallback_candidates "$origin_zone" "$FALLBACK_ZONES"' \
            'machine-images create "$img" --source-instance="$origin"' '--source-machine-image="$img"' \
            '--filter="name~^${INSTANCE}\$"' "trap 'stop_vm; restore_machine_type || true; delete_machine_image' EXIT" \
            'cleanup() { stop_iap_tunnel; reattach_stop_schedules; stop_vm; restore_machine_type || true; delete_machine_image; }' \
            'vm_start_verdict "$img_err"' 'clone_leftover "$clone" "$zone"' 'restore_machine_type || origin_left="$UNRESTORED_TYPE"' \
            'remote_step_postmortem "$log" "$attempt" "$reruns" "$restarts"' '[ "${fz%-*}" = "${ZONE%-*}" ]'; do
  grep -qF -- "$want" <<<"$VMS_SRC" || VMS_WIRE="$VMS_WIRE [$want]"
done
if [ -z "$VMS_WIRE" ]; then pass; else fail "build-platforms-gcp-iap.sh no longer carries:$VMS_WIRE"; fi

# The reconnect and the restart are CALLED from the retry path, not merely
# defined: a bare `reconnect_builder` line on its own, and `restart_builder
# "$status"`, inside remote_step_run.
it "remote_step_run reconnects, or restarts the VM, before it runs a dropped step again"
VMS_RUN_BODY="$(awk '/^remote_step_run\(\)/ { f = 1 } f { print } f && /^}/ { exit }' scripts/build-platforms-gcp-iap.sh)"
if grep -qE '^[[:space:]]+reconnect_builder[[:space:]]*$' <<<"$VMS_RUN_BODY" \
   && grep -qE '^[[:space:]]+restart_builder "\$status"[[:space:]]*$' <<<"$VMS_RUN_BODY" \
   && grep -qE 'remote_step_verdict .*"\$rc"' <<<"$VMS_RUN_BODY" \
   && grep -qE '\|\| rc=\$\?' <<<"$VMS_RUN_BODY"; then pass
else fail "remote_step_run must capture ssh's exit into rc, decide with it, and call reconnect_builder or restart_builder before the next attempt; body:$(printf '\n  %s' "$VMS_RUN_BODY")"; fi

it "the VM start neither discards gcloud's stderr nor retries a fixed six times"
if grep -E 'instances start ' <<<"$VMS_SRC" | grep -q '2>/dev/null' \
   || ! grep -E 'instances start ' <<<"$VMS_SRC" | grep -q '2>&1' \
   || grep -q 'for VM_ATTEMPT in 1 2 3 4 5 6' <<<"$VMS_SRC"; then
  fail "the blind six-attempt start loop is back"
else pass; fi

# The restore needs the VM TERMINATED, so the type change has to be undone by an
# EXIT trap armed BEFORE the first set-machine-type runs, and in cleanup() only
# after the stop. The trap the walk arms also stops what the walk started and
# deletes the zone fallback's image: the first start runs before cleanup() is
# the trap, and the fallback with it. Both guard the restore with `|| true`,
# since under `set -e` its non-zero status would end the trap right there.
# Asserted by line order, since that is the property.
it "the walk's trap (stop, restore, image) is armed before the first set-machine-type, and cleanup stops before it restores"
VMS_TRAP_LINE="$(grep -nF "trap 'stop_vm; restore_machine_type || true; delete_machine_image' EXIT" scripts/build-platforms-gcp-iap.sh | head -1 | cut -d: -f1)"
VMS_SET_LINE="$(grep -n -- '--machine-type="$vm_type"' scripts/build-platforms-gcp-iap.sh | head -1 | cut -d: -f1)"
VMS_STOP_DEF_LINE="$(grep -n '^stop_vm() {' scripts/build-platforms-gcp-iap.sh | head -1 | cut -d: -f1)"
VMS_IMG_DEF_LINE="$(grep -n '^delete_machine_image() {' scripts/build-platforms-gcp-iap.sh | head -1 | cut -d: -f1)"
if [ -n "$VMS_TRAP_LINE" ] && [ -n "$VMS_SET_LINE" ] && [ "$VMS_TRAP_LINE" -lt "$VMS_SET_LINE" ] \
   && [ -n "$VMS_STOP_DEF_LINE" ] && [ "$VMS_STOP_DEF_LINE" -lt "$VMS_TRAP_LINE" ] \
   && [ -n "$VMS_IMG_DEF_LINE" ] && [ "$VMS_IMG_DEF_LINE" -lt "$VMS_TRAP_LINE" ] \
   && grep -qF 'stop_vm; restore_machine_type || true; delete_machine_image; }' scripts/build-platforms-gcp-iap.sh; then pass
else fail "order is stop_vm@${VMS_STOP_DEF_LINE:-?} delete_machine_image@${VMS_IMG_DEF_LINE:-?} trap@${VMS_TRAP_LINE:-?} set-machine-type@${VMS_SET_LINE:-?}; the trap's functions must be defined above it, and cleanup must run stop_vm, then restore_machine_type || true, then delete_machine_image"; fi

# =============================================================================
group "build-platforms-gcp-iap.sh -- the start loop against a stubbed gcloud"
# =============================================================================
# The loop itself, run for real under the script's `set -euo pipefail`, with
# gcloud and ssh replaced by stubs that log every call and answer from a state
# directory: a start on the "good" type turns the VM RUNNING, any other start
# prints today's stderr and fails, a stop turns it TERMINATED, set-machine-type
# records the type. Nothing here reaches GCP. TMPDIR is private to the run.
VMS_BIN="$SUITE_TMP/vms-bin"; VMS_TMP="$SUITE_TMP/vms-tmp"; VMS_STATE="$SUITE_TMP/vms-state"
mkdir -p "$VMS_BIN" "$VMS_TMP" "$VMS_STATE"
# CR on every line, as gcloud prints it: sed into the file, since printf '%s\r\n'
# would put one CR after the last line only, and $(...) drops a trailing CRLF.
sed 's/$/\r/' <<<"$VMS_EXHAUSTED" > "$VMS_STATE/exhausted.txt"
printf '%s' "$VMS_QUOTA" > "$VMS_STATE/quota.txt"
cat > "$VMS_BIN/gcloud" <<EOF
#!/bin/bash
S="$VMS_STATE"
printf '%s\n' "\$*" >> "\$S/log"
a="\$*"
# State reads are bash builtins, not cat: under a full ci-local.sh run one
# forked cat came back empty (2026-10-04), and the orchestrator read that as
# an unreadable VM. The orchestrator now re-asks a blank describe; the stub
# still does not fork for a one-line file. blank-status-once makes the next
# status describe answer blank, exit 0 -- the shape that bit.
rd(){ local l; IFS= read -r l < "\$1" || true; printf '%s\n' "\$l"; }
# Once the zone fallback has made a clone, a call that names it reads and
# writes the clone's own status, type and IP; every other call is the
# original's, which the clone must leave exactly as it was.
ST="\$S/status"; TY="\$S/type"; IP=203.0.113.9
if [ -e "\$S/clone" ]; then
  case "\$a" in *" \$(rd "\$S/clone") "*) ST="\$S/clone-status"; TY="\$S/clone-type"; IP=203.0.113.10 ;; esac
fi
# An instance that moved zones: nothing answers in the old zone but the list.
if [ -e "\$S/moved" ]; then
  case "\$a" in
    *"instances list"*"name~^yaw-linux-builder\$"*) printf 'us-west1-c\r\n'; exit 0 ;;
    *"--zone=us-west1-b"*)
      printf 'ERROR: (gcloud.compute.instances.describe) Could not fetch resource:\r\n - The resource '"'"'projects/yaw-labs-prod/zones/us-west1-b/instances/yaw-linux-builder'"'"' was not found\r\n\r\n' >&2; exit 1 ;;
  esac
fi
case "\$a" in
  *"instances describe"*"value(name)"*)                 echo yaw-linux-builder ;;
  *"instances describe"*"value(status)"*)
    if [ -e "\$S/blank-status-once" ]; then rm -f "\$S/blank-status-once"; exit 0; fi
    # stopping-for=<n>: the next n status reads of the ORIGINAL answer STOPPING
    # (a stop in progress -- the previous run's --async stop, a schedule's),
    # then the state file speaks again. A clone's reads never go through it.
    if [ "\$ST" = "\$S/status" ] && [ -e "\$S/stopping-for" ]; then
      n="\$(rd "\$S/stopping-for")"
      if [ "\${n:-0}" -gt 0 ] 2>/dev/null; then echo \$((n - 1)) > "\$S/stopping-for"; echo STOPPING; exit 0; fi
    fi
    rd "\$ST" ;;
  *"instances describe"*"machineType.basename()"*)      rd "\$TY" ;;
  *"instances describe"*"resourcePolicies"*)            echo ;;
  *"instances describe"*"natIP"*)                       echo "\$IP" ;;
  # The preflight's lookup for a clone an earlier run left behind: one,
  # stopped in us-west1-a, when leftover-clone says so; else nothing.
  *"instances list"*"-[a-z]+-[a-z]+[0-9]+-[a-z]\$"*)
    if [ -e "\$S/leftover-clone" ]; then printf 'yaw-linux-builder-us-west1-a\tus-west1-a\tTERMINATED\r\n'; fi ;;
  *"instances set-machine-type"*)
    t="\${a##*--machine-type=}"; t="\${t%% *}"
    # reject-set-<type>: the API refuses the type. interrupt-set-<type>: the
    # change is applied, then gcloud reports a Ctrl-C the way Windows sees it.
    if [ -e "\$S/reject-set-\$t" ]; then
      printf "ERROR: (gcloud.compute.instances.set-machine-type) Could not fetch resource:\r\n - Invalid value for field 'resource.machineType': 'zones/us-west1-b/machineTypes/\$t'.\r\n\r\n" >&2; exit 1
    fi
    echo "\$t" > "\$TY"
    if [ -e "\$S/interrupt-set-\$t" ]; then printf '\n\nCommand killed by keyboard interrupt\n' >&2; exit 2; fi ;;
  *"instances start"*)
    if [ "\$(rd "\$TY")" = "\$(rd "\$S/good-type")" ]; then echo RUNNING > "\$ST"
    elif [ -e "\$S/start-quota" ]; then cat "\$S/quota.txt" >&2; exit 1
    else cat "\$S/exhausted.txt" >&2; exit 1; fi ;;
  *"instances stop"*)              echo 'Stopping instance(s) yaw-linux-builder...' >&2; echo TERMINATED > "\$ST" ;;
  *"get-serial-port-output"*)      echo 'Started ssh.service - OpenBSD Secure Shell server.' ;;
  # The postmortem's question after a drop -- the newest stop operation on
  # the instance, insertTime and user -- answered as the 2026-09-25 schedule
  # stop read in the operations log.
  *"operations list"*)
    printf '2026-10-04T03:00:00.000-07:00\tservice-123456789@compute-system.iam.gserviceaccount.com\r\n' ;;
  # The zone fallback. zones=<names> is what \`zones list\` reports, each UP in
  # us-west1 unless suffixed :DOWN; machine-images create records the image
  # (interrupt-image: records it, then reports a Ctrl-C the way Windows sees
  # it); instances create succeeds only in the zone clone-ok-zone names, and
  # from then on the clone exists -- RUNNING, on the type asked for -- for
  # every later call that names it; in the zone interrupt-create-zone names
  # the clone comes up the same way but gcloud reports a Ctrl-C (the request
  # went through, the answer did not); deny-create refuses every create for a
  # missing permission; elsewhere the create is refused for capacity, and
  # with leftover-clone set, us-west1-a already holds the earlier clone.
  *"zones list"*)
    for z in \$(rd "\$S/zones" 2>/dev/null); do
      st=UP; case "\$z" in *:*) st="\${z#*:}"; z="\${z%%:*}" ;; esac
      printf '%s\t%s\tus-west1\r\n' "\$z" "\$st"
    done ;;
  *"machine-images create"*)
    i="\${a##*machine-images create }"; i="\${i%% *}"; echo "\$i" > "\$S/image"
    if [ -e "\$S/interrupt-image" ]; then printf '\n\nCommand killed by keyboard interrupt\n' >&2; exit 2; fi
    echo "Created [https://www.googleapis.com/compute/v1/projects/yaw-labs-prod/global/machineImages/\$i]." >&2 ;;
  *"machine-images delete"*)       rm -f "\$S/image" ;;
  *"instances create"*)
    n="\${a##*instances create }"; n="\${n%% *}"
    z="\${a##*--zone=}"; z="\${z%% *}"
    t="\${a##*--machine-type=}"; t="\${t%% *}"
    if [ -e "\$S/leftover-clone" ] && [ "\$n" = "yaw-linux-builder-us-west1-a" ]; then
      printf "ERROR: (gcloud.compute.instances.create) Could not fetch resource:\r\n - The resource 'projects/yaw-labs-prod/zones/us-west1-a/instances/\$n' already exists\r\n\r\n" >&2; exit 1
    elif [ -e "\$S/deny-create" ]; then
      printf "ERROR: (gcloud.compute.instances.create) Could not fetch resource:\r\n - Required 'iam.serviceAccounts.actAs' permission for 'projects/yaw-labs-prod/serviceAccounts/123456789-compute@developer.gserviceaccount.com'\r\n\r\n" >&2; exit 1
    elif [ -e "\$S/interrupt-create-zone" ] && [ "\$z" = "\$(rd "\$S/interrupt-create-zone")" ]; then
      echo "\$n" > "\$S/clone"; echo RUNNING > "\$S/clone-status"; echo "\$t" > "\$S/clone-type"
      printf '\n\nCommand killed by keyboard interrupt\n' >&2; exit 2
    elif [ -e "\$S/clone-ok-zone" ] && [ "\$z" = "\$(rd "\$S/clone-ok-zone")" ]; then
      echo "\$n" > "\$S/clone"; echo RUNNING > "\$S/clone-status"; echo "\$t" > "\$S/clone-type"
    else cat "\$S/exhausted.txt" >&2; exit 1; fi ;;
  *) echo "stub gcloud: unexpected call: \$a" >&2; exit 97 ;;
esac
EOF
# ssh: by default one permanent failure, so a run ends at the connect step
# with the start walk already judged. ssh-mode=builder answers as the builder
# would -- the probe's `true`, the sync's extract, df, each build-remote.sh
# dispatch -- and drops the next ssh-drop dispatches the way the 2026-09-25
# schedule stop looked from here (OpenSSH's closing line, exit 255, the VM
# TERMINATED behind it -- or in the state drop-status names, one word per
# drop in turn; drop-stopping-for=<n> arms the gcloud stub's stopping-for at
# the drop, for a stop still in progress behind it); fail-step-<dispatch>
# makes that dispatch exit 1, a real failure that ends a run where a case
# wants it to. Every call lands in the same log as gcloud's, so an order can
# be asserted across both.
cat > "$VMS_BIN/ssh" <<EOF
#!/bin/bash
S="$VMS_STATE"
printf 'ssh %s\n' "\$*" >> "\$S/log"
rd(){ local l; IFS= read -r l < "\$1" || true; printf '%s\n' "\$l"; }
[ -e "\$S/ssh-mode" ] && [ "\$(rd "\$S/ssh-mode")" = "builder" ] || { echo 'Host key verification failed.' >&2; exit 255; }
cmd="\${@: -1}"
case "\$cmd" in
  true) exit 0 ;;
  "df -BG --output=avail /") printf 'Avail\n100G\n' ;;
  *"tar xzf"*) exit 0 ;;
  *build-remote.sh*)
    d="\${cmd##* }"
    left="\$(rd "\$S/ssh-drop" 2>/dev/null)"
    if [ -n "\$left" ] && [ "\$left" -gt 0 ]; then
      echo \$((left - 1)) > "\$S/ssh-drop"
      echo "[remote] \$d: running..."
      echo 'Connection to localhost closed by remote host.' >&2
      ds="\$(rd "\$S/drop-status" 2>/dev/null)"; st="\${ds%% *}"; [ -n "\$st" ] || st=TERMINATED
      case "\$ds" in *" "*) echo "\${ds#* }" > "\$S/drop-status" ;; *) rm -f "\$S/drop-status" ;; esac
      echo "\$st" > "\$S/status"
      if [ -e "\$S/drop-stopping-for" ]; then cp "\$S/drop-stopping-for" "\$S/stopping-for"; fi
      exit 255
    fi
    if [ -e "\$S/fail-step-\$d" ]; then echo "[remote] \$d FAILED"; exit 1; fi
    echo "[remote] \$d ok" ;;
  *) echo "stub ssh: unexpected command: \$cmd" >&2; exit 97 ;;
esac
EOF
# scp: the sync's upload, accepted and logged.
cat > "$VMS_BIN/scp" <<EOF
#!/bin/bash
printf 'scp %s\n' "\$*" >> "$VMS_STATE/log"
EOF
chmod +x "$VMS_BIN/gcloud" "$VMS_BIN/ssh" "$VMS_BIN/scp"
# vms_run <good-type|none> [flag...]  -- the orchestrator against the stubs,
# from a TERMINATED e2-highmem-4, with n2-highmem-4 the one fallback and no
# budget for a second pass. Flags are state files the stubs read: a bare name
# is an empty file (moved, start-quota, reject-set-<type>, interrupt-set-<type>,
# leftover-clone, fail-step-<dispatch>, interrupt-image, deny-create);
# name=value writes the value (zones=, clone-ok-zone=, interrupt-create-zone=,
# ssh-mode=builder, ssh-drop=<n>, drop-status=<states>, drop-stopping-for=<n>,
# stopping-for=<n>, and the knob files zone-fallback, step-attempts,
# fallback-zones, start-budget, keep-vm and keep-schedule, which feed
# OAM_GCP_ZONE_FALLBACK, OAM_REMOTE_STEP_ATTEMPTS, OAM_GCP_FALLBACK_ZONES,
# OAM_VM_START_BUDGET_S, OAM_KEEP_VM and OAM_KEEP_VM_SCHEDULE).
# The zone fallback is pinned OFF unless a case turns it on: the stub's default
# answers describe a same-zone walk, and the capacity cases assert that walk's
# own failure. Every OAM_* knob the orchestrator reads is pinned, so a shell
# that exports OAM_KEEP_VM=1 or another project cannot turn a run red. Stdout
# to VMS_OUT, stderr to VMS_ERR, status to VMS_RC, the stubs' call log to
# VMS_LOG.
# vms_knob <file> <default> -- the knob file's first line, else the default.
# The existence test comes first: a `read < missing` prints its error before a
# trailing 2>/dev/null has been applied.
vms_knob(){ local v; if [ -e "$VMS_STATE/$1" ] && IFS= read -r v < "$VMS_STATE/$1"; then printf '%s' "$v"; else printf '%s' "$2"; fi; }
vms_run(){
  rm -f "$VMS_STATE/log" "$VMS_STATE/moved" "$VMS_STATE/start-quota" "$VMS_STATE/blank-status-once" "$VMS_STATE"/reject-set-* "$VMS_STATE"/interrupt-set-* \
        "$VMS_STATE"/clone* "$VMS_STATE/image" "$VMS_STATE/zones" "$VMS_STATE/leftover-clone" "$VMS_STATE/ssh-mode" "$VMS_STATE/ssh-drop" "$VMS_STATE"/fail-step-* \
        "$VMS_STATE/zone-fallback" "$VMS_STATE/step-attempts" "$VMS_STATE/fallback-zones" \
        "$VMS_STATE/stopping-for" "$VMS_STATE/drop-status" "$VMS_STATE/drop-stopping-for" "$VMS_STATE/interrupt-image" "$VMS_STATE/deny-create" \
        "$VMS_STATE/interrupt-create-zone" "$VMS_STATE/start-budget" "$VMS_STATE/keep-vm" "$VMS_STATE/keep-schedule"
  echo TERMINATED > "$VMS_STATE/status"; echo e2-highmem-4 > "$VMS_STATE/type"
  echo "$1" > "$VMS_STATE/good-type"; shift
  local flag
  for flag in "$@"; do
    case "$flag" in *=*) printf '%s\n' "${flag#*=}" > "$VMS_STATE/${flag%%=*}" ;; *) : > "$VMS_STATE/$flag" ;; esac
  done
  VMS_OUT="$(PATH="$VMS_BIN:$PATH" TMPDIR="$VMS_TMP" OAM_GCP_FALLBACK_MACHINE_TYPES=n2-highmem-4 \
    OAM_VM_START_BUDGET_S="$(vms_knob start-budget 0)" OAM_IAP_SSH_MODE=direct OAM_GCP_BUILDER_ZONE=us-west1-b \
    OAM_GCP_PROJECT=yaw-labs-prod OAM_GCP_BUILDER_INSTANCE=yaw-linux-builder OAM_LINUX_USER=jeff \
    OAM_KEEP_VM="$(vms_knob keep-vm 0)" OAM_KEEP_VM_SCHEDULE="$(vms_knob keep-schedule 0)" OAM_LINUX_FAST=0 \
    OAM_GCP_ZONE_FALLBACK="$(vms_knob zone-fallback 0)" OAM_REMOTE_STEP_ATTEMPTS="$(vms_knob step-attempts 3)" \
    OAM_GCP_FALLBACK_ZONES="$(vms_knob fallback-zones '')" \
    bash scripts/build-platforms-gcp-iap.sh --mode=release 2>"$SUITE_TMP/vms-err")"
  VMS_RC=$?
  VMS_ERR="$(sed 's/\x1b\[[0-9;]*m//g' "$SUITE_TMP/vms-err")"
  VMS_LOG="$(cat "$VMS_STATE/log" 2>/dev/null)"
}

vms_run none
it "a zone that stays exhausted ends the run with gcloud's own words, every type tried, and the VM's own type put back"
if [ "$VMS_RC" != "0" ] && [ -z "$VMS_OUT" ] \
   && grep -qF "no e2-highmem-4 capacity in us-west1-b (pass 1): The zone 'projects/yaw-labs-prod/zones/us-west1-b' does not have enough resources available to fulfill the request.  Try a different zone, or try again later." <<<"$VMS_ERR" \
   && grep -qF "no n2-highmem-4 capacity in us-west1-b (pass 1)" <<<"$VMS_ERR" \
   && grep -qF "could not start yaw-linux-builder in us-west1-b within 0s (tried: e2-highmem-4 n2-highmem-4; last: The zone" <<<"$VMS_ERR" \
   && grep -qF "OAM_VM_START_BUDGET_S" <<<"$VMS_ERR" \
   && grep -qF "set yaw-linux-builder back to e2-highmem-4" <<<"$VMS_ERR" \
   && [[ "$(tail -1 <<<"$VMS_LOG")" == *"instances set-machine-type yaw-linux-builder"*"--machine-type=e2-highmem-4" ]] \
   && ! grep -q 'instances stop' <<<"$VMS_LOG"; then pass
else fail "rc=$VMS_RC stdout=[$VMS_OUT] log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

vms_run n2-highmem-4
it "a start that succeeds on a fallback says so, and the exit stops the VM before putting its type back"
VMS_STOP_LINE="$(grep -n 'instances stop' <<<"$VMS_LOG" | head -1 | cut -d: -f1)"
VMS_BACK_LINE="$(grep -n -- 'set-machine-type yaw-linux-builder .*--machine-type=e2-highmem-4' <<<"$VMS_LOG" | tail -1 | cut -d: -f1)"
if [ "$VMS_RC" != "0" ] && [ -z "$VMS_OUT" ] \
   && grep -qF "VM started as n2-highmem-4 (its own type is e2-highmem-4; set back when this run stops it)" <<<"$VMS_ERR" \
   && grep -qF "OAM_IAP_SSH_MODE=direct, but direct ssh to yaw-linux-builder did not answer: Host key verification failed." <<<"$VMS_ERR" \
   && grep -qF -- "-- if this is interrupted, run: gcloud compute instances stop yaw-linux-builder --zone=us-west1-b --project=yaw-labs-prod && gcloud compute instances set-machine-type yaw-linux-builder --zone=us-west1-b --project=yaw-labs-prod --machine-type=e2-highmem-4" <<<"$VMS_ERR" \
   && grep -qF "set yaw-linux-builder back to e2-highmem-4" <<<"$VMS_ERR" \
   && [ -n "$VMS_STOP_LINE" ] && [ -n "$VMS_BACK_LINE" ] && [ "$VMS_STOP_LINE" -lt "$VMS_BACK_LINE" ] \
   && ! grep -E 'instances stop.*--async' <<<"$VMS_LOG" >/dev/null \
   && [ "$(cat "$VMS_STATE/type")" = "e2-highmem-4" ] && [ "$(cat "$VMS_STATE/status")" = "TERMINATED" ]; then pass
else fail "rc=$VMS_RC stop@${VMS_STOP_LINE:-?} restore@${VMS_BACK_LINE:-?} type=$(cat "$VMS_STATE/type") log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

it "and the way back is printed before the stop begins"
VMS_HINT_LINE="$(grep -n -- '-- if this is interrupted, run:' <<<"$VMS_ERR" | head -1 | cut -d: -f1)"
VMS_STOPPING_LINE="$(grep -n 'Stopping instance(s) yaw-linux-builder' <<<"$VMS_ERR" | head -1 | cut -d: -f1)"
if [ -n "$VMS_HINT_LINE" ] && [ -n "$VMS_STOPPING_LINE" ] && [ "$VMS_HINT_LINE" -lt "$VMS_STOPPING_LINE" ]; then pass
else fail "hint@${VMS_HINT_LINE:-?} stop@${VMS_STOPPING_LINE:-?} in stderr"; fi

# The same start, with the very first status describe answering blank (exit
# 0, no text): it is asked again and the run goes on exactly as above. Before
# vm_describe this read as "unreadable, not TERMINATED" and, with no budget
# left, ended the run -- the 2026-10-04 gate failure.
vms_run n2-highmem-4 blank-status-once
it "a status describe that answers blank once is asked again, not read as an unreadable VM"
if [ "$VMS_RC" != "0" ] && [ -z "$VMS_OUT" ] \
   && grep -qF "VM started as n2-highmem-4 (its own type is e2-highmem-4; set back when this run stops it)" <<<"$VMS_ERR" \
   && ! grep -q "unreadable" <<<"$VMS_ERR" \
   && [ "$(sed -n '2p;3p' <<<"$VMS_LOG" | grep -c 'value(status)')" = "2" ] \
   && [ "$(cat "$VMS_STATE/type")" = "e2-highmem-4" ] && [ "$(cat "$VMS_STATE/status")" = "TERMINATED" ]; then pass
else fail "rc=$VMS_RC type=$(cat "$VMS_STATE/type") log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

vms_run n2-highmem-4 interrupt-set-n2-highmem-4
it "a Ctrl-C that lands after set-machine-type was applied ends the run, and the VM's real type is what gets put back"
if [ "$VMS_RC" != "0" ] \
   && grep -qF "interrupted while setting yaw-linux-builder to n2-highmem-4" <<<"$VMS_ERR" \
   && grep -qF "set yaw-linux-builder back to e2-highmem-4" <<<"$VMS_ERR" \
   && [[ "$(tail -1 <<<"$VMS_LOG")" == *"instances set-machine-type yaw-linux-builder"*"--machine-type=e2-highmem-4" ]] \
   && [ "$(cat "$VMS_STATE/type")" = "e2-highmem-4" ]; then pass
else fail "rc=$VMS_RC type=$(cat "$VMS_STATE/type") log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

vms_run none start-quota reject-set-n2-highmem-4
it "a quota on the VM's type and a fallback the API rejects drop both, and the run ends with nothing left to try"
if [ "$VMS_RC" != "0" ] \
   && grep -qF "e2-highmem-4 is not usable here -- not trying it again this run: Quota 'N2_CPUS' exceeded." <<<"$VMS_ERR" \
   && grep -qF "yaw-linux-builder cannot be set to n2-highmem-4 -- not trying it again this run: Invalid value for field" <<<"$VMS_ERR" \
   && grep -qF "no machine type left to try for yaw-linux-builder in us-west1-b (tried: e2-highmem-4; last: Invalid value" <<<"$VMS_ERR" \
   && [ "$(cat "$VMS_STATE/type")" = "e2-highmem-4" ] && ! grep -q 'instances stop' <<<"$VMS_LOG"; then pass
else fail "rc=$VMS_RC log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

vms_run none moved
it "an instance that moved zones is found by name, and every later call goes to the zone it is in"
if [ "$VMS_RC" != "0" ] \
   && grep -qF "instance yaw-linux-builder is not in us-west1-b but in us-west1-c -- using that (OAM_GCP_BUILDER_ZONE=us-west1-c silences this)" <<<"$VMS_ERR" \
   && grep -qF -- "--filter=name~^yaw-linux-builder$" <<<"$VMS_LOG" \
   && [ "$(grep -c -- '--zone=us-west1-b' <<<"$VMS_LOG")" = "1" ] \
   && grep -qF -- "instances start yaw-linux-builder --zone=us-west1-c" <<<"$VMS_LOG" \
   && grep -qF "no e2-highmem-4 capacity in us-west1-c (pass 1)" <<<"$VMS_ERR"; then pass
else fail "rc=$VMS_RC log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# --- a builder stopped under a step is started again ---------------------------
# The whole leg against a builder that answers (ssh-mode=builder), the first
# `prep` dropping the way the 2026-09-25 schedule stop looked with the VM
# TERMINATED behind it, and `gate` then failing for real, which ends the run
# where this case wants it. The claim is the ORDER across both stubs' calls:
# the dropped step, the operations-list postmortem, the start, the reconnect
# probe, the step again -- and only then gate.
vms_line(){ grep -nE -- "$1" <<<"$VMS_LOG" | sed -n "${2:-1}p" | cut -d: -f1; }
vms_run e2-highmem-4 ssh-mode=builder ssh-drop=1 fail-step-gate
it "a builder stopped under a step is started again, reconnected to, and the step is run again -- in that order, after the postmortem"
VMS_L_DROP="$(vms_line 'build-remote.sh prep$' 1)"
VMS_L_OPS="$(vms_line 'operations list' 1)"
VMS_L_START="$(vms_line 'instances start yaw-linux-builder' 2)"
VMS_L_PROBE="$(grep -nE -- '^ssh .* true$' <<<"$VMS_LOG" | awk -F: -v s="${VMS_L_START:-0}" '$1 > s { print $1; exit }')"
VMS_L_AGAIN="$(vms_line 'build-remote.sh prep$' 2)"
VMS_L_GATE="$(vms_line 'build-remote.sh gate$' 1)"
if [ "$VMS_RC" != "0" ] && [ -z "$VMS_OUT" ] \
   && grep -qF "ssh transport dropped because yaw-linux-builder is TERMINATED -- last stop operation: 2026-10-04T03:00:00.000-07:00 service-123456789@compute-system.iam.gserviceaccount.com" <<<"$VMS_ERR" \
   && grep -qF "remote 'prep' lost its ssh transport on attempt 1 of 3 because yaw-linux-builder was stopped under it (TERMINATED) -- starting it again, reconnecting and running it again" <<<"$VMS_ERR" \
   && [ "$(grep -c 'Start VM yaw-linux-builder (status: TERMINATED)' <<<"$VMS_ERR")" = "2" ] \
   && grep -qF "Reconnect to yaw-linux-builder after an ssh transport drop" <<<"$VMS_ERR" \
   && grep -qF "remote prep ok" <<<"$VMS_ERR" && grep -qF "remote 'gate' failed" <<<"$VMS_ERR" \
   && [ -n "$VMS_L_DROP" ] && [ -n "$VMS_L_OPS" ] && [ -n "$VMS_L_START" ] && [ -n "$VMS_L_PROBE" ] && [ -n "$VMS_L_AGAIN" ] && [ -n "$VMS_L_GATE" ] \
   && [ "$VMS_L_DROP" -lt "$VMS_L_OPS" ] && [ "$VMS_L_OPS" -lt "$VMS_L_START" ] && [ "$VMS_L_START" -lt "$VMS_L_PROBE" ] \
   && [ "$VMS_L_PROBE" -lt "$VMS_L_AGAIN" ] && [ "$VMS_L_AGAIN" -lt "$VMS_L_GATE" ] \
   && grep -q 'instances stop yaw-linux-builder' <<<"$VMS_LOG"; then pass
else fail "rc=$VMS_RC drop@${VMS_L_DROP:-?} ops@${VMS_L_OPS:-?} start@${VMS_L_START:-?} probe@${VMS_L_PROBE:-?} again@${VMS_L_AGAIN:-?} gate@${VMS_L_GATE:-?} log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# Two drops with two attempts in all: one restart, then the second drop is the
# last attempt, and the run ends with the postmortem -- not a third start.
vms_run e2-highmem-4 ssh-mode=builder ssh-drop=2 step-attempts=2
it "a builder stopped under every attempt ends the run with the postmortem once the attempts are spent"
if [ "$VMS_RC" != "0" ] && [ -z "$VMS_OUT" ] \
   && [ "$(grep -c 'instances start yaw-linux-builder' <<<"$VMS_LOG")" = "2" ] \
   && [ "$(grep -cE 'build-remote.sh prep$' <<<"$VMS_LOG")" = "2" ] \
   && [ "$(grep -c 'ssh transport dropped because yaw-linux-builder is TERMINATED -- last stop operation:' <<<"$VMS_ERR")" = "2" ] \
   && grep -qF "remote 'prep' lost its ssh transport on attempt 1 of 2 because yaw-linux-builder was stopped under it" <<<"$VMS_ERR" \
   && ! grep -qF "attempt 2 of 2 because" <<<"$VMS_ERR" \
   && grep -qF "remote 'prep' failed -- see" <<<"$VMS_ERR" \
   && ! grep -qE 'build-remote.sh gate$' <<<"$VMS_LOG"; then pass
else fail "rc=$VMS_RC log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# --- the zone fallback ---------------------------------------------------------
# The exhausted zone of the first case with the fallback on. us-west1-a refuses
# both types for capacity and us-west1-c takes the first; the DOWN zone and
# the builder's own are never tried. The run then goes on against the clone:
# the schedule read, the IP read and, at exit, the stop name the clone in its
# zone; the original went back to its own type BEFORE it was imaged and is
# never started, stopped or deleted; both delete commands are printed; the
# exit deletes the image and not the clone.
vms_run none zone-fallback=1 "zones=us-west1-a us-west1-b us-west1-c us-west1-d:DOWN" clone-ok-zone=us-west1-c
it "a zone out of capacity for the whole budget clones the builder into the next zone that takes it, and the run goes on against the clone"
VMS_IMG="$(grep -oE 'yaw-linux-builder-img-[0-9]{8}-[0-9]{6}' <<<"$VMS_LOG" | head -1)"
VMS_L_BACK="$(grep -n -- 'set-machine-type yaw-linux-builder --zone=us-west1-b --project=yaw-labs-prod --machine-type=e2-highmem-4' <<<"$VMS_LOG" | tail -1 | cut -d: -f1)"
VMS_L_IMG="$(vms_line 'machine-images create' 1)"
VMS_L_CLONE_STOP="$(vms_line 'instances stop yaw-linux-builder-us-west1-c --zone=us-west1-c' 1)"
VMS_L_IMG_DEL="$(vms_line 'machine-images delete' 1)"
if [ "$VMS_RC" != "0" ] && [ -n "$VMS_IMG" ] \
   && grep -qF -- "compute machine-images create $VMS_IMG --source-instance=yaw-linux-builder --source-instance-zone=us-west1-b --project=yaw-labs-prod" <<<"$VMS_LOG" \
   && grep -qF -- "compute zones list --project=yaw-labs-prod --filter=region:us-west1 --format=value(name,status,region.basename())" <<<"$VMS_LOG" \
   && grep -qF "zones to try, in order: us-west1-a us-west1-c" <<<"$VMS_ERR" \
   && grep -qF -- "compute instances create yaw-linux-builder-us-west1-a --zone=us-west1-a --source-machine-image=$VMS_IMG --machine-type=e2-highmem-4 --project=yaw-labs-prod" <<<"$VMS_LOG" \
   && grep -qF -- "compute instances create yaw-linux-builder-us-west1-a --zone=us-west1-a --source-machine-image=$VMS_IMG --machine-type=n2-highmem-4 --project=yaw-labs-prod" <<<"$VMS_LOG" \
   && grep -qF -- "compute instances create yaw-linux-builder-us-west1-c --zone=us-west1-c --source-machine-image=$VMS_IMG --machine-type=e2-highmem-4 --project=yaw-labs-prod" <<<"$VMS_LOG" \
   && [ "$(grep -c 'instances create ' <<<"$VMS_LOG")" = "3" ] \
   && grep -qF "no e2-highmem-4 capacity in us-west1-a either:" <<<"$VMS_ERR" \
   && grep -qF "THIS RUN NOW BUILDS ON yaw-linux-builder-us-west1-c IN us-west1-c (as e2-highmem-4), a clone of yaw-linux-builder (us-west1-b) made from machine image $VMS_IMG" <<<"$VMS_ERR" \
   && grep -qF "gcloud compute instances delete yaw-linux-builder-us-west1-c --zone=us-west1-c --project=yaw-labs-prod && gcloud compute machine-images delete $VMS_IMG --project=yaw-labs-prod" <<<"$VMS_ERR" \
   && grep -qF -- "instances describe yaw-linux-builder-us-west1-c --zone=us-west1-c --project=yaw-labs-prod --format=value(resourcePolicies)" <<<"$VMS_LOG" \
   && grep -qF -- "instances describe yaw-linux-builder-us-west1-c --zone=us-west1-c --project=yaw-labs-prod --format=value(networkInterfaces[0].accessConfigs[0].natIP)" <<<"$VMS_LOG" \
   && grep -qF "OAM_IAP_SSH_MODE=direct, but direct ssh to yaw-linux-builder-us-west1-c did not answer" <<<"$VMS_ERR" \
   && [ -n "$VMS_L_BACK" ] && [ -n "$VMS_L_IMG" ] && [ "$VMS_L_BACK" -lt "$VMS_L_IMG" ] \
   && [ -n "$VMS_L_CLONE_STOP" ] && [ -n "$VMS_L_IMG_DEL" ] && [ "$VMS_L_CLONE_STOP" -lt "$VMS_L_IMG_DEL" ] \
   && grep -qF -- "instances stop yaw-linux-builder-us-west1-c --zone=us-west1-c --project=yaw-labs-prod --async" <<<"$VMS_LOG" \
   && ! grep -q 'instances stop yaw-linux-builder ' <<<"$VMS_LOG" \
   && ! grep -q 'instances delete' <<<"$VMS_LOG" \
   && [ "$(grep -c 'instances start ' <<<"$VMS_LOG")" = "2" ] \
   && grep -qF -- "machine-images delete $VMS_IMG --project=yaw-labs-prod --quiet" <<<"$VMS_LOG" \
   && grep -qF "deleted machine image $VMS_IMG" <<<"$VMS_ERR" \
   && grep -qF "yaw-linux-builder-us-west1-c in us-west1-c, this run's clone of yaw-linux-builder in us-west1-b, is left stopped and is yours to delete" <<<"$VMS_ERR" \
   && [ "$(cat "$VMS_STATE/type")" = "e2-highmem-4" ] && [ "$(cat "$VMS_STATE/status")" = "TERMINATED" ] && [ "$(cat "$VMS_STATE/clone-status")" = "TERMINATED" ]; then pass
else fail "rc=$VMS_RC img=${VMS_IMG:-?} back@${VMS_L_BACK:-?} img@${VMS_L_IMG:-?} clone-stop@${VMS_L_CLONE_STOP:-?} img-del@${VMS_L_IMG_DEL:-?} log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# The operator's own zone list: no zones list call, the zones in the order
# given, the builder's own skipped.
vms_run none zone-fallback=1 "fallback-zones=us-west1-c us-west1-b us-west1-a" clone-ok-zone=us-west1-a
it "OAM_GCP_FALLBACK_ZONES names the zones to try, in order, and the region is not asked"
VMS_IMG="$(grep -oE 'yaw-linux-builder-img-[0-9]{8}-[0-9]{6}' <<<"$VMS_LOG" | head -1)"
if [ "$VMS_RC" != "0" ] && [ -n "$VMS_IMG" ] \
   && ! grep -q 'zones list' <<<"$VMS_LOG" \
   && grep -qF "zones to try, in order: us-west1-c us-west1-a" <<<"$VMS_ERR" \
   && [ "$(grep -c 'instances create yaw-linux-builder-us-west1-c ' <<<"$VMS_LOG")" = "2" ] \
   && [ "$(grep -c 'instances create yaw-linux-builder-us-west1-a ' <<<"$VMS_LOG")" = "1" ] \
   && ! grep -q 'instances create yaw-linux-builder-us-west1-b ' <<<"$VMS_LOG" \
   && grep -qF "THIS RUN NOW BUILDS ON yaw-linux-builder-us-west1-a IN us-west1-a (as e2-highmem-4)" <<<"$VMS_ERR" \
   && grep -qF -- "machine-images delete $VMS_IMG --project=yaw-labs-prod --quiet" <<<"$VMS_LOG"; then pass
else fail "rc=$VMS_RC log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# No zone takes it: the original failure, the image KEPT and named with the
# hand move, nothing stopped or deleted. The earlier run's clone in us-west1-a
# is pointed out by the preflight, and that zone is skipped when the create
# finds the name taken.
vms_run none zone-fallback=1 "zones=us-west1-a us-west1-c" leftover-clone
it "a clone no zone takes ends the run with the original message, the image kept and named with the hand move; an earlier run's clone is pointed out and not created over"
VMS_IMG="$(grep -oE 'yaw-linux-builder-img-[0-9]{8}-[0-9]{6}' <<<"$VMS_LOG" | head -1)"
if [ "$VMS_RC" != "0" ] && [ -n "$VMS_IMG" ] \
   && grep -qF "a clone of yaw-linux-builder from an earlier run is still there: yaw-linux-builder-us-west1-a in us-west1-a (TERMINATED)" <<<"$VMS_ERR" \
   && grep -qF "delete it: gcloud compute instances delete yaw-linux-builder-us-west1-a --zone=us-west1-a --project=yaw-labs-prod" <<<"$VMS_ERR" \
   && grep -qF "yaw-linux-builder-us-west1-a already exists in us-west1-a -- an earlier run's clone" <<<"$VMS_ERR" \
   && [ "$(grep -c 'instances create yaw-linux-builder-us-west1-a ' <<<"$VMS_LOG")" = "1" ] \
   && [ "$(grep -c 'instances create yaw-linux-builder-us-west1-c ' <<<"$VMS_LOG")" = "2" ] \
   && grep -qF "no n2-highmem-4 capacity in us-west1-c either:" <<<"$VMS_ERR" \
   && grep -qF "could not start yaw-linux-builder in us-west1-b within 0s (tried: e2-highmem-4 n2-highmem-4; last: The zone" <<<"$VMS_ERR" \
   && grep -qF "no zone of us-west1 took it (tried: us-west1-a us-west1-c). Machine image $VMS_IMG is KEPT for a move by hand -- gcloud compute instances create yaw-linux-builder-<zone> --zone=<zone> --source-machine-image=$VMS_IMG --project=yaw-labs-prod, then run again with OAM_GCP_BUILDER_INSTANCE=yaw-linux-builder-<zone> OAM_GCP_BUILDER_ZONE=<zone>" <<<"$VMS_ERR" \
   && grep -qF "gcloud compute machine-images delete $VMS_IMG --project=yaw-labs-prod" <<<"$VMS_ERR" \
   && ! grep -q 'machine-images delete' <<<"$VMS_LOG" \
   && ! grep -qE 'instances (stop|delete)' <<<"$VMS_LOG" \
   && [ -e "$VMS_STATE/image" ] \
   && [ "$(cat "$VMS_STATE/type")" = "e2-highmem-4" ]; then pass
else fail "rc=$VMS_RC img=${VMS_IMG:-?} log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

vms_run none zone-fallback=0 "zones=us-west1-a us-west1-c" clone-ok-zone=us-west1-c
it "OAM_GCP_ZONE_FALLBACK=0 keeps today's failure: no image, no clone, the knob named"
if [ "$VMS_RC" != "0" ] \
   && grep -qF "could not start yaw-linux-builder in us-west1-b within 0s (tried: e2-highmem-4 n2-highmem-4; last: The zone" <<<"$VMS_ERR" \
   && grep -qF "OAM_GCP_ZONE_FALLBACK=1 (the default) clones the builder into another zone of us-west1 for the run instead." <<<"$VMS_ERR" \
   && ! grep -qE 'machine-images|instances create|zones list' <<<"$VMS_LOG" \
   && [[ "$(tail -1 <<<"$VMS_LOG")" == *"instances set-machine-type yaw-linux-builder"*"--machine-type=e2-highmem-4" ]]; then pass
else fail "rc=$VMS_RC log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# Both knobs are checked before the first gcloud call, like the other knobs.
vms_run none zone-fallback=2
VMS_ERR_A="$VMS_ERR"; VMS_LOG_A="$VMS_LOG"
vms_run none "fallback-zones=us-west1-c west1c"
it "a bad OAM_GCP_ZONE_FALLBACK or OAM_GCP_FALLBACK_ZONES fails before anything touches a VM"
if grep -qF "invalid OAM_GCP_ZONE_FALLBACK='2' (want 0 or 1)" <<<"$VMS_ERR_A" && [ -z "$VMS_LOG_A" ] \
   && grep -qF "invalid zone 'west1c' in OAM_GCP_FALLBACK_ZONES='us-west1-c west1c' (want zone names like us-west1-c, space-separated)" <<<"$VMS_ERR" && [ -z "$VMS_LOG" ]; then pass
else fail "fallback=2: log:[$VMS_LOG_A] stderr:$(printf '\n  %s' "$VMS_ERR_A") zones: log:[$VMS_LOG] stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# A zone of another region in OAM_GCP_FALLBACK_ZONES (a typo for us-west1-a)
# fails once the builder's zone is final -- after the describe, before any
# start -- rather than reaching `instances create` there once per machine
# type, each refusal worded as the type's.
vms_run none zone-fallback=1 "fallback-zones=us-central1-a us-west1-c" clone-ok-zone=us-west1-c
it "a zone of another region in OAM_GCP_FALLBACK_ZONES fails once the builder's zone is known, before any start"
if [ "$VMS_RC" != "0" ] \
   && grep -qF "zone 'us-central1-a' in OAM_GCP_FALLBACK_ZONES='us-central1-a us-west1-c' is not in us-west1, the region of yaw-linux-builder's zone us-west1-b -- the zone fallback clones within the region (want other zones of us-west1)" <<<"$VMS_ERR" \
   && grep -q 'instances describe' <<<"$VMS_LOG" \
   && ! grep -qE 'instances (start|set-machine-type|create)|machine-images' <<<"$VMS_LOG"; then pass
else fail "rc=$VMS_RC log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# --- the walk and the clone under what the 2026-10-04 review found -------------

# A VM still STOPPING when the walk begins -- the previous run's exit stop is
# --async, so a re-run inside the minute sees exactly this -- says nothing
# about capacity. It reads STOPPING at the top and on pass 1, which settles;
# pass 2 is refused for capacity on every type with the budget then spent, and
# the zone fallback must follow -- not a refusal to clone because the zone 'was
# not shown to be out of capacity'. Budget 5s: pass 1 (one describe) ends
# inside it, and what is left is the pause before pass 2.
vms_run none zone-fallback=1 "zones=us-west1-a us-west1-c" clone-ok-zone=us-west1-c stopping-for=2 start-budget=5
it "a VM found STOPPING on the first pass does not cost the walk its zone fallback"
if [ "$VMS_RC" != "0" ] \
   && grep -qF "Start VM yaw-linux-builder (status: STOPPING)" <<<"$VMS_ERR" \
   && grep -qF "yaw-linux-builder is STOPPING, not TERMINATED -- waiting for it to settle before the next pass" <<<"$VMS_ERR" \
   && grep -qF "no e2-highmem-4 capacity in us-west1-b (pass 2)" <<<"$VMS_ERR" \
   && ! grep -qF "was not shown to be out of capacity" <<<"$VMS_ERR" \
   && grep -qF "THIS RUN NOW BUILDS ON yaw-linux-builder-us-west1-c IN us-west1-c" <<<"$VMS_ERR"; then pass
else fail "rc=$VMS_RC log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# A restart under a step walks again with the first walk's drops: e2 was
# refused for quota on pass 1 (regional; a minute changes nothing), so the
# second walk must not set the VM back to it and ask again -- one quota
# warning, three starts in all (e2 refused, n2 up, n2 up again), and the only
# set back to e2 is the exit's.
vms_run n2-highmem-4 start-quota ssh-mode=builder ssh-drop=1 fail-step-gate
it "a second walk, after a stop under a step, keeps the types the first walk dropped"
if [ "$VMS_RC" != "0" ] \
   && [ "$(grep -c "e2-highmem-4 is not usable here -- not trying it again this run" <<<"$VMS_ERR")" = "1" ] \
   && [ "$(grep -c 'Start VM yaw-linux-builder (status: TERMINATED)' <<<"$VMS_ERR")" = "2" ] \
   && [ "$(grep -c 'instances start yaw-linux-builder' <<<"$VMS_LOG")" = "3" ] \
   && [ "$(grep -c -- 'set-machine-type yaw-linux-builder --zone=us-west1-b --project=yaw-labs-prod --machine-type=e2-highmem-4' <<<"$VMS_LOG")" = "1" ] \
   && grep -qF "remote prep ok" <<<"$VMS_ERR" && grep -qF "remote 'gate' failed" <<<"$VMS_ERR" \
   && [ "$(cat "$VMS_STATE/type")" = "e2-highmem-4" ]; then pass
else fail "rc=$VMS_RC type=$(cat "$VMS_STATE/type") log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# A Ctrl-C during the clone's `instances create`: gcloud exits 2 with the
# request already through, and the clone comes up RUNNING in the other zone
# while INSTANCE still names the original. The run must look, name the clone
# with its delete command (and stop it, since nothing will build on it), and
# still delete the image -- on the FIRST start, where cleanup() is not yet the
# EXIT trap and the failure text promised the delete all the same.
vms_run none zone-fallback=1 "zones=us-west1-a us-west1-c" interrupt-create-zone=us-west1-a
it "a Ctrl-C during the clone's create names and stops the clone that came up anyway, and the first-start exit deletes the image"
VMS_IMG="$(grep -oE 'yaw-linux-builder-img-[0-9]{8}-[0-9]{6}' <<<"$VMS_LOG" | head -1)"
if [ "$VMS_RC" != "0" ] && [ -n "$VMS_IMG" ] \
   && grep -qF "interrupted while creating yaw-linux-builder-us-west1-a in us-west1-a -- yaw-linux-builder-us-west1-a EXISTS in us-west1-a (RUNNING; a stop was issued) and is yours to delete: gcloud compute instances delete yaw-linux-builder-us-west1-a --zone=us-west1-a --project=yaw-labs-prod; machine image $VMS_IMG is deleted on exit" <<<"$VMS_ERR" \
   && grep -qF -- "instances describe yaw-linux-builder-us-west1-a --zone=us-west1-a --project=yaw-labs-prod --format=value(status)" <<<"$VMS_LOG" \
   && grep -qF -- "instances stop yaw-linux-builder-us-west1-a --zone=us-west1-a --project=yaw-labs-prod --async" <<<"$VMS_LOG" \
   && [ "$(grep -c 'instances create ' <<<"$VMS_LOG")" = "1" ] \
   && grep -qF -- "machine-images delete $VMS_IMG --project=yaw-labs-prod --quiet" <<<"$VMS_LOG" \
   && grep -qF "deleted machine image $VMS_IMG" <<<"$VMS_ERR" \
   && [ ! -e "$VMS_STATE/image" ] && [ "$(cat "$VMS_STATE/clone-status")" = "TERMINATED" ] \
   && ! grep -q 'instances stop yaw-linux-builder ' <<<"$VMS_LOG" \
   && [ "$(cat "$VMS_STATE/type")" = "e2-highmem-4" ]; then pass
else fail "rc=$VMS_RC img=${VMS_IMG:-?} log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# The cold-run case: the image made, the first create refused for a missing
# actAs permission (permanent), on the first start. The image must go at exit
# as the failure text says -- and go even though the restore of the original
# before the image failed (reject-set-e2: the stub's API says no): under
# `set -e` a restore that returned non-zero unguarded would end the trap
# before the delete. The restore is warned twice, there and again at exit.
vms_run none zone-fallback=1 "zones=us-west1-a us-west1-c" deny-create reject-set-e2-highmem-4
it "a clone create refused for good on the first start deletes the image at exit, as the failure says, even after a failed restore"
VMS_IMG="$(grep -oE 'yaw-linux-builder-img-[0-9]{8}-[0-9]{6}' <<<"$VMS_LOG" | head -1)"
if [ "$VMS_RC" != "0" ] && [ -n "$VMS_IMG" ] \
   && grep -qF "creating yaw-linux-builder-us-west1-a in us-west1-a failed, and retrying cannot help: Required 'iam.serviceAccounts.actAs' permission" <<<"$VMS_ERR" \
   && grep -qF "(machine image $VMS_IMG is deleted on exit)" <<<"$VMS_ERR" \
   && [ "$(grep -c 'yaw-linux-builder is still n2-highmem-4 (status TERMINATED) -- set it back with:' <<<"$VMS_ERR")" = "2" ] \
   && grep -qF -- "machine-images delete $VMS_IMG --project=yaw-labs-prod --quiet" <<<"$VMS_LOG" \
   && grep -qF "deleted machine image $VMS_IMG" <<<"$VMS_ERR" \
   && [ ! -e "$VMS_STATE/image" ] \
   && [ "$(grep -c 'instances create ' <<<"$VMS_LOG")" = "1" ] \
   && [ "$(cat "$VMS_STATE/type")" = "n2-highmem-4" ]; then pass
else fail "rc=$VMS_RC img=${VMS_IMG:-?} log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# A Ctrl-C during the minutes-long image create: an interrupt, not a refused
# image. The image gcloud was polling exists, so the exit deletes it, and the
# run stops on the interrupt rather than going on to 'could not be made'.
vms_run none zone-fallback=1 "zones=us-west1-a us-west1-c" clone-ok-zone=us-west1-c interrupt-image
it "a Ctrl-C during the machine image's create is an interrupt, and the image it left is deleted at exit"
VMS_IMG="$(grep -oE 'yaw-linux-builder-img-[0-9]{8}-[0-9]{6}' <<<"$VMS_LOG" | head -1)"
if [ "$VMS_RC" != "0" ] && [ -n "$VMS_IMG" ] \
   && grep -qF "interrupted while creating machine image $VMS_IMG from yaw-linux-builder -- the request may have gone through, so the exit deletes the image if it exists" <<<"$VMS_ERR" \
   && ! grep -qF "could not be made" <<<"$VMS_ERR" \
   && ! grep -q 'instances create' <<<"$VMS_LOG" \
   && grep -qF -- "machine-images delete $VMS_IMG --project=yaw-labs-prod --quiet" <<<"$VMS_LOG" \
   && [ ! -e "$VMS_STATE/image" ] \
   && [ "$(cat "$VMS_STATE/type")" = "e2-highmem-4" ]; then pass
else fail "rc=$VMS_RC img=${VMS_IMG:-?} log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# The walk left the original on n2-highmem-4 and the restore before the image
# is refused (reject-set-e2), while OAM_KEEP_VM=1 keeps the clone up at exit.
# The clone message and the closing word must say what is so: the original
# still on the fallback type, with the command that puts it back, at the clone
# and again at exit after the clone's own line; the clone RUNNING, with the
# command that stops it, and no stop issued.
vms_run none zone-fallback=1 "zones=us-west1-a us-west1-c" clone-ok-zone=us-west1-c reject-set-e2-highmem-4 keep-vm=1
it "an original whose restore failed before the clone is named, at the clone and at exit, as still on the fallback type"
VMS_IMG="$(grep -oE 'yaw-linux-builder-img-[0-9]{8}-[0-9]{6}' <<<"$VMS_LOG" | head -1)"
VMS_ORIGIN_NOTE="yaw-linux-builder in us-west1-b is stopped but STILL ON n2-highmem-4, not its own e2-highmem-4: setting it back failed before the clone was made. Set it back with: gcloud compute instances stop yaw-linux-builder --zone=us-west1-b --project=yaw-labs-prod && gcloud compute instances set-machine-type yaw-linux-builder --zone=us-west1-b --project=yaw-labs-prod --machine-type=e2-highmem-4"
VMS_L_CLONE_WORD="$(grep -nF "this run's clone of yaw-linux-builder in us-west1-b" <<<"$VMS_ERR" | head -1 | cut -d: -f1)"
VMS_L_ORIGIN_WORD="$(grep -nF "$VMS_ORIGIN_NOTE" <<<"$VMS_ERR" | tail -1 | cut -d: -f1)"
if [ "$VMS_RC" != "0" ] && [ -n "$VMS_IMG" ] \
   && grep -qF "yaw-linux-builder is still n2-highmem-4 (status TERMINATED) -- set it back with:" <<<"$VMS_ERR" \
   && [ "$(grep -cF "$VMS_ORIGIN_NOTE" <<<"$VMS_ERR")" = "2" ] \
   && grep -qF "made from machine image $VMS_IMG. $VMS_ORIGIN_NOTE The next run finds it by name as before, and reads that type as its own." <<<"$VMS_ERR" \
   && ! grep -qF "yaw-linux-builder is untouched" <<<"$VMS_ERR" \
   && [ -n "$VMS_L_CLONE_WORD" ] && [ -n "$VMS_L_ORIGIN_WORD" ] && [ "$VMS_L_CLONE_WORD" -lt "$VMS_L_ORIGIN_WORD" ] \
   && [ "$(cat "$VMS_STATE/type")" = "n2-highmem-4" ]; then pass
else fail "rc=$VMS_RC clone-word@${VMS_L_CLONE_WORD:-?} origin-word@${VMS_L_ORIGIN_WORD:-?} type=$(cat "$VMS_STATE/type") log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

it "with OAM_KEEP_VM=1 the clone is left RUNNING, and the clone message and the closing word say so, with the stop command"
if grep -qF "The clone is left RUNNING when this run exits (OAM_KEEP_VM=1) and is NOT deleted; the image is." <<<"$VMS_ERR" \
   && grep -qF "yaw-linux-builder-us-west1-c in us-west1-c, this run's clone of yaw-linux-builder in us-west1-b, is left RUNNING (OAM_KEEP_VM=1) -- it costs compute until: gcloud compute instances stop yaw-linux-builder-us-west1-c --zone=us-west1-c --project=yaw-labs-prod -- and is yours to delete when nothing needs it: gcloud compute instances delete yaw-linux-builder-us-west1-c --zone=us-west1-c --project=yaw-labs-prod" <<<"$VMS_ERR" \
   && ! grep -qF "is left stopped" <<<"$VMS_ERR" \
   && ! grep -q 'instances stop' <<<"$VMS_LOG" \
   && [ "$(cat "$VMS_STATE/clone-status")" = "RUNNING" ] \
   && grep -qF -- "machine-images delete $VMS_IMG --project=yaw-labs-prod --quiet" <<<"$VMS_LOG"; then pass
else fail "rc=$VMS_RC clone-status=$(cat "$VMS_STATE/clone-status") log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# The VM read STOPPING behind the drop and came back RUNNING by someone else's
# hand while the restart waited for the stop to settle: a guest seconds into
# its boot. Its sshd is waited for on the serial console before the reconnect,
# as after this run's own start -- and it is not started again. (The settle
# wait polls every 10s, so this case costs one of those.)
vms_run e2-highmem-4 ssh-mode=builder ssh-drop=1 fail-step-gate drop-status=RUNNING drop-stopping-for=2
it "a VM that came back RUNNING on its own under the restart's settle wait is waited for like a fresh boot before the reconnect"
VMS_L_OPS="$(vms_line 'operations list' 1)"
VMS_L_SERIAL2="$(vms_line 'get-serial-port-output' 2)"
VMS_L_PROBE2="$(grep -nE -- '^ssh .* true$' <<<"$VMS_LOG" | awk -F: -v s="${VMS_L_OPS:-0}" '$1 > s { print $1; exit }')"
if [ "$VMS_RC" != "0" ] \
   && grep -qF "ssh transport dropped because yaw-linux-builder is STOPPING -- last stop operation:" <<<"$VMS_ERR" \
   && grep -qF "yaw-linux-builder is STOPPING -- waiting for it to settle before starting it again (0s of 180s)" <<<"$VMS_ERR" \
   && grep -qF "yaw-linux-builder is RUNNING again -- someone else started it; waiting for its sshd, then reconnecting without starting it" <<<"$VMS_ERR" \
   && [ "$(grep -c 'instances start yaw-linux-builder' <<<"$VMS_LOG")" = "1" ] \
   && [ "$(grep -c 'guest sshd up' <<<"$VMS_ERR")" = "2" ] \
   && [ -n "$VMS_L_OPS" ] && [ -n "$VMS_L_SERIAL2" ] && [ -n "$VMS_L_PROBE2" ] \
   && [ "$VMS_L_OPS" -lt "$VMS_L_SERIAL2" ] && [ "$VMS_L_SERIAL2" -lt "$VMS_L_PROBE2" ] \
   && grep -qF "remote prep ok" <<<"$VMS_ERR" && grep -qF "remote 'gate' failed" <<<"$VMS_ERR"; then pass
else fail "rc=$VMS_RC ops@${VMS_L_OPS:-?} serial2@${VMS_L_SERIAL2:-?} probe2@${VMS_L_PROBE2:-?} log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# Three attempts: the first drop a VM stop (started again), the next two with
# the VM RUNNING. The closing postmortem is about the transport, so it counts
# the two RUNNING drops and names the stop apart -- not 'three times in a row'
# with advice to switch transports. OAM_KEEP_VM_SCHEDULE=1 on the same run:
# its warning has to describe this restart, not a run the stop kills.
vms_run e2-highmem-4 ssh-mode=builder ssh-drop=3 step-attempts=3 "drop-status=TERMINATED RUNNING RUNNING" keep-schedule=1
it "the postmortem after a stop and two transport drops counts the two drops and names the stop apart"
if [ "$VMS_RC" != "0" ] \
   && grep -qF "remote 'prep' lost its ssh transport on attempt 1 of 3 because yaw-linux-builder was stopped under it (TERMINATED)" <<<"$VMS_ERR" \
   && grep -qF "remote 'prep' lost its ssh transport on attempt 2 of 3 while yaw-linux-builder is RUNNING -- reconnecting and running it again" <<<"$VMS_ERR" \
   && grep -qF "ssh transport dropped under this step 2 times while yaw-linux-builder was RUNNING, each time reconnected (1 of the 3 attempts ended with yaw-linux-builder stopped under the step instead, and it was started again each time) -- the path to the builder is not holding (last transport: direct, direct IP 203.0.113.9). Try the other one: OAM_IAP_SSH_MODE=tunnel or =direct" <<<"$VMS_ERR" \
   && ! grep -qF "times in a row" <<<"$VMS_ERR" \
   && [ "$(grep -c 'instances start yaw-linux-builder' <<<"$VMS_LOG")" = "2" ] \
   && [ "$(grep -cE 'build-remote.sh prep$' <<<"$VMS_LOG")" = "3" ] \
   && grep -qF "remote 'prep' failed -- see" <<<"$VMS_ERR"; then pass
else fail "rc=$VMS_RC log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

it "the OAM_KEEP_VM_SCHEDULE=1 warning describes the restart that follows a scheduled stop, not a run the stop kills"
if grep -qF "OAM_KEEP_VM_SCHEDULE=1 -- leaving the VM's instance schedules attached. A scheduled stop under a remote step is followed by a start of the VM (the same walk as at the top, which can clone it into another zone) and the step again, counting against OAM_REMOTE_STEP_ATTEMPTS=3, and the VM is then this run's to stop on exit (OAM_KEEP_VM=1 keeps it); a stop outside a remote step, or on the last attempt, ends the run" <<<"$VMS_ERR" \
   && ! grep -qF "will kill this run" <<<"$VMS_ERR" \
   && ! grep -q 'value(resourcePolicies)' <<<"$VMS_LOG"; then pass
else fail "rc=$VMS_RC log:$(printf '\n  %s' "$VMS_LOG") stderr:$(printf '\n  %s' "$VMS_ERR")"; fi

# =============================================================================
group "tailnet-helpers.sh -- the mac build host preflight"
# =============================================================================
# On 2026-09-30 the release box and the Air were not on the same tailnet, and
# the mac leg said "mac host '<the Air's tailnet IP>' does not resolve via DNS.
# Use the Tailscale MagicDNS name ... or the tailnet IP" -- of a tailnet IP --
# after the release had already tagged, run the whole local gate and built both
# Windows assets. The leg now asks ssh alone and classifies its answer, and
# release-local.sh runs that preflight before it bumps or tags.
#
# The fixtures below are real output with the addresses, device names, tailnet
# name and accounts replaced by placeholders: this box is release-box at
# 100.80.0.1, the Air was at 100.90.0.5 and came back at 100.90.0.6.
# shellcheck source=lib/tailnet-helpers.sh
. scripts/lib/tailnet-helpers.sh

# The first three lines are this orchestrator's own ssh (OpenSSH_10.2p1, Git
# Bash), captured 2026-09-30: the dead tailnet address, a name that does not
# resolve, and a host with no sshd. The two banner lines are what ssh prints
# when something ACCEPTED the connection and then sent nothing: a timeout, but
# not an address nothing answers at.
it "ssh's own line is classed by what actually failed"
TSF_BAD=""
while IFS='|' read -r TSF_WANT TSF_TEXT; do
  TSF_GOT="$(tailnet_ssh_failure "$TSF_TEXT")"
  [ "$TSF_GOT" = "$TSF_WANT" ] || TSF_BAD="$TSF_BAD [$TSF_TEXT -> ${TSF_GOT:-nothing}, want $TSF_WANT]"
done <<'EOF'
unreachable|ssh: connect to host 100.90.0.5 port 22: Connection timed out
resolve|ssh: Could not resolve hostname oam-no-such-host.invalid: Name or service not known
refused|ssh: connect to host release-box.tail1234.ts.net port 22: Connection refused
unreachable|ssh: connect to host 100.90.0.5 port 22: Operation timed out
unreachable|ssh: connect to host 100.90.0.5 port 22: No route to host
unreachable|ssh: connect to host 100.90.0.5 port 22: Network is unreachable
unreachable|ssh: connect to host 100.90.0.5 port 22: Host is down
resolve|ssh: Could not resolve hostname air: nodename nor servname provided, or not known
auth|builder@100.90.0.5: Permission denied (publickey,password,keyboard-interactive).
unknown|Connection timed out during banner exchange
unknown|Connection to 100.90.0.5 port 22 timed out
unknown|kex_exchange_identification: read: Connection reset by peer
unknown|
EOF
if [ -z "$TSF_BAD" ]; then pass; else fail "misclassified:$TSF_BAD"; fi

it "a refused key is found under the known_hosts line ssh prints first"
eq "$(tailnet_ssh_failure $'Warning: Permanently added \'100.90.0.5\' (ED25519) to the list of known hosts.\r\nbuilder@100.90.0.5: Permission denied (publickey).\r\n')" "auth"

# sshd's refusal reads the same when ssh never offered the key because it could
# not load it; the line that says so comes first, which is why the orchestrator
# prints all of ssh's output for this class.
it "a key ssh could not load is an authentication refusal too"
eq "$(tailnet_ssh_failure $'Load key "/c/Users/me/.ssh/yaw_mac_air": error in libcrypto\r\nbuilder@100.90.0.6: Permission denied (publickey,password,keyboard-interactive).')" "auth"

# connect() itself can fail with EACCES, which ssh prints with the same two
# words. "Enroll the key" is the wrong instruction for a blocked connection.
it "a connect() that was denied is not an authentication refusal"
eq "$(tailnet_ssh_failure 'ssh: connect to host 100.90.0.5 port 22: Permission denied')" "unknown"

it "a changed host key is named as that"
eq "$(tailnet_ssh_failure $'@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nHost key verification failed.')" "hostkey"

# `tailscale status` on the release box, captured 2026-09-30, twice: when the
# release failed (this machine and nothing else -- the address the release was
# given is simply not in it), and after the Air was signed in to this tailnet,
# where it came back under a NEW address.
TS_ALONE='100.80.0.1  release-box  owner@  windows  -  '
TS_NOW='100.80.0.1  release-box  owner@  windows  -
100.90.0.6  macbook-air  owner@  macOS    -  '

it "an address with no row is absent, and a table holding this box alone lists no peer"
eq "$(tailnet_peer_state 100.90.0.5 "$TS_ALONE")|$(tailnet_peer_rows "$TS_ALONE")" "absent|"

it "the old address is absent from the table the Air came back in, which lists the new one"
eq "$(tailnet_peer_state 100.90.0.5 "$TS_NOW")|$(tailnet_peer_rows "$TS_NOW")" \
   "absent|100.90.0.6  macbook-air  macOS"

# The states tailscale 1.102 prints for a peer: no traffic ("-"), idle, offline,
# idle and offline, and active -- that last one on a device shared in from
# another tailnet, which is listed by its full DNS name. Then the health
# warnings it appends as `#` lines.
TS_TABLE='100.80.0.1      release-box           owner@  windows  -
100.64.0.7      air                   owner@  macOS    -
100.64.0.8      studio                owner@  macOS    idle, tx 17420 rx 20844
100.64.0.9      old-mini              owner@  macOS    offline, last seen 3d ago
100.64.0.10     old-book              owner@  macOS    idle; offline, last seen 3d ago, tx 1 rx 2
100.70.1.2      build.tail0abc.ts.net other@    linux    active; direct 203.0.113.5:41641, tx 1204 rx 3388

# Health check:
#     - Some peers are advertising routes but --accept-routes is false'

it "a peer is found by address, by MagicDNS name and by bare label, in any case"
TSP_BAD=""
for h in 100.64.0.7 air air.tail1234.ts.net Air.Tail1234.ts.net. AIR studio \
         100.70.1.2 build.tail0abc.ts.net build; do
  TSP_GOT="$(tailnet_peer_state "$h" "$TS_TABLE")"
  [ "$TSP_GOT" = "listed" ] || TSP_BAD="$TSP_BAD [$h -> ${TSP_GOT:-nothing}]"
done
if [ -z "$TSP_BAD" ]; then pass; else fail "not found:$TSP_BAD"; fi

it "a peer marked offline is reported offline, idle or not, by address or by name"
eq "$(tailnet_peer_state 100.64.0.9 "$TS_TABLE") $(tailnet_peer_state old-mini "$TS_TABLE") $(tailnet_peer_state old-book "$TS_TABLE")" \
   "offline offline offline"

# 100.64.0.70 and 100.64.0 must not match the row for 100.64.0.7, nor 100.64.0.1
# the row for 100.64.0.10, and the word after `#` on a health line is not a
# device name.
it "a longer address, a shorter one and a health-warning word match no row"
TSP_BAD=""
for h in 100.64.0.70 100.64.0 100.64.0.77 100.64.0.1 Health ai mini; do
  TSP_GOT="$(tailnet_peer_state "$h" "$TS_TABLE")"
  [ "$TSP_GOT" = "absent" ] || TSP_BAD="$TSP_BAD [$h -> ${TSP_GOT:-nothing}]"
done
if [ -z "$TSP_BAD" ]; then pass; else fail "matched a row:$TSP_BAD"; fi

it "the peer rows leave out this box, the state detail, blank lines and health warnings"
eq "$(tailnet_peer_rows "$TS_TABLE")" \
   '100.64.0.7  air  macOS
100.64.0.8  studio  macOS
100.64.0.9  old-mini  macOS  offline
100.64.0.10  old-book  macOS  offline
100.70.1.2  build.tail0abc.ts.net  linux'

# The orchestrator captures the CLI's stderr with its stdout, and the CLI warns
# there when it and its daemon are different versions. That line is not a row:
# counted as one, a box alone on its tailnet would be told it has a peer.
it "a warning the CLI wrote to stderr is neither a device nor a match"
TS_WARN='Warning: client version "1.102.4-t3caf7d9e7" != tailscaled server version "1.100.1-t0abc"
'"$TS_ALONE"
eq "$(tailnet_peer_rows "$TS_WARN")|$(tailnet_peer_state client "$TS_WARN")" "|absent"

# A peer that reported no OS has an empty OS cell, so its state sits one field
# to the left of where the other rows have it.
it "a peer with no OS in its row still reads as offline"
eq "$(tailnet_peer_state bare "$TS_ALONE"$'\n''100.64.0.11  bare  owner@  offline, last seen 1h ago')" "offline"

# tailscale writes LF; a capture that has been through a CRLF translation must
# read the same -- a blank "\r" line is not a row, and "offline,\r" is offline.
it "a CRLF table reads the same as an LF one"
TS_CRLF="${TS_TABLE//$'\n'/$'\r\n'}"
eq "$(tailnet_peer_state old-mini "$TS_CRLF") $(tailnet_peer_state 100.64.0.7 "$TS_CRLF") $(tailnet_peer_rows "$TS_CRLF" | wc -l | tr -d ' ')" \
   "offline listed 5"

# The table shows IPv4 only, so it cannot say an IPv6 tailnet address is absent.
it "an IPv6 address gets no verdict rather than a wrong one"
eq "$(tailnet_peer_state fd7a:115c:a1e0::4a34:d531 "$TS_TABLE")" ""

it "an empty table lists nothing and has no peers"
eq "$(tailnet_peer_state 100.64.0.7 "")|$(tailnet_peer_rows "")" "absent|"

# The rows go into a failure message: eight are enough to spot the Air in.
it "a large tailnet is cut to eight rows and a count of the rest"
TS_BIG="$TS_ALONE"
for i in 1 2 3 4 5 6 7 8 9 10 11; do TS_BIG="$TS_BIG"$'\n'"100.64.1.$i  peer$i  owner@  linux  -"; done
TS_BIG_ROWS="$(tailnet_peer_rows "$TS_BIG")"
eq "$(wc -l <<<"$TS_BIG_ROWS" | tr -d ' ')|$(tail -1 <<<"$TS_BIG_ROWS")" "9|... and 3 more"

# The preflight itself, run for real with every tool that would leave this box
# replaced: ssh and tailscale by what they printed on 2026-09-30, scp and
# mktemp by stubs that record the call and fail -- so a preflight that went on
# to stage or sync is caught offline, not by a real scp to the Air. TMPDIR is an
# empty directory of the run's own, so "staged nothing" is checked, not assumed.
MACPF_BIN="$SUITE_TMP/macpf-bin"; MACPF_TMP="$SUITE_TMP/macpf-tmp"
mkdir -p "$MACPF_BIN" "$MACPF_TMP"
: > "$SUITE_TMP/macpf-key"
# The table comes from a file each test writes. The stub also prints the
# version warning on stderr, which the orchestrator captures with the table.
cat > "$MACPF_BIN/tailscale" <<EOF
#!/bin/bash
[ "\$1" = "status" ] || exit 1
echo 'Warning: client version "1.102.4-t3caf7d9e7" != tailscaled server version "1.100.1-t0abc"' >&2
cat "$SUITE_TMP/macpf-table"
EOF
for t in scp mktemp; do
  cat > "$MACPF_BIN/$t" <<EOF
#!/bin/bash
: > "$SUITE_TMP/macpf-went-on"
exit 98
EOF
done
# macpf_ssh  -- installs the ssh stub whose body is on stdin.
macpf_ssh(){ { echo '#!/bin/bash'; cat; } > "$MACPF_BIN/ssh"; chmod +x "$MACPF_BIN/ssh"; }
chmod +x "$MACPF_BIN/tailscale" "$MACPF_BIN/scp" "$MACPF_BIN/mktemp"
# macpf [args...]  -- the preflight against the stubs; stdout to MACPF_OUT,
# stderr to MACPF_ERR, status to MACPF_RC. OAM_SKIP_MAC_SIGN=1 keeps the mac
# signing preflight (its own group, against a fixture pin) out of these host
# checks: with a pin committed it would make a second ssh call, which the
# stubs here treat as the leg going on.
macpf(){
  MACPF_OUT="$(PATH="$MACPF_BIN:$PATH" TMPDIR="$MACPF_TMP" OAM_MAC_KEY="$SUITE_TMP/macpf-key" OAM_SKIP_MAC_SIGN=1 \
    OAM_MAC_HOST=100.90.0.5 OAM_MAC_USER=builder \
    bash scripts/build-platforms-tailnet.sh "$@" 2>"$SUITE_TMP/macpf-err")"
  MACPF_RC=$?
  MACPF_ERR="$(cat "$SUITE_TMP/macpf-err")"
}
# What the runs so far left behind: entries in the private TMPDIR (dotfiles
# included), and whether any of them got as far as mktemp or scp.
macpf_left(){
  local n; n="$(ls -A "$MACPF_TMP" 2>/dev/null | wc -l | tr -d ' ')"
  if [ -e "$SUITE_TMP/macpf-went-on" ]; then echo "tmp=$n went-on"; else echo "tmp=$n"; fi
}

macpf_ssh <<'EOF'
echo 'ssh: connect to host 100.90.0.5 port 22: Connection timed out' >&2
exit 255
EOF
printf '%s\n' "$TS_ALONE" > "$SUITE_TMP/macpf-table"
macpf --preflight-only

it "the 2026-09-30 failure is reported as a box that is alone on its tailnet"
if [ "$MACPF_RC" != "0" ] \
   && grep -qF 'nothing answered at 100.90.0.5 on tcp:22' <<<"$MACPF_ERR" \
   && grep -qF "lists no device at '100.90.0.5', and no other device at all" <<<"$MACPF_ERR"; then pass
else fail "rc=$MACPF_RC stderr: $MACPF_ERR"; fi

it "an address that was given as an IP is never told it does not resolve"
if grep -qiF 'does not resolve' <<<"$MACPF_ERR"; then fail "stderr: $MACPF_ERR"; else pass; fi

it "a failed preflight prints nothing on stdout and stages nothing"
eq "out='$MACPF_OUT' $(macpf_left)" "out='' tmp=0"

# The same failure on a full run must stop there too, before any sync.
macpf --mode=release
it "a full run stops at the same preflight failure, with nothing staged or synced"
if [ "$MACPF_RC" != "0" ] && [ -z "$MACPF_OUT" ] && [ "$(macpf_left)" = "tmp=0" ] \
   && grep -qF 'nothing answered at 100.90.0.5 on tcp:22' <<<"$MACPF_ERR"; then pass
else fail "rc=$MACPF_RC out='$MACPF_OUT' $(macpf_left) stderr: $MACPF_ERR"; fi

# Later the same day: the Air is on the tailnet again, under a new address, and
# the release is still being given the old one. The failure has to show the row
# that answers it rather than tell the operator to sign in a device that is
# already there.
printf '%s\n' "$TS_NOW" > "$SUITE_TMP/macpf-table"
macpf --preflight-only
it "a stale address is answered with the devices the tailnet does list"
if [ "$MACPF_RC" != "0" ] \
   && grep -qF "lists no device at '100.90.0.5'. It lists:" <<<"$MACPF_ERR" \
   && grep -qF '      100.90.0.6  macbook-air  macOS' <<<"$MACPF_ERR" \
   && grep -qF 'If one of these is the Air, set OAM_MAC_HOST to its address or name' <<<"$MACPF_ERR"; then pass
else fail "rc=$MACPF_RC stderr: $MACPF_ERR"; fi

# "Last error: (empty stderr)" is all a 2026-09-25 linux-leg failure left the
# operator (see last_nonblank_line). An ssh that fails silently gets a sentence.
macpf_ssh <<'EOF'
exit 255
EOF
macpf --preflight-only
it "an ssh that fails and prints nothing is reported as that, with its exit code"
if [ "$MACPF_RC" != "0" ] && grep -qF 'ssh exited 255 and printed nothing' <<<"$MACPF_ERR"; then pass
else fail "rc=$MACPF_RC stderr: $MACPF_ERR"; fi

# The messages go through `echo -e`. A Windows path in ssh's own text used to
# be read as escapes: \U and \j pass, but \c cuts the message off where it
# stands, taking the advice line with it.
macpf_ssh <<'EOF'
echo 'C:\Users\me\.ssh\config: line 3: Bad configuration option: \checkhostip' >&2
exit 255
EOF
macpf --preflight-only
it "a backslash in ssh's text is printed as it came, and the advice after it survives"
if grep -qF 'C:\Users\me\.ssh\config: line 3: Bad configuration option: \checkhostip' <<<"$MACPF_ERR" \
   && grep -qF 'run it by hand' <<<"$MACPF_ERR"; then pass
else fail "stderr: $MACPF_ERR"; fi

# A host that answers: --preflight-only must succeed WITHOUT going on to stage
# and sync, which is what release-local.sh relies on when it calls this before
# tagging. The mktemp and scp stubs record and fail a run that kept going, and
# the ssh stub fails any call after the first.
macpf_ssh <<EOF
[ -e "$SUITE_TMP/macpf-ssh-called" ] && exit 97
: > "$SUITE_TMP/macpf-ssh-called"
exit 0
EOF
macpf --preflight-only
it "a usable host passes --preflight-only, which then stages, syncs and builds nothing"
if [ "$MACPF_RC" = "0" ] && [ -z "$MACPF_OUT" ] && [ "$(macpf_left)" = "tmp=0" ] \
   && grep -qF 'key auth OK on builder@100.90.0.5' <<<"$MACPF_ERR"; then pass
else fail "rc=$MACPF_RC out='$MACPF_OUT' $(macpf_left) stderr: $MACPF_ERR"; fi

# The real ssh, not a stub: what THIS host's OpenSSH prints for a name that
# cannot resolve has to be a shape the classifier knows. An empty label under
# the reserved .invalid TLD can never be a DNS name, so no resolver answers it.
it "a name this host's own ssh cannot resolve is reported as exactly that"
if command -v ssh >/dev/null 2>&1; then
  MACPF_REAL="$(TMPDIR="$MACPF_TMP" OAM_MAC_KEY="$SUITE_TMP/macpf-key" OAM_MAC_HOST=oam-no-such-host..invalid \
    OAM_MAC_USER=nobody bash scripts/build-platforms-tailnet.sh --preflight-only 2>&1)"; MACPF_RC=$?
  if [ "$MACPF_RC" != "0" ] \
     && grep -qF "OAM_MAC_HOST 'oam-no-such-host..invalid' does not resolve on this box" <<<"$MACPF_REAL"; then pass
  else fail "rc=$MACPF_RC output: $MACPF_REAL"; fi
else
  skip "no ssh on this host"
fi

# Asserted on the source, comments stripped: a second resolver is how the leg
# came to disagree with ssh about a host ssh could reach. Captured, then
# matched: `sed | grep -q` under pipefail reads a HIT as a miss once the body
# is big enough for grep to exit before sed has finished writing.
it "the mac leg consults no resolver but ssh's own"
MACLEG_CODE="$(sed 's/#.*//' scripts/build-platforms-tailnet.sh)"
if grep -qE 'nslookup|getent|Resolve-DnsName|dscacheutil' <<<"$MACLEG_CODE"; then
  fail "scripts/build-platforms-tailnet.sh runs a DNS tool again"
else pass; fi

# The old check sat at the end of release-local.sh's preflight and only asked
# whether OAM_MAC_HOST was set, so an unusable Air surfaced after the bump, the
# tag and the local gate. The bump's first write is the Cargo.toml rewrite its
# warning announces.
it "release-local.sh proves the mac host usable before it bumps or tags"
REL_MACPF="$(grep -n 'build-platforms-tailnet\.sh" --preflight-only' scripts/release-local.sh | head -1 | cut -d: -f1)"
REL_BUMP="$(grep -n 'bumping Cargo\.toml to' scripts/release-local.sh | head -1 | cut -d: -f1)"
REL_TAG="$(grep -n 'git tag -a "$TAG" -m "$TAG"' scripts/release-local.sh | head -1 | cut -d: -f1)"
if [ -n "$REL_MACPF" ] && [ -n "$REL_BUMP" ] && [ -n "$REL_TAG" ] \
   && [ "$REL_MACPF" -lt "$REL_BUMP" ] && [ "$REL_MACPF" -lt "$REL_TAG" ]; then pass
else fail "host preflight@${REL_MACPF:-none} bump@${REL_BUMP:-none} tag@${REL_TAG:-none}"; fi

REL_MACPF_BLOCK="$(awk '/^if \[ "\$SKIP_MAC" != "1" \]; then$/ { f = 1 } f { print } f && /^fi$/ { exit }' scripts/release-local.sh)"
it "OAM_SKIP_MAC=1 skips the mac host preflight along with the mac leg"
grep -qF -- '--preflight-only' <<<"$REL_MACPF_BLOCK" && pass || fail "the --preflight-only call is not inside the SKIP_MAC guard: '$REL_MACPF_BLOCK'"

it "a mac host preflight that fails stops the release"
REL_MACPF_CALL="$(grep -A1 -- '--preflight-only' <<<"$REL_MACPF_BLOCK")"
case "$REL_MACPF_CALL" in
  *'|| fail "'*) pass ;;
  *) fail "the --preflight-only call has no '|| fail' after it: '$REL_MACPF_CALL'" ;;
esac

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
# The two exemptions are TRACKED fixtures whose path happens to spell a build
# directory: the node-suite's vendored node_modules, and the vendored MIT
# proxy-agent packages the http client's tests run against, which are shipped
# as their published form (a `dist/`). Both are asserted below, so exempting
# them here cannot quietly hide their loss. Everything else named here is
# output or scratch, and neither belongs in a source tarball.
OFFENDERS="$(grep -nE '(^|/)(target|node_modules|dist)/|^\.claude/|^\.git/' "$SRC_LIST" \
  | grep -v 'conformance/vendor/node/test/fixtures/.*/node_modules/' \
  | grep -v 'crates/oam_cli/tests/fixtures/.*/dist/' | head -5)"
if [ -z "$OFFENDERS" ]; then pass; else fail "build output or agent scratch in the sync list: $OFFENDERS"; fi

# The other half of that guard, and the reason the naive fix (dropping the `./`
# anchors) is wrong: an unanchored --exclude=node_modules also deletes these
# four TRACKED fixture files, which the node-suite's warning_node_modules case
# asserts on. Verified on GNU tar 1.35, 2026-08-31.
it "still ships the tracked vendored node_modules conformance fixtures"
eq "$(grep -c 'conformance/vendor/node/test/fixtures/warning_node_modules/node_modules/' "$SRC_LIST")" "4"

# The same for the proxy-agent fixtures: the remote legs run the http client's
# proxy tests, and a tarball without these leaves them with nothing to require.
it "still ships the tracked vendored proxy-agent test fixtures"
PROXY_FIXTURES="$(grep -c 'crates/oam_cli/tests/fixtures/.*/dist/' "$SRC_LIST")"
if [ "$PROXY_FIXTURES" -gt 0 ]; then pass; else fail "the vendored proxy-agent fixtures are not in the sync list"; fi

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

# vendor/ is outside the workspace and the unsafe-budget scan, so
# check-vendor.sh is the only gate a vendored crate has. A ci-local.sh that
# stopped calling it, or a vendored crate without the files it checks
# against, would leave that crate ungated with every step still green.
it "ci-local.sh runs the vendored-crate check, and every vendored crate carries its patch files"
VENDOR_BAD=""
grep -qE 'bash scripts/check-vendor\.sh --build' scripts/ci-local.sh || VENDOR_BAD="$VENDOR_BAD ci-local.sh-does-not-call-check-vendor.sh--build"
for vdir in vendor/*/; do
  [ -d "$vdir" ] || continue
  for vfile in OAM-PATCH.sha256 OAM-PATCH.diff OAM-PATCH.md; do
    [ -f "$vdir$vfile" ] || VENDOR_BAD="$VENDOR_BAD $vdir$vfile(missing)"
  done
done
if [ -z "$VENDOR_BAD" ]; then pass; else fail "vendored-crate gate out of sync:$VENDOR_BAD"; fi

# The sidecar matrix's self-test holds the release gate's own verdicts, and it
# ran in no gate at all until ci-local.sh step 13 called it -- so a matrix edit
# that turned an oam regression into a warn passed every release. A ci-local.sh
# that stopped calling it would put it back there with every step green.
# Anchored to the `if`/`elif` that RUNS it: the step's failure message quotes
# the same command as a reproduce hint, and an unanchored match on that hint
# stayed green with the call itself changed or gone.
it "ci-local.sh runs the sidecar matrix self-test"
if grep -qE '^[[:space:]]*(if|elif)[[:space:]]+node scripts/mcp-sidecar-matrix\.mjs --self-test;' scripts/ci-local.sh; then pass
else fail "ci-local.sh no longer runs 'node scripts/mcp-sidecar-matrix.mjs --self-test' as a step's condition"; fi

# The sidecar gate installs with no lifecycle script and no git: a script can
# start a daemon that outlives npm, which no kill of npm's tree reaches, and
# which ran on writing into the gate's stage. The self-test holds the arguments
# too; this is the belt, should that pin be edited along with them.
it "the sidecar matrix never runs install scripts or git"
MX_ARGS_FN="$(awk '/^function npmInstallArgs\(/ { f = 1 } f { print } f && /^}$/ { exit }' scripts/mcp-sidecar-matrix.mjs)"
MX_INSTALL_FN="$(awk '/^async function npmInstall\(/ { f = 1 } f { print } f && /^}$/ { exit }' scripts/mcp-sidecar-matrix.mjs)"
MX_SEAL_MISSING=""
for want in '"--ignore-scripts"' '`--script-shell=${join(dir, basename(NO_SCRIPT_SHELL))}`' '`--git=${join(dir, basename(NO_GIT))}`' '"--allow-git=none"'; do
  case "$MX_ARGS_FN" in *"$want"*) ;; *) MX_SEAL_MISSING="$MX_SEAL_MISSING $want" ;; esac
done
# Inside npmInstall itself: the self-test's own quote of the call is elsewhere
# in the file, and must not stand in for it.
case "$MX_INSTALL_FN" in *'npmInstallArgs(specs)'*) ;; *) MX_SEAL_MISSING="$MX_SEAL_MISSING npmInstallArgs(specs)-in-npmInstall" ;; esac
if [ -z "$MX_SEAL_MISSING" ]; then pass
else fail "the matrix's npm install is no longer sealed (missing:$MX_SEAL_MISSING)"; fi

# The release's own reading of the matrix's status, run as written: the lines
# from `matrix_status=$?` to the `esac`, after a command that exits with each
# status, with ok/warn/fail stubbed to say which fired. A gate killed by a
# signal -- a Ctrl-C (130; Git Bash reports Windows' STATUS_CONTROL_C_EXIT the
# same way), SIGHUP, SIGKILL, SIGTERM -- or ended by Stop-Process or a native
# crash (127 under Git Bash) must stop the release. Every status above 1 used
# to be a warn, and a warn carries on to publish. And 1 is "a sidecar failed"
# only with the report the gate writes once it has a verdict: taskkill /F and
# an uncaught error end it with 1 and no report (1-noreport).
it "release-local.sh stops the release when the sidecar matrix is interrupted, killed or crashed"
MX_BLOCK="$(awk '/^  matrix_status=\$\?$/ { f = 1 } f { print } f && /^  esac$/ { exit }' scripts/release-local.sh)"
MX_STUBS='ok(){ echo ok; }; warn(){ echo warn; }; fail(){ case "$1" in *"a sidecar failed"*) echo fail-verdict ;; *) echo fail ;; esac; exit 1; }'
MX_REPORT="$SUITE_TMP/mcp-sidecar-matrix.json"
MX_GOT=""
for st in 0 1 1-noreport 2 3 127 129 130 137 143; do
  MX_CODE="${st%-noreport}"
  rm -f "$MX_REPORT"
  [ "$st" = "$MX_CODE" ] && printf '{\n  "exitCode": %s,\n  "sidecars": []\n}\n' "$MX_CODE" > "$MX_REPORT"
  MX_GOT="$MX_GOT $st:$(bash -c "$MX_STUBS; matrix_report='$MX_REPORT'; (exit $MX_CODE); $MX_BLOCK" 2>/dev/null | head -1)"
done
if [ "$MX_GOT" = " 0:ok 1:fail-verdict 1-noreport:fail 2:warn 3:warn 127:fail 129:fail 130:fail 137:fail 143:fail" ]; then pass
else fail "release-local.sh's matrix step read the statuses as:$MX_GOT"; fi

# #220: an INCOMPLETE matrix is a warning, and the rows it could not answer
# used to be one word each in the matrix's own list, scrolled past -- the fetch
# row read UPSTREAM through a release that way. Each now gets a warn line of
# its own from the report; a verified or boot row gets none.
it "release-local.sh names every sidecar the matrix did not exercise, one line each"
MX_FN="$(awk '/^matrix_unanswered\(\)\{$/ { f = 1 } f { print } f && /^}$/ { exit }' scripts/release-local.sh)"
MX_WARN_STUBS='ok(){ echo "ok $*"; }; warn(){ echo "warn $*"; }; fail(){ echo "fail $*"; exit 1; }'
cat > "$MX_REPORT" <<'JSON'
{
  "exitCode": 3,
  "sidecars": [
    { "name": "memory", "state": "verified" },
    { "name": "fetch", "state": "upstream", "why": "node refused the call too" },
    { "name": "redis", "state": "skip", "why": "no redis-server" },
    { "name": "ctxlint", "state": "boot", "why": "boot-only by design" }
  ]
}
JSON
MX_OUT="$(bash -c "$MX_WARN_STUBS; $MX_FN; matrix_report='$MX_REPORT'; (exit 3); $MX_BLOCK" 2>&1)"
if [ -z "$MX_FN" ]; then fail "matrix_unanswered() not found in release-local.sh"
elif ! grep -qx 'warn NOT EXERCISED on this build: fetch (UPSTREAM: node refused the call too)' <<<"$MX_OUT"; then
  fail "the upstream fetch row was not named on its own line:$(printf '\n%s' "$MX_OUT")"
elif ! grep -qx 'warn NOT EXERCISED on this build: redis (SKIP: no redis-server)' <<<"$MX_OUT"; then
  fail "the skipped redis row was not named on its own line:$(printf '\n%s' "$MX_OUT")"
elif grep -qE 'NOT EXERCISED.*(memory|ctxlint)' <<<"$MX_OUT"; then
  fail "a verified or boot-only row was named as not exercised:$(printf '\n%s' "$MX_OUT")"
elif grep -q '^fail' <<<"$MX_OUT"; then
  fail "an INCOMPLETE matrix became fatal:$(printf '\n%s' "$MX_OUT")"
else pass; fi

it "release-local.sh's unanswered-row reader says nothing for a missing report"
rm -f "$MX_REPORT"
eq "$(bash -c "$MX_FN; matrix_unanswered '$MX_REPORT'")" ""

# And the report it reads is this run's. The stash is a fresh mktemp dir per
# run, so no earlier report is there today; the rm keeps it so if the stash is
# ever reused, and this keeps the rm.
it "release-local.sh removes the previous sidecar matrix report before running the gate"
MX_RM="$(grep -n '^  rm -f "\$matrix_report"$' scripts/release-local.sh | head -1 | cut -d: -f1)"
MX_RUN="$(grep -n 'node "\$REPO_DIR/scripts/mcp-sidecar-matrix\.mjs" --json="\$matrix_report"$' scripts/release-local.sh | head -1 | cut -d: -f1)"
if [ -n "$MX_RM" ] && [ -n "$MX_RUN" ] && [ "$MX_RM" -lt "$MX_RUN" ]; then pass
else fail "release-local.sh no longer removes \$matrix_report before the gate runs (rm at '${MX_RM}', gate at '${MX_RUN}')"; fi

# The site's refresh-downloads.sh refuses for a reason -- no published release,
# a manifest that disagrees with SHA256SUMS, a page with no row for an asset --
# and the release step used to send that reason to /dev/null with the rest of
# its output, leaving a bare "failed". The two functions run here verbatim,
# under the release's own set -euo pipefail and with its real ok/warn (warn is
# echo -e), against a stub site script. The stub's refusal carries a Windows
# path (echo -e reads "\c" as "stop printing") and command substitutions that
# would create $RS_PWN if anything evaluated the text.
RS="$SUITE_TMP/site-refresh"
RS_STUB="$RS/site/scripts/refresh-downloads.sh"
mkdir -p "$RS/site/scripts"
RS_FNS="$(awk '/^site_refresh_reason\(\) \{$/ || /^refresh_site_downloads\(\) \{$/ { f = 1 } f { print } f && /^}$/ { f = 0 }' scripts/release-local.sh)"
RS_LOG="$(grep -E '^(ok|warn|fail)\(\)' scripts/release-local.sh)"
rs_run() {
  rm -f "$RS/pwned"
  chmod +x "$RS_STUB"
  RS_OUT="$(cd "$RS" && GRN='' YEL='' RED='' NC='' RS_PWN="$RS/pwned" \
    bash -c "set -euo pipefail; $RS_LOG; $RS_FNS"'; refresh_site_downloads "$1" v9.9.9; echo "after rc=$?"' rs "$RS/site" 2>&1)"
}

it "release-local.sh: a refused site refresh puts the script's last [fail] line in the warn, verbatim and unevaluated"
cat > "$RS_STUB" <<'STUB'
#!/bin/bash
echo STDOUT-NOISE
esc=$'\033'
echo "${esc}[0;33m  [warn]${esc}[0m $1 predates signing" >&2
echo "  [fail] an earlier refusal" >&2
echo "${esc}[0;31m  [fail]${esc}[0m no published release for $1 at C:\cache\oam"' $(touch "$RS_PWN") `touch "$RS_PWN"`' >&2
exit 3
STUB
rs_run
RS_WANT='  [warn] refresh-downloads.sh failed (exit 3): no published release for v9.9.9 at C:\cache\oam $(touch "$RS_PWN") `touch "$RS_PWN"` -- the downloads page and checksums post still advertise the previous release'
if [ -z "$RS_FNS" ] || [ "$(grep -c '() {$' <<<"$RS_FNS")" != "2" ]; then fail "site_refresh_reason() / refresh_site_downloads() not found in release-local.sh"
elif ! grep -qxF -- "$RS_WANT" <<<"$RS_OUT"; then fail "warn did not carry the refusal:$(printf '\n%s' "$RS_OUT")"
elif [ -e "$RS/pwned" ]; then fail "the captured stderr was evaluated"
elif grep -q 'STDOUT-NOISE\|earlier refusal' <<<"$RS_OUT"; then fail "stdout or an earlier line leaked:$(printf '\n%s' "$RS_OUT")"
elif grep -q $'\033' <<<"$RS_OUT"; then fail "colour codes survived into the warn"
elif ! grep -qx 'after rc=0' <<<"$RS_OUT"; then fail "the step was fatal under set -euo pipefail:$(printf '\n%s' "$RS_OUT")"
else pass; fi

it "release-local.sh: with no [fail] line the warn carries the last error-ish line, else a short tail, else says stderr was empty"
cat > "$RS_STUB" <<'STUB'
#!/bin/bash
echo "  [ok] resolved $1" >&2
echo "marker block downloads-table not found in the page" >&2
echo "Traceback noise that follows" >&2
exit 1
STUB
rs_run; RS_A="$RS_OUT"
cat > "$RS_STUB" <<'STUB'
#!/bin/bash
printf 'one\ntwo\n\nthree\n\tfour\n' >&2
exit 2
STUB
rs_run; RS_B="$RS_OUT"
printf '#!/bin/bash\necho only-stdout\nexit 1\n' > "$RS_STUB"
rs_run; RS_C="$RS_OUT"
if ! grep -qF 'failed (exit 1): marker block downloads-table not found in the page -- ' <<<"$RS_A"; then fail "error line: $RS_A"
elif ! grep -qF 'failed (exit 2): two | three | four -- ' <<<"$RS_B"; then fail "tail: $RS_B"
elif ! grep -qF 'failed (exit 1): it printed nothing on stderr -- ' <<<"$RS_C"; then fail "empty: $RS_C"
elif [ "$(grep -c 'after rc=0' <<<"$RS_A$RS_B$RS_C")" != "3" ]; then fail "a failure was fatal: $RS_A / $RS_B / $RS_C"
else pass; fi

it "release-local.sh: a successful site refresh says ok, and drops the script's own chatter"
printf '#!/bin/bash\necho STDOUT-NOISE\necho "  [warn] review git diff" >&2\nexit 0\n' > "$RS_STUB"
rs_run
eq "$RS_OUT" $'  [ok] release pages regenerated for v9.9.9 (downloads page + checksums post)\nafter rc=0'

it "release-local.sh: the site step runs refresh-downloads.sh through refresh_site_downloads, never blind"
if grep -qxF '    refresh_site_downloads "$SITE_DIR" "$TAG"' scripts/release-local.sh \
   && ! grep -qF '>/dev/null 2>&1' <(grep -v '^[[:space:]]*#' scripts/release-local.sh | grep -F 'refresh-downloads.sh'); then pass
else fail "release-local.sh runs refresh-downloads.sh outside refresh_site_downloads, or discards its stderr again"; fi

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
         scripts/lib/tap-verify.sh scripts/lib/attribution.sh \
         scripts/lib/ci-ledger.sh \
         scripts/lib/signing.sh scripts/release-upload-local-arm64.sh; do
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
group "ci-local.sh -- the step ledger"
# =============================================================================
# A re-run of the gate on the same tree skips the steps that already passed
# (step 13 flaked an hour into a release's gate on 2026-10-04, and the way
# back to it was all fourteen steps again). A skip is a claim -- "this step
# passed on exactly what is on disk" -- so the cases here are the ways that
# claim could go false while the ledger still said yes: a key that misses an
# input, an entry from another key or toolchain, a half-written entry from a
# run killed mid-mark, and a step that stopped consulting the ledger (or one
# that started, when its skip is not safe). The lib takes the dir and the key
# as arguments, so passed/mark/clear need no git; the tree id gets one fixture
# repo, and the composed key is checked against its parts once.
# shellcheck source=lib/ci-ledger.sh
. scripts/lib/ci-ledger.sh

LG="$SUITE_TMP/ledger"
LG_TC='rustc 1.96.0 (ac68faa20 2026-05-25)'$'\n''cargo 1.96.0 (30a34c682 2026-05-25)'
LG_K1="$(ci_ledger_key_from 1111111 aaaaaaa "$LG_TC")"
LG_K2="$(ci_ledger_key_from 1111111 aaaaaaa "$LG_TC" --fast)"

it "the key is one sha256 token"
case "$LG_K1" in
  ""|*[!0-9a-f]*) fail "not a hex token: '$LG_K1'" ;;
  *) [ "${#LG_K1}" = "64" ] && pass || fail "length ${#LG_K1}, want 64" ;;
esac

# Eight keys, seven distinct: only the first two share every input. A flag is
# a separate part, not a substring of one -- the last line is the pair.
it "the same inputs give the same key; HEAD, the tree, the toolchain and each flag change it"
LG_KEYS="$LG_K1
$(ci_ledger_key_from 1111111 aaaaaaa "$LG_TC")
$LG_K2
$(ci_ledger_key_from 2222222 aaaaaaa "$LG_TC")
$(ci_ledger_key_from 1111111 bbbbbbb "$LG_TC")
$(ci_ledger_key_from 1111111 aaaaaaa 'rustc 1.97.0 (0000000 2026-07-01)')
$(ci_ledger_key_from 1111111 aaaaaaa "$LG_TC" --no-tests)
$(ci_ledger_key_from 1111111 aaaaaaa "$LG_TC" --fast --no-tests)"
eq "$(printf '%s\n' "$LG_KEYS" | sort -u | wc -l | tr -d ' ')" "7"

it "the key refuses a missing HEAD or tree id rather than keying half a tree"
LG_BAD=""
ci_ledger_key_from "" aaaaaaa "$LG_TC" >/dev/null 2>&1 && LG_BAD="$LG_BAD no-head"
ci_ledger_key_from 1111111 "" "$LG_TC" >/dev/null 2>&1 && LG_BAD="$LG_BAD no-tree"
if [ -z "$LG_BAD" ]; then pass; else fail "a key was produced with:$LG_BAD"; fi

it "an unmarked step has not passed; marked, it has; a mark is per step and per key"
LG_BAD=""
ci_ledger_passed "$LG" "$LG_K1" 03-clippy && LG_BAD="$LG_BAD unmarked-counted"
ci_ledger_mark "$LG" "$LG_K1" 03-clippy || LG_BAD="$LG_BAD mark-failed"
ci_ledger_passed "$LG" "$LG_K1" 03-clippy || LG_BAD="$LG_BAD marked-not-counted"
ci_ledger_passed "$LG" "$LG_K1" 06-tests && LG_BAD="$LG_BAD other-step-counted"
ci_ledger_passed "$LG" "$LG_K2" 03-clippy && LG_BAD="$LG_BAD other-key-counted"
if [ -z "$LG_BAD" ]; then pass; else fail "$LG_BAD"; fi

it "re-marking under a new key replaces the entry, and the old key no longer counts"
ci_ledger_mark "$LG" "$LG_K2" 03-clippy
if ci_ledger_passed "$LG" "$LG_K2" 03-clippy && ! ci_ledger_passed "$LG" "$LG_K1" 03-clippy; then pass
else fail "entry holds '$(cat "$LG/03-clippy")'"; fi

# A prefix is what a run killed mid-write would leave if the mark were not
# written through a rename; the other two are a concatenating write and a
# double write. None may count: "holds exactly the key" is the contract.
it "a torn or over-long entry never counts: a prefix of the key, the key plus a tail, the key twice"
LG_BAD=""
printf '%s' "${LG_K1:0:40}" > "$LG/04-feature-off"
ci_ledger_passed "$LG" "$LG_K1" 04-feature-off && LG_BAD="$LG_BAD prefix"
printf '%sx\n' "$LG_K1" > "$LG/04-feature-off"
ci_ledger_passed "$LG" "$LG_K1" 04-feature-off && LG_BAD="$LG_BAD tail"
printf '%s\n%s\n' "$LG_K1" "$LG_K1" > "$LG/04-feature-off"
ci_ledger_passed "$LG" "$LG_K1" 04-feature-off && LG_BAD="$LG_BAD doubled"
if [ -z "$LG_BAD" ]; then pass; else fail "counted as passed:$LG_BAD"; fi

it "an empty key never counts, not even against an empty entry, and neither an empty key nor an empty step can be marked"
: > "$LG/07-smoke"
LG_BAD=""
ci_ledger_passed "$LG" "" 07-smoke && LG_BAD="$LG_BAD empty-key-counted"
ci_ledger_mark "$LG" "" 07-smoke 2>/dev/null && LG_BAD="$LG_BAD empty-key-marked"
ci_ledger_mark "$LG" "$LG_K1" "" 2>/dev/null && LG_BAD="$LG_BAD empty-step-marked"
if [ -z "$LG_BAD" ]; then pass; else fail "$LG_BAD"; fi

# Source-level for the shape -- the same-filesystem rename is what makes a kill
# mid-write leave the old entry or the new one, never half of one -- and a
# listing for the residue the marks above would have left.
it "a mark is a temp file in the ledger dir renamed over the entry, and leaves nothing else behind"
LG_LEFT="$(ls -A "$LG" | grep -c 'tmp')"
if grep -qF 'tmp="$dir/.$step.$$.tmp"' scripts/lib/ci-ledger.sh && grep -qF 'mv -f "$tmp" "$dir/$step"' scripts/lib/ci-ledger.sh \
   && [ "$LG_LEFT" = "0" ]; then pass
else fail "mark no longer writes through a same-dir temp file + mv, or left $LG_LEFT temp file(s) behind"; fi

it "mark creates a missing ledger dir"
ci_ledger_mark "$LG/fresh/deeper" "$LG_K1" 01-control-bytes
ck ci_ledger_passed "$LG/fresh/deeper" "$LG_K1" 01-control-bytes

it "clear forgets every step and leaves an empty dir; an empty dir argument is refused, not expanded"
ci_ledger_clear "$LG"
LG_BAD=""
[ -d "$LG" ] || LG_BAD="$LG_BAD dir-gone"
[ -z "$(ls -A "$LG")" ] || LG_BAD="$LG_BAD not-empty:$(ls -A "$LG" | tr '\n' ' ')"
ci_ledger_clear "" 2>/dev/null && LG_BAD="$LG_BAD empty-arg-accepted"
if [ -z "$LG_BAD" ]; then pass; else fail "$LG_BAD"; fi

# One fixture repo for the tree id. The claims: a clean checkout hashes to
# HEAD's own tree (so a `git status`-clean tree keys the same run after run,
# and a mode bit or an ignored-but-tracked file does not move it -- the reason
# the scratch index is seeded from the real one), an untracked file moves it,
# an ignored one does not, and the REAL index is never touched.
LG_REPO="$SUITE_TMP/ledger-repo"
mkdir -p "$LG_REPO"
( cd "$LG_REPO" && git init -q . && git config user.email t@t && git config user.name t \
  && printf 'tracked\n' > a && printf 'scratch\n' > .gitignore && git add a .gitignore && git commit -qm one ) >/dev/null 2>&1
LG_HEAD_TREE="$(cd "$LG_REPO" && git rev-parse 'HEAD^{tree}')"

it "tree id: a clean checkout hashes to HEAD's own tree"
eq "$(cd "$LG_REPO" && ci_ledger_tree_id)" "$LG_HEAD_TREE"

it "tree id: an untracked file changes it and stays untracked in the real index; an ignored file changes nothing"
printf 'new\n' > "$LG_REPO/untracked"
LG_T_UNTRACKED="$(cd "$LG_REPO" && ci_ledger_tree_id)"
LG_STATUS_U="$(cd "$LG_REPO" && git status --porcelain)"
rm -f "$LG_REPO/untracked"
printf 'junk\n' > "$LG_REPO/scratch"
LG_T_IGNORED="$(cd "$LG_REPO" && ci_ledger_tree_id)"
LG_STATUS_I="$(cd "$LG_REPO" && git status --porcelain)"
LG_BAD=""
[ "$LG_T_UNTRACKED" != "$LG_HEAD_TREE" ] || LG_BAD="$LG_BAD untracked-file-invisible"
[ "$LG_STATUS_U" = "?? untracked" ] || LG_BAD="$LG_BAD real-index-touched:'$LG_STATUS_U'"
[ "$LG_T_IGNORED" = "$LG_HEAD_TREE" ] || LG_BAD="$LG_BAD ignored-file-counted"
[ -z "$LG_STATUS_I" ] || LG_BAD="$LG_BAD status-after:'$LG_STATUS_I'"
if [ -z "$LG_BAD" ]; then pass; else fail "$LG_BAD"; fi

it "tree id: an uncommitted edit to a tracked file changes it"
printf 'edited\n' > "$LG_REPO/a"
LG_T_EDIT="$(cd "$LG_REPO" && ci_ledger_tree_id)"
( cd "$LG_REPO" && git checkout -q -- a )
if [ -n "$LG_T_EDIT" ] && [ "$LG_T_EDIT" != "$LG_HEAD_TREE" ]; then pass; else fail "edit invisible: '$LG_T_EDIT'"; fi

# The composed key is its parts, hashed: proven by rebuilding it from HEAD,
# the tree id and the two version lines, which is also the only way to check
# it without a second slow call.
it "ci_ledger_key is the sha256 of HEAD, the tree id, rustc -V, cargo -V and the flags"
if command -v rustc >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1; then
  LG_KA="$(cd "$LG_REPO" && ci_ledger_key --fast)"
  LG_KX="$(ci_ledger_key_from "$(cd "$LG_REPO" && git rev-parse HEAD)" "$LG_HEAD_TREE" "$(rustc -V)"$'\n'"$(cargo -V)" --fast)"
  eq "$LG_KA" "$LG_KX"
else
  skip "rustc/cargo not on PATH -- the composed key cannot be computed here"
fi

# --- the wiring in ci-local.sh -------------------------------------------------
lg_line(){ grep -nF -- "$1" scripts/ci-local.sh | head -1 | cut -d: -f1; }

it "ci-local.sh sources the lib, keeps the ledger under target/, and validates OAM_CI_FRESH before the key, before step 1"
LG_BAD=""
grep -qxF '. scripts/lib/ci-ledger.sh' scripts/ci-local.sh || LG_BAD="$LG_BAD lib-not-sourced"
grep -qxF 'CI_LEDGER_DIR="target/ci-local/passed"' scripts/ci-local.sh || LG_BAD="$LG_BAD ledger-not-under-target"
grep -qF 'ci_ledger_clear "$CI_LEDGER_DIR"' scripts/ci-local.sh || LG_BAD="$LG_BAD fresh-does-not-clear"
grep -qE '^#   OAM_CI_FRESH=1 ' scripts/ci-local.sh || LG_BAD="$LG_BAD knob-undocumented-in-header"
LG_L_FRESH="$(lg_line 'case "${OAM_CI_FRESH:-}" in')"
LG_L_KEY="$(lg_line 'CI_KEY="$(ci_ledger_key $CI_KEY_FLAGS)"')"
LG_L_SAY1="$(lg_line 'say "1/14 ')"
if [ -z "$LG_L_FRESH" ] || [ -z "$LG_L_KEY" ] || [ -z "$LG_L_SAY1" ] \
   || [ "$LG_L_FRESH" -ge "$LG_L_KEY" ] || [ "$LG_L_KEY" -ge "$LG_L_SAY1" ]; then
  LG_BAD="$LG_BAD order(fresh@${LG_L_FRESH:-?} key@${LG_L_KEY:-?} step1@${LG_L_SAY1:-?})"
fi
if [ -z "$LG_BAD" ]; then pass; else fail "$LG_BAD"; fi

# The knob's validation, run as written with ok/ko/clear stubbed: an unset or
# 0 value is silent, 1 clears, and anything else is refused before a key is
# computed -- a mistyped value must not be read either way by accident.
it "OAM_CI_FRESH: unset and 0 do nothing, 1 clears the ledger, anything else is refused"
# index(), not a regex: the braces and the dollar would need escapes that mawk
# and gawk read differently.
LG_CASE="$(awk 'index($0, "case \"${OAM_CI_FRESH:-}\" in") == 1 { f = 1 } f { print } f && /^esac$/ { exit }' scripts/ci-local.sh)"
LG_STUBS='ok(){ echo "ok"; }; ko(){ echo "ko"; exit 1; }; ci_ledger_clear(){ echo "clear $1"; }; CI_LEDGER_DIR=L'
LG_GOT=""
for v in unset 0 1 yes; do
  if [ "$v" = "unset" ]; then LG_OUT="$(bash -c "$LG_STUBS; $LG_CASE" 2>/dev/null | tr '\n' ' ')"
  else LG_OUT="$(OAM_CI_FRESH="$v" bash -c "$LG_STUBS; $LG_CASE" 2>/dev/null | tr '\n' ' ')"; fi
  LG_GOT="$LG_GOT$v:[${LG_OUT% }] "
done
if [ -z "$LG_CASE" ]; then fail "the OAM_CI_FRESH case block was not found in ci-local.sh"
else eq "${LG_GOT% }" "unset:[] 0:[] 1:[clear L ok] yes:[ko]"; fi

# The two helpers every step goes through, sliced verbatim and run against a
# fixture ledger: the first ask counts the step as run and says no, the mark
# records it, the second ask says yes with the notice the operator reads.
it "step_done_earlier / step_passed: ask, run, record, then skip with the notice -- and the counts follow"
LG_FNS="$(awk '/^step_done_earlier\(\) \{$/ || /^step_passed\(\) \{$/ { f = 1 } f { print } f && /^}$/ { f = 0 }' scripts/ci-local.sh)"
LG_W="$SUITE_TMP/ledger-wire"
LG_OUT="$(bash -c ". scripts/lib/ci-ledger.sh; ok(){ echo \"ok \$*\"; }; warn(){ echo \"warn \$*\"; }
  CI_LEDGER_DIR='$LG_W'; CI_KEY='$LG_K1'; STEPS_RAN=0; STEPS_SKIPPED=0; $LG_FNS
  step_done_earlier 06-tests && echo first-said-yes
  step_passed 06-tests
  step_done_earlier 06-tests || echo second-said-no
  CI_KEY='$LG_K2'; step_done_earlier 06-tests && echo other-key-said-yes
  echo \"ran=\$STEPS_RAN skipped=\$STEPS_SKIPPED\"" 2>&1)"
if [ "$(grep -c '() {$' <<<"$LG_FNS")" != "2" ]; then fail "step_done_earlier/step_passed not found in ci-local.sh"
elif grep -q 'said' <<<"$LG_OUT"; then fail "wrong answers:$(printf '\n%s' "$LG_OUT")"
elif ! grep -qxF "ok 06-tests -- passed earlier on this exact tree ($LG_W), skipped; OAM_CI_FRESH=1 runs it again" <<<"$LG_OUT"; then
  fail "the skip notice changed:$(printf '\n%s' "$LG_OUT")"
elif ! grep -qx 'ran=2 skipped=1' <<<"$LG_OUT"; then fail "counts:$(printf '\n%s' "$LG_OUT")"
else pass; fi

# Which steps consult the ledger is the safety question. Every gating step but
# 5 must; 5 must NOT, and must say why: it is the no-op rebuild that
# guarantees target/debug/oam is the default-feature binary steps 7-9 run,
# after step 4 (or a hand build) left a napi-less one -- target/ is outside
# the key, so nothing else could notice.
it "every gating step but the build consults the ledger and records its pass; the build says why it never skips"
LG_ASKED="$(grep -oE 'step_done_earlier [0-9][0-9]-[a-z-]+' scripts/ci-local.sh | sed 's/step_done_earlier //' | sort -u | tr '\n' ' ')"
LG_MARKED="$(grep -oE 'step_passed [0-9][0-9]-[a-z-]+' scripts/ci-local.sh | sed 's/step_passed //' | sort -u | tr '\n' ' ')"
LG_WANT="01-control-bytes 02-fmt 03-clippy 04-feature-off 06-tests 07-smoke 08-conformance 09-node-suite 10-attribution 11-unsafe-budget 12-npm 13-scripts 14-miri "
if [ "$LG_ASKED" != "$LG_WANT" ]; then fail "steps consulting the ledger: '$LG_ASKED' -- want '$LG_WANT'"
elif [ "$LG_MARKED" != "$LG_WANT" ]; then fail "steps recording a pass: '$LG_MARKED' -- want '$LG_WANT'"
elif ! grep -q 'ci_ledger_passed "\$CI_LEDGER_DIR" "\$CI_KEY"' scripts/ci-local.sh || ! grep -q 'ci_ledger_mark "\$CI_LEDGER_DIR" "\$CI_KEY"' scripts/ci-local.sh; then
  fail "the helpers no longer decide through ci_ledger_passed / ci_ledger_mark"
elif ! grep -q 'The one step the ledger never skips' scripts/ci-local.sh; then fail "step 5's exemption lost its explanation"
else pass; fi

# A mark must be the LAST statement of a step's body, after every ko: a step
# recorded before its own checks ran would be skipped next time on the
# strength of nothing. Checked for the steps whose body ends in a mark on its
# own line -- the mark's line must come after the step's last ko.
it "a step's pass is recorded after its last failure exit, never before"
LG_BAD=""
for s in 02-fmt 03-clippy 04-feature-off 06-tests 07-smoke 08-conformance 09-node-suite 11-unsafe-budget; do
  LG_ASK="$(lg_line "step_done_earlier $s")"; LG_MARK="$(lg_line "step_passed $s")"
  LG_LASTKO="$(awk -v a="$LG_ASK" -v m="$LG_MARK" 'NR > a && NR < m && /^ *(\|\| )?ko "/ { n = NR } END { print n + 0 }' scripts/ci-local.sh)"
  [ -n "$LG_ASK" ] && [ -n "$LG_MARK" ] && [ "$LG_LASTKO" -gt "$LG_ASK" ] && [ "$LG_LASTKO" -lt "$LG_MARK" ] \
    || LG_BAD="$LG_BAD $s(ask@${LG_ASK:-?} last-ko@$LG_LASTKO mark@${LG_MARK:-?})"
done
if [ -z "$LG_BAD" ]; then pass; else fail "mark not after the step's last ko:$LG_BAD"; fi

# Steps 8 and 9 are the two whose xtask rewrites TRACKED receipts; the key is
# re-read right after each, inside the branch that ran it, and nowhere else.
it "the key is re-read after steps 8 and 9, which rewrite tracked receipts, and only there"
LG_REKEYS="$(grep -c '^ *rekey_after "step' scripts/ci-local.sh)"
LG_L_CONF="$(lg_line 'if cargo run -p xtask -- conformance; then')"; LG_L_RK8="$(lg_line 'rekey_after "step 8')"
LG_L_NODE="$(lg_line 'if cargo run -p xtask -- node-suite; then')"; LG_L_RK9="$(lg_line 'rekey_after "step 9')"
if [ "$LG_REKEYS" = "2" ] && [ -n "$LG_L_CONF" ] && [ -n "$LG_L_RK8" ] && [ -n "$LG_L_NODE" ] && [ -n "$LG_L_RK9" ] \
   && [ "$LG_L_CONF" -lt "$LG_L_RK8" ] && [ "$LG_L_RK8" -lt "$LG_L_NODE" ] && [ "$LG_L_NODE" -lt "$LG_L_RK9" ]; then pass
else fail "rekey_after calls: $LG_REKEYS (conformance@${LG_L_CONF:-?} rekey8@${LG_L_RK8:-?} node-suite@${LG_L_NODE:-?} rekey9@${LG_L_RK9:-?})"; fi

it "the final line says how many steps ran and how many were skipped"
ck grep -qF 'All local CI gates passed ($STEPS_RAN steps ran, $STEPS_SKIPPED skipped' scripts/ci-local.sh

it "release-local.sh runs the gate unchanged, and its header says a same-tree re-run skips the passed steps"
if grep -qxF '  bash "$SCRIPT_DIR/ci-local.sh" || fail "local CI gate failed -- fix before releasing"' scripts/release-local.sh \
   && grep -q '^#.*target/ci-local/passed' scripts/release-local.sh; then pass
else fail "release-local.sh's gate call changed, or its header no longer mentions the ledger"; fi

# =============================================================================
group "attribution -- drift decision, comparison and delta"
# =============================================================================
# THIRD_PARTY_LICENSES.md is generated, shipped with every asset, and was gated
# ONLY by the full ci-local.sh run: --fast skipped step 10 outright, so a PR that
# changed Cargo.lock and gated with --fast landed stale attribution and the
# v0.15.0 RELEASE was the first thing to notice. The fix has two halves -- a
# --fast skip rule that keys on the inputs, and a preflight reconcile in
# release-local.sh -- and the logic both share lives in lib/attribution.sh so
# it can be driven here with fixtures, on boxes with or without cargo-about.
# shellcheck source=lib/attribution.sh
. scripts/lib/attribution.sh

ATTR_FIX="$SUITE_TMP/attr"
mkdir -p "$ATTR_FIX"
# A committed file and a fresh generate that differ by exactly the v0.15.0
# drift: one summary count and three crates under one license's "Used by:".
printf -- '- Apache License 2.0 -- 254 crate(s)\nUsed by:\n- aho-corasick 1.1.3\n- bytes 1.10.1\n- tower-http 0.6.11\n' > "$ATTR_FIX/committed.md"
printf -- '- Apache License 2.0 -- 257 crate(s)\nUsed by:\n- aho-corasick 1.1.3\n- async-compression 0.4.43\n- bytes 1.10.1\n- compression-codecs 0.4.38\n- compression-core 0.4.32\n- tower-http 0.6.11\n' > "$ATTR_FIX/fresh.md"
# The same bytes as committed.md but CRLF -- what an old clone or a stray
# core.autocrlf produces.
sed 's/$/\r/' "$ATTR_FIX/committed.md" > "$ATTR_FIX/committed-crlf.md"
# A crate REMOVED (tower-http gone) and nothing added.
printf -- '- Apache License 2.0 -- 253 crate(s)\nUsed by:\n- aho-corasick 1.1.3\n- bytes 1.10.1\n' > "$ATTR_FIX/removed.md"

it "attribution_matches: identical files match"
ck attribution_matches "$ATTR_FIX/committed.md" "$ATTR_FIX/committed.md"

it "attribution_matches: CR bytes carry no meaning (CRLF copy still matches)"
ck attribution_matches "$ATTR_FIX/committed.md" "$ATTR_FIX/committed-crlf.md"

it "attribution_matches: a real drift does not match"
if attribution_matches "$ATTR_FIX/fresh.md" "$ATTR_FIX/committed.md"; then
  fail "fresh.md and committed.md differ by three crates but matched"
else pass; fi

# The delta is what the release commit body says and what the operator reads in
# the log, so its shape is asserted exactly: crate lines only, +/- prefixed, in
# file order, and NOT the summary-count line.
it "attribution_delta: reports each crate that entered, and only those"
ATTR_DELTA="$(attribution_delta "$ATTR_FIX/committed.md" "$ATTR_FIX/fresh.md")"
ATTR_WANT="$(printf '+ async-compression 0.4.43\n+ compression-codecs 0.4.38\n+ compression-core 0.4.32')"
if [ "$ATTR_DELTA" = "$ATTR_WANT" ]; then pass
else fail "got:$(printf '\n%s' "$ATTR_DELTA")$(printf '\nwant:\n%s' "$ATTR_WANT")"; fi

it "attribution_delta: reports a crate that left"
ATTR_DELTA="$(attribution_delta "$ATTR_FIX/committed.md" "$ATTR_FIX/removed.md")"
if [ "$ATTR_DELTA" = "- tower-http 0.6.11" ]; then pass
else fail "got '$ATTR_DELTA', want '- tower-http 0.6.11'"; fi

# diff exits 1 whenever the files differ, which is every real call. Both
# callers run under `set -e -o pipefail`; a bare `diff | awk` there would abort
# the release on the exact input the function exists for. So: differing files
# must return 0 with output, identical files must return 0 with NO output, and
# an unreadable side must NOT be reported as an empty (clean) delta.
it "attribution_delta: returns 0 on a drift and 0-with-nothing on identical files"
ATTR_RC=0
ATTR_OUT="$(attribution_delta "$ATTR_FIX/committed.md" "$ATTR_FIX/fresh.md")" || ATTR_RC=$?
ATTR_RC2=0
ATTR_OUT2="$(attribution_delta "$ATTR_FIX/committed.md" "$ATTR_FIX/committed.md")" || ATTR_RC2=$?
if [ "$ATTR_RC" = 0 ] && [ -n "$ATTR_OUT" ] && [ "$ATTR_RC2" = 0 ] && [ -z "$ATTR_OUT2" ]; then pass
else fail "drift: rc=$ATTR_RC out='$ATTR_OUT'; identical: rc=$ATTR_RC2 out='$ATTR_OUT2'"; fi

it "attribution_delta: an unreadable side is an error, not an empty delta"
ATTR_RC=0
ATTR_OUT="$(attribution_delta "$ATTR_FIX/committed.md" "$ATTR_FIX/does-not-exist.md" 2>/dev/null)" || ATTR_RC=$?
if [ "$ATTR_RC" != 0 ]; then pass; else fail "missing file returned 0 with '$ATTR_OUT'"; fi

# The --fast rule, as the pure function ci-local.sh calls. It must fail toward
# RUNNING: no base and any changed input both mean run; only a provably
# unchanged input set may skip. The prefix is the contract ci-local.sh
# dispatches on, so it is asserted literally.
it "attribution_fast_decision: no base to compare against -> run"
case "$(attribution_fast_decision "" "")" in run:*) pass ;; *) fail "expected run:<why>" ;; esac

it "attribution_fast_decision: a changed input -> run, naming the path"
ATTR_DEC="$(attribution_fast_decision deadbeef "Cargo.lock")"
case "$ATTR_DEC" in run:*Cargo.lock*) pass ;; *) fail "got '$ATTR_DEC'" ;; esac

it "attribution_fast_decision: several changed inputs are all named on one line"
ATTR_DEC="$(attribution_fast_decision deadbeef "$(printf 'Cargo.lock\nabout.toml')")"
case "$ATTR_DEC" in
  run:*Cargo.lock*about.toml*) [ "$(printf '%s\n' "$ATTR_DEC" | wc -l)" -eq 1 ] && pass || fail "multi-line: '$ATTR_DEC'" ;;
  *) fail "got '$ATTR_DEC'" ;;
esac

it "attribution_fast_decision: base present and nothing changed -> skip"
case "$(attribution_fast_decision deadbeef "")" in skip:*) pass ;; *) fail "expected skip:<why>" ;; esac

# The input list is the whole basis for the skip. Every path that can move the
# generated file must be on it: the lock, the workspace and member manifests
# (publish=false is what about.toml's private filter keys on; features and
# target deps start in a Cargo.toml), the generator's config and template, and
# the output itself (a hand edit must be re-verified). Measured against the
# real index, which is what ci-local.sh does.
it "attribution_inputs covers the lock, every workspace manifest, about.*, and the output"
ATTR_INPUTS="$(attribution_inputs)"
ATTR_MISSING=""
for want in Cargo.lock Cargo.toml about.toml about.hbs THIRD_PARTY_LICENSES.md \
            $(git ls-files -- 'crates/*/Cargo.toml' xtask/Cargo.toml); do
  case $'\n'"$ATTR_INPUTS"$'\n' in *$'\n'"$want"$'\n'*) ;; *) ATTR_MISSING="$ATTR_MISSING $want" ;; esac
done
if [ -z "$ATTR_MISSING" ]; then pass; else fail "attribution_inputs is missing:$ATTR_MISSING"; fi

# --- wiring: the gate and the release must decide through the lib ------------
# An inline re-implementation of either half would be untested again.
it "ci-local.sh --fast dispatches step 10 through attribution_fast_decision"
ATTR_WIRE=""
for want in 'attribution_fast_decision "$attr_base" "$attr_changed"' 'attribution_changed_paths "$attr_base"' \
            'run:\*)  attribution_step' 'skip:\*) say "10/14 Attribution SKIPPED' \
            'attribution_matches "$attr_tmp" THIRD_PARTY_LICENSES.md' '\. scripts/lib/attribution.sh'; do
  grep -q -- "$want" scripts/ci-local.sh || ATTR_WIRE="$ATTR_WIRE [$want]"
done
if [ -z "$ATTR_WIRE" ]; then pass; else fail "ci-local.sh no longer references:$ATTR_WIRE"; fi

it "ci-local.sh --fast no longer skips step 10 unconditionally"
if grep -q '8/14 + 9/14 + 10/14' scripts/ci-local.sh; then
  fail "the combined '8/14 + 9/14 + 10/14 ... SKIPPED (--fast)' label is back -- attribution is being skipped with conformance again"
else pass; fi

it "release-local.sh reconciles attribution through the lib and lands it via land_on_main"
ATTR_WIRE=""
for want in 'step "Attribution reconcile' 'attribution_matches "$attr_tmp" THIRD_PARTY_LICENSES.md' \
            'attribution_delta THIRD_PARTY_LICENSES.md "$attr_tmp"' \
            'land_on_main "chore(attribution): regenerate THIRD_PARTY_LICENSES.md for $TAG"' \
            'OAM_NO_AUTO_ATTRIBUTION' 'the attribution reconcile touched unexpected paths' \
            '\. "$SCRIPT_DIR/lib/attribution.sh"'; do
  grep -q -- "$want" scripts/release-local.sh || ATTR_WIRE="$ATTR_WIRE [$want]"
done
if [ -z "$ATTR_WIRE" ]; then pass; else fail "release-local.sh no longer carries:$ATTR_WIRE"; fi

# The whole point: reconcile BEFORE the tag exists, so a drift never leaves a
# tag on origin pointing at a tree the gate rejects (the v0.15.0 shape, and the
# changelog gate's before it). Asserted by line order, since that is the
# property -- a correct block moved below the tag push is the bug again.
it "release-local.sh reconciles attribution BEFORE creating or pushing the tag"
ATTR_LINE="$(grep -n 'step "Attribution reconcile' scripts/release-local.sh | head -1 | cut -d: -f1)"
TAG_LINE="$(grep -n 'git tag -a "$TAG" -m "$TAG"' scripts/release-local.sh | head -1 | cut -d: -f1)"
BUMP_LINE="$(grep -n 'land_on_main "chore(release): bump workspace version' scripts/release-local.sh | head -1 | cut -d: -f1)"
if [ -n "$ATTR_LINE" ] && [ -n "$TAG_LINE" ] && [ -n "$BUMP_LINE" ] \
   && [ "$BUMP_LINE" -lt "$ATTR_LINE" ] && [ "$ATTR_LINE" -lt "$TAG_LINE" ]; then pass
else fail "order is bump@$BUMP_LINE attribution@$ATTR_LINE tag@$TAG_LINE -- want bump < attribution < tag"; fi

it "release-local.sh requires cargo-about (fails closed) rather than warning like the gate"
if grep -q 'fail "cargo-about not installed' scripts/release-local.sh \
   && ! grep -q 'warn "cargo-about not installed' scripts/release-local.sh; then pass
else fail "the release must hard-fail without cargo-about; only ci-local.sh may warn"; fi

# =============================================================================
group "signing.sh -- bootstrap decision and the committed key files"
# =============================================================================
# The release's trust root is two hand-edited files plus one rule: with no key
# committed, skip loudly (fail under OAM_SIGN_REQUIRED=1); with any key
# committed, sign, and nothing can turn that off. None of this needs ssh.
#
# The operator's own knob must not leak into the fixtures' verdicts.
unset OAM_SIGN_REQUIRED OAM_RELEASE_SIGNING_KEY
SG="$SUITE_TMP/signing"
mkdir -p "$SG"
# sg <keys-dir> <command...> -- run <command> in a subshell with the lib
# sourced, its trust root pointed at <keys-dir>, and the Windows agent-service
# probes pointed at $SG_INBOX and $SG_REG (default: nothing). A test must never
# consult the operator's real agent service or its registry store, let alone
# depend on what they hold. The subshell's own EXIT trap stops any agent a
# failing case leaves running.
sg(){
  local keys="$1"; shift
  ( # shellcheck source=lib/signing.sh
    . scripts/lib/signing.sh
    RELEASE_KEYS_DIR="$keys"
    RELEASE_INBOX_SSH_ADD="${SG_INBOX:-$SG/no-such-inbox-ssh-add}"
    RELEASE_INBOX_REG="${SG_REG:-$SG/no-such-reg}"
    trap release_agent_stop EXIT
    "$@" )
}
# sg_keys <dir> <allowed_signers body> <ranges body>
sg_keys(){ mkdir -p "$1"; printf '%s' "$2" >"$1/allowed_signers"; printf '%s' "$3" >"$1/ranges"; }
# A syntactically valid blob; the lint checks shape, ssh-keygen checks keys.
SG_K='AAAAC3NzaC1lZDI1NTE5AAAAIExampleExampleExampleExampleExampleExample'
SG_NS='namespaces="oam-release"'

it "the committed release-keys/ files are well-formed"
SG_LINT="$(sg "$REPO_DIR/release-keys" release_keys_lint 2>&1)" && pass || fail "$SG_LINT"

it "the committed release-keys/ decide sign or skip, never fail, with the knob unset"
SG_D="$(sg "$REPO_DIR/release-keys" release_signing_decision)"
case "$SG_D" in sign | skip:*) pass ;; *) fail "got '$SG_D'" ;; esac

SG_BOOT="$SG/boot"
sg_keys "$SG_BOOT" $'# comments only\n\n' $'# comments only\n'
it "no key committed: the manifest step is skipped, with a loud reason"
SG_D="$(sg "$SG_BOOT" release_signing_decision)"
case "$SG_D" in skip:*"WITHOUT a signed RELEASE-MANIFEST"*) pass ;; *) fail "got '$SG_D'" ;; esac

it "no key committed + OAM_SIGN_REQUIRED=1: fatal, and the way out is an explicit 0 (release-local.sh defaults an unset knob to 1)"
SG_D="$(OAM_SIGN_REQUIRED=1 sg "$SG_BOOT" release_signing_decision)"
case "$SG_D" in fail:*"OAM_SIGN_REQUIRED=1"*"set OAM_SIGN_REQUIRED=0"*) pass ;; *) fail "got '$SG_D'" ;; esac

it "OAM_SIGN_REQUIRED takes 0 or 1 and nothing else"
SG_D="$(OAM_SIGN_REQUIRED=yes sg "$SG_BOOT" release_signing_decision)"
case "$SG_D" in fail:*"0 or 1"*) pass ;; *) fail "got '$SG_D'" ;; esac

SG_ONE="$SG/one"
sg_keys "$SG_ONE" "oam-release-k1 $SG_NS ssh-ed25519 $SG_K"$'\n' $'k1 v0.18.0 -\n'
it "a committed key makes signing mandatory, whatever OAM_SIGN_REQUIRED says"
SG_D="$(OAM_SIGN_REQUIRED=0 sg "$SG_ONE" release_signing_decision) $(OAM_SIGN_REQUIRED=1 sg "$SG_ONE" release_signing_decision)"
eq "$SG_D" "sign sign"

# A READ of the variable ($X or ${X...}), not the name: the Windows decision's
# messages name OAM_SKIP_WIN_SIGN, which its caller reads and hands in.
it "the lib has no knob that skips the manifest"
if grep -v '^[[:space:]]*#' scripts/lib/signing.sh | grep -qE '\$\{?(OAM_SKIP|OAM_NO_SIGN|[A-Z_]*SKIP_SIGN|SKIP_MANIFEST)|printenv'; then
  fail "scripts/lib/signing.sh reads a skip knob"
else pass; fi

# One fixture per rule; every one must be REFUSED. A lint that let any of them
# through would trust a key for the wrong namespace, for every principal, or
# for an ambiguous range.
it "malformed key files are refused, not trusted (one case per rule)"
SG_ACCEPTED=""
sg_lint_case(){ # <label> <allowed_signers body> <ranges body>
  sg_keys "$SG/lint-$1" "$2" "$3"
  if sg "$SG/lint-$1" release_keys_lint 2>/dev/null; then SG_ACCEPTED="$SG_ACCEPTED $1"; fi
}
sg_lint_case no-namespace    "oam-release-k1 ssh-ed25519 $SG_K"$'\n' ''
sg_lint_case wrong-namespace "oam-release-k1 namespaces=\"git\" ssh-ed25519 $SG_K"$'\n' ''
sg_lint_case pattern         "oam-release-* $SG_NS ssh-ed25519 $SG_K"$'\n' ''
sg_lint_case list            "oam-release-k1,oam-release-k2 $SG_NS ssh-ed25519 $SG_K"$'\n' ''
sg_lint_case foreign         "someone $SG_NS ssh-ed25519 $SG_K"$'\n' ''
sg_lint_case rsa             "oam-release-k1 $SG_NS ssh-rsa $SG_K"$'\n' ''
sg_lint_case no-blob         "oam-release-k1 $SG_NS ssh-ed25519"$'\n' ''
sg_lint_case dup-key         "oam-release-k1 $SG_NS ssh-ed25519 $SG_K"$'\n'"oam-release-k1 $SG_NS ssh-ed25519 $SG_K"$'\n' ''
sg_lint_case dup-range       '' $'k1 v0.18.0 -\nk1 v0.1.0 -\n'
sg_lint_case backwards       '' $'k1 v0.19.0 v0.18.0\n'
sg_lint_case prerelease      '' $'k1 v0.18.0-rc.1 -\n'
sg_lint_case short-line      '' $'k1 v0.18.0\n'
sg_lint_case long-line       '' $'k1 v0.18.0 - extra\n'
if [ -z "$SG_ACCEPTED" ]; then pass; else fail "accepted:$SG_ACCEPTED"; fi

it "a key with no range line (the staged next key) lints clean"
sg_keys "$SG/staged" "oam-release-k1 $SG_NS ssh-ed25519 $SG_K"$'\n'"oam-release-k2 $SG_NS ssh-ed25519 $SG_K"$'\n' $'k1 v0.18.0 -\n'
SG_LINT="$(sg "$SG/staged" release_keys_lint 2>&1)" && pass || fail "$SG_LINT"

sg_le(){ if _rs_tag_le "$1" "$2"; then printf 'le'; else printf 'gt'; fi; }
it "tags compare numerically per field (v0.10.0 is after v0.9.0; v0.08.0 is not octal)"
eq "$(sg "$SG_ONE" sg_le v0.9.0 v0.10.0) $(sg "$SG_ONE" sg_le v0.10.0 v0.9.0) $(sg "$SG_ONE" sg_le v1.0.0 v1.0.0) $(sg "$SG_ONE" sg_le v0.08.0 v0.9.0)" "le gt le le"

sg_in(){ if release_tag_in_range "$1" "$2" 2>/dev/null; then printf 'in'; else printf 'out'; fi; }
sg_in_all(){ local p="$1" t out=""; shift; for t in "$@"; do out="$out$(sg_in "$p" "$t") "; done; printf '%s' "${out% }"; }
it "a closed range is inclusive at both ends and nothing outside it"
sg_keys "$SG/closed" "oam-release-k1 $SG_NS ssh-ed25519 $SG_K"$'\n' $'k1 v0.18.0 v0.19.5\n'
eq "$(sg "$SG/closed" sg_in_all oam-release-k1 v0.17.9 v0.18.0 v0.19.5 v0.19.6 v1.0.0)" "out in in out out"

it "an open range ('-') runs forever; a staged key with no range signs nothing"
eq "$(sg "$SG/staged" sg_in_all oam-release-k1 v9.0.0 v0.17.0) $(sg "$SG/staged" sg_in oam-release-k2 v0.18.0)" "in out out"

# The arm64 patch path decides "a release with no manifest is benign" from
# this, never from the asset list alone: deleting the pair is how an attacker
# with upload access would pass a signed release off as a pre-signing one.
sg_pre_sig(){ if release_tag_predates_signing "$1" 2>/dev/null; then printf 'pre'; else printf 'era'; fi; }
sg_pre_sig_all(){ local t out=""; for t in "$@"; do out="$out$(sg_pre_sig "$t") "; done; printf '%s' "${out% }"; }
it "predates signing: only tags before EVERY range start; a gap between ranges is still the signing era"
sg_keys "$SG/gap" "oam-release-k1 $SG_NS ssh-ed25519 $SG_K"$'\n'"oam-release-k2 $SG_NS ssh-ed25519 $SG_K"$'\n' \
  $'k1 v0.18.0 v0.18.3\nk2 v0.18.5 -\n'
eq "$(sg "$SG/gap" sg_pre_sig_all v0.17.9 v0.18.0 v0.18.4 v0.19.0 v0.9.99)" "pre era era era pre"

it "predates signing: no range at all (bootstrap, or only a staged key) means every tag predates it"
sg_keys "$SG/staged-only" "oam-release-k2 $SG_NS ssh-ed25519 $SG_K"$'\n' $'# no range yet\n'
eq "$(sg "$SG_BOOT" sg_pre_sig v9.9.9) $(sg "$SG/staged-only" sg_pre_sig v9.9.9)" "pre pre"

# release_keys_from_commit: the arm64 patch path reads the CURRENT trust root
# from origin/main rather than the old tag's frozen copy. Against a real git
# repo with two commits, so "the files at that commit" is tested, not "the
# files on disk".
it "release_keys_from_commit reads release-keys/ at the named commit, not the working tree"
SG_KREPO="$SG/keys-repo"
mkdir -p "$SG_KREPO/release-keys"
sg_kgit(){ git -C "$SG_KREPO" -c user.name=t -c user.email=t@example.invalid -c commit.gpgsign=false "$@"; }
sg_keys "$SG_KREPO/release-keys" "oam-release-k1 $SG_NS ssh-ed25519 $SG_K"$'\n' $'k1 v0.18.0 -\n'
{ sg_kgit init -q && sg_kgit add release-keys && sg_kgit commit -qm one; } >/dev/null 2>&1
SG_C1="$(sg_kgit rev-parse HEAD 2>/dev/null)"
sg_keys "$SG_KREPO/release-keys" "oam-release-k1 $SG_NS ssh-ed25519 $SG_K"$'\n' $'k1 v0.18.0 v0.18.3\n'
{ sg_kgit add release-keys && sg_kgit commit -qm two; } >/dev/null 2>&1
SG_C2="$(sg_kgit rev-parse HEAD 2>/dev/null)"
printf 'k1 v0.1.0 -\n' >"$SG_KREPO/release-keys/ranges"   # uncommitted: must not be read
sg_from(){ # <commit> <out-dir>
  mkdir -p "$2"; cd "$SG_KREPO" || return 1
  release_keys_from_commit "$1" "$2" || return 1
  printf 'DIR=%s RANGE=%s\n' "$RELEASE_KEYS_DIR" "$(awk '!/^#/' "$RELEASE_KEYS_DIR/ranges")"
}
SG_OUT="$(sg "$SG_ONE" sg_from "$SG_C1" "$SG/from1" 2>&1) | $(sg "$SG_ONE" sg_from "$SG_C2" "$SG/from2" 2>&1)"
eq "$SG_OUT" "DIR=$SG/from1 RANGE=k1 v0.18.0 - | DIR=$SG/from2 RANGE=k1 v0.18.0 v0.18.3"

it "release_keys_from_commit: a commit this clone lacks is refused, and the trust root is left alone"
sg_from_bad(){
  cd "$SG_KREPO" || return 1
  release_keys_from_commit 0123456789abcdef0123456789abcdef01234567 "$SG/from-bad"
  local rc=$?
  printf 'DIR=%s\n' "$RELEASE_KEYS_DIR"
  return $rc
}
mkdir -p "$SG/from-bad"
SG_RC=0; SG_OUT="$(sg "$SG_ONE" sg_from_bad 2>&1)" || SG_RC=$?
if [ "$SG_RC" != "0" ] && grep -qF 'is not in this clone' <<<"$SG_OUT" && grep -qxF "DIR=$SG_ONE" <<<"$SG_OUT"; then pass
else fail "rc=$SG_RC: $SG_OUT"; fi

# =============================================================================
group "signing.sh -- a real ssh-keygen round trip"
# =============================================================================
# Real keys, a real private agent, real signatures: the failure modes worth
# catching here (a namespace that is not enforced, a principal loop that
# accepts the first key it sees, an agent that outlives the run) are behaviour
# of ssh-keygen and ssh-agent, which no stub would encode correctly.
SG_SSH=0
if command -v ssh-keygen >/dev/null 2>&1 && command -v ssh-add >/dev/null 2>&1 \
   && command -v ssh-agent >/dev/null 2>&1; then
  # k1 current, k2 the staged next key (no range), k3 a stranger to
  # allowed_signers. Unencrypted: the passphrase case below makes its own.
  for k in k1 k2 k3; do
    ssh-keygen -q -t ed25519 -N '' -C "oam-release-$k" -f "$SG/$k" </dev/null >/dev/null 2>&1 || break
  done
  printf 'probe\n' >"$SG/probe"
  if [ -f "$SG/k3" ] && ssh-keygen -Y sign -f "$SG/k1" -n oam-release "$SG/probe" </dev/null >/dev/null 2>&1 \
     && [ -s "$SG/probe.sig" ]; then
    SG_SSH=1
  fi
fi
it "this host's ssh-keygen signs with -Y"
if [ "$SG_SSH" = "1" ]; then pass
else skip "no ssh-keygen/ssh-add/ssh-agent with -Y here (OpenSSH >= 8.2) -- every round-trip case below is skipped"; fi

if [ "$SG_SSH" = "1" ]; then
  sg_pub(){ cut -d' ' -f1,2 "$SG/$1.pub"; }
  SG_TRUST="$SG/trust"
  sg_keys "$SG_TRUST" \
    "oam-release-k1 $SG_NS $(sg_pub k1)"$'\n'"oam-release-k2 $SG_NS $(sg_pub k2)"$'\n' \
    $'k1 v0.18.0 v0.19.5\n'
  # sg_reldir <dir> -- two stand-in binaries and their SHA256SUMS, the way
  # release-local.sh writes it.
  # shasum where sha256sum is absent (macOS before 14); the bytes are the same.
  sg_reldir(){
    mkdir -p "$1"; printf 'a\n' >"$1/oam-a"; printf 'b\n' >"$1/oam-b"
    if command -v sha256sum >/dev/null 2>&1; then ( cd "$1" && sha256sum oam-* >SHA256SUMS )
    else ( cd "$1" && shasum -a 256 oam-* >SHA256SUMS ); fi
  }
  # sg_flip <file> <line> -- change exactly one byte: the line's first
  # character (a hex digit here) becomes X. Through a temp file, not `sed -i`,
  # which BSD sed spells differently.
  sg_flip(){ sed "${2}s/^./X/" "$1" >"$1.flip" && mv "$1.flip" "$1"; }
  # sg_raw_sign <key> <namespace> <file> -- a signature made WITHOUT the lib,
  # for the forgeries the lib would never produce.
  sg_raw_sign(){ rm -f "$3.sig"; ssh-keygen -Y sign -f "$SG/$1" -n "$2" "$3" </dev/null >/dev/null 2>&1; }
  # sg_reject <keys> <dir> <tag> <needle> -- verify must fail, saying <needle>.
  sg_reject(){
    local out rc=0
    out="$(sg "$1" release_verify_manifest "$2" "$3" 2>&1)" || rc=$?
    if [ "$rc" != "0" ] && grep -qF -- "$4" <<<"$out"; then pass; else fail "rc=$rc, wanted '$4': $out"; fi
  }
  # sg_variant <name> -- a copy of the good release to damage.
  sg_variant(){ rm -rf "$SG/v-$1"; cp -R "$SG_GOOD" "$SG/v-$1"; printf '%s' "$SG/v-$1"; }

  # The full producer path, as release-local.sh runs it.
  sg_good(){ # <dir> <tag>
    OAM_RELEASE_SIGNING_KEY="$SG/k1" release_agent_start || return 1
    release_signing_preflight "$2" || return 1
    release_write_manifest "$1" "$2" || return 1
    release_sign_manifest "$1" || return 1
    release_verify_manifest "$1" "$2" || return 1
    printf 'AGENT %s %s\n' "$RELEASE_AGENT_PID" "$RELEASE_AGENT_DIR"
    release_agent_stop
  }
  SG_GOOD="$SG/good"
  sg_reldir "$SG_GOOD"
  SG_RC=0
  SG_OUT="$(sg "$SG_TRUST" sg_good "$SG_GOOD" v0.18.0 2>&1)" || SG_RC=$?

  it "good: the agent signs, and the manifest verifies from disk"
  if [ "$SG_RC" = "0" ] && grep -q 'RELEASE-MANIFEST verifies: v0.18.0, signed by oam-release-k1' <<<"$SG_OUT"; then pass
  else fail "rc=$SG_RC: $SG_OUT"; fi

  it "the manifest is the v1 header, the tag line, then SHA256SUMS byte for byte"
  { printf 'oam-release-manifest v1\ntag v0.18.0\n'; cat "$SG_GOOD/SHA256SUMS"; } >"$SG/expect"
  ck cmp -s "$SG/expect" "$SG_GOOD/RELEASE-MANIFEST"

  it "SHA256SUMS still covers the binaries only"
  if grep -q 'RELEASE-MANIFEST' "$SG_GOOD/SHA256SUMS"; then fail "SHA256SUMS lists the manifest"; else pass; fi

  it "the signature verifies with plain ssh-keygen, as a consumer would run it"
  sg_consumer_verify(){
    ssh-keygen -Y verify -f "$SG_TRUST/allowed_signers" -I oam-release-k1 -n oam-release \
      -s "$SG_GOOD/RELEASE-MANIFEST.sig" <"$SG_GOOD/RELEASE-MANIFEST" >/dev/null 2>&1
  }
  ck sg_consumer_verify

  it "a key without a passphrase is used, with a loud warning"
  grep -q 'has NO passphrase' <<<"$SG_OUT" && pass || fail "no passphrase warning in: $SG_OUT"

  # Best effort on Windows: kill -0 sees MSYS pids, which ssh-agent is there.
  it "release_agent_stop leaves no agent process and no socket directory"
  read -r _ SG_PID SG_ADIR <<<"$(grep '^AGENT ' <<<"$SG_OUT")"
  if [ -n "${SG_PID:-}" ] && ! kill -0 "$SG_PID" 2>/dev/null && [ -n "${SG_ADIR:-}" ] && [ ! -e "$SG_ADIR" ]; then pass
  else fail "pid '${SG_PID:-}' alive=$(kill -0 "${SG_PID:-0}" 2>/dev/null && echo yes || echo no), dir '${SG_ADIR:-}' exists=$([ -e "${SG_ADIR:-/nonexistent}" ] && echo yes || echo no)"; fi

  # --- forgeries and accidents: every one must be refused, for its reason ---
  it "one tampered byte in SHA256SUMS (manifest intact): refused"
  SG_V="$(sg_variant sums)"; sg_flip "$SG_V/SHA256SUMS" 1
  sg_reject "$SG_TRUST" "$SG_V" v0.18.0 "not byte-identical"

  it "one tampered byte in the manifest's SUMS section: the signature fails"
  SG_V="$(sg_variant manifest)"; sg_flip "$SG_V/RELEASE-MANIFEST" 3
  sg_reject "$SG_TRUST" "$SG_V" v0.18.0 "does not verify"

  it "signed by the right key in the wrong namespace: refused"
  SG_V="$(sg_variant ns)"; sg_raw_sign k1 oam-other "$SG_V/RELEASE-MANIFEST"
  sg_reject "$SG_TRUST" "$SG_V" v0.18.0 "does not verify"

  it "signed by a key not in allowed_signers: refused"
  SG_V="$(sg_variant stranger)"; sg_raw_sign k3 oam-release "$SG_V/RELEASE-MANIFEST"
  sg_reject "$SG_TRUST" "$SG_V" v0.18.0 "does not verify"

  it "signed by the staged next key, which has no range yet: refused"
  SG_V="$(sg_variant staged)"; sg_raw_sign k2 oam-release "$SG_V/RELEASE-MANIFEST"
  sg_reject "$SG_TRUST" "$SG_V" v0.18.0 "no line in release-keys/ranges"

  it "a tag after the key's range closed: refused"
  SG_V="$(sg_variant late)"; sg "$SG_TRUST" release_write_manifest "$SG_V" v0.20.0; sg_raw_sign k1 oam-release "$SG_V/RELEASE-MANIFEST"
  sg_reject "$SG_TRUST" "$SG_V" v0.20.0 "retired after v0.19.5"

  it "a tag before the key's range opened: refused"
  SG_V="$(sg_variant early)"; sg "$SG_TRUST" release_write_manifest "$SG_V" v0.17.9; sg_raw_sign k1 oam-release "$SG_V/RELEASE-MANIFEST"
  sg_reject "$SG_TRUST" "$SG_V" v0.17.9 "is before that"

  it "a valid manifest for another tag (replay): refused"
  sg_reject "$SG_TRUST" "$SG_GOOD" v0.18.1 "signed for tag 'v0.18.0', not v0.18.1"

  it "a signed manifest with another header version: refused"
  SG_V="$(sg_variant header)"
  { printf 'oam-release-manifest v2\ntag v0.18.0\n'; cat "$SG_V/SHA256SUMS"; } >"$SG_V/RELEASE-MANIFEST"
  sg_raw_sign k1 oam-release "$SG_V/RELEASE-MANIFEST"
  sg_reject "$SG_TRUST" "$SG_V" v0.18.0 "line 1 is 'oam-release-manifest v2'"

  it "a signed manifest with CRLF line endings: refused"
  SG_V="$(sg_variant crlf)"
  { printf 'oam-release-manifest v1\r\ntag v0.18.0\r\n'; cat "$SG_V/SHA256SUMS"; } >"$SG_V/RELEASE-MANIFEST"
  sg_raw_sign k1 oam-release "$SG_V/RELEASE-MANIFEST"
  sg_reject "$SG_TRUST" "$SG_V" v0.18.0 "header has CR line endings"

  it "a missing signature: refused"
  SG_V="$(sg_variant nosig)"; rm -f "$SG_V/RELEASE-MANIFEST.sig"
  sg_reject "$SG_TRUST" "$SG_V" v0.18.0 "RELEASE-MANIFEST.sig is missing"

  it "an allowed_signers with no key verifies nothing"
  sg_reject "$SG_BOOT" "$SG_GOOD" v0.18.0 "holds no key"

  # --- the agent and the preflight ---------------------------------------------
  sg_pre(){ OAM_RELEASE_SIGNING_KEY="$SG/$1" release_agent_start && release_signing_preflight "$2"; }
  sg_pre_rejects(){ # <key> <tag> <needle>
    local out rc=0
    out="$(sg "$SG_TRUST" sg_pre "$1" "$2" 2>&1)" || rc=$?
    if [ "$rc" != "0" ] && grep -qF -- "$3" <<<"$out"; then pass; else fail "rc=$rc, wanted '$3': $out"; fi
  }
  it "preflight: no OAM_RELEASE_SIGNING_KEY is fatal, by name"
  SG_RC=0; SG_OUT="$(sg "$SG_TRUST" release_agent_start 2>&1)" || SG_RC=$?
  if [ "$SG_RC" != "0" ] && grep -qF 'OAM_RELEASE_SIGNING_KEY is not set' <<<"$SG_OUT"; then pass; else fail "rc=$SG_RC: $SG_OUT"; fi

  it "preflight: a key that is not committed fails before anything is tagged"
  sg_pre_rejects k3 v0.18.0 "not a committed release key"

  it "preflight: a committed key outside its range for this tag fails"
  sg_pre_rejects k1 v0.20.0 "retired after v0.19.5"

  # The service check, against a stand-in that lists k1: the refusal must come
  # BEFORE any agent starts, so there is nothing to clean up after it.
  it "preflight refuses when the Windows agent service holds the key, starting no agent"
  SG_FP="$(ssh-keygen -lf "$SG/k1.pub" | awk '{print $2}')"
  printf '#!/bin/sh\necho "256 %s oam-release-k1 (ED25519)"\n' "$SG_FP" >"$SG/inbox-ssh-add"
  chmod +x "$SG/inbox-ssh-add"
  sg_inbox(){ OAM_RELEASE_SIGNING_KEY="$SG/k1" release_agent_start; local rc=$?; printf 'PID=[%s]\n' "$RELEASE_AGENT_PID"; return $rc; }
  SG_RC=0; SG_OUT="$(SG_INBOX="$SG/inbox-ssh-add" sg "$SG_TRUST" sg_inbox 2>&1)" || SG_RC=$?
  if [ "$SG_RC" != "0" ] && grep -qF 'Windows OpenSSH agent SERVICE holds the release key' <<<"$SG_OUT" \
     && grep -qF 'PID=[]' <<<"$SG_OUT"; then pass
  else fail "rc=$SG_RC: $SG_OUT"; fi

  # release-local.sh runs the preflight right before its dirty-tree check: a
  # probe file left in the repo would fail that check (or, worse, ship).
  #
  # The temp dir is a SHORT one of its own, not nested under SUITE_TMP: on
  # macOS SUITE_TMP sits under the per-user /var/folders/.../T/, and the agent
  # socket below it would be ~109 bytes, past sun_path. (release_agent_start
  # falls back to /tmp then, which would leave this case checking the wrong
  # directory; its own fallback has its own case below.)
  it "preflight leaves the working tree clean and its temp dir empty"
  SG_REPO="$SG/repo"; SG_TMP="$(mktemp -d /tmp/oamsg.XXXXXX)"
  mkdir -p "$SG_REPO"
  ( cd "$SG_REPO" && git init -q && printf 'x\n' >f && git add f \
      && git -c user.name=t -c user.email=t@example.invalid -c commit.gpgsign=false commit -qm init ) >/dev/null 2>&1
  sg_in_repo(){ cd "$SG_REPO" && sg_pre k1 v0.18.0 && release_agent_stop; }
  SG_RC=0; SG_OUT="$(TMPDIR="$SG_TMP" sg "$SG_TRUST" sg_in_repo 2>&1)" || SG_RC=$?
  SG_DIRTY="$(git -C "$SG_REPO" status --porcelain --untracked-files=all 2>&1)"
  SG_LEFT="$(ls -A "$SG_TMP")"
  if [ "$SG_RC" = "0" ] && [ -z "$SG_DIRTY" ] && [ -z "$SG_LEFT" ]; then pass
  else fail "rc=$SG_RC dirty='$SG_DIRTY' left-in-tmp='$SG_LEFT': $SG_OUT"; fi
  rm -rf "$SG_TMP"

  # The macOS shape, forced: a $TMPDIR so long the socket would not fit in
  # sun_path. ssh-agent would refuse it and exit, and the release would die on
  # "did not come up"; the lib must pick /tmp instead.
  it "an over-long TMPDIR: the agent socket goes under /tmp, and the agent comes up"
  SG_LONG="$SG/$(printf 'd%.0s' $(seq 1 90))"
  mkdir -p "$SG_LONG"
  sg_long(){ OAM_RELEASE_SIGNING_KEY="$SG/k1" release_agent_start || return 1; printf 'SOCK=%s\n' "$RELEASE_AGENT_SOCK"; }
  SG_RC=0; SG_OUT="$(TMPDIR="$SG_LONG" sg "$SG_TRUST" sg_long 2>&1)" || SG_RC=$?
  if [ "$SG_RC" = "0" ] && grep -q '^SOCK=/tmp/oam-sign\.' <<<"$SG_OUT"; then pass
  else fail "rc=$SG_RC: $SG_OUT"; fi

  # The service's AT-REST store, as reg.exe prints it: the release key's
  # public blob, hex, in a subkey's "pub" value. The service itself is absent
  # here (no inbox ssh-add), which is the stopped-service case: the store
  # alone must be enough to refuse.
  SG_HEX="$(cut -d' ' -f2 "$SG/k1.pub" | base64 -d | od -An -v -tx1 | tr -d ' \n' | tr 'a-f' 'A-F')"
  SG_HEX3="$(cut -d' ' -f2 "$SG/k3.pub" | base64 -d | od -An -v -tx1 | tr -d ' \n' | tr 'a-f' 'A-F')"
  sg_fake_reg(){ # <out-script> <pub-hex>
    { printf '%s\n' '' 'HKEY_CURRENT_USER\Software\OpenSSH\Agent\Keys' '' \
        'HKEY_CURRENT_USER\Software\OpenSSH\Agent\Keys\SHA256-stand-in' \
        '    (Default)    REG_BINARY    0102030405060708' "    pub    REG_BINARY    $2" \
        '    type    REG_DWORD    0x3' '    comment    REG_BINARY    6B6579'
    } >"$1.out"
    printf '#!/bin/sh\ncat "%s"\n' "$1.out" >"$1"
    chmod +x "$1"
  }
  sg_fake_reg "$SG/reg-k1" "$SG_HEX"
  sg_fake_reg "$SG/reg-k3" "$SG_HEX3"
  it "preflight refuses when the agent service's registry store holds the key, service stopped, starting no agent"
  SG_RC=0; SG_OUT="$(SG_REG="$SG/reg-k1" sg "$SG_TRUST" sg_inbox 2>&1)" || SG_RC=$?
  if [ "$SG_RC" != "0" ] && grep -qF 'live or in its registry store' <<<"$SG_OUT" \
     && grep -qF 'PID=[]' <<<"$SG_OUT"; then pass
  else fail "rc=$SG_RC: $SG_OUT"; fi

  it "a stopped service whose store holds only OTHER keys: warned about, not refused"
  printf '#!/bin/sh\necho "Error connecting to agent: No such file or directory" >&2\nexit 2\n' >"$SG/inbox-stopped"
  chmod +x "$SG/inbox-stopped"
  SG_RC=0; SG_OUT="$(SG_INBOX="$SG/inbox-stopped" SG_REG="$SG/reg-k3" sg "$SG_TRUST" sg_inbox 2>&1)" || SG_RC=$?
  if [ "$SG_RC" = "0" ] && grep -qF 'is not running, and its key store' <<<"$SG_OUT" \
     && grep -qF 'holds 1 key(s)' <<<"$SG_OUT"; then pass
  else fail "rc=$SG_RC: $SG_OUT"; fi

  # --- the release scripts' own EXIT handlers, run for real ----------------------
  # The wiring group checks the trap LINES exist; this runs the handler BODIES,
  # extracted verbatim from each script, in a child bash that sources the lib,
  # starts the agent with k1, traps the handler on EXIT, then dies -- by exit 1
  # (any fail()) or by SIGTERM. Replace release_agent_stop with ':' in either
  # handler and a passphrase-unlocked key outlives the run; these go red.
  sg_handler(){ # <script> <function-name> -- its definition, verbatim
    awk -v f="$2" '$0 == f "() {" { p = 1 } p { print } p && /^}$/ { exit }' "$1"
  }
  sg_handler scripts/release-local.sh release_on_exit >"$SG/h-local.sh"
  sg_handler scripts/release-upload-local-arm64.sh cleanup >"$SG/h-arm64.sh"
  cat >"$SG/exit-child.sh" <<'CHILD'
# $1 handler file, $2 handler name, $3 exit|term, $4 release dir, $5 RELEASE_LIVE
# shellcheck source=lib/signing.sh
. scripts/lib/signing.sh
RELEASE_KEYS_DIR="$SG_TRUST"
RELEASE_INBOX_SSH_ADD=/nonexistent; RELEASE_INBOX_REG=/nonexistent
. "$1"
tmp=""; trust_dir=""; RELEASE_DIR="$4"; RELEASE_LIVE="$5"
trap "$2" EXIT
OAM_RELEASE_SIGNING_KEY="$SG/k1" release_agent_start 2>/dev/null || exit 99
printf 'AGENT %s %s\n' "$RELEASE_AGENT_PID" "$RELEASE_AGENT_DIR"
case "$3" in
  exit) exit 1 ;;
  term) kill -TERM $$; sleep 5; exit 0 ;;
esac
CHILD
  sg_exit_case(){ # <handler-file> <handler-name> <exit|term> <live> -- "agent=.. dir=.. sig=.."
    local rel="$SG/hrel-$2-$3-$4" child="$SG/exit-child.sh" out pid adir
    rm -rf "$rel"; mkdir -p "$rel"; printf 'sig\n' >"$rel/RELEASE-MANIFEST.sig"
    out="$(SG="$SG" SG_TRUST="$SG_TRUST" bash "$child" "$1" "$2" "$3" "$rel" "$4" 2>&1)"
    read -r _ pid adir <<<"$(grep '^AGENT ' <<<"$out")"
    [ -n "${pid:-}" ] || { printf 'no-agent-started: %s' "$out"; return 0; }
    printf 'agent=%s dir=%s sig=%s' \
      "$(kill -0 "$pid" 2>/dev/null && echo alive || echo gone)" \
      "$([ -e "$adir" ] && echo left || echo gone)" \
      "$([ -e "$rel/RELEASE-MANIFEST.sig" ] && echo kept || echo gone)"
    kill "$pid" 2>/dev/null; rm -rf "$adir"
  }
  it "release-local.sh's EXIT handler stops the agent on a fail() exit, and drops the unpublished .sig"
  eq "$(sg_exit_case "$SG/h-local.sh" release_on_exit exit 0)" "agent=gone dir=gone sig=gone"
  it "release-local.sh's EXIT handler stops the agent on SIGTERM"
  eq "$(sg_exit_case "$SG/h-local.sh" release_on_exit term 0)" "agent=gone dir=gone sig=gone"
  it "release-local.sh's EXIT handler keeps the .sig of a release that went live"
  eq "$(sg_exit_case "$SG/h-local.sh" release_on_exit exit 1)" "agent=gone dir=gone sig=kept"
  it "release-upload-local-arm64.sh's EXIT handler stops the agent on exit 1 and on SIGTERM"
  eq "$(sg_exit_case "$SG/h-arm64.sh" cleanup exit 0) | $(sg_exit_case "$SG/h-arm64.sh" cleanup term 0)" \
     "agent=gone dir=gone sig=kept | agent=gone dir=gone sig=kept"

  # The passphrase is asked for once per run -- and once more only if the
  # agent's lifetime ran out before the manifest step, which is simulated by
  # emptying the agent. SSH_ASKPASS_REQUIRE=force (OpenSSH >= 8.4) stands in
  # for the operator; without it this case cannot run unattended.
  it "an encrypted key: one passphrase prompt for preflight + sign; one more after the lifetime lapses"
  ssh-keygen -q -t ed25519 -N 'test-passphrase' -C oam-release-k1 -f "$SG/k1e" </dev/null >/dev/null 2>&1
  sg_keys "$SG/trust-e" "oam-release-k1 $SG_NS $(sg_pub k1e)"$'\n' $'k1 v0.18.0 -\n'
  printf '#!/bin/sh\necho x >>"%s"\necho test-passphrase\n' "$SG/askpass.count" >"$SG/askpass"
  chmod +x "$SG/askpass"
  rm -f "$SG/askpass.count"
  sg_enc(){
    OAM_RELEASE_SIGNING_KEY="$SG/k1e" release_agent_start || return 1
    release_signing_preflight v0.18.0 || return 1
    release_write_manifest "$1" v0.18.0 && release_sign_manifest "$1" && release_verify_manifest "$1" v0.18.0 || return 1
    printf 'PROMPTS-AFTER-SIGN %s\n' "$(wc -l <"$SG/askpass.count" | tr -d ' ')"
    SSH_AUTH_SOCK="$RELEASE_AGENT_SOCK" ssh-add -D >/dev/null 2>&1
    release_sign_manifest "$1" && release_verify_manifest "$1" v0.18.0
  }
  sg_reldir "$SG/enc"
  SG_RC=0
  SG_OUT="$(SSH_ASKPASS="$SG/askpass" SSH_ASKPASS_REQUIRE=force DISPLAY=:0 sg "$SG/trust-e" sg_enc "$SG/enc" 2>&1 </dev/null)" || SG_RC=$?
  SG_PROMPTS="$(wc -l <"$SG/askpass.count" 2>/dev/null | tr -d ' ')"
  if [ ! -s "$SG/askpass.count" ] && [ "$SG_RC" != "0" ]; then
    skip "ssh-add did not use SSH_ASKPASS (OpenSSH < 8.4?) -- cannot drive a passphrase unattended: $SG_OUT"
  elif [ "$SG_RC" = "0" ] && grep -q 'PROMPTS-AFTER-SIGN 1' <<<"$SG_OUT" && [ "$SG_PROMPTS" = "2" ] \
       && grep -q 'lifetime ran out' <<<"$SG_OUT" && ! grep -q 'has NO passphrase' <<<"$SG_OUT"; then pass
  else fail "rc=$SG_RC prompts=${SG_PROMPTS:-0}: $SG_OUT"; fi
fi

# =============================================================================
group "signing.sh -- Windows Authenticode: decision, locators, the verify gate"
# =============================================================================
# The verify gate is what makes Windows signing fail-closed, so most of this
# group is about IT: a signtool that exits 0 without signing (Microsoft's own
# FAQ documents that failure, without the x64 .NET runtime) must still fail
# the release, and the PowerShell half must accept a real signature only for
# the publisher and intermediate it was told to pin. No Azure here: the
# account, the az session and the TSA are the preflight's job, on the box.
#
# The operator's own configuration must not leak into the fixtures' verdicts.
unset OAM_WIN_SIGN_METADATA OAM_WIN_SIGN_PUBLISHER OAM_SKIP_WIN_SIGN OAM_SIGN_REQUIRED
WS="$SUITE_TMP/winsign"
mkdir -p "$WS/bin" "$WS/stage"
# wg <command...> -- the lib sourced in a subshell, with every search root and
# external program pointed at a fixture (or at nothing) unless the case sets
# the matching WS_* itself. The box's real signtool, dlib, dotnet and az are
# never consulted by accident.
wg(){
  ( # shellcheck source=lib/signing.sh
    . scripts/lib/signing.sh
    WIN_SDK_BIN_ROOT="${WS_SDK:-$WS/no-sdk}"
    WIN_DLIB_DIRS=("${WS_DLIB_DIR:-$WS/no-dlib}")
    WIN_DOTNET_CANDIDATES=("${WS_DOTNET:-$WS/no-dotnet}")
    WIN_AZ="${WS_AZ:-$WS/no-az}"
    WIN_POWERSHELL="${WS_PS:-$WIN_POWERSHELL}"
    WIN_SIGNTOOL="${WS_SIGNTOOL:-}"
    WIN_SIGN_DLIB="${WS_DLIB:-}"
    WIN_DOTNET_X64="${WS_DOTNET_OK:-}"
    WIN_SIGN_INTERMEDIATE="${WS_INTER:-$WIN_SIGN_INTERMEDIATE}"
    "$@" )
}
ws_signtool(){ locate_signtool_x64 && printf '%s\n' "$WIN_SIGNTOOL"; }
ws_dlib(){ locate_artifact_signing_dlib && printf '%s\n' "$WIN_SIGN_DLIB"; }
WS_HAVE_PS=0
if command -v powershell.exe >/dev/null 2>&1 && command -v cygpath >/dev/null 2>&1; then WS_HAVE_PS=1; fi

it "nothing configured: skipped, with a loud bootstrap reason"
WS_D="$(wg win_sign_decision 0)"
case "$WS_D" in skip:*"WITHOUT Authenticode"*bootstrap*) pass ;; *) fail "got '$WS_D'" ;; esac

it "nothing configured + OAM_SIGN_REQUIRED=1: fatal, naming all three ways forward (an unset knob is not one of them)"
WS_D="$(OAM_SIGN_REQUIRED=1 wg win_sign_decision 0)"
case "$WS_D" in
  fail:"signing is required (OAM_SIGN_REQUIRED, default 1 under release-local.sh)"*"OAM_WIN_SIGN_METADATA is not set"*"OAM_SKIP_WIN_SIGN=1"*"OAM_SIGN_REQUIRED=0"*) pass ;;
  *) fail "got '$WS_D'" ;;
esac

it "configured: sign, required or not"
WS_D="$(OAM_WIN_SIGN_METADATA=/m OAM_WIN_SIGN_PUBLISHER=P wg win_sign_decision 0) $(OAM_SIGN_REQUIRED=1 OAM_WIN_SIGN_METADATA=/m OAM_WIN_SIGN_PUBLISHER=P wg win_sign_decision 0)"
eq "$WS_D" "sign sign"

it "OAM_SKIP_WIN_SIGN=1 is honored even when configured and required -- loudly"
WS_D="$(OAM_SIGN_REQUIRED=1 OAM_WIN_SIGN_METADATA=/m OAM_WIN_SIGN_PUBLISHER=P wg win_sign_decision 1)"
case "$WS_D" in skip:*"OAM_SKIP_WIN_SIGN=1"*"WITHOUT Authenticode"*) pass ;; *) fail "got '$WS_D'" ;; esac

it "half a configuration is fatal, either half"
WS_D="$(OAM_WIN_SIGN_METADATA=/m wg win_sign_decision 0)|$(OAM_WIN_SIGN_PUBLISHER=P wg win_sign_decision 0)"
case "$WS_D" in fail:*"half configured"*"|fail:"*"half configured"*) pass ;; *) fail "got '$WS_D'" ;; esac

it "malformed knobs are fatal, not guessed at"
WS_D="$(wg win_sign_decision yes)|$(OAM_SIGN_REQUIRED=2 wg win_sign_decision 0)"
case "$WS_D" in fail:*"OAM_SKIP_WIN_SIGN must be 0 or 1"*"|fail:"*"OAM_SIGN_REQUIRED must be 0 or 1"*) pass ;; *) fail "got '$WS_D'" ;; esac

it "the lib ignores OAM_SKIP_WIN_SIGN in its environment -- only the caller's argument counts"
WS_D="$(OAM_SKIP_WIN_SIGN=1 OAM_WIN_SIGN_METADATA=/m OAM_WIN_SIGN_PUBLISHER=P wg win_sign_decision 0)"
eq "$WS_D" "sign"

WS_PE="$WS/unsigned.exe"
it "win_make_unsigned_pe writes a 1024-byte x64 PE image"
if wg win_make_unsigned_pe "$WS_PE" 2>/dev/null; then
  eq "$(wg _ws_pe_machine "$WS_PE") $(wc -c <"$WS_PE" | tr -d ' ')" "8664 1024"
else fail "win_make_unsigned_pe failed"; fi

it "_ws_pe_machine reads nothing from a file that is not a PE"
eq "$(wg _ws_pe_machine scripts/lib/signing.sh)" ""

# A fake Windows Kits bin/ tree. The generated PE stands in for signtool: the
# locator reads versions from directory names and the arch from the header.
WS_SDK_T="$WS/sdk"
for v in 10.0.17134.0 10.0.20348.0 10.0.22621.0 10.0.26100.0; do
  mkdir -p "$WS_SDK_T/$v/x64" && cp "$WS_PE" "$WS_SDK_T/$v/x64/signtool.exe"
done
it "locate_signtool_x64 takes the newest SDK at or above the floor"
WS_D="$(WS_SDK="$WS_SDK_T" wg ws_signtool 2>&1)"
eq "$WS_D" "$WS_SDK_T/10.0.26100.0/x64/signtool.exe"

WS_SDK_OLD="$WS/sdk-old"
for v in 10.0.17134.0 10.0.20348.0; do
  mkdir -p "$WS_SDK_OLD/$v/x64" && cp "$WS_PE" "$WS_SDK_OLD/$v/x64/signtool.exe"
done
it "locate_signtool_x64 refuses SDKs below the floor and 10.0.20348, naming what it found"
WS_RC=0; WS_D="$(WS_SDK="$WS_SDK_OLD" wg ws_signtool 2>&1)" || WS_RC=$?
if [ "$WS_RC" != "0" ] && grep -qF '10.0.22621' <<<"$WS_D" && grep -qF '10.0.20348.0' <<<"$WS_D"; then pass
else fail "rc=$WS_RC: $WS_D"; fi

WS_SDK_ARM="$WS/sdk-arm"
mkdir -p "$WS_SDK_ARM/10.0.26100.0/x64" && echo 'not a PE' >"$WS_SDK_ARM/10.0.26100.0/x64/signtool.exe"
it "locate_signtool_x64 refuses a signtool whose header is not x64"
WS_RC=0; WS_D="$(WS_SDK="$WS_SDK_ARM" wg ws_signtool 2>&1)" || WS_RC=$?
if [ "$WS_RC" != "0" ] && grep -qF 'not an x64 binary' <<<"$WS_D"; then pass; else fail "rc=$WS_RC: $WS_D"; fi

mkdir -p "$WS/dlib-ok" "$WS/dlib-bad"
cp "$WS_PE" "$WS/dlib-ok/Azure.CodeSigning.Dlib.dll"
echo 'not a PE' >"$WS/dlib-bad/Azure.CodeSigning.Dlib.dll"
it "locate_artifact_signing_dlib finds the x64 dlib, and refuses one that is not x64"
WS_D="$(WS_DLIB_DIR="$WS/dlib-ok" wg ws_dlib 2>/dev/null)"
WS_RC=0; WS_E="$(WS_DLIB_DIR="$WS/dlib-bad" wg ws_dlib 2>&1)" || WS_RC=$?
if [ "$WS_D" = "$WS/dlib-ok/Azure.CodeSigning.Dlib.dll" ] && [ "$WS_RC" != "0" ] \
   && grep -qF 'not the x64 build' <<<"$WS_E" && grep -qF 'winget install -e --id Microsoft.Azure.ArtifactSigningClientTools' <<<"$WS_E"; then pass
else fail "found '$WS_D'; bad dir rc=$WS_RC: $WS_E"; fi

it "probe_dotnet_x64 refuses a host that is not an x64 PE, and names the fix"
echo 'not a PE' >"$WS/bin/dotnet.exe"
WS_RC=0; WS_D="$(WS_DOTNET="$WS/bin/dotnet.exe" wg probe_dotnet_x64 2>&1)" || WS_RC=$?
if [ "$WS_RC" != "0" ] && grep -qF 'sign NOTHING' <<<"$WS_D"; then pass; else fail "rc=$WS_RC: $WS_D"; fi

it "probe_dotnet_x64 accepts this box's x64 .NET >= 8, when it has one"
if [ -f "/c/Program Files/dotnet/x64/dotnet.exe" ]; then
  WS_RC=0; WS_D="$(WS_DOTNET="/c/Program Files/dotnet/x64/dotnet.exe" wg probe_dotnet_x64 2>&1)" || WS_RC=$?
  if [ "$WS_RC" = "0" ]; then pass
  elif [ "$(wg _ws_pe_machine "/c/Program Files/dotnet/x64/dotnet.exe")" = "8664" ]; then skip "x64 dotnet present but lists no runtime >= 8: $WS_D"
  else fail "rc=$WS_RC: $WS_D"; fi
else skip "no /c/Program Files/dotnet/x64/dotnet.exe on this host"; fi

# metadata.json: only the shape is checked, and placeholder values (the
# sample file's "<...>") do not count as configured.
printf '{\n  "Endpoint": "https://example.invalid",\n  "CodeSigningAccountName": "example",\n  "CertificateProfileName": "example"\n}\n' >"$WS/metadata.json"
printf '{\n  "Endpoint": "<Artifact Signing account endpoint>",\n  "CodeSigningAccountName": "example",\n  "CertificateProfileName": "example"\n}\n' >"$WS/metadata-sample.json"
printf '{\n  "Endpoint": "https://example.invalid",\n  "CodeSigningAccountName": "example"\n}\n' >"$WS/metadata-short.json"
it "metadata.json needs all three fields, with real values"
WS_OK=0; OAM_WIN_SIGN_METADATA="$WS/metadata.json" wg _ws_metadata >/dev/null 2>&1 && WS_OK=1
WS_S="$(OAM_WIN_SIGN_METADATA="$WS/metadata-sample.json" wg _ws_metadata 2>&1)"
WS_T="$(OAM_WIN_SIGN_METADATA="$WS/metadata-short.json" wg _ws_metadata 2>&1)"
if [ "$WS_OK" = "1" ] && grep -qF '"Endpoint"' <<<"$WS_S" && grep -qF '"CertificateProfileName"' <<<"$WS_T"; then pass
else fail "ok=$WS_OK sample='$WS_S' short='$WS_T'"; fi

# The stub: exits 0 for every call and signs nothing -- what signtool + the
# dlib do without the x64 .NET runtime. It logs its argv for the shape check.
WS_LOG="$WS/signtool.log"
{ echo '#!/bin/bash'; echo "printf '%s\\n' \"\$*\" >>'$WS_LOG'"; echo 'exit 0'; } >"$WS/bin/signtool"
# A stale az session, as one really reads: it names the signed-in account, and
# Entra's errors carry tenant, trace and correlation IDs.
cat >"$WS/bin/az-lapsed" <<'AZ'
#!/bin/bash
echo "ERROR: User 'op.erator@example.com' does not exist in MSAL token cache. Run \`az login\`." >&2
echo "AADSTS700082: The refresh token has expired. Trace ID: 0a1b2c3d-4e5f-6789-abcd-ef0123456789 Correlation ID: 11111111-2222-3333-4444-555555555555" >&2
echo "Authority: https://login.microsoftonline.com/aabbccdd-1111-2222-3333-444455556666 (tenant 'Example Tenant')" >&2
echo "Cache: C:\\Users\\opname\\.azure\\msal_token_cache.bin" >&2
exit 1
AZ
printf '#!/bin/bash\nexit 0\n' >"$WS/bin/az-live"
chmod +x "$WS/bin/signtool" "$WS/bin/az-lapsed" "$WS/bin/az-live"
# ws_stubbed <command...> -- fully configured, with the stub as signtool.
# A case may still override signtool, powershell or az (WS_SIGNTOOL / WS_PS / WS_AZ).
ws_stubbed(){
  WS_SIGNTOOL="${WS_SIGNTOOL:-$WS/bin/signtool}" WS_DLIB="$WS/dlib-ok/Azure.CodeSigning.Dlib.dll" WS_DOTNET_OK=stub \
    OAM_WIN_SIGN_METADATA="$WS/metadata.json" OAM_WIN_SIGN_PUBLISHER="Example Publisher" wg "$@"
}

it "win_sign hands signtool the documented command: /fd, the ACS TSA, /td, /dlib, /dmdf, native paths"
cp "$WS_PE" "$WS/stage/oam-stub.exe"
: >"$WS_LOG"
WS_RC=0; WS_D="$(ws_stubbed win_sign "$WS/stage/oam-stub.exe" 2>&1)" || WS_RC=$?
WS_L="$(head -1 "$WS_LOG")"
WS_WANT="sign /v /fd SHA256 /tr http://timestamp.acs.microsoft.com /td SHA256 /dlib $(wg _ws_winpath "$WS/dlib-ok/Azure.CodeSigning.Dlib.dll") /dmdf $(wg _ws_winpath "$WS/metadata.json") $(wg _ws_winpath "$WS/stage/oam-stub.exe")"
if [ "$WS_RC" = "0" ] && [ "$WS_L" = "$WS_WANT" ]; then pass
else fail "rc=$WS_RC out='$WS_D'"$'\n'"       logged: $WS_L"$'\n'"       wanted: $WS_WANT"; fi

it "a signtool that exits 0 without signing fails the gate: win_verify rejects the file"
if [ "$WS_HAVE_PS" = "1" ]; then
  WS_RC=0; WS_D="$(ws_stubbed win_verify "$WS/stage/oam-stub.exe" 2>&1)" || WS_RC=$?
  if [ "$WS_RC" != "0" ] && grep -qF "'NotSigned'" <<<"$WS_D" && grep -qF 'verify /pa /v' "$WS_LOG"; then pass
  else fail "rc=$WS_RC: $WS_D"; fi
else skip "no powershell.exe/cygpath -- the Authenticode reader runs on Windows only"; fi

it "win_sign refuses to sign a build output under target/, and never calls signtool for it"
mkdir -p "$WS/target/release" && cp "$WS_PE" "$WS/target/release/oam.exe"
: >"$WS_LOG"
WS_RC=0; WS_D="$(ws_stubbed win_sign "$WS/target/release/oam.exe" 2>&1)" || WS_RC=$?
if [ "$WS_RC" != "0" ] && grep -qF 'refusing to sign' <<<"$WS_D" && [ ! -s "$WS_LOG" ]; then pass
else fail "rc=$WS_RC log='$(cat "$WS_LOG")': $WS_D"; fi

it "win_sign_preflight on a lapsed az session fails, saying to run az login, before signing anything -- and names nobody"
: >"$WS_LOG"
WS_RC=0; WS_D="$(WS_AZ="$WS/bin/az-lapsed" ws_stubbed win_sign_preflight 2>&1)" || WS_RC=$?
if [ "$WS_RC" != "0" ] && grep -qF "run 'az login'" <<<"$WS_D" && [ ! -s "$WS_LOG" ] \
   && grep -qF "az: ERROR: User '<email>' does not exist in MSAL token cache" <<<"$WS_D" && grep -qF 'AADSTS700082' <<<"$WS_D" \
   && ! grep -qiE 'op\.erator|example\.com|[0-9a-f]{8}-[0-9a-f]{4}-|Example Tenant|opname' <<<"$WS_D"; then pass
else fail "rc=$WS_RC log='$(cat "$WS_LOG")': $WS_D"; fi

it "win_sign_preflight with a signtool that signs nothing fails, and leaves no probe behind"
if [ "$WS_HAVE_PS" = "1" ]; then
  mkdir -p "$WS/pftmp"
  WS_RC=0; WS_D="$(TMPDIR="$WS/pftmp" WS_AZ="$WS/bin/az-live" ws_stubbed win_sign_preflight 2>&1)" || WS_RC=$?
  if [ "$WS_RC" != "0" ] && grep -qF 'throwaway signature did not sign and verify' <<<"$WS_D" \
     && [ -z "$(ls -A "$WS/pftmp")" ]; then pass
  else fail "rc=$WS_RC left='$(ls -A "$WS/pftmp")': $WS_D"; fi
else skip "no powershell.exe/cygpath -- the Authenticode reader runs on Windows only"; fi

# A stalled endpoint: signtool + the dlib print "Submitting digest..." and
# never return. The stand-ins hang in a CHILD process -- cmd.exe running
# ping.exe on Windows, sleep elsewhere -- because that is the shape that beat
# timeout(1): az is a script around python.exe, and killing only the direct
# child left the grandchild running, holding the output pipe. Each stand-in's
# child carries its own count as a marker, so ws_orphans can find it.
# ws_hang_stub <file> <marker>
ws_hang_stub(){
  { echo '#!/bin/bash'
    echo 'echo "Submitting digest for signing..."'
    echo 'case "$(uname -s)" in'
    # MSYS_NO_PATHCONV=1 spelled out: win_sign/win_verify run signtool under
    # it, the stand-in inherits it, and `cmd //c` would then reach cmd.exe
    # unconverted and exit at once instead of hanging.
    echo "  MINGW* | MSYS* | CYGWIN*) MSYS_NO_PATHCONV=1 cmd /c \"ping -n $2 127.0.0.1\" >/dev/null ;;"
    echo "  *) sleep $2 ;;"
    echo 'esac'
  } >"$1"
  chmod +x "$1"
}
# ws_orphans <marker> -- how many of that stand-in's children are still
# running (any it finds are killed, so a failure does not leak them).
ws_orphans(){
  if [ "$WS_HAVE_PS" = "1" ]; then
    powershell.exe -NoProfile -NonInteractive -Command \
      "\$p = @(Get-CimInstance Win32_Process -Filter \"Name='PING.EXE'\" | Where-Object { \$_.CommandLine -like ('*-n ' + '$1' + ' *') }); \$p | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }; \$p.Count" \
      </dev/null 2>/dev/null | tr -d '\r'
  else
    local n
    n="$(ps -A -o args= 2>/dev/null | grep -cx "sleep $1")"
    pkill -x -f "sleep $1" 2>/dev/null
    echo "$n"
  fi
}
ws_hang_stub "$WS/bin/signtool-hang" 591
ws_hang_stub "$WS/bin/verify-hang" 592
ws_hang_stub "$WS/bin/ps-hang" 594
ws_hang_stub "$WS/bin/az-hang" 595
it "win_sign kills a signtool that hangs -- and its children -- after OAM_WIN_SIGN_TIMEOUT, and says the service or TSA stalled"
cp "$WS_PE" "$WS/stage/oam-hang.exe"
WS_T0="$(date +%s)"
WS_RC=0; WS_D="$(OAM_WIN_SIGN_TIMEOUT=2 WS_SIGNTOOL="$WS/bin/signtool-hang" ws_stubbed win_sign "$WS/stage/oam-hang.exe" 2>&1)" || WS_RC=$?
WS_DT=$(( $(date +%s) - WS_T0 ))
WS_O="$(ws_orphans 591)"
if [ "$WS_RC" != "0" ] && grep -qF 'did not finish in 2s' <<<"$WS_D" && grep -qF 'TSA' <<<"$WS_D" && [ "$WS_O" = "0" ]; then pass
else fail "rc=$WS_RC after ${WS_DT}s, $WS_O orphan(s): $WS_D"; fi

it "win_verify kills a signtool verify that hangs, and its children"
WS_T0="$(date +%s)"
WS_RC=0; WS_D="$(OAM_WIN_SIGN_TIMEOUT=2 WS_SIGNTOOL="$WS/bin/verify-hang" ws_stubbed win_verify "$WS/stage/oam-hang.exe" 2>&1)" || WS_RC=$?
WS_DT=$(( $(date +%s) - WS_T0 ))
WS_O="$(ws_orphans 592)"
if [ "$WS_RC" != "0" ] && grep -qF 'signtool verify /pa rejects' <<<"$WS_D" && grep -qF 'killed after 2s' <<<"$WS_D" && [ "$WS_O" = "0" ]; then pass
else fail "rc=$WS_RC after ${WS_DT}s, $WS_O orphan(s): $WS_D"; fi

it "win_verify kills a verify-authenticode.ps1 run that hangs, and its children"
WS_T0="$(date +%s)"
WS_RC=0; WS_D="$(OAM_WIN_SIGN_TIMEOUT=2 WS_PS="$WS/bin/ps-hang" ws_stubbed win_verify "$WS/stage/oam-hang.exe" 2>&1)" || WS_RC=$?
WS_DT=$(( $(date +%s) - WS_T0 ))
WS_O="$(ws_orphans 594)"
if [ "$WS_RC" != "0" ] && grep -qF 'Authenticode verification failed' <<<"$WS_D" && grep -qF 'killed after 2s' <<<"$WS_D" && [ "$WS_O" = "0" ]; then pass
else fail "rc=$WS_RC after ${WS_DT}s, $WS_O orphan(s): $WS_D"; fi

it "win_sign_preflight kills an az that hangs, and its children, before signing anything"
: >"$WS_LOG"
WS_T0="$(date +%s)"
WS_RC=0; WS_D="$(OAM_WIN_SIGN_TIMEOUT=2 WS_AZ="$WS/bin/az-hang" ws_stubbed win_sign_preflight 2>&1)" || WS_RC=$?
WS_DT=$(( $(date +%s) - WS_T0 ))
WS_O="$(ws_orphans 595)"
if [ "$WS_RC" != "0" ] && grep -qF 'az account get-access-token did not finish in 2s' <<<"$WS_D" && [ ! -s "$WS_LOG" ] && [ "$WS_O" = "0" ]; then pass
else fail "rc=$WS_RC after ${WS_DT}s, $WS_O orphan(s), log='$(cat "$WS_LOG")': $WS_D"; fi

it "a malformed OAM_WIN_SIGN_TIMEOUT is fatal before signtool runs"
: >"$WS_LOG"
WS_RC=0; WS_D="$(OAM_WIN_SIGN_TIMEOUT=5m ws_stubbed win_sign "$WS/stage/oam-stub.exe" 2>&1)" || WS_RC=$?
if [ "$WS_RC" != "0" ] && grep -qF 'OAM_WIN_SIGN_TIMEOUT must be a whole number of seconds from 1 to 3600' <<<"$WS_D" && [ ! -s "$WS_LOG" ]; then pass
else fail "rc=$WS_RC log='$(cat "$WS_LOG")': $WS_D"; fi

it "OAM_WIN_SIGN_TIMEOUT is capped: 0, 3601 and a 20-digit value are refused; 1 and 3600 are not"
WS_D=""
for t in 0 3601 99999999999999999999 1 3600; do
  if OAM_WIN_SIGN_TIMEOUT="$t" wg _ws_timeout_ok 2>/dev/null; then WS_D="$WS_D $t:ok"; else WS_D="$WS_D $t:no"; fi
done
eq "$WS_D" " 0:no 3601:no 99999999999999999999:no 1:ok 3600:ok"

# signtool /v echoes the dlib's metadata block, and the service's errors name
# the account in URLs. None of it may reach the failure output.
printf '{\n  "Endpoint": "https://zz-q7.codesigning.azure.net",\n  "CodeSigningAccountName": "AcctQ7zz",\n  "CertificateProfileName": "ProfQ7zz"\n}\n' >"$WS/metadata-real.json"
{ echo '#!/bin/bash'
  echo 'echo "Metadata:"'
  echo "cat '$WS/metadata-real.json'"
  echo 'echo "POST https://zz-q7.codesigning.azure.net/codesigningaccounts/acctq7zz/certificateprofiles/ProfQ7zz/sign: 403 Forbidden"'
  echo 'echo "SignerSign() failed. (-2147024891/0x80070005)"'
  echo 'exit 1'; } >"$WS/bin/signtool-leaky"
chmod +x "$WS/bin/signtool-leaky"
it "win_sign's failure output redacts every metadata.json value, and keeps the rest"
WS_RC=0; WS_D="$(WS_SIGNTOOL="$WS/bin/signtool-leaky" WS_DLIB="$WS/dlib-ok/Azure.CodeSigning.Dlib.dll" WS_DOTNET_OK=stub \
  OAM_WIN_SIGN_METADATA="$WS/metadata-real.json" OAM_WIN_SIGN_PUBLISHER="Example Publisher" wg win_sign "$WS/stage/oam-stub.exe" 2>&1)" || WS_RC=$?
if [ "$WS_RC" != "0" ] && ! grep -qiE 'zz-q7|AcctQ7zz|ProfQ7zz' <<<"$WS_D" \
   && grep -qF '"CodeSigningAccountName": "<redacted>"' <<<"$WS_D" && grep -qF 'SignerSign() failed' <<<"$WS_D" \
   && grep -qF 'run '"'"'az login'"'" <<<"$WS_D"; then pass
else fail "rc=$WS_RC: $WS_D"; fi

it "_ws_redact: metadata values in any case, emails, GUIDs, tenants and the profile path all go"
WS_D="$(printf '%s\n' \
  'POST HTTPS://ZZ-Q7.CODESIGNING.AZURE.NET/CodeSigningAccounts/ACCTQ7ZZ/certificateProfiles/profq7zz/sign' \
  'acct AcCtQ7Zz, profile PROFQ7ZZ' \
  "User 'someone.x@example-corp.co.uk' does not exist in MSAL token cache. Run \`az login\`." \
  'Trace ID: 0A1B2C3D-4E5F-6789-ABCD-EF0123456789 at contoso.onmicrosoft.com' \
  "in tenant 'Some Tenant'" \
  'C:\Users\Some Body\AppData\Local\Temp\x.exe and C:\Users\jdoe\y and C:/Users/jdoe/z and /c/Users/jdoe/w' \
  | USERPROFILE='C:\Users\Some Body' wg _ws_redact "$WS/metadata-real.json")"
if ! grep -qiE 'zz-q7|acctq7zz|profq7zz|someone|example-corp|0a1b2c3d|contoso|Some Tenant|Some Body|jdoe' <<<"$WS_D" \
   && grep -qF 'Run `az login`' <<<"$WS_D" && [ "$(grep -o '<home>' <<<"$WS_D" | wc -l | tr -d ' ')" = "4" ] \
   && grep -qF '<home>\AppData\Local\Temp\x.exe' <<<"$WS_D"; then pass
else fail "$WS_D"; fi

# The Windows section's own [fail] lines name paths too (a metadata.json typo,
# a probe in TMPDIR), and a path under the user profile names the operator.
it "the Windows status lines redact a path under the user profile"
WS_D="$( { HOME=/c/Users/jdoe USERPROFILE='C:\Users\jdoe' OAM_WIN_SIGN_METADATA=/c/Users/jdoe/.oam-signing/typo.json wg _ws_metadata
           HOME=/c/Users/jdoe USERPROFILE='C:\Users\jdoe' OAM_WIN_SIGN_PUBLISHER="Example Publisher" wg win_verify /c/Users/jdoe/nope.exe; } 2>&1)"
if ! grep -qF 'jdoe' <<<"$WS_D" && grep -qF '<home>/.oam-signing/typo.json does not exist' <<<"$WS_D" \
   && grep -qF 'win_verify: <home>/nope.exe does not exist' <<<"$WS_D"; then pass
else fail "$WS_D"; fi

# A real dlib failure is a .NET exception: ~30 lines, with the HTTP status near
# the top and a stack trace after it. A bare tail showed only the stack.
{ echo '#!/bin/bash'
  echo 'echo "The following certificate was selected:"'
  echo 'echo "Submitting digest for signing..."'
  echo 'echo "Azure.RequestFailedException: Service request failed."'
  echo 'echo "Status: 403 (Forbidden)"'
  echo 'echo "Content:"'
  echo 'echo "{\"errorDetail\":{\"code\":\"Forbidden\"}}"'
  echo 'echo "Headers:"'
  echo 'for i in $(seq 1 15); do echo "x-ms-header-$i: REDACTED"; done'
  echo 'for i in $(seq 1 8); do echo "   at Azure.Core.Pipeline.Frame$i()"; done'
  echo 'echo "SignTool Error: An unexpected internal error has occurred."'
  echo 'echo "Error information: \"Error: SignerSign() failed.\" (-2146893775/0x80090031)"'
  echo 'exit 1'; } >"$WS/bin/signtool-dotnet"
chmod +x "$WS/bin/signtool-dotnet"
it "win_sign's failure output leads with the lines that say why: the HTTP status survives a 32-line .NET failure"
WS_RC=0; WS_D="$(WS_SIGNTOOL="$WS/bin/signtool-dotnet" ws_stubbed win_sign "$WS/stage/oam-stub.exe" 2>&1)" || WS_RC=$?
if [ "$WS_RC" != "0" ] && grep -qF 'signtool: Status: 403 (Forbidden)' <<<"$WS_D" \
   && grep -qF 'signtool: Azure.RequestFailedException: Service request failed.' <<<"$WS_D" \
   && grep -qF 'SignerSign() failed' <<<"$WS_D" && grep -qF 'the last 12 of 32 lines' <<<"$WS_D"; then pass
else fail "rc=$WS_RC: $WS_D"; fi

# win_pe_signature_state reads the certificate table (data directory 4); the
# fixture gets a non-empty one by patching the directory entry at
# e_lfanew(0x40) + 24 + 112 + 4*8 = 232: offset 0x400, size 0x10.
cp "$WS_PE" "$WS/has-cert-table.exe"
printf '\x00\x04\x00\x00\x10\x00\x00\x00' | dd of="$WS/has-cert-table.exe" bs=1 seek=232 conv=notrunc 2>/dev/null
it "win_pe_signature_state: an unsigned PE, a PE with a certificate table, and a non-PE"
eq "$(wg win_pe_signature_state "$WS_PE") $(wg win_pe_signature_state "$WS/has-cert-table.exe") $(wg win_pe_signature_state scripts/lib/signing.sh)" \
   "unsigned signed unknown"

# NumberOfRvaAndSizes sits just before the directories, at 232 - 4*8 - 4 = 196.
# Four or fewer means the file has no entry 4 at all: not provably unsigned.
cp "$WS_PE" "$WS/few-dirs.exe"
printf '\x04\x00\x00\x00' | dd of="$WS/few-dirs.exe" bs=1 seek=196 conv=notrunc 2>/dev/null
it "win_pe_signature_state: a PE with too few data directories to hold a certificate table is unknown, not unsigned"
eq "$(wg win_pe_signature_state "$WS/few-dirs.exe")" "unknown"

# The pins one at a time, on fabricated signatures. The real fixture below is
# timestamped, embedded, and has CN == O, so it can never take these
# branches: without these cases, deleting the timestamp pin stays green.
if [ "$WS_HAVE_PS" = "1" ]; then
  printf '%s\n' 'param([string]$Ps1, [string]$Case)' \
    '. $Ps1 -Path x -Publisher x -Intermediate x' \
    "\$dn = 'CN=Example Publisher, O=Example Publisher'" \
    "\$pub = 'Example Publisher'" \
    "\$sig = @{ Status = 'Valid'; StatusMessage = 'ok'; SignatureType = 'Authenticode'; TimeStamperCertificate = 'ts' }" \
    "switch (\$Case) {" \
    "  'not-valid' { \$sig.Status = 'HashMismatch' }" \
    "  'catalog' { \$sig.SignatureType = 'Catalog' }" \
    "  'no-timestamp' { \$sig.TimeStamperCertificate = \$null }" \
    "  'o-mismatch' { \$dn = 'CN=Example Publisher, O=Other Org' }" \
    "  'prefix' { \$dn = 'CN=Yaw Labs LLC, O=Yaw Labs LLC'; \$pub = 'Yaw Labs' }" \
    "  'o-case' { \$dn = 'CN=Example Publisher, O=example publisher' }" \
    "  'cn-twice' { \$dn = 'CN=Example Publisher, CN=Example Publisher, O=Example Publisher' }" \
    "  'o-twice' { \$dn = 'CN=Example Publisher, O=Example Publisher, O=Example Publisher' }" \
    '}' \
    '$name = New-Object System.Security.Cryptography.X509Certificates.X500DistinguishedName($dn)' \
    '$sig.SignerCertificate = [pscustomobject]@{ SubjectName = $name; Subject = $name.Name }' \
    "\$why = Test-SignaturePins ([pscustomobject]\$sig) 'fake.exe' \$pub 'Some PCA'" \
    "if (\$null -eq \$why) { 'PASSED' } else { \"FAILED: \$why\" }" >"$WS/pins.ps1"
  ws_pins(){
    powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(cygpath -w "$WS/pins.ps1")" \
      -Ps1 "$(cygpath -w scripts/lib/verify-authenticode.ps1)" -Case "$1" </dev/null 2>&1 | tr -d '\r'
  }
  it "the pins refuse a signature whose status is not Valid"
  eq "$(ws_pins not-valid)" "FAILED: fake.exe signature status is 'HashMismatch', not Valid (ok)"
  it "the pins refuse a catalog signature (nothing embedded in the file)"
  eq "$(ws_pins catalog)" "FAILED: fake.exe is signed by 'Catalog', not an embedded Authenticode signature"
  it "the pins refuse a signature with no timestamp"
  eq "$(ws_pins no-timestamp)" "FAILED: fake.exe has no timestamp -- the signature would stop verifying when its certificate expires"
  it "the pins refuse a signer whose CN matches but whose O does not"
  eq "$(ws_pins o-mismatch)" "FAILED: fake.exe signer O is 'Other Org', expected 'Example Publisher' (subject: CN=Example Publisher, O=Other Org)"
  it "the pins refuse a publisher that is only a prefix of the signer's name"
  eq "$(ws_pins prefix)" "FAILED: fake.exe signer CN is 'Yaw Labs LLC', expected 'Yaw Labs' (subject: CN=Yaw Labs LLC, O=Yaw Labs LLC)"
  it "the pins refuse an O that differs only in case"
  eq "$(ws_pins o-case)" "FAILED: fake.exe signer O is 'example publisher', expected 'Example Publisher' (subject: CN=Example Publisher, O=example publisher)"
  it "the pins refuse a subject that repeats the CN, or the O"
  eq "$(ws_pins cn-twice)|$(ws_pins o-twice)" \
    "FAILED: fake.exe signer CN is '', expected 'Example Publisher' (subject: CN=Example Publisher, CN=Example Publisher, O=Example Publisher)|FAILED: fake.exe signer O is '', expected 'Example Publisher' (subject: CN=Example Publisher, O=Example Publisher, O=Example Publisher)"
else
  it "verify-authenticode.ps1's pins on fabricated signatures"
  skip "no powershell.exe/cygpath -- the Authenticode reader runs on Windows only"
fi

# verify-authenticode.ps1 against a REAL embedded signature: the box's x64
# signtool.exe, Microsoft-signed. Its intermediate is read separately (by a
# chain walk that is not the script under test) so the case follows whatever
# CA Microsoft signs that SDK with.
WS_MS="$( . scripts/lib/signing.sh; locate_signtool_x64 2>/dev/null && printf '%s' "$WIN_SIGNTOOL" )"
WS_INTER_MS=""
if [ "$WS_HAVE_PS" = "1" ] && [ -n "$WS_MS" ]; then
  printf '%s\n' 'param([string]$p)' \
    '$s = Get-AuthenticodeSignature -LiteralPath $p' \
    '$c = New-Object System.Security.Cryptography.X509Certificates.X509Chain' \
    "\$c.ChainPolicy.RevocationMode = 'NoCheck'" \
    '[void]$c.Build($s.SignerCertificate)' \
    "\$c.ChainElements[1].Certificate.GetNameInfo('SimpleName', \$false)" >"$WS/chain1.ps1"
  WS_INTER_MS="$(powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(cygpath -w "$WS/chain1.ps1")" "$(cygpath -w "$WS_MS")" 2>/dev/null | tr -d '\r')"
fi
# ws_ps1 <file> <publisher> <intermediate> -- the script alone, as win_verify calls it.
ws_ps1(){
  powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(cygpath -w scripts/lib/verify-authenticode.ps1)" \
    -Path "$(cygpath -w "$1")" -Publisher "$2" -Intermediate "$3" </dev/null 2>&1 | tr -d '\r'
  return "${PIPESTATUS[0]}"
}
if [ "$WS_HAVE_PS" = "1" ] && [ -n "$WS_MS" ] && [ -n "$WS_INTER_MS" ]; then
  it "verify-authenticode.ps1 passes a real signature for its real publisher and intermediate"
  WS_RC=0; WS_D="$(ws_ps1 "$WS_MS" "Microsoft Corporation" "$WS_INTER_MS")" || WS_RC=$?
  if [ "$WS_RC" = "0" ]; then pass; else fail "rc=$WS_RC: $WS_D"; fi

  it "verify-authenticode.ps1 fails the same signature for another publisher (exact, case-sensitive)"
  WS_RC=0; WS_D="$(ws_ps1 "$WS_MS" "Example Publisher" "$WS_INTER_MS")" || WS_RC=$?
  WS_RC2=0; WS_E="$(ws_ps1 "$WS_MS" "microsoft corporation" "$WS_INTER_MS")" || WS_RC2=$?
  if [ "$WS_RC" != "0" ] && grep -qF "signer CN is 'Microsoft Corporation', expected 'Example Publisher'" <<<"$WS_D" \
     && [ "$WS_RC2" != "0" ]; then pass
  else fail "rc=$WS_RC/$WS_RC2: $WS_D / $WS_E"; fi

  it "verify-authenticode.ps1 fails a valid signature that does not chain through the Artifact Signing intermediate"
  WS_RC=0; WS_D="$(ws_ps1 "$WS_MS" "Microsoft Corporation" "Microsoft ID Verified Code Signing PCA 2021")" || WS_RC=$?
  if [ "$WS_RC" != "0" ] && grep -qF "does not pass through 'Microsoft ID Verified Code Signing PCA 2021'" <<<"$WS_D"; then pass
  else fail "rc=$WS_RC: $WS_D"; fi

  # The leaf is chain element 0; only a CA above it may satisfy -Intermediate.
  it "verify-authenticode.ps1 fails when -Intermediate names the signer itself"
  WS_RC=0; WS_D="$(ws_ps1 "$WS_MS" "Microsoft Corporation" "Microsoft Corporation")" || WS_RC=$?
  if [ "$WS_RC" != "0" ] && grep -qF "does not pass through 'Microsoft Corporation'" <<<"$WS_D"; then pass
  else fail "rc=$WS_RC: $WS_D"; fi

  it "win_pe_signature_state calls the real Microsoft-signed signtool.exe signed"
  eq "$(wg win_pe_signature_state "$WS_MS")" "signed"

  it "verify-authenticode.ps1 fails an unsigned file"
  WS_RC=0; WS_D="$(ws_ps1 "$WS_PE" "Microsoft Corporation" "$WS_INTER_MS")" || WS_RC=$?
  if [ "$WS_RC" != "0" ] && grep -qF "'NotSigned'" <<<"$WS_D"; then pass; else fail "rc=$WS_RC: $WS_D"; fi

  # A path is an argument, never code: quotes, $ and spaces reach the cmdlet
  # as data and the copy (signature and all) verifies like the original.
  it "verify-authenticode.ps1 takes a hostile path as data"
  WS_ODD="$WS/it's a \$(x) dir"
  mkdir -p "$WS_ODD" && cp "$WS_MS" "$WS_ODD/sign tool;copy.exe"
  WS_RC=0; WS_D="$(ws_ps1 "$WS_ODD/sign tool;copy.exe" "Microsoft Corporation" "$WS_INTER_MS")" || WS_RC=$?
  if [ "$WS_RC" = "0" ]; then pass; else fail "rc=$WS_RC: $WS_D"; fi

  it "win_verify end to end: real signtool verify /pa, then the pins, on the real signature"
  WS_RC=0; WS_D="$(WS_SIGNTOOL="$WS_MS" WS_INTER="$WS_INTER_MS" OAM_WIN_SIGN_PUBLISHER="Microsoft Corporation" wg win_verify "$WS_ODD/sign tool;copy.exe" 2>&1)" || WS_RC=$?
  WS_RC2=0; WS_E="$(WS_SIGNTOOL="$WS_MS" OAM_WIN_SIGN_PUBLISHER="Microsoft Corporation" wg win_verify "$WS_MS" 2>&1)" || WS_RC2=$?
  if [ "$WS_RC" = "0" ] && [ "$WS_RC2" != "0" ] && grep -qF 'PCA 2021' <<<"$WS_E"; then pass
  else fail "pinned rc=$WS_RC: $WS_D"$'\n'"       default-intermediate rc=$WS_RC2: $WS_E"; fi
else
  it "verify-authenticode.ps1 against a real Microsoft signature"
  skip "needs powershell.exe, cygpath and an x64 signtool >= SDK 10.0.22621 (found: '${WS_MS:-none}', intermediate '${WS_INTER_MS:-?}')"
fi

it "verify-authenticode.ps1 is ASCII and takes its inputs as parameters"
if LC_ALL=C grep -q '[^[:print:][:space:]]' scripts/lib/verify-authenticode.ps1; then fail "non-ASCII bytes"
elif grep -qE '^param\(' scripts/lib/verify-authenticode.ps1 && ! grep -qE 'Invoke-Expression|iex[[:space:]]' scripts/lib/verify-authenticode.ps1; then pass
else fail "no param() block, or an Invoke-Expression"; fi

# =============================================================================
group "release scripts -- signing wiring and order"
# =============================================================================
# Order is the property: a correct signing block that moved after the tag push
# (or a manifest signed before SHA256SUMS is final) is the bug, so these are
# line-order asserts on the code, comment lines excluded -- the header comments
# quote several of the same commands.
# sg_line <file> <fixed string> -- first non-comment line carrying it.
sg_line(){ grep -nF -- "$2" "$1" | grep -v '^[0-9]*:[[:space:]]*#' | head -1 | cut -d: -f1; }
# sg_order <file> <string>... -- each string present, strictly in this order.
sg_order(){
  local file="$1" prev=0 n s got="" bad=0; shift
  for s in "$@"; do
    n="$(sg_line "$file" "$s")"
    got="$got [${n:-missing}] $s"$'\n'
    if [ -z "$n" ] || [ "$n" -le "$prev" ]; then bad=1; fi
    prev="${n:-0}"
  done
  if [ "$bad" = "0" ]; then pass; else fail "out of order or missing in $file:"$'\n'"$got"; fi
}

it "release-local.sh: trap, then the agent and signing preflight, all before the second dirty-tree check, the bump and the tag"
sg_order scripts/release-local.sh 'trap release_on_exit EXIT' 'release_agent_start || fail' \
  'release_signing_preflight "$TAG" || fail' 'assert_tree_clean "preflight, after the signing probes"' \
  'bumping Cargo.toml to' 'git tag -a "$TAG" -m "$TAG"'

# The cheap half of the preflight first: a forgotten changelog-release.sh, a
# dirty tree or a leftover draft must fail before the ssh probe of the build
# Mac, the release key's passphrase prompt and the quota-counted Artifact
# Signing probe -- or every retry repeats all three. Then, after those, the
# re-reads the bump and the tag move rely on: the tree (the probes must leave
# nothing), HEAD (the changelog verdict was for it), origin/main and the
# release's absence.
it "release-local.sh: the cheap preflight checks precede the mac, release-key and Windows signing probes"
sg_order scripts/release-local.sh 'step "Preflight $TAG"' 'restore_gate_artifacts "preflight"' \
  'preflight_head="$(git rev-parse HEAD)"' 'if [ -f CHANGELOG.md ]; then' \
  'assert_release_unpublished "preflight"' 'command -v gcloud >/dev/null 2>&1 || fail' \
  'build-platforms-tailnet.sh" --preflight-only' 'release_agent_start || fail' \
  'win_sign_preflight || fail' 'assert_tree_clean "preflight, after the signing probes"' \
  '[ "$(git rev-parse HEAD)" = "$preflight_head" ]' 'git fetch -q origin main 2>/dev/null || true' \
  'assert_release_unpublished "before the bump"' 'bumping Cargo.toml to' 'git tag -a "$TAG" -m "$TAG"'

# Signing is required by default: the docs promise signed assets, so a box
# without the Windows setup must stop in preflight unless the operator opts
# out with an explicit OAM_SIGN_REQUIRED=0. The default is exported (the libs
# and build-platforms-tailnet.sh read the environment) and set before any
# preflight probe reads it.
it "release-local.sh: OAM_SIGN_REQUIRED defaults to 1, exported before the preflight"
if [ "$(grep -cxF 'export OAM_SIGN_REQUIRED="${OAM_SIGN_REQUIRED:-1}"' scripts/release-local.sh)" = "1" ]; then
  sg_order scripts/release-local.sh 'export OAM_SIGN_REQUIRED="${OAM_SIGN_REQUIRED:-1}"' 'step "Preflight $TAG"' \
    'build-platforms-tailnet.sh" --preflight-only' 'win_sign_preflight || fail'
else fail "release-local.sh no longer exports OAM_SIGN_REQUIRED with a default of 1"; fi

it "the default makes an unconfigured Windows box fail, and an explicit 0 still downgrades it to a warning"
SG_D1="$( ( unset OAM_WIN_SIGN_METADATA OAM_WIN_SIGN_PUBLISHER; export OAM_SIGN_REQUIRED="${OAM_SIGN_REQUIRED:-1}"
            . scripts/lib/signing.sh; win_sign_decision 0 ) )"
SG_D0="$( ( unset OAM_WIN_SIGN_METADATA OAM_WIN_SIGN_PUBLISHER; export OAM_SIGN_REQUIRED=0
            export OAM_SIGN_REQUIRED="${OAM_SIGN_REQUIRED:-1}"; . scripts/lib/signing.sh; win_sign_decision 0 ) )"
case "$SG_D1|$SG_D0" in fail:*"|skip:"*) pass ;; *) fail "default: $SG_D1 / explicit 0: $SG_D0" ;; esac

# Because of that default, "unset OAM_SIGN_REQUIRED" is no way out: under
# release-local.sh an unset knob is 1. Every refusal used to advise it.
it "no operator advice says to unset OAM_SIGN_REQUIRED -- the way out is OAM_SIGN_REQUIRED=0"
SG_UNSET="$(git -C "$REPO_DIR" grep -nE '(or|then|just) unset OAM_SIGN_REQUIRED|unset OAM_SIGN_REQUIRED (for|to)' -- . ':!scripts/test-scripts.sh' || true)"
if [ -z "$SG_UNSET" ]; then pass; else fail "stale opt-out advice:"$'\n'"$SG_UNSET"; fi

it "release-local.sh: the release-exists check is one function, and fails closed on anything but gh's exact 'release not found'"
SG_H="$(awk '$0 == "assert_release_unpublished() {" { p = 1 } p { print } p && /^}$/ { exit }' scripts/release-local.sh)"
if grep -qF "grep -qxF 'release not found'" <<<"$SG_H" && grep -qF '[ "$rc" -ne 1 ]' <<<"$SG_H" \
   && [ "$(grep -v '^[[:space:]]*#' scripts/release-local.sh | grep -c 'gh release view "$TAG" --repo "$REPO" 2>&1')" = "1" ]; then pass
else fail "assert_release_unpublished lost its fail-closed shape, or a second inline copy appeared:"$'\n'"$SG_H"; fi

it "release-local.sh: SHA256SUMS, then write/sign/verify the manifest, then the dry-run exit and the upload"
sg_order scripts/release-local.sh 'sha256sum oam-* > SHA256SUMS && cat SHA256SUMS' \
  'release_write_manifest "$RELEASE_DIR" "$TAG"' 'release_sign_manifest "$RELEASE_DIR"' \
  'release_verify_manifest "$RELEASE_DIR" "$TAG"' 'step "DRY RUN' 'gh release create "$TAG" "$RELEASE_DIR"/*' \
  'draft_assets="$(gh release view' 'gh release edit "$TAG" --repo "$REPO" --draft=false'

it "release-local.sh: SHA256SUMS still checksums oam-* only"
ck grep -qF '( cd "$RELEASE_DIR" && ls -lh oam-* >&2 && sha256sum oam-* > SHA256SUMS && cat SHA256SUMS >&2 )' scripts/release-local.sh

it "release-local.sh: the bootstrap skip warns, a fail: decision is fatal, and anything else is refused"
SG_BLK="$(awk '/^sign_decision="\$\(release_signing_decision\)"$/ { f = 1 } f { print } f && /^esac$/ { exit }' scripts/release-local.sh)"
SG_MISS=""
for want in 'skip:*) warn "${sign_decision#skip:}"' 'fail:*) fail "${sign_decision#fail:}"' '*) fail "release_signing_decision returned'; do
  grep -qF -- "$want" <<<"$SG_BLK" || SG_MISS="$SG_MISS [$want]"
done
if [ -z "$SG_MISS" ]; then pass; else fail "preflight decision block lacks:$SG_MISS"; fi

it "arm64 upload: agent before the build; verify < patch < re-sign < ONE upload"
sg_order scripts/release-upload-local-arm64.sh 'trap cleanup EXIT' 'release_agent_start ||' \
  'cargo build --release -p oam_cli' \
  '--pattern SHA256SUMS --pattern RELEASE-MANIFEST --pattern RELEASE-MANIFEST.sig' \
  'release_verify_manifest "$tmp" "$TAG" \' "if (f != a) print }'" \
  'release_write_manifest "$tmp" "$TAG"' 'release_sign_manifest "$tmp"' \
  'upload+=("${tmp}/RELEASE-MANIFEST" "${tmp}/RELEASE-MANIFEST.sig")' 'gh release upload "$TAG"'

it "arm64 upload: the trust root comes from origin/main, before anything decides, signs or verifies"
sg_order scripts/release-upload-local-arm64.sh 'git rev-parse --verify -q "${TAG}^{commit}"' \
  'git ls-remote origin refs/heads/main' 'release_keys_from_commit "$main_sha" "$trust_dir"' \
  'sign_decision="$(release_signing_decision)"' 'release_agent_start ||' 'release_verify_manifest "$tmp" "$TAG" \'

it "arm64 upload: a release with no manifest is patched unsigned only when its tag predates every key range"
SG_BLK="$(awk '/^if \[ "\$SIGNED" = "1" \]; then$/ { f = 1 } f { print } f && /^fi$/ { exit }' scripts/release-upload-local-arm64.sh)"
if grep -qF '[ "$sign_decision" = "sign" ] && ! release_tag_predates_signing "$TAG"' <<<"$SG_BLK" \
   && [ "$(grep -n 'release_tag_predates_signing' <<<"$SG_BLK" | head -1 | cut -d: -f1)" -lt "$(grep -n 'a pre-signing release' <<<"$SG_BLK" | head -1 | cut -d: -f1)" ]; then pass
else fail "the unsigned branch no longer gates on release_tag_predates_signing before its warn:"$'\n'"$SG_BLK"; fi

# The handler BODIES, not just the trap lines: a handler that stopped calling
# release_agent_stop would leave the unlocked key in an orphaned agent on any
# fail() between preflight and the manifest step. (The round-trip group runs
# these bodies for real; this half needs no ssh.)
it "each script's EXIT handler calls release_agent_stop"
SG_MISS=""
SG_H="$(awk '$0 == "release_on_exit() {" { p = 1 } p { print } p && /^}$/ { exit }' scripts/release-local.sh)"
grep -qE '^[[:space:]]*release_agent_stop$' <<<"$SG_H" || SG_MISS="$SG_MISS release-local.sh:release_on_exit"
SG_H="$(awk '$0 == "cleanup() {" { p = 1 } p { print } p && /^}$/ { exit }' scripts/release-upload-local-arm64.sh)"
grep -qE '^[[:space:]]*release_agent_stop$' <<<"$SG_H" || SG_MISS="$SG_MISS release-upload-local-arm64.sh:cleanup"
if [ -z "$SG_MISS" ]; then pass; else fail "handler does not stop the agent:$SG_MISS"; fi

it "release-local.sh: RELEASE_LIVE flips only after the draft goes live"
sg_order scripts/release-local.sh 'RELEASE_LIVE=0' 'trap release_on_exit EXIT' \
  'gh release edit "$TAG" --repo "$REPO" --draft=false' 'RELEASE_LIVE=1'

it "arm64 upload: exactly one gh release upload call"
SG_UPS="$(grep -v '^[[:space:]]*#' scripts/release-upload-local-arm64.sh | grep -c 'gh release upload')"
eq "$SG_UPS" "1"

# One EXIT trap per script (a second silently replaces the first), and the
# release key's agent socket never exported -- an exported SSH_AUTH_SOCK hands
# the key to every ssh the remote legs open. No eval either: the agent is
# started with -D -a precisely so nothing has to eval its output.
it "one EXIT trap per script; SSH_AUTH_SOCK never exported; no eval"
SG_BAD=""
for s in scripts/release-local.sh scripts/release-upload-local-arm64.sh scripts/lib/signing.sh; do
  SG_CODE="$(sed 's/#.*//' "$s")"
  # A command position, not a substring: "bootstrap release" is prose.
  SG_TRAPS="$(grep -cE '(^|[;&|[:space:]])trap[[:space:]]' <<<"$SG_CODE")"
  case "$s" in
    scripts/lib/*) [ "$SG_TRAPS" = "0" ] || SG_BAD="$SG_BAD $s(sets a trap)" ;;
    *) [ "$SG_TRAPS" = "1" ] || SG_BAD="$SG_BAD $s($SG_TRAPS traps)" ;;
  esac
  grep -qE 'export[[:space:]]+SSH_AUTH_SOCK|^[[:space:]]*SSH_AUTH_SOCK=[^[:space:]]*[[:space:]]*$' <<<"$SG_CODE" \
    && SG_BAD="$SG_BAD $s(exports SSH_AUTH_SOCK)"
  grep -qE '(^|[;&|[:space:]])eval[[:space:]]' <<<"$SG_CODE" && SG_BAD="$SG_BAD $s(eval)"
done
if [ -z "$SG_BAD" ]; then pass; else fail "violations:$SG_BAD"; fi

it "release-local.sh: the Windows signing preflight follows the release key's, before the second dirty-tree check, the bump and the tag"
sg_order scripts/release-local.sh 'release_signing_preflight "$TAG" || fail' \
  'win_decision="$(win_sign_decision "$SKIP_WIN_SIGN")"' 'win_sign_preflight || fail' \
  'assert_tree_clean "preflight, after the signing probes"' 'bumping Cargo.toml to' 'git tag -a "$TAG" -m "$TAG"'

# Signing between cp and smoke is what makes every later gate (smoke's CRT
# check, the conpty e2e, the sidecar matrix) run the bytes that ship; the
# re-verify before SHA256SUMS is what makes the checksums and the manifest
# cover signed bytes.
it "release-local.sh: each Windows asset is signed between its cp and its smoke, and re-verified before SHA256SUMS"
sg_order scripts/release-local.sh \
  'cp target/release/oam.exe "$RELEASE_DIR/oam-aarch64-pc-windows-msvc.exe"' \
  'sign_win_asset "$RELEASE_DIR/oam-aarch64-pc-windows-msvc.exe"' \
  'smoke "$RELEASE_DIR/oam-aarch64-pc-windows-msvc.exe"' \
  'cp target/x64-host/release/oam.exe "$RELEASE_DIR/oam-x86_64-pc-windows-msvc.exe"' \
  'sign_win_asset "$RELEASE_DIR/oam-x86_64-pc-windows-msvc.exe"' \
  'smoke "$RELEASE_DIR/oam-x86_64-pc-windows-msvc.exe"' \
  'OAM_CONPTY_BIN="$(cygpath -w "$asset")"' \
  'win_verify "$exe" || fail' 'sha256sum oam-* > SHA256SUMS && cat SHA256SUMS' \
  'release_write_manifest "$RELEASE_DIR" "$TAG"'

it "release-local.sh: sign_win_asset is a no-op unless preflight decided to sign, and signs before it verifies"
SG_H="$(awk '$0 == "sign_win_asset() {" { p = 1 } p { print } p && /^}$/ { exit }' scripts/release-local.sh)"
SG_A="$(grep -nF '[ "$WIN_SIGNING" = "1" ] || return 0' <<<"$SG_H" | cut -d: -f1)"
SG_S="$(grep -nF 'win_sign "$1" || fail' <<<"$SG_H" | cut -d: -f1)"
SG_V="$(grep -nF 'win_verify "$1" || fail' <<<"$SG_H" | cut -d: -f1)"
if [ -n "$SG_A" ] && [ -n "$SG_S" ] && [ -n "$SG_V" ] && [ "$SG_A" -lt "$SG_S" ] && [ "$SG_S" -lt "$SG_V" ]; then pass
else fail "sign_win_asset body out of shape:"$'\n'"$SG_H"; fi

# The body RUN, with stand-ins: the line order above cannot see an early
# `return 0` (or a guard that is never true) that skips signing altogether.
printf '%s\n' "$SG_H" >"$WS/sign-win-asset.sh"
ws_swa(){ # <WIN_SIGNING> <win_sign rc> <win_verify rc>
  ( # shellcheck disable=SC2034  # read by the sourced function
    WIN_SIGNING="$1"; WS_SRC="$2"; WS_VRC="$3"
    win_sign(){ echo "SIGN ${1##*/}"; return "$WS_SRC"; }
    win_verify(){ echo "VERIFY ${1##*/}"; return "$WS_VRC"; }
    fail(){ echo "FAIL: $*"; exit 1; }
    # shellcheck disable=SC1091
    . "$WS/sign-win-asset.sh"
    sign_win_asset "$WS/rel/oam-x86_64-pc-windows-msvc.exe"
    echo "DONE" ) 2>&1
}
it "release-local.sh: sign_win_asset, run: signs then verifies when signing, does nothing when not, and stops on either failure"
WS_A="$(ws_swa 1 0 0)"; WS_B="$(ws_swa 0 0 0)"; WS_C="$(ws_swa 1 1 0)"; WS_E="$(ws_swa 1 0 1)"
if [ "$WS_A" = $'SIGN oam-x86_64-pc-windows-msvc.exe\nVERIFY oam-x86_64-pc-windows-msvc.exe\nDONE' ] && [ "$WS_B" = "DONE" ] \
   && grep -qF 'FAIL: Authenticode signing failed for oam-x86_64-pc-windows-msvc.exe' <<<"$WS_C" && ! grep -qE '^(VERIFY|DONE)' <<<"$WS_C" \
   && grep -qF 'does not verify after signing' <<<"$WS_E" && ! grep -q '^DONE$' <<<"$WS_E"; then pass
else fail "signing: '$WS_A'"$'\n'"       off: '$WS_B'"$'\n'"       sign fails: '$WS_C'"$'\n'"       verify fails: '$WS_E'"; fi

it "no script signs a build output under target/ in place"
SG_BAD="$(grep -nE '(win_sign|sign_win_asset)[[:space:]].*target/' scripts/release-local.sh scripts/release-upload-local-arm64.sh | grep -vE '^[^:]+:[0-9]+:[[:space:]]*#')"
if [ -z "$SG_BAD" ]; then pass; else fail "signs under target/: $SG_BAD"; fi

# The decision block itself, RUN, with the lib's real decision and stand-ins
# for fail/warn and the (Azure-bound) preflight: required + unconfigured must
# die here -- which the order assert above puts before any tag work -- and the
# skip and bootstrap cases must warn and carry on unsigned.
SG_BLK="$(awk '/^WIN_SIGNING=0$/ { f = 1 } f { print } f && /^esac$/ { exit }' scripts/release-local.sh)"
printf '%s\n' "$SG_BLK" >"$WS/decision-block.sh"
ws_block(){ # <SKIP_WIN_SIGN> <preflight rc>
  ( # shellcheck source=lib/signing.sh
    . scripts/lib/signing.sh
    fail(){ echo "FAIL: $*"; exit 1; }
    warn(){ echo "WARN: $*"; }
    # shellcheck disable=SC2034  # read by the sourced block
    SKIP_WIN_SIGN="$1"; WS_PF_RC="$2"
    win_sign_preflight(){ echo "PREFLIGHT"; return "$WS_PF_RC"; }
    # shellcheck disable=SC1091
    . "$WS/decision-block.sh"
    echo "WIN_SIGNING=$WIN_SIGNING" ) 2>&1
}
it "release-local.sh's Windows decision block: required + unconfigured fails; skip and bootstrap warn and go on unsigned"
WS_A="$(OAM_SIGN_REQUIRED=1 ws_block 0 0)"; WS_RA=$?
WS_B="$(ws_block 0 0)"
WS_C="$(OAM_SIGN_REQUIRED=1 OAM_WIN_SIGN_METADATA=/m OAM_WIN_SIGN_PUBLISHER=P ws_block 1 0)"
if [ "$WS_RA" != "0" ] && grep -q '^FAIL: signing is required (OAM_SIGN_REQUIRED' <<<"$WS_A" && ! grep -q 'WIN_SIGNING=' <<<"$WS_A" \
   && grep -q '^WARN: .*bootstrap' <<<"$WS_B" && grep -q '^WIN_SIGNING=0$' <<<"$WS_B" \
   && grep -q '^WARN: OAM_SKIP_WIN_SIGN=1' <<<"$WS_C" && grep -q '^WIN_SIGNING=0$' <<<"$WS_C" \
   && ! grep -q PREFLIGHT <<<"$WS_B$WS_C"; then pass
else fail "required: rc=$WS_RA '$WS_A'"$'\n'"       bootstrap: '$WS_B'"$'\n'"       skip: '$WS_C'"; fi

it "release-local.sh's Windows decision block: configured runs the preflight and signs; a failed preflight is fatal"
WS_A="$(OAM_WIN_SIGN_METADATA=/m OAM_WIN_SIGN_PUBLISHER=P ws_block 0 0)"
WS_B="$(OAM_WIN_SIGN_METADATA=/m OAM_WIN_SIGN_PUBLISHER=P ws_block 0 1)"; WS_RB=$?
if grep -q '^PREFLIGHT$' <<<"$WS_A" && grep -q '^WIN_SIGNING=1$' <<<"$WS_A" \
   && [ "$WS_RB" != "0" ] && grep -q '^FAIL: Windows signing preflight failed' <<<"$WS_B" && ! grep -q 'WIN_SIGNING=' <<<"$WS_B"; then pass
else fail "configured: '$WS_A'"$'\n'"       preflight fails: rc=$WS_RB '$WS_B'"; fi

it "arm64 upload: Windows preflight before the build; the staged copy signed and verified before SHA256SUMS is patched"
sg_order scripts/release-upload-local-arm64.sh 'win_decision="$(win_sign_decision' 'win_sign_preflight ||' \
  'cargo build --release -p oam_cli' 'cp target/release/oam.exe "${tmp}/${ASSET}"' \
  'win_sign "${tmp}/${ASSET}"' 'win_verify "${tmp}/${ASSET}"' "if (f != a) print }'" \
  'sha256sum "$ASSET" >> SHA256SUMS.new' 'gh release upload "$TAG"'

# The WIN_SIGNING guards themselves, RUN: the order asserts above only prove
# the lines exist, and a guard that can never be true (= "2") would keep
# every one of them green while the assets ship unsigned.
# ws_guard <file> <first line of the block> -- the top-level `if` block that
# starts with that exact line, through its closing `fi`.
ws_guard(){ awk -v s="$2" '$0 == s { f = 1 } f { print } f && /^fi$/ { exit }' "$1"; }
# ...and there must be exactly ONE such block per file, or the case below runs
# the first while a second, different one is what guards the checksums. In
# release-local.sh it is the block right before SHA256SUMS is written.
it "each script has exactly one top-level WIN_SIGNING guard; release-local.sh's sits right before sha256sum"
SG_N="$(grep -cxF 'if [ "$WIN_SIGNING" = "1" ]; then' scripts/release-upload-local-arm64.sh) $(grep -cxF 'if [ "$WIN_SIGNING" = "1" ]; then' scripts/release-local.sh)"
SG_NEXT="$(awk '$0 == "if [ \"$WIN_SIGNING\" = \"1\" ]; then" { f = 1 } f && /^fi$/ { getline; print; exit }' scripts/release-local.sh)"
if [ "$SG_N" = "1 1" ] && grep -qF 'sha256sum oam-* > SHA256SUMS' <<<"$SG_NEXT"; then pass
else fail "guard blocks (upload, local): $SG_N; line after release-local.sh's: $SG_NEXT"; fi
ws_guard scripts/release-upload-local-arm64.sh 'if [ "$WIN_SIGNING" = "1" ]; then' >"$WS/upload-guard.sh"
ws_guard scripts/release-local.sh 'if [ "$WIN_SIGNING" = "1" ]; then' >"$WS/local-guard.sh"
ws_run_guard(){ # <block file> <WIN_SIGNING> <win_verify rc>
  ( WIN_SIGNING="$2"; WS_VRC="$3"
    # shellcheck disable=SC2034  # read by the sourced block
    { tmp="$WS/up"; ASSET="oam-aarch64-pc-windows-msvc.exe"; RELEASE_DIR="$WS/rel"; }
    win_sign(){ echo "SIGN ${1##*/}"; }
    win_verify(){ echo "VERIFY ${1##*/}"; return "$WS_VRC"; }
    fail(){ echo "FAIL: $*"; exit 1; }
    # shellcheck disable=SC1090
    . "$1"
    echo "DONE" ) 2>&1
}
mkdir -p "$WS/rel" && : >"$WS/rel/oam-aarch64-pc-windows-msvc.exe" && : >"$WS/rel/oam-x86_64-pc-windows-msvc.exe"
it "arm64 upload: WIN_SIGNING=1 signs then verifies the staged asset; 0 touches nothing; a failed verify stops the upload"
WS_A="$(ws_run_guard "$WS/upload-guard.sh" 1 0)"
WS_B="$(ws_run_guard "$WS/upload-guard.sh" 0 0)"
WS_C="$(ws_run_guard "$WS/upload-guard.sh" 1 1)"
if [ "$WS_A" = $'SIGN oam-aarch64-pc-windows-msvc.exe\nVERIFY oam-aarch64-pc-windows-msvc.exe\nDONE' ] \
   && [ "$WS_B" = "DONE" ] && grep -qF 'does not verify after signing' <<<"$WS_C" && ! grep -q '^DONE$' <<<"$WS_C"; then pass
else fail "signing: '$WS_A'"$'\n'"       off: '$WS_B'"$'\n'"       verify fails: '$WS_C'"; fi

it "release-local.sh: WIN_SIGNING=1 re-verifies every staged .exe before SHA256SUMS; 0 skips; a failure is fatal"
WS_A="$(ws_run_guard "$WS/local-guard.sh" 1 0)"
WS_B="$(ws_run_guard "$WS/local-guard.sh" 0 0)"
WS_C="$(ws_run_guard "$WS/local-guard.sh" 1 1)"
if [ "$WS_A" = $'VERIFY oam-aarch64-pc-windows-msvc.exe\nVERIFY oam-x86_64-pc-windows-msvc.exe\nDONE' ] \
   && [ "$WS_B" = "DONE" ] && grep -qF 'FAIL: oam-aarch64-pc-windows-msvc.exe no longer verifies' <<<"$WS_C" && ! grep -q '^DONE$' <<<"$WS_C"; then pass
else fail "signing: '$WS_A'"$'\n'"       off: '$WS_B'"$'\n'"       verify fails: '$WS_C'"; fi

# The upload script's decision block, RUN, through its check of the asset it
# is about to replace: a run that will not sign must not clobber a signed
# asset unless OAM_SKIP_WIN_SIGN=1 says so. gh is a stand-in that serves a
# fixture (or fails); the PE reading is the lib's own.
awk '/^guard_signed_asset\(\) \{$/ { f = 1 } f { print } f && $0 == "guard_signed_asset \"nothing was built or uploaded\"" { exit }' \
  scripts/release-upload-local-arm64.sh >"$WS/upload-decision.sh"
# ws_upload_block <prior asset fixture, or "" for a failed download> <published
# asset list> [the fixture the asset has become by the re-check before upload]
ws_upload_block(){
  ( # shellcheck source=lib/signing.sh
    . scripts/lib/signing.sh
    WS_PRIOR="$1"; WS_ASSETS="$2"
    # shellcheck disable=SC2034  # read by the sourced block
    { TAG=v9.9.9; REPO=example/example; ASSET="oam-aarch64-pc-windows-msvc.exe"; }
    win_sign_preflight(){ echo "PREFLIGHT"; }
    gh(){
      local dir="" a prev=""
      echo "GH $1 $2" >&2
      if [ "$2" = "view" ]; then printf '%s\n' "$WS_ASSETS"; return 0; fi
      for a in "$@"; do if [ "$prev" = "--dir" ]; then dir="$a"; fi; prev="$a"; done
      [ -n "$WS_PRIOR" ] || return 1
      cp "$WS_PRIOR" "$dir/$ASSET"
    }
    TMPDIR="$WS/dtmp"
    # shellcheck disable=SC1091
    . "$WS/upload-decision.sh"
    echo "WIN_SIGNING=$WIN_SIGNING"
    if [ -n "${3:-}" ]; then
      WS_PRIOR="$3"
      guard_signed_asset "nothing was uploaded"
      echo "UPLOAD"
    fi ) 2>&1
}
mkdir -p "$WS/dtmp"
WS_LIST=$'SHA256SUMS\noam-aarch64-pc-windows-msvc.exe'
it "arm64 upload, not signing: a signed published asset stops the run before the build"
WS_A="$(ws_upload_block "$WS/has-cert-table.exe" "$WS_LIST")"; WS_RA=$?
if [ "$WS_RA" != "0" ] && grep -qF 'is Authenticode-signed' <<<"$WS_A" && grep -qF 'nothing was built or uploaded' <<<"$WS_A" \
   && ! grep -q 'WIN_SIGNING=' <<<"$WS_A" && [ -z "$(ls -A "$WS/dtmp")" ]; then pass
else fail "rc=$WS_RA left='$(ls -A "$WS/dtmp")': $WS_A"; fi

it "arm64 upload, not signing: an asset that cannot be downloaded counts as signed"
WS_A="$(ws_upload_block "" "$WS_LIST")"; WS_RA=$?
if [ "$WS_RA" != "0" ] && grep -qF 'could not be read: unknown' <<<"$WS_A"; then pass; else fail "rc=$WS_RA: $WS_A"; fi

it "arm64 upload, not signing: an unsigned published asset (or none) is replaced, with the bootstrap warning"
WS_A="$(ws_upload_block "$WS_PE" "$WS_LIST")"; WS_RA=$?
WS_B="$(ws_upload_block "" "SHA256SUMS")"; WS_RB=$?
if [ "$WS_RA" = "0" ] && grep -q '^WIN_SIGNING=0$' <<<"$WS_A" && grep -qF 'bootstrap' <<<"$WS_A" \
   && [ "$WS_RB" = "0" ] && grep -q '^WIN_SIGNING=0$' <<<"$WS_B" && ! grep -q '^GH release download' <<<"$WS_B"; then pass
else fail "unsigned prior: rc=$WS_RA '$WS_A'"$'\n'"       no prior: rc=$WS_RB '$WS_B'"; fi

it "arm64 upload: OAM_SKIP_WIN_SIGN=1 may replace a signed asset, saying so; a signing run never looks"
WS_A="$(OAM_SKIP_WIN_SIGN=1 ws_upload_block "$WS/has-cert-table.exe" "$WS_LIST")"; WS_RA=$?
WS_B="$(OAM_WIN_SIGN_METADATA=/m OAM_WIN_SIGN_PUBLISHER=P ws_upload_block "$WS/has-cert-table.exe" "$WS_LIST")"; WS_RB=$?
if [ "$WS_RA" = "0" ] && grep -qF 'OAM_SKIP_WIN_SIGN=1 replaces it with an UNSIGNED build' <<<"$WS_A" && grep -q '^WIN_SIGNING=0$' <<<"$WS_A" \
   && [ "$WS_RB" = "0" ] && grep -q '^PREFLIGHT$' <<<"$WS_B" && grep -q '^WIN_SIGNING=1$' <<<"$WS_B" && ! grep -q '^GH ' <<<"$WS_B"; then pass
else fail "skip: rc=$WS_RA '$WS_A'"$'\n'"       signing: rc=$WS_RB '$WS_B'"; fi

it "arm64 upload, not signing: an asset that became signed during the build stops the upload"
WS_A="$(ws_upload_block "$WS_PE" "$WS_LIST" "$WS/has-cert-table.exe")"; WS_RA=$?
WS_B="$(ws_upload_block "$WS_PE" "$WS_LIST" "$WS_PE")"; WS_RB=$?
if [ "$WS_RA" != "0" ] && grep -q '^WIN_SIGNING=0$' <<<"$WS_A" && grep -qF 'is Authenticode-signed' <<<"$WS_A" \
   && grep -qF '; nothing was uploaded' <<<"$WS_A" && ! grep -q '^UPLOAD$' <<<"$WS_A" \
   && [ "$WS_RB" = "0" ] && grep -q '^UPLOAD$' <<<"$WS_B" && [ -z "$(ls -A "$WS/dtmp")" ]; then pass
else fail "became signed: rc=$WS_RA '$WS_A'"$'\n'"       still unsigned: rc=$WS_RB '$WS_B'"; fi

it "arm64 upload: the signed-asset check runs after the decision, before the build, and again right before the upload"
sg_order scripts/release-upload-local-arm64.sh 'win_decision="$(win_sign_decision' \
  'guard_signed_asset "nothing was built or uploaded"' 'cargo build --release -p oam_cli' \
  'release_verify_manifest "$tmp" "$TAG" || { echo "error: the re-signed' \
  'guard_signed_asset "nothing was uploaded"' 'gh release upload "$TAG" --repo "$REPO" "${upload[@]}" --clobber'

it "arm64 upload: the EXIT trap removes the signature check's scratch dir"
SG_H="$(awk '$0 == "cleanup() {" { p = 1 } p { print } p && /^}$/ { exit }' scripts/release-upload-local-arm64.sh)"
if grep -qF 'if [ -n "$prior_dir" ]; then rm -rf "$prior_dir"; fi' <<<"$SG_H" \
   && grep -qxF 'prior_dir=""' scripts/release-upload-local-arm64.sh; then pass
else fail "cleanup():"$'\n'"$SG_H"; fi

# =============================================================================
group "install.sh / install.ps1 -- the embedded trust root"
# =============================================================================
# The installers carry their own copies of release-keys/: a key list fetched
# from the release would be whatever the release says, so it is embedded. And
# copies drift. Each block is compared with its source byte for byte (CRs
# dropped first: a tool that ignores .gitattributes may check the .ps1 out
# CRLF, and the .ps1 normalizes them before use).
IN="$SUITE_TMP/install"
mkdir -p "$IN"
# in_block <file> <start-line> <end-line> -- the lines strictly between the
# first <start-line> and the <end-line> after it, both matched whole.
in_block(){ tr -d '\r' <"$1" | awk -v s="$2" -v e="$3" 'f && $0 == e { exit } f { print } !f && $0 == s { f = 1 }'; }
# in_swap <file> <start-line> <end-line> <body-file> -- that block's body
# replaced by <body-file>'s lines, in place. This is the suite's ONLY way to
# point an installer at test keys: it edits a COPY of the script, the way an
# attacker would have to edit the script itself. There is no env var for it.
in_swap(){
  awk -v s="$2" -v e="$3" -v f="$4" '
    skip && $0 == e { skip = 0 }
    !skip { print }
    !done && $0 == s { while ((getline l < f) > 0) print l; close(f); skip = 1; done = 1 }
  ' "$1" >"$1.swap" && mv "$1.swap" "$1"
}
# <source file>|<start line>|<end line>, one block per line.
IN_SH_BLOCKS="allowed_signers|  cat <<'OAM_EMBED_ALLOWED_SIGNERS'|OAM_EMBED_ALLOWED_SIGNERS
ranges|  cat <<'OAM_EMBED_RANGES'|OAM_EMBED_RANGES
presigning-sums|  cat <<'OAM_EMBED_PRESIGNING_SUMS'|OAM_EMBED_PRESIGNING_SUMS"
IN_PS_BLOCKS="allowed_signers|\$embeddedAllowedSigners = @'|'@
ranges|\$embeddedRanges = @'|'@
presigning-sums|\$embeddedPresigningSums = @'|'@"
# in_drift <script> <blocks> -- each block present once and identical to
# release-keys/<file>.
in_drift(){
  local bad="" name s e
  while IFS='|' read -r name s e; do
    [ "$(tr -d '\r' <"$1" | grep -cxF -- "$s")" = "1" ] || { bad="$bad $name(start line not there exactly once)"; continue; }
    [ -s "release-keys/$name" ] || { bad="$bad $name(release-keys/$name missing)"; continue; }
    in_block "$1" "$s" "$e" | cmp -s - "release-keys/$name" || bad="$bad $name"
  done <<<"$2"
  if [ -z "$bad" ]; then pass; else fail "$1 embeds a copy that differs from release-keys/:$bad -- paste the file in verbatim"; fi
}

it "install.sh embeds allowed_signers, ranges and presigning-sums byte for byte"
in_drift install/install.sh "$IN_SH_BLOCKS"
it "install.ps1 embeds allowed_signers, ranges and presigning-sums byte for byte"
in_drift install/install.ps1 "$IN_PS_BLOCKS"

# A wrong pin blocks every install of that tag, and a pin for a tag in the
# signing era would let a manifest-less release of it through. So: plain tags,
# all before the first range start (the signing cutoff), 64 hex digits each,
# no tag twice.
IN_CUT="$(awk '!/^[[:space:]]*(#|$)/ { t = $2; sub(/^v/, "", t); split(t, a, "."); k = a[1] * 1000000 + a[2] * 1000 + a[3]
  if (min == "" || k < min) { min = k; tag = $2 } } END { print tag }' release-keys/ranges)"
it "presigning-sums: plain tags before the signing cutoff ($IN_CUT), a sha256 each, no repeats"
IN_BAD="$(awk -v cut="$IN_CUT" 'function key(t, a) { sub(/^v/, "", t); split(t, a, "."); return a[1] * 1000000 + a[2] * 1000 + a[3] }
  !/^[[:space:]]*(#|$)/ {
    if (NF != 2 || $1 !~ /^v[0-9]+\.[0-9]+\.[0-9]+$/ || $2 !~ /^[0-9a-f]+$/ || length($2) != 64 || seen[$1]++ || key($1) >= key(cut)) printf " line %d (%s)", NR, $0
    n++ }
  END { if (n == 0) printf " no data lines" }' release-keys/presigning-sums)"
if [ -n "$IN_CUT" ] && [ -z "$IN_BAD" ]; then pass; else fail "cutoff '$IN_CUT'; bad:$IN_BAD"; fi

it "both installers' signing cutoff is the first tag any key's range opens at"
IN_CUTS="$(grep -xE 'FIRST_MANIFEST_SIG_TAG="[^"]*"' install/install.sh | cut -d'"' -f2) $(tr -d '\r' <install/install.ps1 | grep -xE "\\\$firstManifestSigTag = '[^']*'" | cut -d"'" -f2)"
eq "$IN_CUTS" "$IN_CUT $IN_CUT"

# The env vars an installer reads are its whole override surface. One that
# pointed the key set, the pins or the cutoff elsewhere would be a way round
# every check below, so the set is pinned here.
it "install.sh and install.ps1 read only the documented OAM_* variables"
IN_ENV="$(grep -v '^[[:space:]]*#' install/install.sh | grep -oE '\$\{?OAM_[A-Z0-9_]+' | tr -d '${' | LC_ALL=C sort -u | tr '\n' ' ')|$(tr -d '\r' <install/install.ps1 | grep -v '^[[:space:]]*#' | grep -oE 'env:OAM_[A-Z0-9_]+' | cut -d: -f2 | LC_ALL=C sort -u | tr '\n' ' ')"
eq "$IN_ENV" "OAM_GH_API OAM_INSECURE_SKIP_SIGNATURE OAM_INSTALL_BASE OAM_INSTALL_DIR OAM_VERSION |OAM_GH_API OAM_INSECURE_SKIP_SIGNATURE OAM_INSTALL_BASE OAM_INSTALL_DIR OAM_VERSION "

it "install.sh parses as POSIX sh"
ck sh -n install/install.sh

# The arm64 patch path rewrites a release's SHA256SUMS. On a pinned
# pre-signing release that would change the very hash every installer checks
# it by, so it must refuse -- before the build, from origin/main's table.
it "release-upload-local-arm64.sh refuses a pinned pre-signing tag before it builds anything"
sg_order scripts/release-upload-local-arm64.sh 'release_keys_from_commit "$main_sha" "$trust_dir"' \
  ':release-keys/presigning-sums"' 'is a pre-signing release pinned in release-keys/presigning-sums' \
  'cargo build --release -p oam_cli' 'gh release upload'

# Behaviour, not just order. A pinned tag's own tree carries a copy of this
# script from before the check, so the check only protects anything if
# main's copy can be run against an old tag WITHOUT checking it out. So: the
# real script in a fixture repo (with a local bare origin), HEAD on main, two
# old tags behind it. gh and cargo are stubs that only log, REPO is rewritten
# to a name that does not exist, and GH_TOKEN is bogus -- three separate
# things standing between this test and a real release.
AU="$SUITE_TMP/arm64-upload"
mkdir -p "$AU/work/scripts/lib" "$AU/work/release-keys" "$AU/bin"
au_git(){ git -C "$AU/work" -c user.name=t -c user.email=t@example.invalid -c commit.gpgsign=false -c tag.gpgsign=false "$@"; }
{
  git init -q --bare "$AU/origin.git"
  au_git init -q && au_git checkout -q -b main
  printf 'old\n' >"$AU/work/old.txt"
  au_git add old.txt && au_git commit -qm old
  au_git tag v0.16.4   # pinned in release-keys/presigning-sums
  au_git tag v0.5.0    # before every range and not pinned: patchable unsigned
  sed 's#^REPO="YawLabs/oam"$#REPO="example-invalid/fixture"#' scripts/release-upload-local-arm64.sh \
    >"$AU/work/scripts/release-upload-local-arm64.sh"
  # signing.sh sources iap-helpers.sh (kill_proc_tree).
  cp scripts/lib/build-locks.sh scripts/lib/signing.sh scripts/lib/iap-helpers.sh "$AU/work/scripts/lib/"
  cp release-keys/allowed_signers release-keys/ranges release-keys/presigning-sums "$AU/work/release-keys/"
  printf 'target/\n' >"$AU/work/.gitignore"
  au_git add . && au_git commit -qm main
  au_git remote add origin "$AU/origin.git"
  au_git push -q origin main --tags
} >/dev/null 2>&1
# gh: view succeeds and lists no manifest; download writes a SHA256SUMS
# naming another asset; upload just logs. cargo: logs where it ran and on
# which commit, and writes the binary where --target-dir says.
cat >"$AU/bin/gh" <<EOF
#!/bin/sh
echo "gh \$*" >>"$AU/gh.log"
case "\$1 \$2" in
  "release view") case "\$*" in *--json*) echo SHA256SUMS ;; esac; exit 0 ;;
  "release download")
    d=""; prev=""; for a in "\$@"; do [ "\$prev" = "--dir" ] && d="\$a"; prev="\$a"; done
    printf '%s *oam-x86_64-unknown-linux-gnu\n' 0000000000000000000000000000000000000000000000000000000000000000 >"\$d/SHA256SUMS"
    exit 0 ;;
  "release upload") exit 0 ;;
esac
exit 1
EOF
cat >"$AU/bin/cargo" <<EOF
#!/bin/sh
t=""; prev=""; for a in "\$@"; do [ "\$prev" = "--target-dir" ] && t="\$a"; prev="\$a"; done
printf '%s|%s|%s\n' "\$(pwd)" "\$(git rev-parse HEAD)" "\$t" >>"$AU/cargo.log"
mkdir -p "\$t/release" && printf 'fixture arm64 binary\n' >"\$t/release/oam.exe"
EOF
chmod +x "$AU/bin/gh" "$AU/bin/cargo"
au_run(){ # <tag> -- run main's copy from the fixture's main checkout
  rm -f "$AU/gh.log" "$AU/cargo.log"
  AU_RC=0
  AU_OUT="$(cd "$AU/work" && env -u GITHUB_TOKEN GH_TOKEN=invalid-fixture-token PATH="$AU/bin:$PATH" \
    bash scripts/release-upload-local-arm64.sh "$1" 2>&1)" || AU_RC=$?
}
AU_OLD="$(au_git rev-parse v0.16.4 2>/dev/null)"
AU_STUBS="$(PATH="$AU/bin:$PATH" command -v gh)|$(PATH="$AU/bin:$PATH" command -v cargo)"

it "arm64 upload fixture: HEAD is main, not the tags; gh and cargo resolve to the stubs; REPO is not the real one"
if [ -n "$AU_OLD" ] && [ "$(au_git rev-parse HEAD 2>/dev/null)" != "$AU_OLD" ] \
   && [ "$AU_STUBS" = "$AU/bin/gh|$AU/bin/cargo" ] \
   && grep -qx 'REPO="example-invalid/fixture"' "$AU/work/scripts/release-upload-local-arm64.sh"; then pass; AU_OK=1
else fail "fixture not as expected (stubs: $AU_STUBS) -- not running the script"; AU_OK=0; fi

if [ "$AU_OK" = "1" ]; then
  it "arm64 upload, main's copy: a pinned tag is refused without a checkout of it, before any build or upload"
  au_run v0.16.4
  if [ "$AU_RC" != "0" ] && grep -qF 'v0.16.4 is a pre-signing release pinned in release-keys/presigning-sums' <<<"$AU_OUT" \
     && [ ! -e "$AU/cargo.log" ] && ! grep -q 'release upload' "$AU/gh.log" 2>/dev/null; then pass
  else fail "rc=$AU_RC cargo=$(cat "$AU/cargo.log" 2>/dev/null) gh=$(tr '\n' ';' <"$AU/gh.log" 2>/dev/null): $AU_OUT"; fi

  it "arm64 upload, main's copy: a patchable tag builds in a worktree of the tag, into this checkout's target/, and cleans it up"
  au_run v0.5.0
  IFS='|' read -r AU_CWD AU_HEAD AU_TD <"$AU/cargo.log" 2>/dev/null || true
  AU_WTS="$(au_git worktree list --porcelain 2>/dev/null | grep -c '^worktree ')"
  if [ "$AU_RC" = "0" ] && [ "${AU_HEAD:-}" = "$AU_OLD" ] && [ "${AU_CWD:-}" != "$AU/work" ] && [ ! -e "${AU_CWD:-/nonexistent}" ] \
     && [ "${AU_TD:-}" = "$AU/work/target" ] && [ "$AU_WTS" = "1" ] && grep -q 'release upload v0.5.0' "$AU/gh.log"; then pass
  else fail "rc=$AU_RC cargo ran in '${AU_CWD:-}' on '${AU_HEAD:-}' (tag $AU_OLD) into '${AU_TD:-}'; worktrees=$AU_WTS: $AU_OUT"; fi
fi

# =============================================================================
group "install.sh -- the verify chain, run against a local release fixture"
# =============================================================================
# The real installer, end to end: a copy of install.sh whose embedded blocks
# are swapped (in_swap) for throwaway keys and a fixture pin table, fetching
# from local directories as file:// URLs through OAM_INSTALL_BASE +
# OAM_VERSION. A stub uname makes every host a Linux x86_64 one, and a stub gh
# that always fails keeps the installer's gh fallback off the network.
IN_SSH=0
if command -v ssh-keygen >/dev/null 2>&1 && command -v curl >/dev/null 2>&1; then
  # k1 trusted for v0.18.0..v0.19.5, k2 a staged key with no range, k3 a
  # stranger to allowed_signers, k4 trusted from v0.19.6 on (k1's successor:
  # the lower bound of a range is only exercised by a key that opens ABOVE
  # the cutoff).
  for k in k1 k2 k3 k4; do
    ssh-keygen -q -t ed25519 -N '' -C "oam-release-$k" -f "$IN/$k" </dev/null >/dev/null 2>&1 || break
  done
  printf 'probe\n' >"$IN/probe"
  if [ -f "$IN/k4" ] && ssh-keygen -Y sign -f "$IN/k1" -n oam-release "$IN/probe" </dev/null >/dev/null 2>&1 \
     && [ -s "$IN/probe.sig" ]; then
    IN_SSH=1
  fi
fi
it "this host can sign a fixture release (ssh-keygen -Y, curl)"
if [ "$IN_SSH" = "1" ]; then pass
else skip "no ssh-keygen with -Y or no curl here -- every installer fixture case below is skipped"; fi

if [ "$IN_SSH" = "1" ]; then
  in_url(){ if command -v cygpath >/dev/null 2>&1; then printf 'file:///%s' "$(cygpath -m "$1")"; else printf 'file://%s' "$1"; fi; }
  in_sha(){ if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | awk '{ print $1 }'; }
  IN_LINUX=oam-x86_64-unknown-linux-gnu
  # The Windows assets: any small real executable, so install.ps1's closing
  # `oam --version` smoke has something it can start.
  IN_WIN_EXE="$IN/fixture.exe"
  if [ -f /c/Windows/System32/where.exe ]; then cp /c/Windows/System32/where.exe "$IN_WIN_EXE"
  else printf 'not a windows host\n' >"$IN_WIN_EXE"; fi

  # The trust root the copies embed.
  { printf 'oam-release-k1 namespaces="oam-release" %s\n' "$(cut -d' ' -f1,2 "$IN/k1.pub")"
    printf 'oam-release-k2 namespaces="oam-release" %s\n' "$(cut -d' ' -f1,2 "$IN/k2.pub")"
    printf 'oam-release-k4 namespaces="oam-release" %s\n' "$(cut -d' ' -f1,2 "$IN/k4.pub")"; } >"$IN/allowed_signers"
  printf '# fixture ranges\nk1 v0.18.0 v0.19.5\nk4 v0.19.6 -\n' >"$IN/ranges"

  # in_rel <name> <manifest tag|-> <signing key|-> -- a release directory: the
  # three binaries, license files, SHA256SUMS written the way release-local.sh
  # writes it ("<hash> *<asset>"), and optionally the manifest and its .sig.
  in_rel(){
    local d="$IN/rel/$1" f
    mkdir -p "$d"
    printf '#!/bin/sh\necho "oam fixture %s"\n' "$1" >"$d/$IN_LINUX"
    cp "$IN_WIN_EXE" "$d/oam-x86_64-pc-windows-msvc.exe"
    cp "$IN_WIN_EXE" "$d/oam-aarch64-pc-windows-msvc.exe"
    ( cd "$d" && for f in oam-*; do printf '%s *%s\n' "$(in_sha "$f")" "$f"; done >SHA256SUMS )
    for f in LICENSE NOTICE THIRD_PARTY_LICENSES.md; do printf 'fixture %s\n' "$f" >"$d/$f"; done
    if [ "$2" != "-" ]; then { printf 'oam-release-manifest v1\ntag %s\n' "$2"; cat "$d/SHA256SUMS"; } >"$d/RELEASE-MANIFEST"; fi
    if [ "$3" != "-" ]; then ssh-keygen -Y sign -f "$IN/$3" -n oam-release "$d/RELEASE-MANIFEST" </dev/null >/dev/null 2>&1; fi
  }
  in_rel good     v0.18.0 k1
  in_rel stranger v0.18.0 k3   # signed, by a key that is not ours
  in_rel forged   v0.18.0 k1   # signed by k1, then a SUMS line swapped
  sed '3s/^./0/' "$IN/rel/forged/RELEASE-MANIFEST" >"$IN/m" && mv "$IN/m" "$IN/rel/forged/RELEASE-MANIFEST"
  in_rel garbled  v0.18.0 k1   # a .sig that is not a signature at all
  printf -- '-----BEGIN SSH SIGNATURE-----\nnot base64\n-----END SSH SIGNATURE-----\n' >"$IN/rel/garbled/RELEASE-MANIFEST.sig"
  in_rel replay   v0.18.0 k1   # served as v0.18.1: a genuine manifest for another tag
  in_rel retired  v0.20.0 k1   # after k1's range closed
  in_rel staged   v0.18.0 k2   # k2 is in allowed_signers but has no range
  in_rel early    v0.18.1 k4   # k4 signing a tag before its range opens
  in_rel succ     v0.20.0 k4   # ...and one inside it
  in_rel unsigned -       -    # a v0.18.0+ release with no manifest at all
  in_rel tampered v0.18.0 k1   # manifest fine, binary swapped after signing
  printf 'evil\n' >>"$IN/rel/tampered/$IN_LINUX"
  printf 'evil\n' >>"$IN/rel/tampered/oam-x86_64-pc-windows-msvc.exe"
  printf 'evil\n' >>"$IN/rel/tampered/oam-aarch64-pc-windows-msvc.exe"
  in_rel pre      -       -    # v0.17.1: pinned, and the pin matches
  in_rel prebad   -       -    # v0.16.4: pinned, and the pin does not match
  { printf '# fixture pins\n'
    printf 'v0.17.1 %s\n' "$(in_sha "$IN/rel/pre/SHA256SUMS")"
    printf 'v0.16.4 %s\n' "0000000000000000000000000000000000000000000000000000000000000000"; } >"$IN/presigning-sums"

  # The copies under test, and proof the seam took: a copy still carrying the
  # real keys would fail every case below for the wrong reason.
  cp install/install.sh "$IN/install.sh"
  # The .ps1 copy also loses its user-PATH write the moment it exists: the
  # real script persists its install dir in the registry, and a test run must
  # never touch the operator's PATH. ENVIRON, not -v: awk -v would read
  # backslashes as escapes.
  IN_PS_PATHSET="[Environment]::SetEnvironmentVariable('Path', \$newPath, 'User')"
  L="    $IN_PS_PATHSET" awk '$0 == ENVIRON["L"] { print "    # (fixture copy: the user PATH is left alone)"; next } { print }' \
    install/install.ps1 >"$IN/install.ps1"
  IN_SWAP_BAD=""
  for in_pair in "$IN/install.sh|$IN_SH_BLOCKS" "$IN/install.ps1|$IN_PS_BLOCKS"; do
    in_f="${in_pair%%|*}"
    while IFS='|' read -r name s e; do
      in_swap "$in_f" "$s" "$e" "$IN/$name"
      in_block "$in_f" "$s" "$e" | cmp -s - "$IN/$name" || IN_SWAP_BAD="$IN_SWAP_BAD $in_f:$name"
    done <<<"${in_pair#*|}"
  done
  it "the fixture copies carry the throwaway keys and pins"
  if [ -z "$IN_SWAP_BAD" ]; then pass; else fail "swap did not take:$IN_SWAP_BAD"; fi

  # PATHs. Stubs first: uname (Linux x86_64) and a gh that always fails.
  IN_STUB="$IN/stub"; mkdir -p "$IN_STUB"
  printf '#!/bin/sh\ncase "$1" in -s) echo Linux ;; -m) echo x86_64 ;; *) echo Linux ;; esac\n' >"$IN_STUB/uname"
  printf '#!/bin/sh\nexit 1\n' >"$IN_STUB/gh"
  # An ssh-keygen from before -Y (OpenSSH 8.0 answers -Y with this).
  IN_OLDKG="$IN/oldkg"; mkdir -p "$IN_OLDKG"
  printf '#!/bin/sh\necho "unknown option -- Y" >&2\necho "usage: ssh-keygen [-q] [-b bits]" >&2\nexit 1\n' >"$IN_OLDKG/ssh-keygen"
  # No ssh-keygen at all: a PATH of wrappers for exactly what install.sh runs.
  # (Copies of the binaries would lose their DLLs on Windows; a wrapper does
  # not.) A tool missing from this list fails the no-ssh-keygen cases loudly.
  IN_NOKG="$IN/nokg"; mkdir -p "$IN_NOKG"
  for t in awk sed grep head tail tr cut mktemp chmod mv rm rmdir mkdir cat curl sha256sum shasum; do
    p="$(command -v "$t" 2>/dev/null)" || continue
    printf '#!/bin/sh\nexec "%s" "$@"\n' "$p" >"$IN_NOKG/$t"
  done
  chmod +x "$IN_STUB"/* "$IN_OLDKG"/* "$IN_NOKG"/*
  IN_PATH_KG="$IN_STUB:$PATH"
  IN_PATH_OLDKG="$IN_STUB:$IN_OLDKG:$PATH"
  IN_PATH_NOKG="$IN_STUB:$IN_NOKG"
  # dash where there is one: it is the /bin/sh that `curl | sh` meets on
  # Debian and Ubuntu, and it has none of bash's forgiveness.
  IN_SH="$(command -v dash 2>/dev/null || command -v sh)"

  # in_sh <release> <tag> <PATH> [VAR=value...] -- run the copy into a fresh
  # $IN/dest; IN_RC and IN_OUT get the result. The operator's own token and
  # knobs never reach it.
  in_sh(){
    local rel="$1" tag="$2" path="$3"; shift 3
    rm -rf "$IN/dest"
    IN_RC=0
    IN_OUT="$(env -u GH_TOKEN -u GITHUB_TOKEN -u OAM_INSECURE_SKIP_SIGNATURE -u OAM_GH_API \
      HOME="$IN" PATH="$path" OAM_INSTALL_BASE="$(in_url "$IN/rel/$rel")" OAM_VERSION="$tag" \
      OAM_INSTALL_DIR="$IN/dest" "$@" "$IN_SH" "$IN/install.sh" 2>&1)" || IN_RC=$?
  }
  # in_refused <needle> -- the run failed, said <needle>, and left nothing in
  # the install dir: no oam, no half-written temp file.
  in_refused(){
    local left
    left="$(ls -A "$IN/dest" 2>/dev/null | tr '\n' ' ')"
    if [ "$IN_RC" != "0" ] && grep -qF -- "$1" <<<"$IN_OUT" && [ -z "$left" ]; then pass
    else fail "rc=$IN_RC, wanted a refusal saying '$1' and an empty install dir (holds: '$left'): $IN_OUT"; fi
  }
  # in_installed <release> <needle> -- the run succeeded, said <needle>, and
  # installed exactly that release's binary, executable.
  in_installed(){
    if [ "$IN_RC" = "0" ] && grep -qF -- "$2" <<<"$IN_OUT" && cmp -s "$IN/dest/oam" "$IN/rel/$1/$IN_LINUX" \
       && [ -x "$IN/dest/oam" ] && ! compgen -G "$IN/dest/.oam.*" >/dev/null; then pass
    else fail "rc=$IN_RC, wanted '$2' and rel/$1's binary in place: $IN_OUT"; fi
  }

  it "good: a manifest signed by an in-range key installs, hash from the manifest"
  in_sh good v0.18.0 "$IN_PATH_KG"
  in_installed good 'signature ok: v0.18.0, signed by oam-release-k1'
  it "good: the installed binary runs (the closing --version smoke)"
  grep -qF 'oam fixture good' <<<"$IN_OUT" && pass || fail "$IN_OUT"
  it "good: license files land beside the binary"
  ck cmp -s "$IN/dest/licenses/NOTICE" "$IN/rel/good/NOTICE"

  it "bad signature: signed by a key that is not in allowed_signers"
  in_sh stranger v0.18.0 "$IN_PATH_KG"
  in_refused 'does not verify against any oam release key'
  it "bad signature: a SUMS line changed after signing"
  in_sh forged v0.18.0 "$IN_PATH_KG"
  in_refused 'does not verify against any oam release key'
  it "bad signature: a .sig that does not even parse"
  in_sh garbled v0.18.0 "$IN_PATH_KG"
  in_refused 'does not verify against any oam release key'
  it "tag mismatch: a genuine v0.18.0 manifest served as v0.18.1"
  in_sh replay v0.18.1 "$IN_PATH_KG"
  in_refused "signed for tag 'v0.18.0', not v0.18.1"
  it "key out of range: k1 signing after its range closed"
  in_sh retired v0.20.0 "$IN_PATH_KG"
  in_refused 'which was retired after v0.19.5'
  it "key out of range: the staged key, which has no range yet"
  in_sh staged v0.18.0 "$IN_PATH_KG"
  in_refused 'oam-release-k2, which has no range'
  # The rotation property: after "k1 .. v0.19.5 / k4 v0.19.6 -", k4 must not
  # be able to vouch for a tag from k1's era.
  it "key out of range: a successor key signing a tag before its range opens"
  in_sh early v0.18.1 "$IN_PATH_KG"
  in_refused 'oam-release-k4, which may sign only from v0.19.6 on'
  it "the successor key installs a tag inside its range"
  in_sh succ v0.20.0 "$IN_PATH_KG"
  in_installed succ 'signature ok: v0.20.0, signed by oam-release-k4'
  it "missing manifest on a v0.18.0+ tag is refused"
  in_sh unsigned v0.18.0 "$IN_PATH_KG"
  in_refused 'could not fetch RELEASE-MANIFEST for v0.18.0'
  it "tampered asset: the binary no longer matches the signed manifest"
  in_sh tampered v0.18.0 "$IN_PATH_KG"
  in_refused 'checksum mismatch for oam-x86_64-unknown-linux-gnu'

  it "pre-cutoff: SHA256SUMS matching its pin installs"
  in_sh pre v0.17.1 "$IN_PATH_KG"
  in_installed pre 'v0.17.1 matches its pinned digest'
  it "pre-cutoff: SHA256SUMS not matching its pin is refused"
  in_sh prebad v0.16.4 "$IN_PATH_KG"
  in_refused "v0.16.4's pinned digest is 0000"
  it "pre-cutoff: a tag with no pin is refused, even with a SUMS that would verify"
  in_sh pre v0.17.2 "$IN_PATH_KG"
  in_refused 'v0.17.2 predates signed releases (v0.18.0) and is not in the pinned table'

  it "no ssh-keygen, v0.18.0+: refused, naming the fix and the override"
  in_sh good v0.18.0 "$IN_PATH_NOKG"
  if grep -qF 'apt-get install openssh-client' <<<"$IN_OUT" && grep -qF 'OAM_INSECURE_SKIP_SIGNATURE=1' <<<"$IN_OUT"; then
    in_refused 'ssh-keygen is not installed'
  else fail "rc=$IN_RC, no fix/override named: $IN_OUT"; fi
  it "no ssh-keygen, pre-cutoff: installs by its pin, and says the tool will be needed"
  in_sh pre v0.17.1 "$IN_PATH_NOKG"
  in_installed pre 'Not needed for v0.17.1 (verified by its pinned digest)'
  it "an ssh-keygen without -Y counts as none"
  in_sh good v0.18.0 "$IN_PATH_OLDKG"
  in_refused 'has no -Y'
  # OpenSSH 8.1 (the Windows 10 inbox client): -Y verify works, but the probe
  # text "Unsupported operation for -Y" only arrived in 8.2, and without -n
  # 8.1 stops at "missing namespace" before it looks at the operation. The
  # stub answers the probe as 8.1 does and hands everything else to the real
  # ssh-keygen, so a probe that rejects it fails the whole install.
  IN_KG81="$IN/kg81"; mkdir -p "$IN_KG81"
  printf '#!/bin/sh\ncase "$*" in\n  *"-Y oam-probe"*)\n    case "$*" in *" -n "*) ;; *) echo "Too few arguments for sign/verify: missing namespace" >&2; exit 1 ;; esac\n    echo "usage: ssh-keygen [-q] [-b bits]" >&2\n    echo "       ssh-keygen -Y verify -f allowed_signers_file -I signer_identity" >&2\n    exit 1 ;;\nesac\nexec "%s" "$@"\n' \
    "$(command -v ssh-keygen)" >"$IN_KG81/ssh-keygen"
  chmod +x "$IN_KG81/ssh-keygen"
  it "an OpenSSH 8.1 ssh-keygen (no 'Unsupported operation' text) counts as present, and installs"
  in_sh good v0.18.0 "$IN_STUB:$IN_KG81:$PATH"
  in_installed good 'signature ok: v0.18.0, signed by oam-release-k1'
  it "OAM_INSECURE_SKIP_SIGNATURE=1 installs without ssh-keygen, loudly"
  in_sh good v0.18.0 "$IN_PATH_NOKG" OAM_INSECURE_SKIP_SIGNATURE=1
  in_installed good 'installing WITHOUT signature verification'
  it "OAM_INSECURE_SKIP_SIGNATURE=1 never excuses a bad signature"
  in_sh stranger v0.18.0 "$IN_PATH_KG" OAM_INSECURE_SKIP_SIGNATURE=1
  in_refused 'does not verify against any oam release key'
  it "OAM_INSECURE_SKIP_SIGNATURE=1 never excuses a missing manifest"
  in_sh unsigned v0.18.0 "$IN_PATH_NOKG" OAM_INSECURE_SKIP_SIGNATURE=1
  in_refused 'could not fetch RELEASE-MANIFEST for v0.18.0'
  it "OAM_INSTALL_BASE without OAM_VERSION is refused"
  in_sh good "" "$IN_PATH_KG"
  in_refused 'OAM_INSTALL_BASE needs OAM_VERSION'
  it "a tag with a trailing newline is not a tag"
  in_sh pre $'v0.17.1\n' "$IN_PATH_KG"
  in_refused 'is not a release tag'

  # The rename, not just its end state: a temp file anywhere else (the old
  # mktemp -d in $TMPDIR) also ends with the right bytes in place, but its mv
  # is a cross-filesystem copy over a possibly running binary. So a logging mv
  # goes first on PATH, and the source of the move onto oam must sit in the
  # install dir itself.
  IN_MVLOG="$IN/mvlog"; mkdir -p "$IN_MVLOG"
  printf '#!/bin/sh\nprintf "%%s|" "$@" >>"%s"; echo >>"%s"\nexec "%s" "$@"\n' \
    "$IN/mv.log" "$IN/mv.log" "$(command -v mv)" >"$IN_MVLOG/mv"
  chmod +x "$IN_MVLOG/mv"
  it "a re-install replaces the binary by rename from a temp file in the install dir"
  in_sh good v0.18.0 "$IN_PATH_KG"
  printf 'old\n' >"$IN/dest/oam"
  rm -f "$IN/mv.log"
  IN_RC=0
  IN_OUT="$(env -u GH_TOKEN -u GITHUB_TOKEN HOME="$IN" PATH="$IN_MVLOG:$IN_PATH_KG" OAM_INSTALL_BASE="$(in_url "$IN/rel/good")" \
    OAM_VERSION=v0.18.0 OAM_INSTALL_DIR="$IN/dest" "$IN_SH" "$IN/install.sh" 2>&1)" || IN_RC=$?
  IN_MVSRC="$(D="$IN/dest/oam" awk -F'|' '$(NF-1) == ENVIRON["D"] { print $(NF-2) }' "$IN/mv.log" 2>/dev/null)"
  case "$IN_MVSRC" in
    "$IN/dest/.oam."*) in_installed good 'installed oam v0.18.0' ;;
    *) fail "the move onto $IN/dest/oam came from '${IN_MVSRC:-nowhere}', not a temp file in the install dir (mv log: $(cat "$IN/mv.log" 2>/dev/null))" ;;
  esac

  # A PATH of exactly the tools install.sh runs, curl among them -- proof the
  # list is complete for a signed install, so its wget twin below fails only
  # for wget's sake.
  in_toolpath(){ # <dir> <tool>...
    local d="$1" t p; shift; mkdir -p "$d"
    for t in "$@"; do
      p="$(command -v "$t" 2>/dev/null)" || continue
      printf '#!/bin/sh\nexec "%s" "$@"\n' "$p" >"$d/$t"; chmod +x "$d/$t"
    done
  }
  IN_TOOLS="awk sed grep head tail tr cut mktemp chmod mv rm rmdir mkdir cat sha256sum shasum ssh-keygen"
  # shellcheck disable=SC2086 # IN_TOOLS is a word list
  in_toolpath "$IN/min-curl" $IN_TOOLS curl
  it "a minimal PATH (the tools install.sh needs, with curl) installs a signed release"
  in_sh good v0.18.0 "$IN_STUB:$IN/min-curl"
  in_installed good 'signature ok: v0.18.0, signed by oam-release-k1'

  # Over HTTP, with the tag resolved through /releases/latest: a local server
  # stands in for github.com in a copy whose two github.com URLs point at it.
  # (file:// covers neither the redirect nor wget, which cannot fetch it.)
  IN_PY=""
  for p in python3 python; do
    if command -v "$p" >/dev/null 2>&1 && "$p" -c 'import http.server' >/dev/null 2>&1; then IN_PY="$p"; break; fi
  done
  in_wpath(){ if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
  IN_PORT=""
  if [ -n "$IN_PY" ]; then
    cat >"$IN/srv.py" <<'PY'
import http.server, os, sys, time
root, portfile = sys.argv[1], sys.argv[2]
rel = '/YawLabs/oam/releases'
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def route(self):
        p = self.path.split('?')[0]
        if p == rel + '/latest':
            # Lower-case, as an HTTP/2 front end sends it.
            return 302, b'', 'http://127.0.0.1:%d%s/tag/v0.18.0' % (self.server.server_address[1], rel)
        if p == rel + '/tag/v0.18.0':
            return 200, b'', None
        pre = rel + '/download/v0.18.0/'
        name = p[len(pre):] if p.startswith(pre) else ''
        f = os.path.join(root, name)
        if name and '/' not in name and '\\' not in name and os.path.isfile(f):
            with open(f, 'rb') as fh:
                return 200, fh.read(), None
        return 404, b'', None
    def reply(self, body):
        code, data, loc = self.route()
        self.send_response(code)
        if loc:
            self.send_header('location', loc)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        if body:
            self.wfile.write(data)
    def do_GET(self):
        self.reply(True)
    def do_HEAD(self):
        self.reply(False)
s = http.server.HTTPServer(('127.0.0.1', 0), H)
s.timeout = 1
with open(portfile + '.tmp', 'w') as fh:
    fh.write(str(s.server_address[1]))
os.replace(portfile + '.tmp', portfile)
# Gone by itself when the suite's temp root is, or after ten minutes.
deadline = time.time() + 600
while time.time() < deadline and os.path.isdir(root) and not os.path.exists(portfile + '.stop'):
    s.handle_request()
PY
    "$IN_PY" "$(in_wpath "$IN/srv.py")" "$(in_wpath "$IN/rel/good")" "$(in_wpath "$IN/srv.port")" >/dev/null 2>&1 &
    IN_SRV_PID=$!
    for _ in $(seq 1 50); do [ -s "$IN/srv.port" ] && break; sleep 0.2; done
    IN_PORT="$(cat "$IN/srv.port" 2>/dev/null)"
  fi
  it "a local HTTP stand-in for github.com is up"
  if [ -n "$IN_PORT" ]; then pass
  else skip "no python with http.server here (or it did not start) -- the /releases/latest and wget cases are skipped"; fi

  if [ -n "$IN_PORT" ]; then
    sed "s#https://github.com/#http://127.0.0.1:$IN_PORT/#g" "$IN/install.sh" >"$IN/install-http.sh"
    # in_http <PATH> -- the HTTP copy, with no OAM_VERSION or OAM_INSTALL_BASE.
    in_http(){
      rm -rf "$IN/dest"
      IN_RC=0
      IN_OUT="$(env -u GH_TOKEN -u GITHUB_TOKEN -u OAM_INSECURE_SKIP_SIGNATURE -u OAM_GH_API -u OAM_VERSION -u OAM_INSTALL_BASE \
        HOME="$IN" PATH="$1" OAM_INSTALL_DIR="$IN/dest" "$IN_SH" "$IN/install-http.sh" 2>&1)" || IN_RC=$?
    }
    it "latest, curl: the tag comes from the /releases/latest redirect, the assets from /download/<tag>/"
    if [ "$(grep -c "http://127.0.0.1:$IN_PORT/" "$IN/install-http.sh")" -ge 2 ]; then
      in_http "$IN_STUB:$IN/min-curl"
      in_installed good 'signature ok: v0.18.0, signed by oam-release-k1'
    else fail "the HTTP copy does not point at the local server"; fi
    it "latest, wget and no curl: the same install through install.sh's wget branch"
    if command -v wget >/dev/null 2>&1; then
      # shellcheck disable=SC2086 # IN_TOOLS is a word list
      in_toolpath "$IN/min-wget" $IN_TOOLS wget
      in_http "$IN_STUB:$IN/min-wget"
      in_installed good 'signature ok: v0.18.0, signed by oam-release-k1'
    else skip "no wget here -- install.sh's wget branch (dl, final_url) is not exercised on this host"; fi
  fi
fi

# =============================================================================
group "install.ps1 -- the verify chain, run against the same fixture"
# =============================================================================
# Windows PowerShell 5.1, 64-bit and 32-bit (the 32-bit one reaches the inbox
# ssh-keygen only through Sysnative). The copy is the fixture one above, with
# no user-PATH write; its no-ssh-keygen variant looks in directories that do
# not exist.
IN_PS64=/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe
IN_PS32=/c/Windows/SysWOW64/WindowsPowerShell/v1.0/powershell.exe
it "this host has Windows PowerShell and the fixture"
if [ "$IN_SSH" = "1" ] && [ -x "$IN_PS64" ] && command -v cygpath >/dev/null 2>&1; then pass
else skip "not a Windows host with powershell.exe and the fixture above -- every install.ps1 case is skipped"; IN_PS64=""; fi

if [ -n "$IN_PS64" ]; then
  # The no-ssh-keygen variant: candidates that do not exist (PATH below has no
  # OpenSSH either).
  IN_PS_CAND='$sshKeygenCandidates = @("$env:windir\Sysnative\OpenSSH\ssh-keygen.exe", "$env:windir\System32\OpenSSH\ssh-keygen.exe")'
  L="$IN_PS_CAND" awk '$0 == ENVIRON["L"] { print "$sshKeygenCandidates = @(\"C:\\no-such-dir\\ssh-keygen.exe\")"; next } { print }' \
    "$IN/install.ps1" >"$IN/install-nokg.ps1"
  # Asserted, not assumed: a copy that still writes the user PATH must not run.
  it "the install.ps1 fixture copies dropped the PATH write and (nokg) the ssh-keygen candidates"
  if ! grep -qF "$IN_PS_PATHSET" "$IN/install.ps1" && grep -qxF "$IN_PS_CAND" "$IN/install.ps1" \
     && ! grep -qxF "$IN_PS_CAND" "$IN/install-nokg.ps1" && grep -qF 'no-such-dir' "$IN/install-nokg.ps1"; then pass
  else fail "the copies were not rewritten as expected -- refusing to run install.ps1 against the real user PATH"; IN_PS64=""; fi
fi

if [ -n "$IN_PS64" ]; then
  # Windows' own directories only: no OpenSSH, no Git, no gh on it.
  IN_WINPATH="/c/Windows/System32:/c/Windows:/c/Windows/System32/WindowsPowerShell/v1.0"
  # The console code page is part of the fixture. Under a UTF-8 console (65001:
  # Windows Terminal, Yaw, `chcp 65001`) .NET Framework's Process.StandardInput
  # writer carries a byte-order mark, and the 2026-10-04 v0.18.0 gate failed
  # every signed ps1 case with it: ssh-keygen hashed three bytes of BOM ahead
  # of the manifest. Under conhost's default (437) the same code passed. So
  # each case runs under an explicit page -- 65001 unless IN_PS_CP says
  # otherwise -- and the console's own page is put back afterwards: chcp is
  # per console and sticks, and this suite shares the operator's. chcp.com is
  # called straight from bash (same console as the PowerShell it precedes);
  # not through cmd, which gets every embedded quote MSYS-escaped as \".
  IN_CHCP=/c/Windows/System32/chcp.com
  IN_CP_ORIG="$("$IN_CHCP" 2>/dev/null | tr -dc '0-9')"
  [ -n "$IN_CP_ORIG" ] || IN_CP_ORIG=437
  in_ps_cp(){ "$IN_CHCP" "$1" >/dev/null 2>&1 || true; }
  # in_ps <powershell> <script> <release> <tag> [VAR=value...] -- run it into a
  # fresh $IN/pdest. PowerShell wraps long error lines at the console width,
  # mid-word, so the output is joined back up before any needle is looked for.
  in_ps(){
    local ps="$1" script="$2" rel="$3" tag="$4"; shift 4
    rm -rf "$IN/pdest"
    IN_RC=0
    in_ps_cp "${IN_PS_CP:-65001}"
    IN_OUT="$(env -u GH_TOKEN -u GITHUB_TOKEN -u OAM_INSECURE_SKIP_SIGNATURE -u OAM_GH_API \
      PATH="$IN_WINPATH" OAM_INSTALL_BASE="$(in_url "$IN/rel/$rel")" OAM_VERSION="$tag" \
      OAM_INSTALL_DIR="$(cygpath -w "$IN/pdest")" "$@" \
      "$ps" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(cygpath -w "$script")" 2>&1 | tr -d '\r\n')" || IN_RC=$?
    in_ps_cp "$IN_CP_ORIG"
  }
  in_ps_refused(){
    if [ "$IN_RC" != "0" ] && grep -qF -- "$1" <<<"$IN_OUT" && [ ! -e "$IN/pdest/oam.exe" ]; then pass
    else fail "rc=$IN_RC, wanted a refusal saying '$1' and no oam.exe: $IN_OUT"; fi
  }
  in_ps_installed(){
    if [ "$IN_RC" = "0" ] && grep -qF -- "$1" <<<"$IN_OUT" && cmp -s "$IN/pdest/oam.exe" "$IN_WIN_EXE"; then pass
    else fail "rc=$IN_RC, wanted '$1' and the fixture oam.exe in place: $IN_OUT"; fi
  }

  it "ps1 good: verifies with System32's inbox ssh-keygen and installs (UTF-8 console, 65001)"
  in_ps "$IN_PS64" "$IN/install.ps1" good v0.18.0
  if grep -qF 'System32\OpenSSH\ssh-keygen.exe' <<<"$IN_OUT"; then in_ps_installed 'signature ok: v0.18.0, signed by oam-release-k1'
  else fail "did not verify with the System32 ssh-keygen: $IN_OUT"; fi
  it "ps1 good under conhost's default code page (437)"
  IN_PS_CP=437 in_ps "$IN_PS64" "$IN/install.ps1" good v0.18.0
  in_ps_installed 'signature ok: v0.18.0, signed by oam-release-k1'
  it "ps1 good, 32-bit PowerShell: finds ssh-keygen through Sysnative"
  if [ -x "$IN_PS32" ]; then
    in_ps "$IN_PS32" "$IN/install.ps1" good v0.18.0
    if grep -qF 'Sysnative\OpenSSH\ssh-keygen.exe' <<<"$IN_OUT"; then in_ps_installed 'signature ok: v0.18.0, signed by oam-release-k1'
    else fail "did not verify with the Sysnative ssh-keygen: $IN_OUT"; fi
  else skip "no 32-bit Windows PowerShell at $IN_PS32"; fi
  it "ps1 bad signature: signed by a key that is not ours"
  in_ps "$IN_PS64" "$IN/install.ps1" stranger v0.18.0
  in_ps_refused 'does not verify against any oam release key'
  it "ps1 bad signature: a SUMS line changed after signing"
  in_ps "$IN_PS64" "$IN/install.ps1" forged v0.18.0
  in_ps_refused 'does not verify against any oam release key'
  it "ps1 bad signature: a .sig that does not even parse"
  in_ps "$IN_PS64" "$IN/install.ps1" garbled v0.18.0
  in_ps_refused 'does not verify against any oam release key'
  it "ps1 tag mismatch"
  in_ps "$IN_PS64" "$IN/install.ps1" replay v0.18.1
  in_ps_refused "signed for tag 'v0.18.0', not v0.18.1"
  it "ps1 key out of range: retired"
  in_ps "$IN_PS64" "$IN/install.ps1" retired v0.20.0
  in_ps_refused 'which was retired after v0.19.5'
  it "ps1 key out of range: staged, no range"
  in_ps "$IN_PS64" "$IN/install.ps1" staged v0.18.0
  in_ps_refused 'oam-release-k2, which has no range'
  it "ps1 key out of range: a successor key signing a tag before its range opens"
  in_ps "$IN_PS64" "$IN/install.ps1" early v0.18.1
  in_ps_refused 'oam-release-k4, which may sign only from v0.19.6 on'
  it "ps1 the successor key installs a tag inside its range"
  in_ps "$IN_PS64" "$IN/install.ps1" succ v0.20.0
  in_ps_installed 'signature ok: v0.20.0, signed by oam-release-k4'
  # .NET's $ matches before a final newline; the tag check must not.
  it "ps1 a tag with a trailing newline is not a tag"
  in_ps "$IN_PS64" "$IN/install.ps1" pre $'v0.17.1\n'
  in_ps_refused 'is not a release tag'
  it "ps1 missing manifest on a v0.18.0+ tag"
  in_ps "$IN_PS64" "$IN/install.ps1" unsigned v0.18.0
  in_ps_refused 'could not fetch RELEASE-MANIFEST for v0.18.0'
  it "ps1 tampered asset"
  in_ps "$IN_PS64" "$IN/install.ps1" tampered v0.18.0
  in_ps_refused 'checksum mismatch for oam-'
  it "ps1 pre-cutoff: pin matches"
  in_ps "$IN_PS64" "$IN/install.ps1" pre v0.17.1
  in_ps_installed 'v0.17.1 matches its pinned digest'
  it "ps1 pre-cutoff: pin does not match"
  in_ps "$IN_PS64" "$IN/install.ps1" prebad v0.16.4
  in_ps_refused "v0.16.4's pinned digest is 0000"
  it "ps1 pre-cutoff: unknown tag"
  in_ps "$IN_PS64" "$IN/install.ps1" pre v0.17.2
  in_ps_refused 'v0.17.2 predates signed releases (v0.18.0) and is not in the pinned table'
  it "ps1 no ssh-keygen, v0.18.0+: refused, naming Add-WindowsCapability"
  in_ps "$IN_PS64" "$IN/install-nokg.ps1" good v0.18.0
  if grep -qF 'Add-WindowsCapability -Online -Name OpenSSH.Client~~~~0.0.1.0' <<<"$IN_OUT"; then
    in_ps_refused 'OAM_INSECURE_SKIP_SIGNATURE=1'
  else fail "rc=$IN_RC, the fix is not named: $IN_OUT"; fi
  it "ps1 no ssh-keygen, pre-cutoff: installs by its pin"
  in_ps "$IN_PS64" "$IN/install-nokg.ps1" pre v0.17.1
  in_ps_installed 'Not needed for v0.17.1 (verified by its pinned digest)'
  it "ps1 OAM_INSECURE_SKIP_SIGNATURE=1 installs without ssh-keygen, loudly"
  in_ps "$IN_PS64" "$IN/install-nokg.ps1" good v0.18.0 OAM_INSECURE_SKIP_SIGNATURE=1
  in_ps_installed 'installing WITHOUT signature verification'
  # The 8.1 shape for install.ps1: an ssh-keygen.cmd on PATH (the nokg copy
  # points the inbox candidates at nothing, so PATH is where it looks) that
  # answers the probe as 8.1 does and hands everything else to System32's
  # tool. Start-Process runs a .cmd through cmd.exe with the same redirected
  # handles.
  IN_KG81W="$IN/kg81w"; mkdir -p "$IN_KG81W"
  printf '@echo off\r\necho %%* | findstr /c:"oam-probe" >nul\r\nif errorlevel 1 goto real\r\necho %%* | findstr /c:"-n" >nul\r\nif errorlevel 1 goto nons\r\necho usage: ssh-keygen [-q] [-b bits] 1>&2\r\necho        ssh-keygen -Y verify -f allowed_signers_file -I signer_identity 1>&2\r\nexit /b 1\r\n:nons\r\necho Too few arguments for sign/verify: missing namespace 1>&2\r\nexit /b 1\r\n:real\r\n"%s" %%*\r\nexit /b %%errorlevel%%\r\n' \
    'C:\Windows\System32\OpenSSH\ssh-keygen.exe' >"$IN_KG81W/ssh-keygen.cmd"
  it "ps1: an OpenSSH 8.1-shaped ssh-keygen on PATH counts as present, and installs"
  in_ps "$IN_PS64" "$IN/install-nokg.ps1" good v0.18.0 "PATH=$IN_KG81W:$IN_WINPATH"
  if grep -qF 'ssh-keygen.cmd' <<<"$IN_OUT"; then in_ps_installed 'signature ok: v0.18.0, signed by oam-release-k1'
  else fail "did not verify with the 8.1-shaped ssh-keygen.cmd: $IN_OUT"; fi

  it "ps1 latest: the tag comes from the /releases/latest redirect (the local stand-in above)"
  if [ -n "${IN_PORT:-}" ]; then
    sed "s#https://github.com/#http://127.0.0.1:$IN_PORT/#g" "$IN/install.ps1" >"$IN/install-http.ps1"
    rm -rf "$IN/pdest"
    IN_RC=0
    in_ps_cp 65001
    IN_OUT="$(env -u GH_TOKEN -u GITHUB_TOKEN -u OAM_INSECURE_SKIP_SIGNATURE -u OAM_GH_API -u OAM_VERSION -u OAM_INSTALL_BASE \
      PATH="$IN_WINPATH" OAM_INSTALL_DIR="$(cygpath -w "$IN/pdest")" \
      "$IN_PS64" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(cygpath -w "$IN/install-http.ps1")" 2>&1 | tr -d '\r\n')" || IN_RC=$?
    in_ps_cp "$IN_CP_ORIG"
    in_ps_installed 'signature ok: v0.18.0, signed by oam-release-k1'
  else skip "no local HTTP stand-in (see the install.sh group)"; fi
fi
# The stand-in exits by itself once the temp root is gone; this just makes it
# prompt.
if [ -n "${IN_SRV_PID:-}" ]; then
  : >"$IN/srv.port.stop"
  kill "$IN_SRV_PID" 2>/dev/null
  wait "$IN_SRV_PID" 2>/dev/null
fi

# =============================================================================
group "mac-signing.sh -- the mac leg's signing decision, gate and hand-back"
# =============================================================================
# The mac release leg signs both binaries (hardened runtime + three
# entitlements), verifies the signature and runs a JIT smoke against the signed
# bytes, all between each binary's cp into dist/ and its smoke; the release box
# then checks the pulled bytes against the Air's own hashes.
#
# This box is Windows: there is no real codesign, security or Mac keychain
# here. So the lib's functions run against STUBS on PATH that log their argv
# and answer from fixture files with the shapes codesign prints on macOS
# (`-dv`'s Identifier/CodeDirectory/Signature/Authority lines, `-d -r-`'s
# designated requirement, `-d --entitlements - --xml`'s plist). What a real
# codesign does with these arguments is proven on the Air by the leg itself --
# its verify gate and JIT smoke fail the release, not this suite.
MS="$SUITE_TMP/macsign"
mkdir -p "$MS/bin" "$MS/kc"
MS_LOG="$MS/log"
MS_ENTS="$REPO_DIR/scripts/macos/oam.entitlements.plist"
# Two stand-in leaf certificates (DER bytes, as `codesign -d
# --extract-certificates` writes them) and the pins they hash to: the verify
# gate compares the extracted leaf's SHA-1 with the pin.
printf 'leaf certificate A' > "$MS/leaf-a"
printf 'leaf certificate B' > "$MS/leaf-b"
MS_PIN_A="$(sha1sum "$MS/leaf-a" | cut -c1-40)"
MS_PIN_B="$(sha1sum "$MS/leaf-b" | cut -c1-40)"
: > "$MS/kc/oam-codesign.keychain-db"
printf 'kcpw' > "$MS/kc/pw"
echo "probe source" > "$MS/true-src"
echo "binary" > "$MS/oam-bin"

# MS_LOG joins argv with spaces, which hides argv boundaries; MS_ARGV keeps
# them: one line per codesign call, each element as <element>. An unquoted
# expansion that word-split the -r= requirement would show there as many
# elements.
MS_ARGV="$MS/argv"
cat > "$MS/bin/codesign" <<EOF
#!/bin/bash
echo "codesign \$*" >> "$MS_LOG"
{ printf '<%s>' "\$@"; echo; } >> "$MS_ARGV"
case " \$* " in
  *" --force "*)
    if [ -f "$MS/cs-sign-fail" ]; then cat "$MS/cs-sign-fail" >&2; exit 1; fi
    exit 0 ;;
  *" --verify "*)
    if [ -f "$MS/cs-verify-fail" ]; then echo "$MS/oam-bin: invalid signature (code or signature have been modified)" >&2; exit 1; fi
    exit 0 ;;
  *" --entitlements - --xml "*) cat "$MS/cs-ents" ;;
  *" -r- "*) echo "Executable=/x/oam" >&2; cat "$MS/cs-dr" ;;
  *" -dv "*|*" -dvv "*) cat "$MS/cs-dv" >&2 ;;
  *" --extract-certificates="*)
    if [ -f "$MS/cs-extract-fail" ]; then cat "$MS/cs-extract-fail" >&2; exit 1; fi
    # An interrupt mid-extraction: TERM the shell that ran us.
    if [ -f "$MS/cs-extract-term" ]; then kill -TERM "\$PPID"; exit 0; fi
    # The leaf as <prefix>0 -- none for an ad-hoc signature (no cs-leaf).
    for a in "\$@"; do
      case "\$a" in --extract-certificates=*) [ ! -f "$MS/cs-leaf" ] || cp "$MS/cs-leaf" "\${a#*=}0" ;; esac
    done ;;
esac
exit 0
EOF
cat > "$MS/bin/security" <<EOF
#!/bin/bash
echo "security \$*" >> "$MS_LOG"
[ -f "$MS/sec-fail" ] && exit 51
exit 0
EOF
# The provision script, as the lib sees it: `bash <it> --check`.
cat > "$MS/provision" <<EOF
#!/bin/bash
echo "provision \$*" >> "$MS_LOG"
cat "$MS/prov-out"
exit "\$(cat "$MS/prov-rc")"
EOF
chmod +x "$MS/bin/codesign" "$MS/bin/security" "$MS/provision"

# ms_pin <line>...  -- the fixture pin file, comments and all.
ms_pin(){ { echo '# a comment that names deadbeef is not a pin'; printf '%s\n' "$@"; } > "$MS/pin"; }
# ms_dv <identifier> <flags> <signature-or-authority-lines>...  -- `codesign -dv`.
ms_dv(){
  local id="$1" flags="$2"; shift 2
  { echo "Executable=/x/oam"; echo "Identifier=$id"; echo "Format=Mach-O thin (arm64)"
    echo "CodeDirectory v=20500 size=1234 flags=$flags hashes=30+7 location=embedded"
    printf '%s\n' "$@"; echo "TeamIdentifier=not set"; } > "$MS/cs-dv"
}
# A codesign-shaped entitlements dump: one line, no comment, as --xml prints.
ms_ents(){
  { printf '<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict>'
    local k; for k in "$@"; do printf '<key>%s</key><true/>' "$k"; done
    printf '</dict></plist>'; } > "$MS/cs-ents"
}
MS_K1=com.apple.security.cs.allow-jit
MS_K2=com.apple.security.cs.allow-unsigned-executable-memory
MS_K3=com.apple.security.cs.disable-library-validation
ms_good_adhoc(){
  ms_dv org.oamjs.oam '0x10002(adhoc,runtime)' 'Signature=adhoc'
  ms_ents "$MS_K1" "$MS_K2" "$MS_K3"
  echo '# designated => cdhash H"8d3c3e0b1f0a4f0e9b9e1b6b0e0c8f5a2b7c1d00"' > "$MS/cs-dr"
  rm -f "$MS/cs-leaf"
}
ms_good_selfsigned(){  # ms_good_selfsigned <pin as the requirement prints it> -- leaf A signed
  ms_dv org.oamjs.oam '0x10000(runtime)' 'Signature size=1234' 'Authority=oam Code Signing (self-signed)' 'Signed Time=Oct 2, 2026 at 10:00:00 AM'
  ms_ents "$MS_K1" "$MS_K2" "$MS_K3"
  echo "designated => identifier \"org.oamjs.oam\" and certificate leaf = H\"$1\"" > "$MS/cs-dr"
  cp "$MS/leaf-a" "$MS/cs-leaf"
}
ms_good_devid(){  # a timestamped Developer ID signature by leaf A
  ms_dv org.oamjs.oam '0x10000(runtime)' 'Authority=Developer ID Application: Example LLC (ABCDE12345)' 'Authority=Developer ID Certification Authority' 'Timestamp=Oct 2, 2026 at 10:00:00 AM'
  ms_ents "$MS_K1" "$MS_K2" "$MS_K3"
  echo 'designated => identifier "org.oamjs.oam" and anchor apple generic and certificate leaf[subject.OU] = ABCDE12345' > "$MS/cs-dr"
  cp "$MS/leaf-a" "$MS/cs-leaf"
}
ms_reset(){
  rm -f "$MS/cs-sign-fail" "$MS/cs-verify-fail" "$MS/sec-fail" "$MS/cs-extract-fail" "$MS/cs-extract-term"
  : > "$MS_LOG"; : > "$MS_ARGV"
  printf 'keychain=%s\nsha1=%s\n' "$MS/kc/oam-codesign.keychain-db" "$MS_PIN_A" > "$MS/prov-out"
  echo 0 > "$MS/prov-rc"
  ms_pin
  ms_good_adhoc
}
# ms <cmd> [args...]  -- a fresh shell with the stubs first on PATH, the lib
# sourced against the fixtures, then <cmd>. stdout to MS_OUT, stderr to
# MS_ERR, status to MS_RC. A subshell, so no lib state leaks between tests.
ms(){
  # The redirect sits INSIDE the substitution: on a bare assignment it would
  # not reach the substitution's stderr.
  MS_OUT="$( {
    PATH="$MS/bin:$PATH"
    MAC_SIGNING_PIN_FILE="$MS/pin" MAC_PROVISION_SCRIPT="$MS/provision"
    MAC_ENTITLEMENTS="$MS_ENTS" MAC_PROBE_SOURCE="$MS/true-src" MAC_SIGN_PW_FILE="$MS/kc/pw"
    # shellcheck source=lib/mac-signing.sh
    . scripts/lib/mac-signing.sh
    "$@"
  } 2>"$MS/err" )"
  MS_RC=$?
  MS_ERR="$(cat "$MS/err")"
}
# Setup, then the step under test, in the same shell (MAC_SIGN_MODE is state).
ms_then(){ mac_signing_setup 2>/dev/null || return 90; : > "$MS_LOG"; : > "$MS_ARGV"; "$@"; }

ms_reset

# The suite-top scrub, proven against the exact leak it exists for: a release
# re-run with OAM_SKIP_MAC_SIGN=1 (and friends) exported runs this suite as its
# local gate. The first line is the control -- unscrubbed, the knob really does
# turn both verdicts into skip, so a scrub that stopped working cannot pass.
it "an operator's exported OAM_SKIP_MAC_SIGN=1 cannot reach the bootstrap or pin verdicts once scrubbed"
MS_GOT="$(
  export OAM_SKIP_MAC_SIGN=1 OAM_SKIP_MAC_X64=1 OAM_SIGN_REQUIRED=0 OAM_SIGNING_DIR="$MS/nowhere"
  ms_pin; ms mac_sign_decision; echo "leaked=${MS_OUT%%:*}"
  scrub_operator_knobs
  ms_pin; ms mac_sign_decision; echo "bootstrap=${MS_OUT%%:*}"
  ms_pin "$MS_PIN_A"; ms mac_sign_decision; echo "pin=$MS_OUT"
)"
eq "$MS_GOT" "leaked=skip"$'\n'"bootstrap=adhoc"$'\n'"pin=identity:$MS_PIN_A"
ms_reset

it "every signing, skip and mac-host knob a release script reads is scrubbed at the suite's top"
MS_MISS=""
for f in scripts/release-local.sh scripts/release-upload-local-arm64.sh scripts/build-platforms-tailnet.sh \
         scripts/build-remote.sh scripts/provision-mac-signing.sh scripts/lib/signing.sh scripts/lib/mac-signing.sh \
         install/install.sh; do
  for k in $(sed 's/#.*//' "$f" | grep -oE '[$][{]?OAM_[A-Z0-9_]+' | tr -d '${' | sort -u); do
    case "$k" in *SIGN*|*SKIP*|OAM_MAC_*) ;; *) continue ;; esac
    case " $(echo $OPERATOR_KNOBS) " in *" $k "*) ;; *) MS_MISS="$MS_MISS $f:$k" ;; esac
  done
done
MS_SET=""
for k in $OPERATOR_KNOBS; do [ -z "${!k+x}" ] || MS_SET="$MS_SET $k"; done
if [ -z "$MS_MISS$MS_SET" ]; then pass
else fail "not in OPERATOR_KNOBS:${MS_MISS:- none}; still set here:${MS_SET:- none}"; fi

it "bootstrap: a pin file holding only comments signs ad-hoc, with a warning"
ms mac_sign_decision
case "$MS_OUT" in adhoc:*"holds no SHA-1 yet"*) pass ;; *) fail "decision: $MS_OUT" ;; esac

it "bootstrap under OAM_SIGN_REQUIRED=1 is fatal, and the way out is an explicit 0"
OAM_SIGN_REQUIRED=1 ms mac_sign_decision
case "$MS_OUT" in fail:"OAM_SIGN_REQUIRED=1 but "*"set OAM_SIGN_REQUIRED=0"*) pass ;; *) fail "decision: $MS_OUT" ;; esac

it "a committed pin makes that identity mandatory, required or not"
ms_pin "$MS_PIN_A"
MS_GOT="$(ms mac_sign_decision; echo "$MS_OUT")|$(OAM_SIGN_REQUIRED=1 ms mac_sign_decision; echo "$MS_OUT")"
eq "$MS_GOT" "identity:$MS_PIN_A|identity:$MS_PIN_A"

it "a pin is read case- and separator-insensitively"
ms_pin "$(sed 's/../&:/g; s/:$//' <<<"$MS_PIN_A" | tr 'a-f' 'A-F')"
ms mac_sign_decision
eq "$MS_OUT" "identity:$MS_PIN_A"

it "a truncated, doubled or non-hex pin is fatal, never read as no pin"
MS_BAD=""
for p in "${MS_PIN_A%?}" "$MS_PIN_A $MS_PIN_B" "${MS_PIN_A%?}g"; do
  ms_pin "$p"; ms mac_sign_decision
  case "$MS_OUT" in fail:*) ;; *) MS_BAD="$MS_BAD [$p -> $MS_OUT]" ;; esac
done
rm -f "$MS/pin"; ms mac_sign_decision
case "$MS_OUT" in fail:*"no pin file"*) ;; *) MS_BAD="$MS_BAD [missing file -> $MS_OUT]" ;; esac
if [ -z "$MS_BAD" ]; then pass; else fail "accepted:$MS_BAD"; fi

it "OAM_SKIP_MAC_SIGN=1 is honored even under OAM_SIGN_REQUIRED=1, and says so"
ms_pin "$MS_PIN_A"
OAM_SKIP_MAC_SIGN=1 OAM_SIGN_REQUIRED=1 ms mac_sign_decision
case "$MS_OUT" in skip:*"even though OAM_SIGN_REQUIRED=1"*) pass ;; *) fail "decision: $MS_OUT" ;; esac

it "a knob that is not 0 or 1 is fatal"
MS_GOT="$(OAM_SIGN_REQUIRED=yes ms mac_sign_decision; echo "$MS_OUT")|$(OAM_SKIP_MAC_SIGN=true ms mac_sign_decision; echo "$MS_OUT")"
case "$MS_GOT" in "fail:OAM_SIGN_REQUIRED must be 0 or 1, not yes|fail:OAM_SKIP_MAC_SIGN must be 0 or 1, not true") pass ;; *) fail "got: $MS_GOT" ;; esac

it "the committed pin file parses (empty while bootstrapping, or one SHA-1)"
MS_COMMITTED="$( MAC_SIGNING_PIN_FILE=scripts/mac-signing-identity.sha1; . scripts/lib/mac-signing.sh; mac_sign_decision )"
case "$MS_COMMITTED" in adhoc:*|identity:*) pass ;; *) fail "scripts/mac-signing-identity.sha1 -> $MS_COMMITTED" ;; esac

# --- setup: the pinned identity, proven before the build -----------------------
ms_reset; ms_pin "$MS_PIN_A"
echo 1 > "$MS/prov-rc"; : > "$MS/prov-out"
ms mac_signing_setup
it "a pinned identity whose --check fails is fatal, with the keychain remediation"
if [ "$MS_RC" != "0" ] && grep -qF "the pinned signing identity $MS_PIN_A is not usable" <<<"$MS_ERR" \
   && grep -qF 'set-key-partition-list' <<<"$MS_ERR" && grep -qF 'OAM_SKIP_MAC_SIGN=1' <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC stderr: $MS_ERR"; fi

ms_reset; ms_pin "$MS_PIN_A"
printf 'keychain=%s\nsha1=%s\n' "$MS/kc/oam-codesign.keychain-db" "$MS_PIN_B" > "$MS/prov-out"
ms mac_signing_setup
it "a host whose certificate is not the pinned one is fatal"
if [ "$MS_RC" != "0" ] && grep -qF "this host's signing certificate is $MS_PIN_B but the repo pins $MS_PIN_A" <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC stderr: $MS_ERR"; fi

ms_reset; ms_pin "$MS_PIN_A"
echo 'oam-bin: errSecInternalComponent' > "$MS/cs-sign-fail"
ms mac_signing_setup
it "a pinned identity the probe signature cannot use is fatal (errSecInternalComponent -> remediation)"
if [ "$MS_RC" != "0" ] && grep -qF "the pinned identity $MS_PIN_A cannot sign from this session" <<<"$MS_ERR" \
   && grep -qF 'errSecInternalComponent' <<<"$MS_ERR" && grep -qF 'dedicated build keychain' <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC stderr: $MS_ERR"; fi

# --- signing --------------------------------------------------------------------
ms_reset
ms ms_then mac_sign_binary "$MS/oam-bin"
it "ad-hoc (bootstrap) signs with the hardened runtime, the entitlements and the identifier, and warns"
if [ "$MS_RC" = "0" ] \
   && grep -qxF "codesign --force --sign - --options runtime --timestamp=none --identifier org.oamjs.oam --entitlements $MS_ENTS $MS/oam-bin" "$MS_LOG" \
   && ! grep -q '^security' "$MS_LOG" && grep -qF 'signed AD-HOC' <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC log: $(cat "$MS_LOG") stderr: $MS_ERR"; fi

ms_reset; ms_pin "$MS_PIN_A"
ms ms_then mac_sign_binary "$MS/oam-bin"
# The real signature states the leaf-form designated requirement (-r=): left to
# derive one, codesign can pick `certificate root = H"..."` for a self-signed
# certificate, which the verify gate would then reject.
it "the pinned identity: unlock, a fresh probe, then the real signature, with --keychain, no timestamp and an explicit leaf requirement"
MS_U="$(grep -n '^security unlock-keychain -p kcpw ' "$MS_LOG" | head -1 | cut -d: -f1)"
MS_P="$(grep -nF "codesign --force --sign $MS_PIN_A --keychain $MS/kc/oam-codesign.keychain-db --options runtime --timestamp=none " "$MS_LOG" | grep '/probe$' | head -1 | cut -d: -f1)"
MS_S="$(grep -nxF "codesign --force --sign $MS_PIN_A --keychain $MS/kc/oam-codesign.keychain-db --options runtime --timestamp=none --identifier org.oamjs.oam -r=designated => identifier \"org.oamjs.oam\" and certificate leaf = H\"$MS_PIN_A\" --entitlements $MS_ENTS $MS/oam-bin" "$MS_LOG" | head -1 | cut -d: -f1)"
if [ "$MS_RC" = "0" ] && [ -n "$MS_U" ] && [ -n "$MS_P" ] && [ -n "$MS_S" ] && [ "$MS_U" -lt "$MS_P" ] && [ "$MS_P" -lt "$MS_S" ]; then pass
else fail "rc=$MS_RC unlock@${MS_U:-none} probe@${MS_P:-none} sign@${MS_S:-none} log: $(cat "$MS_LOG") stderr: $MS_ERR"; fi

# The joined log above cannot tell `-r=designated => ...` passed as one
# argument from the same words passed as ten. codesign needs ONE.
it "the pinned identity's -r= requirement reaches codesign as exactly one argv element"
MS_WANT_ARGV="<--force><--sign><$MS_PIN_A><--keychain><$MS/kc/oam-codesign.keychain-db><--options><runtime><--timestamp=none><--identifier><org.oamjs.oam><-r=designated => identifier \"org.oamjs.oam\" and certificate leaf = H\"$MS_PIN_A\"><--entitlements><$MS_ENTS><$MS/oam-bin>"
if grep -qxF -- "$MS_WANT_ARGV" "$MS_ARGV"; then pass
else fail "want: $MS_WANT_ARGV"$'\n'"argv log: $(cat "$MS_ARGV")"; fi

ms_reset; ms_pin "$MS_PIN_A"
ms mac_sign_binary "$MS/oam-bin"
it "signing before setup is refused"
if [ "$MS_RC" != "0" ] && grep -qF 'run mac_signing_setup first' <<<"$MS_ERR" && ! grep -q '^codesign' "$MS_LOG"; then pass
else fail "rc=$MS_RC log: $(cat "$MS_LOG") stderr: $MS_ERR"; fi

# A Developer ID certificate in the same slot gets the secure timestamp: the
# move to it is an identity swap, not an edit here.
ms_reset; ms_pin "$MS_PIN_A"
ms_dv org.oamjs.oam '0x10000(runtime)' 'Authority=Developer ID Application: Example LLC (ABCDE12345)' 'Authority=Developer ID Certification Authority' 'Authority=Apple Root CA'
ms ms_then mac_sign_binary "$MS/oam-bin"
it "a Developer ID identity signs with --timestamp and keeps codesign's derived requirement (no -r=)"
if [ "$MS_RC" = "0" ] && grep -qxF "codesign --force --sign $MS_PIN_A --keychain $MS/kc/oam-codesign.keychain-db --options runtime --timestamp --identifier org.oamjs.oam --entitlements $MS_ENTS $MS/oam-bin" "$MS_LOG" \
   && ! grep -qF -- ' -r=' "$MS_LOG"; then pass
else fail "rc=$MS_RC log: $(cat "$MS_LOG")"; fi

ms_reset; ms_pin "$MS_PIN_A"
OAM_SKIP_MAC_SIGN=1 OAM_SIGN_REQUIRED=1 ms ms_then mac_sign_binary "$MS/oam-bin"
it "OAM_SKIP_MAC_SIGN=1 signs nothing, asks nothing of the keychain, and warns"
if [ "$MS_RC" = "0" ] && [ ! -s "$MS_LOG" ] && grep -qF 'left as the linker signed it' <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC log: $(cat "$MS_LOG") stderr: $MS_ERR"; fi

# --- the verify gate ----------------------------------------------------------
ms_reset
ms ms_then mac_verify_binary "$MS/oam-bin"
it "verify: an ad-hoc signature with runtime + the three entitlements passes while no pin is committed"
if [ "$MS_RC" = "0" ] && grep -qF 'verifies, AD-HOC' <<<"$MS_ERR"; then pass; else fail "rc=$MS_RC stderr: $MS_ERR"; fi

ms_reset; ms_pin "$MS_PIN_A"
MS_UPPER="$(tr 'a-f' 'A-F' <<<"$MS_PIN_A")"
ms_good_selfsigned "$MS_UPPER"
ms ms_then mac_verify_binary "$MS/oam-bin"
it "verify: the pinned self-signed certificate in the designated requirement passes (hex case aside)"
if [ "$MS_RC" = "0" ]; then pass; else fail "rc=$MS_RC stderr: $MS_ERR"; fi

# Each case: one defect on an otherwise passing signature, and the line that
# must name it.
MS_BAD=""
ms_verify_rejects(){  # ms_verify_rejects <label> <expected stderr fragment>
  ms ms_then mac_verify_binary "$MS/oam-bin"
  if [ "$MS_RC" = "0" ] || ! grep -qF -- "$2" <<<"$MS_ERR"; then MS_BAD="$MS_BAD [$1: rc=$MS_RC $MS_ERR]"; fi
}
ms_reset; touch "$MS/cs-verify-fail"; ms_verify_rejects "broken signature" "does not verify"
ms_reset; ms_dv org.oamjs.oam '0x2(adhoc)' 'Signature=adhoc'; ms_verify_rejects "no runtime flag" "no hardened runtime flag"
ms_reset; ms_dv a.out-5555 '0x10002(adhoc,runtime)' 'Signature=adhoc'; ms_verify_rejects "linker identifier" "is not signed as org.oamjs.oam"
ms_reset; ms_ents "$MS_K1" "$MS_K2"; ms_verify_rejects "missing entitlement" "wrong entitlements"
ms_reset; ms_ents "$MS_K1" "$MS_K2" "$MS_K3" com.apple.security.get-task-allow; ms_verify_rejects "extra entitlement" "wrong entitlements"
ms_reset; ms_pin "$MS_PIN_A"; ms_verify_rejects "pinned, but ad-hoc" "is ad-hoc signed (cdhash requirement), but the repo pins $MS_PIN_A"
ms_reset; ms_pin "$MS_PIN_A"; ms_good_selfsigned "$MS_PIN_B"; ms_verify_rejects "another certificate" "does not name the pinned certificate $MS_PIN_A"
# The form codesign was measured to DERIVE for a self-signed certificate, and
# the reason -r= is stated: the pin as the ROOT is not the leaf rule. Leaf A
# really signed, so only the requirement check can reject this.
ms_reset; ms_pin "$MS_PIN_A"; ms_good_selfsigned "$MS_PIN_A"
echo "designated => identifier \"org.oamjs.oam\" and certificate root = H\"$MS_PIN_A\"" > "$MS/cs-dr"
ms_verify_rejects "the pin as certificate root" "does not name the pinned certificate $MS_PIN_A"
ms_reset; ms_dv org.oamjs.oam '0x10000(runtime)' 'Authority=oam Code Signing (self-signed)'; ms_verify_rejects "certificate, no pin" "should be signed ad-hoc"
ms_reset; ms_pin "$MS_PIN_A"; ms_good_devid
ms_dv org.oamjs.oam '0x10000(runtime)' 'Authority=Developer ID Application: Example LLC (ABCDE12345)' 'Authority=Developer ID Certification Authority'
ms_verify_rejects "Developer ID without a timestamp" "without a secure timestamp"
it "verify rejects each defect, by name"
if [ -z "$MS_BAD" ]; then pass; else fail "not rejected as expected:$MS_BAD"; fi

# The requirement is only a statement; the leaf certificate that actually
# signed is the proof. A Developer ID requirement names a team, not the pin.
MS_BAD=""
ms_reset; ms_pin "$MS_PIN_A"; ms_good_selfsigned "$MS_PIN_A"; cp "$MS/leaf-b" "$MS/cs-leaf"
ms_verify_rejects "self-signed, requirement names the pin, leaf B signed" "signed by certificate $MS_PIN_B, not the pinned $MS_PIN_A"
ms_reset; ms_pin "$MS_PIN_A"; ms_good_devid; cp "$MS/leaf-b" "$MS/cs-leaf"
ms_verify_rejects "Developer ID, another certificate of the team" "signed by certificate $MS_PIN_B, not the pinned $MS_PIN_A"
ms_reset; ms_pin "$MS_PIN_A"; ms_good_devid; rm -f "$MS/cs-leaf"
ms_verify_rejects "Developer ID, no certificate extracted" "signed by certificate <none extracted>, not the pinned $MS_PIN_A"
# When extraction itself fails, codesign's own words must reach the operator.
ms_reset; ms_pin "$MS_PIN_A"; ms_good_selfsigned "$MS_PIN_A"
echo "oam-bin: code object is not signed at all (fixture)" > "$MS/cs-extract-fail"
ms_verify_rejects "extraction failed, codesign's reason shown" "code object is not signed at all (fixture)"
ms_verify_rejects "extraction failed, its exit status shown" "codesign extracted no certificate from $MS/oam-bin (exit 1)"
ms_verify_rejects "extraction failed, still rejected" "signed by certificate <none extracted>, not the pinned $MS_PIN_A"
it "verify: with a pin, a leaf certificate that is not the pinned one is rejected on both paths"
if [ -z "$MS_BAD" ]; then pass; else fail "not rejected as expected:$MS_BAD"; fi

# mac_leaf_sha1 runs inside build-remote.sh and the mac probe, both of which
# own their traps: its temp dir must go on every path, its traps must stay its
# own.
MS_LT="$MS/leaf-tmp"
ms_leaf_probe(){ trap 'echo caller-int' INT; mac_leaf_sha1 "$MS/oam-bin"; echo "rc=$?"; trap -p INT; }
MS_BAD=""
rm -rf "$MS_LT"; mkdir -p "$MS_LT"
ms_reset; cp "$MS/leaf-a" "$MS/cs-leaf"
TMPDIR="$MS_LT" ms ms_leaf_probe
[ "$MS_OUT" = "$MS_PIN_A"$'\n'"rc=0"$'\n'"trap -- 'echo caller-int' SIGINT" ] || MS_BAD="$MS_BAD [success: out='$MS_OUT' err: $MS_ERR]"
[ -z "$(ls -A "$MS_LT")" ] || MS_BAD="$MS_BAD [success left: $(ls -A "$MS_LT")]"
ms_reset; touch "$MS/cs-extract-term"
TMPDIR="$MS_LT" ms ms_leaf_probe
[ "$MS_OUT" = "rc=130"$'\n'"trap -- 'echo caller-int' SIGINT" ] || MS_BAD="$MS_BAD [interrupted: out='$MS_OUT' err: $MS_ERR]"
[ -z "$(ls -A "$MS_LT")" ] || MS_BAD="$MS_BAD [interrupt left: $(ls -A "$MS_LT")]"
it "mac_leaf_sha1 removes its temp dir on success and on an interrupt, and leaves the caller's traps alone"
if [ -z "$MS_BAD" ]; then pass; else fail "$MS_BAD"; fi

ms_reset; ms_pin "$MS_PIN_A"; ms_good_devid
ms ms_then mac_verify_binary "$MS/oam-bin"
it "verify: a timestamped Developer ID signature by the pinned leaf passes without the self-signed requirement rule"
if [ "$MS_RC" = "0" ] && grep -qF "(leaf $MS_PIN_A)" <<<"$MS_ERR"; then pass; else fail "rc=$MS_RC stderr: $MS_ERR"; fi

ms_reset
OAM_SKIP_MAC_SIGN=1 ms ms_then mac_verify_binary "$MS/oam-bin"
it "verify under OAM_SKIP_MAC_SIGN=1 warns and passes without asking codesign"
if [ "$MS_RC" = "0" ] && [ ! -s "$MS_LOG" ] && grep -qF 'signature checks skipped' <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC log: $(cat "$MS_LOG") stderr: $MS_ERR"; fi

# --- the committed entitlements file ---------------------------------------------
# plutil is macOS-only, so the plist is read as XML: by the lib's own parser,
# and independently by line, since the file keeps one element per line.
it "the entitlements file holds exactly the three keys, each true"
MS_PAIRS="$( . scripts/lib/mac-signing.sh; mac_entitlement_pairs < scripts/macos/oam.entitlements.plist )"
MS_WANT="$MS_K1 true"$'\n'"$MS_K2 true"$'\n'"$MS_K3 true"
MS_LINES="$(grep -cE '^[[:space:]]*<key>' scripts/macos/oam.entitlements.plist)"
MS_TRUES="$(grep -cE '^[[:space:]]*<true/>[[:space:]]*$' scripts/macos/oam.entitlements.plist)"
if [ "$MS_PAIRS" = "$MS_WANT" ] && [ "$MS_LINES" = "3" ] && [ "$MS_TRUES" = "3" ]; then pass
else fail "pairs: '$MS_PAIRS' key lines=$MS_LINES true lines=$MS_TRUES"; fi

# codesign parses --entitlements with a stricter XML parser than plutil: a
# comment holding "--" (say, quoting `--options runtime`) is reported to fail
# the real signature while ad-hoc bootstrap signing on another day looked
# fine. Ban "--" and the markup characters inside every comment body, and any
# comment opener that is never closed.
it "the entitlements file's XML comments hold no '--', '<', '>' or '&'"
MS_CMT_BAD="$(tr '\r\n' '  ' < scripts/macos/oam.entitlements.plist | awk '{
  s = $0; n = 0
  while ((i = index(s, "<!--")) > 0) {
    rest = substr(s, i + 4); j = index(rest, "-->")
    if (j == 0) { print "unclosed comment"; exit }
    body = substr(rest, 1, j - 1); n++
    if (index(body, "--") > 0) print "comment " n " holds --"
    if (body ~ /[<>&]/) print "comment " n " holds < > or &"
    if (substr(body, length(body), 1) == "-") print "comment " n " ends in -"
    s = substr(rest, j + 3)
  }
}')"
if [ -z "$MS_CMT_BAD" ]; then pass; else fail "$MS_CMT_BAD"; fi

it "the entitlements parser ignores a key named inside an XML comment"
MS_PAIRS="$( . scripts/lib/mac-signing.sh; printf '<dict><!-- <key>com.apple.security.get-task-allow</key><true/> --><key>a</key><true/><key>b</key><string>x</string></dict>' | mac_entitlement_pairs )"
eq "$MS_PAIRS" "a true"$'\n'"b other"

# --- the JIT smoke fixture ------------------------------------------------------
# The gate compares its stdout to one exact line. Run under node here (no mac
# binary on this box): proves the fixture is valid, self-checking and prints
# exactly that line. The Air runs it under the signed oam.
it "jit-smoke.js runs to the one line the gate expects"
if command -v node >/dev/null 2>&1; then
  eq "$(node scripts/fixtures/jit-smoke.js 2>&1)" "jit smoke ok"
else
  skip "no node on this host"
fi

# --- hash hand-back -------------------------------------------------------------
MS_HB="$MS/handback"; mkdir -p "$MS_HB/art"
printf 'arm64 bytes' > "$MS_HB/art/oam-aarch64-apple-darwin"
printf 'x64 bytes' > "$MS_HB/art/oam-x86_64-apple-darwin"
( cd "$MS_HB/art" && sha256sum oam-aarch64-apple-darwin oam-x86_64-apple-darwin ) > "$MS_HB/good.txt"
ms_hb(){ ms mac_handback_check "$@"; }
ms_hb "$MS_HB/good.txt" "$MS_HB/art" oam-aarch64-apple-darwin oam-x86_64-apple-darwin
it "hand-back: pulled binaries that match the Air's hashes pass"
eq "$MS_RC" "0"

MS_BAD=""
sed 's/  oam-/ *oam-/' "$MS_HB/good.txt" > "$MS_HB/binmode.txt"
ms_hb "$MS_HB/binmode.txt" "$MS_HB/art" oam-aarch64-apple-darwin oam-x86_64-apple-darwin
[ "$MS_RC" = "0" ] || MS_BAD="$MS_BAD [binary-mode '*name' rejected: $MS_ERR]"
grep aarch64 "$MS_HB/good.txt" > "$MS_HB/one.txt"
ms_hb "$MS_HB/one.txt" "$MS_HB/art" oam-aarch64-apple-darwin oam-x86_64-apple-darwin
[ "$MS_RC" != "0" ] || MS_BAD="$MS_BAD [an entry missing passed]"
ms_hb "$MS_HB/good.txt" "$MS_HB/art" oam-aarch64-apple-darwin
[ "$MS_RC" != "0" ] || MS_BAD="$MS_BAD [an unexpected entry passed]"
: > "$MS_HB/empty.txt"
ms_hb "$MS_HB/empty.txt" "$MS_HB/art" oam-aarch64-apple-darwin
[ "$MS_RC" != "0" ] || MS_BAD="$MS_BAD [an empty hand-back passed]"
printf 'x64 bytez' > "$MS_HB/art/oam-x86_64-apple-darwin"
ms_hb "$MS_HB/good.txt" "$MS_HB/art" oam-aarch64-apple-darwin oam-x86_64-apple-darwin
if [ "$MS_RC" = "0" ] || ! grep -qF 'oam-x86_64-apple-darwin(mac=' <<<"$MS_ERR"; then MS_BAD="$MS_BAD [a changed byte passed or was not named: $MS_ERR]"; fi
it "hand-back: a changed byte, a missing or extra entry, or an empty file fails; '*name' is accepted"
if [ -z "$MS_BAD" ]; then pass; else fail "$MS_BAD"; fi

# --- placement: between cp and smoke, darwin only -------------------------------
# On the code (comments stripped by sg_line), per function: the bytes that
# ship are signed, then verified, then JIT-smoked, all before the plain smoke
# -- and on the shared build_host_release only inside the darwin case.
MS_FN="$MS/fn"
ms_fn(){ awk -v f="$1() {" '$0 == f { p = 1 } p { print } p && /^}$/ { exit }' scripts/build-remote.sh > "$MS_FN"; }

# "  smoke" with its indent: `jit_smoke "dist/..."` contains `smoke "dist/..."`.
it "build_host_release: cp < darwin guard < sign < verify < JIT smoke < smoke"
ms_fn build_host_release
sg_order "$MS_FN" 'cp "target/release/oam${ext}" "dist/oam-${triple}${ext}"' '*apple-darwin)' 'mac_signing_ready' \
  'mac_sign_binary "dist/oam-${triple}"' 'mac_verify_binary "dist/oam-${triple}"' 'jit_smoke "dist/oam-${triple}"' \
  '  smoke "dist/oam-${triple}${ext}"'

# The Linux leg shares build_host_release: every signing call sits inside the
# darwin arm of the case, and nowhere else in the function.
it "build_host_release: the signing calls are inside the *apple-darwin) arm only"
MS_ARM="$(awk '/^[[:space:]]*\*apple-darwin\)$/ { p = 1 } p { print } p && /^[[:space:]]*;;$/ { exit }' "$MS_FN")"
MS_ALL="$(grep -cE 'mac_signing_ready|mac_sign_binary|mac_verify_binary|jit_smoke' "$MS_FN")"
MS_IN="$(grep -cE 'mac_signing_ready|mac_sign_binary|mac_verify_binary|jit_smoke' <<<"$MS_ARM")"
if [ "$MS_IN" = "4" ] && [ "$MS_ALL" = "4" ]; then pass; else fail "in the darwin arm: $MS_IN of 4; in the function: $MS_ALL"$'\n'"$MS_ARM"; fi

it "build_mac_x64: cp < sign < verify < JIT smoke < smoke"
ms_fn build_mac_x64
sg_order "$MS_FN" 'cp "target/x64-host/x86_64-apple-darwin/release/oam" "dist/oam-x86_64-apple-darwin"' \
  'mac_sign_binary "dist/oam-x86_64-apple-darwin"' 'mac_verify_binary "dist/oam-x86_64-apple-darwin"' \
  'jit_smoke "dist/oam-x86_64-apple-darwin"' '  smoke "dist/oam-x86_64-apple-darwin"'

it "mac-release: drop a stale hand-back, prove signing first, write the hand-back last"
awk '/^  mac-release\)$/ { p = 1 } p { print } p && /^    ;;$/ { exit }' scripts/build-remote.sh > "$MS_FN"
sg_order "$MS_FN" 'rm -f dist/mac-sha256.txt' 'mac_signing_ready' 'remote_prep' 'build_host_release' 'build_mac_x64' 'write_mac_hashes'

# The real build-remote.sh `build` dispatch, end to end, against stubs: rustc
# names the triple, cargo "builds" a stand-in oam (a script that logs and
# answers), codesign logs. The order on the shared log is the order the leg
# ran things in. HOME is a fixture so the host's own ~/.cargo/env cannot put a
# real cargo ahead of the stub.
MS_BR="$MS/br"
mkdir -p "$MS_BR/scripts/lib" "$MS_BR/scripts/fixtures" "$MS_BR/scripts/macos" "$MS_BR/home" "$MS_BR/bin"
cp scripts/build-remote.sh "$MS_BR/scripts/"
cp scripts/lib/mac-signing.sh "$MS_BR/scripts/lib/"
cp scripts/fixtures/jit-smoke.js "$MS_BR/scripts/fixtures/"
cp scripts/macos/oam.entitlements.plist "$MS_BR/scripts/macos/"
cp "$MS/bin/codesign" "$MS/bin/security" "$MS_BR/bin/"
cat > "$MS_BR/oam-stub" <<EOF
#!/bin/bash
echo "oam \$*" >> "$MS_LOG"
# jit-mode, when present, makes the JIT smoke fail: "crash" dies the way a
# binary whose entitlements did not take does, "wrong" prints something else,
# "okcrash" prints the success line and THEN dies (a late JIT, or teardown).
case "\$2" in
  *jit-smoke.js)
    case "\$(cat "$MS/jit-mode" 2>/dev/null)" in
      crash) exit 133 ;;
      okcrash) echo "jit smoke ok"; exit 133 ;;
      wrong) echo "jit smoke ok?" ;;
      *) echo "jit smoke ok" ;;
    esac ;;
  *) echo "ci smoke 42" ;;
esac
EOF
cat > "$MS_BR/bin/cargo" <<EOF
#!/bin/bash
mkdir -p target/release && cp "$MS_BR/oam-stub" target/release/oam && chmod +x target/release/oam
EOF
chmod +x "$MS_BR/bin/cargo" "$MS_BR/oam-stub"
ms_br(){  # ms_br <triple> [env...] -- run the build dispatch; MS_RC, MS_ERR
  printf '#!/bin/bash\necho "host: %s"\n' "$1" > "$MS_BR/bin/rustc"; chmod +x "$MS_BR/bin/rustc"
  shift
  ms_reset
  ( cd "$MS_BR" && rm -rf dist target
    echo '# bootstrap: no pin' > scripts/mac-signing-identity.sha1
    env HOME="$MS_BR/home" PATH="$MS_BR/bin:$PATH" "$@" bash scripts/build-remote.sh build ) >"$MS/br-out" 2>&1
  MS_RC=$?
  MS_ERR="$(cat "$MS/br-out")"
}

ms_br aarch64-apple-darwin
it "build on a darwin host: signs, verifies and JIT-smokes the staged binary, then smokes it"
MS_SIGN="$(grep -n '^codesign --force --sign - .*dist/oam-aarch64-apple-darwin$' "$MS_LOG" | head -1 | cut -d: -f1)"
MS_VER="$(grep -n '^codesign --verify --strict' "$MS_LOG" | head -1 | cut -d: -f1)"
MS_JIT="$(grep -n '^oam run scripts/fixtures/jit-smoke.js$' "$MS_LOG" | head -1 | cut -d: -f1)"
MS_SMK="$(grep -n '^oam run .*smoke\.js$' "$MS_LOG" | grep -v jit-smoke | head -1 | cut -d: -f1)"
if [ "$MS_RC" = "0" ] && [ -n "$MS_SIGN" ] && [ -n "$MS_VER" ] && [ -n "$MS_JIT" ] && [ -n "$MS_SMK" ] \
   && [ "$MS_SIGN" -lt "$MS_VER" ] && [ "$MS_VER" -lt "$MS_JIT" ] && [ "$MS_JIT" -lt "$MS_SMK" ]; then pass
else fail "rc=$MS_RC sign@${MS_SIGN:-none} verify@${MS_VER:-none} jit@${MS_JIT:-none} smoke@${MS_SMK:-none} log: $(cat "$MS_LOG") out: $MS_ERR"; fi

ms_br x86_64-unknown-linux-gnu
it "build on a linux host: no codesign, no JIT smoke -- the Linux leg is untouched"
if [ "$MS_RC" = "0" ] && ! grep -q '^codesign' "$MS_LOG" && ! grep -q 'jit-smoke' "$MS_LOG" \
   && grep -q '^oam run .*smoke\.js$' "$MS_LOG"; then pass
else fail "rc=$MS_RC log: $(cat "$MS_LOG") out: $MS_ERR"; fi

ms_br aarch64-apple-darwin OAM_SIGN_REQUIRED=1
it "build on a darwin host under OAM_SIGN_REQUIRED=1 with no pin fails, and nothing smokes"
if [ "$MS_RC" != "0" ] && grep -qF 'OAM_SIGN_REQUIRED=1 but' <<<"$MS_ERR" && ! grep -q '^oam ' "$MS_LOG"; then pass
else fail "rc=$MS_RC log: $(cat "$MS_LOG") out: $MS_ERR"; fi

# A failing JIT smoke is a hard stop: the signed binary is never smoked, staged
# for the hand-back or handed on. Every way it fails on a real Mac: a crash at
# the first JIT, output that is not the one exact line, and the right line
# followed by a non-zero exit -- the exit status is checked, not just stdout.
MS_BAD=""
for MS_MODE in crash wrong okcrash; do
  echo "$MS_MODE" > "$MS/jit-mode"
  case "$MS_MODE" in wrong) MS_WHY='JIT smoke output unexpected' ;; *) MS_WHY='JIT smoke failed' ;; esac
  ms_br aarch64-apple-darwin
  if [ "$MS_RC" = "0" ] || ! grep -qF "$MS_WHY" <<<"$MS_ERR" \
     || ! grep -q '^oam run scripts/fixtures/jit-smoke.js$' "$MS_LOG" \
     || grep -v jit-smoke "$MS_LOG" | grep -q '^oam run .*smoke\.js$' \
     || [ -e "$MS_BR/dist/mac-sha256.txt" ]; then
    MS_BAD="$MS_BAD [$MS_MODE: rc=$MS_RC log: $(cat "$MS_LOG") out: $MS_ERR]"
  fi
done
rm -f "$MS/jit-mode"
it "build on a darwin host: a JIT smoke that crashes, answers wrong, or answers right then crashes fails the leg before the smoke"
if [ -z "$MS_BAD" ]; then pass; else fail "$MS_BAD"; fi

# --- the release box side ---------------------------------------------------------
it "the mac-release ssh line forwards OAM_SKIP_MAC_X64, OAM_SIGN_REQUIRED and OAM_SKIP_MAC_SIGN"
MS_SSH="$(grep -v '^[[:space:]]*#' scripts/build-platforms-tailnet.sh | grep -F 'bash scripts/build-remote.sh mac-release')"
MS_MISS=""
for want in 'OAM_SKIP_MAC_X64=$SKIP_MAC_X64' 'OAM_SIGN_REQUIRED=$SIGN_REQUIRED' 'OAM_SKIP_MAC_SIGN=$SKIP_MAC_SIGN'; do
  grep -qF -- "$want" <<<"$MS_SSH" || MS_MISS="$MS_MISS $want"
done
if [ -z "$MS_MISS" ] && [ "$(wc -l <<<"$MS_SSH" | tr -d ' ')" = "1" ]; then pass; else fail "missing:$MS_MISS line: $MS_SSH"; fi

it "the hand-back is pulled beside the artifacts, then checked before the artifact dir is handed out"
sg_order scripts/build-platforms-tailnet.sh 'HANDBACK_DIR="$STAGE_DIR/handback"' \
  'pull "$hp" "dist/mac-sha256.txt" "$HANDBACK_DIR/"' 'mac_handback_check "$HANDBACK_DIR/mac-sha256.txt" "$ARTIFACTS_DIR"' \
  'echo "$ARTIFACTS_DIR"'

it "release-local.sh copies the two exact mac asset names, never a glob"
MS_RL="$(grep -v '^[[:space:]]*#' scripts/release-local.sh)"
if grep -qF 'cp "$MAC_ART/oam-aarch64-apple-darwin" "$RELEASE_DIR/"' <<<"$MS_RL" \
   && grep -qF 'cp "$MAC_ART/oam-x86_64-apple-darwin" "$RELEASE_DIR/"' <<<"$MS_RL" \
   && ! grep -qF 'apple-darwin*' <<<"$MS_RL"; then pass
else fail "$(grep -n 'MAC_ART' scripts/release-local.sh)"; fi

# The preflight, run for real from a fixture checkout (its own pin file) with
# ssh replaced: the host probe (`true`) succeeds, and `bash -s -- --check`
# answers from fixture files and keeps the stdin it was handed, which must be
# the checkout's provision script. scp and mktemp record and fail, as above:
# a preflight that went on to stage or sync is caught.
MS_TN="$MS/tn"
mkdir -p "$MS_TN/scripts/lib" "$MS_TN/bin" "$MS_TN/tmp"
cp scripts/build-platforms-tailnet.sh scripts/provision-mac-signing.sh "$MS_TN/scripts/"
cp scripts/lib/src-sync.sh scripts/lib/iap-helpers.sh scripts/lib/tailnet-helpers.sh scripts/lib/mac-signing.sh "$MS_TN/scripts/lib/"
: > "$MS_TN/key"
cat > "$MS_TN/bin/ssh" <<EOF
#!/bin/bash
case " \$* " in
  *" --check "*)
    echo check >> "$MS_TN/calls"
    cat > "$MS_TN/check-stdin"
    cat "$MS_TN/check-out"
    exit "\$(cat "$MS_TN/check-rc")" ;;
  *) echo probe >> "$MS_TN/calls"; exit 0 ;;
esac
EOF
for t in scp mktemp; do printf '#!/bin/bash\necho %s >> "%s/calls"\nexit 98\n' "$t" "$MS_TN" > "$MS_TN/bin/$t"; done
chmod +x "$MS_TN/bin/ssh" "$MS_TN/bin/scp" "$MS_TN/bin/mktemp"
ms_tn(){  # ms_tn <pin-line> [env...] -- --preflight-only; MS_OUT, MS_ERR, MS_RC, MS_CALLS
  { echo '# fixture'; [ -z "$1" ] || echo "$1"; } > "$MS_TN/scripts/mac-signing-identity.sha1"
  shift
  : > "$MS_TN/calls"
  MS_OUT="$(env PATH="$MS_TN/bin:$PATH" TMPDIR="$MS_TN/tmp" OAM_MAC_KEY="$MS_TN/key" \
    OAM_MAC_HOST=100.90.0.5 OAM_MAC_USER=builder "$@" \
    bash "$MS_TN/scripts/build-platforms-tailnet.sh" --preflight-only 2>"$MS/tn-err")"
  MS_RC=$?
  MS_ERR="$(cat "$MS/tn-err")"
  MS_CALLS="$(tr '\n' ' ' < "$MS_TN/calls")"
}
printf 'keychain=/k\nsha1=%s\n' "$MS_PIN_A" > "$MS_TN/check-out"; echo 0 > "$MS_TN/check-rc"

ms_tn "" OAM_SIGN_REQUIRED=1
it "preflight: OAM_SIGN_REQUIRED=1 with no pin fails before touching the Air"
if [ "$MS_RC" != "0" ] && [ -z "$MS_OUT" ] && [ -z "$MS_CALLS" ] && grep -qF 'holds no SHA-1 yet' <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC out='$MS_OUT' calls='$MS_CALLS' stderr: $MS_ERR"; fi

ms_tn "" OAM_SKIP_MAC_SIGN=maybe
it "preflight: a signing knob that is not 0 or 1 fails before touching the Air"
if [ "$MS_RC" != "0" ] && [ -z "$MS_CALLS" ] && grep -qF "OAM_SKIP_MAC_SIGN must be 0 or 1, not 'maybe'" <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC calls='$MS_CALLS' stderr: $MS_ERR"; fi

# OAM_SKIP_MAC_X64 is checked by nothing else before it reaches the Air's
# shell on the mac-release ssh line, so this loop is its only guard.
ms_tn "" OAM_SKIP_MAC_X64=yes
it "preflight: an OAM_SKIP_MAC_X64 that is not 0 or 1 fails before touching the Air"
if [ "$MS_RC" != "0" ] && [ -z "$MS_CALLS" ] && grep -qF "OAM_SKIP_MAC_X64 must be 0 or 1" <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC calls='$MS_CALLS' stderr: $MS_ERR"; fi

ms_tn ""
it "preflight: bootstrap warns ad-hoc and asks the Air nothing about signing"
if [ "$MS_RC" = "0" ] && [ -z "$MS_OUT" ] && [ "$MS_CALLS" = "probe " ] && grep -qF 'signed AD-HOC' <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC out='$MS_OUT' calls='$MS_CALLS' stderr: $MS_ERR"; fi

ms_tn "$MS_PIN_A"
it "preflight: a pin runs this checkout's provision --check on the Air, stdout stays silent"
if [ "$MS_RC" = "0" ] && [ -z "$MS_OUT" ] && [ "$MS_CALLS" = "probe check " ] \
   && cmp -s "$MS_TN/check-stdin" scripts/provision-mac-signing.sh \
   && grep -qF "mac signing identity $MS_PIN_A is usable" <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC out='$MS_OUT' calls='$MS_CALLS' stderr: $MS_ERR"; fi

# The same leak through the leg's own preflight, which inherits the caller's
# environment (env, not env -i): exported and then scrubbed, the knobs must not
# stop a committed pin from being checked on the Air.
it "preflight: exported skip knobs, once scrubbed, still leave a pin's identity check in place"
MS_GOT="$(
  export OAM_SKIP_MAC_SIGN=1 OAM_SKIP_MAC_X64=1
  scrub_operator_knobs
  ms_tn "$MS_PIN_A"; echo "rc=$MS_RC calls=$MS_CALLS"
)"
eq "$MS_GOT" "rc=0 calls=probe check "

printf 'keychain=/k\nsha1=%s\n' "$MS_PIN_B" > "$MS_TN/check-out"
ms_tn "$MS_PIN_A"
it "preflight: an Air holding another certificate fails"
if [ "$MS_RC" != "0" ] && [ -z "$MS_OUT" ] && grep -qF "the Air signs with certificate '$MS_PIN_B' but this checkout pins $MS_PIN_A" <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC out='$MS_OUT' stderr: $MS_ERR"; fi

echo 1 > "$MS_TN/check-rc"
ms_tn "$MS_PIN_A"
it "preflight: an identity --check rejects fails the preflight"
if [ "$MS_RC" != "0" ] && [ -z "$MS_OUT" ] && grep -qF "the pinned mac signing identity $MS_PIN_A is not usable" <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC out='$MS_OUT' stderr: $MS_ERR"; fi

ms_tn "$MS_PIN_A" OAM_SKIP_MAC_SIGN=1 OAM_SIGN_REQUIRED=1
it "preflight: OAM_SKIP_MAC_SIGN=1 skips the identity check even under OAM_SIGN_REQUIRED=1, loudly"
if [ "$MS_RC" = "0" ] && [ "$MS_CALLS" = "probe " ] && grep -qF 'even though OAM_SIGN_REQUIRED=1' <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC calls='$MS_CALLS' stderr: $MS_ERR"; fi

# --- provision-mac-signing.sh --check, piped the way the preflight pipes it -----
# uname says Darwin; security and codesign are stubs, and the security stub
# DRAINS its stdin, as a command that prompted would. Piped to `bash -s`, a
# script whose top level ran commands could lose its own remainder that way;
# this one is functions plus a final `main "$@"`, so it is parsed whole first.
MS_PV="$MS/pv"
mkdir -p "$MS_PV/bin" "$MS_PV/home/.oam-signing"
printf '#!/bin/bash\necho Darwin\n' > "$MS_PV/bin/uname"
cat > "$MS_PV/bin/security" <<EOF
#!/bin/bash
cat > /dev/null
case "\$1" in
  # kc-sha lists the keychain's identities, uppercase, as security prints
  # them. fi-shape picks the listing:
  #   trusted    (default) each in BOTH sections, no suffix: a Developer ID
  #   selfsigned the real self-signed shape -- a (CSSMERR_TP_NOT_TRUSTED)
  #              suffix, under "Matching" only, 0 valid identities
  #   cutshort   one identity printed, then a non-zero exit
  find-identity)
    ids="\$(tr 'a-f' 'A-F' < "$MS_PV/kc-sha")"
    shape="\$(cat "$MS_PV/fi-shape" 2>/dev/null)"
    printf 'Policy: Code Signing\n  Matching identities\n'
    if [ "\$shape" = cutshort ]; then
      printf '  1) %s "oam Code Signing (self-signed)"\n' "\${ids%% *}"
      echo 'security: SecKeychainSearchCopyNext: The specified keychain could not be found.' >&2
      exit 1
    fi
    suffix=""; [ "\$shape" = selfsigned ] && suffix=' (CSSMERR_TP_NOT_TRUSTED)'
    i=0; for h in \$ids; do i=\$((i + 1)); printf '  %s) %s "oam Code Signing (self-signed)"%s\n' "\$i" "\$h" "\$suffix"; done
    printf '     %s identities found\n\n  Valid identities only\n' "\$i"
    if [ "\$shape" = selfsigned ]; then
      printf '     0 valid identities found\n'
    else
      i=0; for h in \$ids; do i=\$((i + 1)); printf '  %s) %s "oam Code Signing (self-signed)"\n' "\$i" "\$h"; done
      printf '     %s valid identities found\n' "\$i"
    fi ;;
  # A p12 with its chain: the intermediate CA lists FIRST. Nothing may take
  # the identity's fingerprint from here.
  find-certificate) printf 'keychain: "x"\nSHA-1 hash: 1111111111111111111111111111111111111111\n'
    for h in \$(cat "$MS_PV/kc-sha"); do printf 'SHA-1 hash: %s\n' "\$h"; done ;;
esac
exit 0
EOF
printf '#!/bin/bash\ncat > /dev/null\nexit 0\n' > "$MS_PV/bin/codesign"
chmod +x "$MS_PV/bin/uname" "$MS_PV/bin/security" "$MS_PV/bin/codesign"
: > "$MS_PV/home/.oam-signing/oam-codesign.keychain-db"
printf 'pw' > "$MS_PV/home/.oam-signing/oam-codesign.keychain-password"
printf '%s\n' "$MS_PIN_A" > "$MS_PV/home/.oam-signing/oam-codesign.sha1"
ms_pv(){  # ms_pv <args...> -- the provision script on stdin; MS_OUT, MS_RC, MS_ERR
  MS_OUT="$(env HOME="$MS_PV/home" PATH="$MS_PV/bin:$PATH" bash -s -- "$@" < scripts/provision-mac-signing.sh 2>"$MS/pv-err")"
  MS_RC=$?
  MS_ERR="$(cat "$MS/pv-err")"
}
printf '%s' "$MS_PIN_A" > "$MS_PV/kc-sha"
ms_pv --check
it "provision --check over stdin: survives a stdin-draining tool, prints keychain= and the lowercase sha1="
eq "rc=$MS_RC $MS_OUT" "rc=0 keychain=$MS_PV/home/.oam-signing/oam-codesign.keychain-db"$'\n'"sha1=$MS_PIN_A"

# What `security find-identity -p codesigning` really prints for oam's
# self-signed identity: untrusted, so only under "Matching", with a trust
# error after the name, and "0 valid identities found".
echo selfsigned > "$MS_PV/fi-shape"
ms_pv --check
it "provision --check: reads the identity from the real self-signed listing (trust-error suffix, 0 valid)"
eq "rc=$MS_RC $MS_OUT" "rc=0 keychain=$MS_PV/home/.oam-signing/oam-codesign.keychain-db"$'\n'"sha1=$MS_PIN_A"

# A listing that printed the one identity and then failed is not a listing.
echo cutshort > "$MS_PV/fi-shape"
ms_pv --check
it "provision --check: a find-identity that exits non-zero fails, even with one identity printed"
if [ "$MS_RC" != "0" ] && [ -z "$MS_OUT" ] && grep -qF "security find-identity -p codesigning" <<<"$MS_ERR" \
   && grep -qF 'SecKeychainSearchCopyNext' <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC out='$MS_OUT' stderr: $MS_ERR"; fi
rm -f "$MS_PV/fi-shape"

printf '%s' "$MS_PIN_B" > "$MS_PV/kc-sha"
ms_pv --check
it "provision --check: a keychain certificate that is not the one recorded fails"
if [ "$MS_RC" != "0" ] && [ -z "$MS_OUT" ] && grep -qF "keychain identity $MS_PIN_B does not match recorded fingerprint $MS_PIN_A" <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC out='$MS_OUT' stderr: $MS_ERR"; fi

# Two signing identities in the dedicated keychain: which one is "the" identity
# is ambiguous, so --check refuses rather than picking the first listed.
printf '%s %s' "$MS_PIN_B" "$MS_PIN_A" > "$MS_PV/kc-sha"
ms_pv --check
it "provision --check: a keychain holding two signing identities fails, naming both"
if [ "$MS_RC" != "0" ] && [ -z "$MS_OUT" ] && grep -qF "holds 2 code-signing identities; exactly one is expected" <<<"$MS_ERR" \
   && grep -qF "$MS_PIN_A" <<<"$MS_ERR" && grep -qF "$MS_PIN_B" <<<"$MS_ERR"; then pass
else fail "rc=$MS_RC out='$MS_OUT' stderr: $MS_ERR"; fi

ms_pv --generate
it "provision --generate refuses an existing identity and deletes nothing"
if [ "$MS_RC" != "0" ] && grep -qF 'an identity (or part of one) already exists' <<<"$MS_ERR" \
   && [ -f "$MS_PV/home/.oam-signing/oam-codesign.keychain-db" ] && [ -f "$MS_PV/home/.oam-signing/oam-codesign.keychain-password" ]; then pass
else fail "rc=$MS_RC stderr: $MS_ERR"; fi

# The failure trap deletes all five identity files, so any ONE left over from
# an earlier attempt must stop --generate / --import before the trap is armed:
# otherwise a later failure would delete a file this run never created.
MS_BAD=""
for MS_STALE in oam-codesign.keychain-password oam-codesign.sha1 oam-codesign.p12-password; do
  rm -rf "$MS_PV/stale"; mkdir -p "$MS_PV/stale"
  printf 'stale' > "$MS_PV/stale/$MS_STALE"
  OAM_SIGNING_DIR="$MS_PV/stale" ms_pv --generate
  if [ "$MS_RC" = "0" ] || ! grep -qF "already exists in $MS_PV/stale: $MS_PV/stale/$MS_STALE" <<<"$MS_ERR" \
     || [ "$(cat "$MS_PV/stale/$MS_STALE" 2>/dev/null)" != "stale" ] || [ "$(ls -A "$MS_PV/stale")" != "$MS_STALE" ]; then
    MS_BAD="$MS_BAD [$MS_STALE: rc=$MS_RC left: $(ls -A "$MS_PV/stale" | tr '\n' ' ') stderr: $MS_ERR]"
  fi
done
rm -rf "$MS_PV/stale"
it "provision --generate refuses a lone stale password or fingerprint file and leaves it untouched"
if [ -z "$MS_BAD" ]; then pass; else fail "$MS_BAD"; fi

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

  it "an early abort does not crash inside the EXIT trap"
  # The trap is armed at bump-taps.sh:163, but BREW_FILE and SCOOP_FILE are
  # assigned only at :331 and :371 -- so under `set -u` every failure in that
  # window (no published SHA256SUMS, this downgrade guard, a missing asset
  # hash) died with "BREW_FILE: unbound variable" INSIDE cleanup, replacing the
  # diagnosis the operator needs with a bash error about the script's own
  # bookkeeping. The case above cannot catch it: `fail` prints its message
  # BEFORE the trap runs, so grepping for that message passes either way.
  OUT="$(run_taps v0.13.2 2>&1)"
  if printf '%s' "$OUT" | grep -q "unbound variable"; then
    fail "the EXIT trap crashed on an early abort: $OUT"
  elif printf '%s' "$OUT" | grep -q "older than the latest published release"; then pass
  else fail "the early abort did not report its real reason: $OUT"; fi

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
group "node-pin.sh -- the pinned Node oracle"
# =============================================================================
# The mac and linux legs compared oam against v22.23.1 for their whole lives
# while every receipt claimed parity with v22.22.2, because each leg used
# whatever `node` its PATH found and nothing checked. These drive the real
# provisioning path end to end -- a planted release served over file:// with
# real curl, tar and sha256 -- so the only thing not exercised is nodejs.org.
# shellcheck source=lib/node-pin.sh
. scripts/lib/node-pin.sh

# The parse is duplicated in xtask/src/node_pin.rs (parse_pin) and
# gen-surface-gaps.mjs; these are the same cases xtask's unit tests pin.
it "the pin parse accepts the spellings version managers accept"
NP_BAD=""
for np_in in "22.22.2" "v22.22.2" "  22.22.2"$'\r\n' "22.22.2"$'\n'; do
  [ "$(node_pin_parse "$np_in")" = "22.22.2" ] || NP_BAD="$NP_BAD $(printf '%q' "$np_in")"
done
if [ -z "$NP_BAD" ]; then pass; else fail "not parsed to 22.22.2:$NP_BAD"; fi

it "the pin parse refuses anything but one exact MAJOR.MINOR.PATCH"
NP_BAD=""
for np_in in "" "22" "22.22" "22.22.2.1" "lts/jod" "22.x" "^22.22.2" "vv22.22.2" \
             "22..2" "22.22.2"$'\n'"22.23.1" "22.22.2 # comment" "22.22. 2" "22.22.-2"; do
  node_pin_parse "$np_in" >/dev/null 2>&1 && NP_BAD="$NP_BAD $(printf '%q' "$np_in")"
done
if [ -z "$NP_BAD" ]; then pass; else fail "accepted:$NP_BAD"; fi

# The corpus is a snapshot of one Node tag, and the pin names the oracle. xtask
# node-suite refuses to run when these disagree; this catches it on a box that
# never runs the node-suite.
it "the committed .node-version matches the vendored corpus's nodeVersion"
NP_PIN="$(node_pin_read .)"
NP_CORPUS="$(grep -o '"nodeVersion"[[:space:]]*:[[:space:]]*"[^"]*"' conformance/vendor/node/manifest.json | head -1 | grep -o 'v[0-9.]*')"
eq "v$NP_PIN" "$NP_CORPUS"

it "a missing pin file is refused with the reason, not read as empty"
mkdir -p "$SUITE_TMP/np-nopin"
NP_OUT="$(node_pin_read "$SUITE_TMP/np-nopin" 2>&1)"; NP_RC=$?
if [ "$NP_RC" != "0" ] && grep -q 'is missing' <<<"$NP_OUT"; then pass; else fail "rc=$NP_RC out=$NP_OUT"; fi

# verdict: "<echoed verdict> <return code>" for each input.
np_verdict(){ local v rc; v="$(node_pin_verdict "$@")"; rc=$?; echo "$v $rc"; }
it "the verdict passes exactly the pinned version"
eq "$(np_verdict 22.22.2 v22.22.2 0)" "pinned 0"
it "the verdict refuses another version, including a prefix-sibling"
eq "$(np_verdict 22.22.2 v22.23.1 0) / $(np_verdict 22.22.2 v22.22.20 0) / $(np_verdict 22.22.2 22.22.2 0)" \
   "mismatch 1 / mismatch 1 / mismatch 1"
it "the verdict refuses an absent node"
eq "$(np_verdict 22.22.2 "" 0)" "absent 1"
it "only a literal 1 opens the escape hatch"
eq "$(np_verdict 22.22.2 v22.23.1 1) / $(np_verdict 22.22.2 "" 1) / $(np_verdict 22.22.2 v22.23.1 true) / $(np_verdict 22.22.2 v22.23.1 "")" \
   "mismatch-allowed 0 / absent-allowed 0 / mismatch 1 / mismatch 1"

it "tarball platforms map from uname, and a host without one is refused"
eq "$(node_dist_platform Darwin arm64) $(node_dist_platform Linux x86_64) $(node_dist_platform Linux aarch64) $(node_dist_platform Darwin x86_64) $(node_dist_platform MINGW64_NT-10.0-26200 aarch64 || echo refused) $(node_dist_platform Linux ppc64le || echo refused)" \
   "darwin-arm64 linux-x64 linux-arm64 darwin-x64 refused refused"

NP_SUMS="$SUITE_TMP/np-SHASUMS256.txt"
NP_H1="1111111111111111111111111111111111111111111111111111111111111111"
NP_H2="2222222222222222222222222222222222222222222222222222222222222222"
# The longer names come FIRST: a prefix or substring match would stop on them
# before ever reaching the real line (a mutation with the lines the other way
# round sailed through).
printf '%s  node-v9.8.7-linux-x64.tar.gz.sig\n%s  node-v9.8.7-linux-x64.tar.xz\n%s  node-v9.8.7-linux-x64.tar.gz\nnothex  node-v9.8.7-darwin-arm64.tar.gz\n' \
  "$NP_H1" "$NP_H1" "$NP_H2" > "$NP_SUMS"
it "the checksum lookup matches the whole file name, not a prefix-sibling"
eq "$(node_shasum_for "$NP_SUMS" node-v9.8.7-linux-x64.tar.gz)" "$NP_H2"
it "the checksum lookup refuses an unlisted name and a malformed digest"
eq "$(node_shasum_for "$NP_SUMS" node-v9.8.7-linux-arm64.tar.gz || echo refused) $(node_shasum_for "$NP_SUMS" node-v9.8.7-darwin-arm64.tar.gz || echo refused)" \
   "refused refused"

# A planted release: node-v9.8.7-linux-x64.tar.gz whose bin/node is a shell
# script reporting its version, so it runs on every host this suite does
# (including the Windows box, which has no nodejs.org tarball of its own).
NP_DIST="$SUITE_TMP/np-dist"
NP_CACHE="$SUITE_TMP/np-cache"
np_stage="$SUITE_TMP/np-stage/node-v9.8.7-linux-x64/bin"
mkdir -p "$np_stage" "$NP_DIST/v9.8.7"
printf '#!/bin/sh\necho v9.8.7\n' > "$np_stage/node"
chmod +x "$np_stage/node"
tar -czf "$NP_DIST/v9.8.7/node-v9.8.7-linux-x64.tar.gz" -C "$SUITE_TMP/np-stage" node-v9.8.7-linux-x64
# The same bytes under the arm64 name, listed with a WRONG digest below.
cp "$NP_DIST/v9.8.7/node-v9.8.7-linux-x64.tar.gz" "$NP_DIST/v9.8.7/node-v9.8.7-linux-arm64.tar.gz"
printf '%s  node-v9.8.7-linux-x64.tar.gz\n%s  node-v9.8.7-linux-arm64.tar.gz\n' \
  "$(node_sha256 "$NP_DIST/v9.8.7/node-v9.8.7-linux-x64.tar.gz")" "$NP_H1" > "$NP_DIST/v9.8.7/SHASUMS256.txt"
# curl on the Windows box is a native program and wants a drive-letter URL.
if command -v cygpath >/dev/null 2>&1; then NP_URL="file:///$(cygpath -m "$NP_DIST")"; else NP_URL="file://$NP_DIST"; fi
np_partials(){ ls -A "$NP_CACHE" 2>/dev/null | grep -c '^\.partial-' || true; }

# A cache entry that exists but does not report the pinned version -- a
# truncated extract, a hand edit -- must be replaced, never believed.
mkdir -p "$NP_CACHE/node-v9.8.7-linux-x64/bin"
printf '#!/bin/sh\necho v0.0.1\n' > "$NP_CACHE/node-v9.8.7-linux-x64/bin/node"
chmod +x "$NP_CACHE/node-v9.8.7-linux-x64/bin/node"
it "provisioning verifies, installs and replaces a broken cache entry"
NP_OUT="$(OAM_NODE_DIST_URL="$NP_URL" OAM_NODE_CACHE="$NP_CACHE" node_pin_provision 9.8.7 linux-x64 2>&1)"; NP_RC=$?
if [ "$NP_RC" = "0" ] && [ "$NP_OUT" = "$NP_CACHE/node-v9.8.7-linux-x64/bin" ] \
   && [ "$("$NP_CACHE/node-v9.8.7-linux-x64/bin/node")" = "v9.8.7" ] && [ "$(np_partials)" = "0" ]; then pass
else fail "rc=$NP_RC out=$NP_OUT partials=$(np_partials)"; fi

it "a warm cache needs no network, and node_pin_use puts it first on PATH"
printf '9.8.7\n' > "$SUITE_TMP/np-nopin/.node-version"
NP_OUT="$(
  OAM_NODE_DIST_URL="file:///nonexistent-dist" OAM_NODE_CACHE="$NP_CACHE"
  node_pin_use "$SUITE_TMP/np-nopin" linux-x64 || exit 1
  echo "$(command -v node) $(node --version)"
)"; NP_RC=$?
eq "$NP_RC $NP_OUT" "0 $NP_CACHE/node-v9.8.7-linux-x64/bin/node v9.8.7"

it "a download that fails its checksum is refused and never installed"
NP_OUT="$(OAM_NODE_DIST_URL="$NP_URL" OAM_NODE_CACHE="$NP_CACHE" node_pin_provision 9.8.7 linux-arm64 2>&1)"; NP_RC=$?
if [ "$NP_RC" != "0" ] && grep -q 'CHECKSUM MISMATCH' <<<"$NP_OUT" \
   && [ ! -e "$NP_CACHE/node-v9.8.7-linux-arm64" ] && [ "$(np_partials)" = "0" ]; then pass
else fail "rc=$NP_RC partials=$(np_partials) out=$NP_OUT"; fi

it "a platform the release does not list is refused with the reason"
NP_OUT="$(OAM_NODE_DIST_URL="$NP_URL" OAM_NODE_CACHE="$NP_CACHE" node_pin_provision 9.8.7 darwin-arm64 2>&1)"; NP_RC=$?
if [ "$NP_RC" != "0" ] && grep -q 'publishes no official build' <<<"$NP_OUT" && [ "$(np_partials)" = "0" ]; then pass
else fail "rc=$NP_RC out=$NP_OUT"; fi

# --- the consumers ------------------------------------------------------------

# A leg that cannot get the pinned Node must STOP, not quietly compare against
# the host's own -- which is exactly what every leg used to do. The bare tree
# has no Cargo.toml, so if the dispatch got past the oracle it would say so.
it "build-remote.sh conformance dies before cargo when the pinned Node is unavailable"
NP_BARE="$SUITE_TMP/np-bare"
mkdir -p "$NP_BARE/scripts/lib"
cp scripts/build-remote.sh "$NP_BARE/scripts/"
cp scripts/lib/node-pin.sh "$NP_BARE/scripts/lib/"
printf '9.8.6\n' > "$NP_BARE/.node-version"
NP_OUT="$(cd "$NP_BARE" && OAM_NODE_DIST_URL="file:///nonexistent-dist" OAM_NODE_CACHE="$SUITE_TMP/np-empty-cache" \
  OAM_ALLOW_NODE_MISMATCH= bash scripts/build-remote.sh conformance 2>&1)"; NP_RC=$?
if [ "$NP_RC" != "0" ] && grep -q 'could not put the pinned Node' <<<"$NP_OUT" && ! grep -q 'Cargo.toml' <<<"$NP_OUT"; then pass
else fail "rc=$NP_RC out=$NP_OUT"; fi

it "OAM_ALLOW_NODE_MISMATCH=1 lets a hand-run dispatch continue, loudly"
NP_OUT="$(cd "$NP_BARE" && OAM_NODE_DIST_URL="file:///nonexistent-dist" OAM_NODE_CACHE="$SUITE_TMP/np-empty-cache" \
  OAM_ALLOW_NODE_MISMATCH=1 PATH="/usr/bin:/bin" HOME="$SUITE_TMP/np-home" bash scripts/build-remote.sh conformance 2>&1)"
if grep -q 'OAM_ALLOW_NODE_MISMATCH=1, continuing' <<<"$NP_OUT" && ! grep -q 'could not put the pinned Node' <<<"$NP_OUT"; then pass
else fail "out=$NP_OUT"; fi

# Source-level, like the ci-local wiring group: which dispatches take the
# oracle. `bash -c` over a sourced copy would run them; this reads them.
np_body(){ awk -v f="$1() {" '$0 == f {on = 1; next} on && /^}/ {exit} on {print}' scripts/build-remote.sh; }
it "every oracle dispatch goes through use_pinned_node"
NP_BAD=""
for fn in run_conformance run_surface_gaps run_bench; do
  grep -q '^[[:space:]]*use_pinned_node$' <<<"$(np_body "$fn")" || NP_BAD="$NP_BAD $fn"
done
if [ -z "$NP_BAD" ]; then pass; else fail "not provisioning the pinned Node:$NP_BAD"; fi

it "no dispatch checks for a bare node on PATH instead of the pin"
NP_HITS="$(grep -n 'command -v node >/dev/null 2>&1 ||' scripts/build-remote.sh || true)"
if [ -z "$NP_HITS" ]; then pass; else fail "bare node checks remain: $NP_HITS"; fi

it "the node-suite dispatch does not require node (its oracle is the exit code)"
if ! grep -q 'node' <<<"$(np_body run_node_suite | grep -v 'node-suite')"; then pass
else fail "run_node_suite mentions node: $(np_body run_node_suite)"; fi

it "ci-local.sh checks the pinned Node before step 1, and only when conformance runs"
NP_PRE="$(grep -n 'node_oracle_preflight$' scripts/ci-local.sh | tail -1 | cut -d: -f1)"
NP_STEP1="$(grep -n 'say "1/14' scripts/ci-local.sh | cut -d: -f1)"
NP_GUARD="$(sed -n "$((NP_PRE - 2))p" scripts/ci-local.sh)"
if [ -n "$NP_PRE" ] && [ -n "$NP_STEP1" ] && [ "$NP_PRE" -lt "$NP_STEP1" ] \
   && [[ "$NP_GUARD" == *'"$FAST" -eq 0'* ]] && grep -q 'node_pin_verdict' scripts/ci-local.sh; then pass
else fail "preflight line=$NP_PRE step1=$NP_STEP1 guard='$NP_GUARD'"; fi

# gen-surface-gaps.mjs writes COMMITTED ratchet data, so a wrong node there
# poisons every later gate run. A copy in a scratch tree: the pin check has to
# fire before it builds anything, and the tree has nothing to build.
if ! command -v node >/dev/null 2>&1; then
  it "gen-surface-gaps.mjs refuses a node that is not the pin"
  skip "node not on PATH -- the generator cannot run here"
else
  NP_SG="$SUITE_TMP/np-sg"
  mkdir -p "$NP_SG/scripts"
  cp scripts/gen-surface-gaps.mjs "$NP_SG/scripts/"
  np_sg(){ ( cd "$NP_SG" && node scripts/gen-surface-gaps.mjs "$@" 2>&1 ); }

  it "gen-surface-gaps.mjs refuses a node that is not the pin, before building"
  printf '0.0.1\n' > "$NP_SG/.node-version"
  NP_OUT="$(OAM_ALLOW_NODE_MISMATCH= np_sg)"; NP_RC=$?
  if [ "$NP_RC" != "0" ] && grep -q 'refusing to record' <<<"$NP_OUT" && ! grep -q 'building oam' <<<"$NP_OUT"; then pass
  else fail "rc=$NP_RC out=$NP_OUT"; fi

  it "gen-surface-gaps.mjs refuses a malformed pin"
  printf 'lts/jod\n' > "$NP_SG/.node-version"
  NP_OUT="$(np_sg)"; NP_RC=$?
  if [ "$NP_RC" != "0" ] && grep -q 'MAJOR.MINOR.PATCH' <<<"$NP_OUT"; then pass; else fail "rc=$NP_RC out=$NP_OUT"; fi

  # Past the check, the explicit (nonexistent) oam path is what fails -- which
  # is how these two tell "the gate let it through" from "the gate refused".
  it "gen-surface-gaps.mjs lets the pinned node through without a warning"
  node --version > "$NP_SG/.node-version"
  NP_OUT="$(np_sg "$SUITE_TMP/no-such-oam")"
  if ! grep -q 'refusing to record\|WARNING' <<<"$NP_OUT"; then pass; else fail "out=$NP_OUT"; fi

  it "gen-surface-gaps.mjs honours OAM_ALLOW_NODE_MISMATCH=1, loudly"
  printf '0.0.1\n' > "$NP_SG/.node-version"
  NP_OUT="$(OAM_ALLOW_NODE_MISMATCH=1 np_sg "$SUITE_TMP/no-such-oam")"
  if grep -q 'WARNING: recording against node' <<<"$NP_OUT" && ! grep -q 'refusing to record' <<<"$NP_OUT"; then pass
  else fail "out=$NP_OUT"; fi
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
# --- check-control-bytes.sh ---------------------------------------------------
# The gate ci-local runs FIRST, and the one whose failure mode is a false CLEAN
# -- so it needs a suite more than most. Its predecessor used `git grep -I`,
# which skips files git calls binary; a file containing a NUL IS binary to git,
# so it skipped precisely what it hunted and exited 0.
#
# Every control byte below is built from a code point rather than typed as an
# escape: a typed escape through a shell layer is the exact bug this gate
# exists to catch, and writing one here is how the fixture would quietly stop
# testing anything.
CCB="$REPO_DIR/scripts/check-control-bytes.sh"
CCB_REPO="$SUITE_TMP/ccb"
ccb_reset(){
  rm -rf "$CCB_REPO"
  mkdir -p "$CCB_REPO"
  ( cd "$CCB_REPO" && git init -q . && git config user.email t@t && git config user.name t )
}
# 0 clean, 1 finding, 2 could-not-run.
ccb(){ ( cd "$CCB_REPO" && bash "$CCB" "$@" >/dev/null 2>&1; echo $? ); }
ccb_add(){ ( cd "$CCB_REPO" && git add "$@" >/dev/null 2>&1 ); }
ccb_node(){ node -e "$1" "$CCB_REPO/$2"; }

if ! command -v node >/dev/null 2>&1; then
  it "check-control-bytes.sh"
  skip "node not on PATH -- the scanner cannot run here"
else
  ccb_reset
  ccb_node 'require("fs").writeFileSync(process.argv[1], "fine")' clean.txt
  ccb_add clean.txt
  it "reports clean on a tree with no control bytes"
  eq "$(ccb)" "0"

  ccb_reset
  ccb_node 'require("fs").writeFileSync(process.argv[1], "let a = " + String.fromCharCode(34,0,34) + ";")' bad.ts
  ccb_add bad.ts
  it "finds a planted NUL in tracked mode"
  eq "$(ccb)" "1"

  it "finds the same NUL in --staged mode"
  eq "$(ccb --staged)" "1"

  ccb_reset
  ccb_node 'require("fs").writeFileSync(process.argv[1], "snap = " + String.fromCharCode(34,27,34) + ";")' x.snap
  ccb_add x.snap
  it "scans .snap, which is text and the likeliest to carry a stray escape"
  eq "$(ccb)" "1"

  ccb_reset
  ccb_node 'require("fs").writeFileSync(process.argv[1], Buffer.from([0x89,0x50,0x4e,0x47,0x00,0x01]))' logo.png
  ccb_add logo.png
  it "skips a real binary extension, which cannot carry the marker"
  eq "$(ccb)" "0"

  ccb_reset
  ccb_node 'require("fs").writeFileSync(process.argv[1], "// control-byte-ok: allow" + String.fromCharCode(10) + "let a = " + String.fromCharCode(34,0,34) + ";")' ok.ts
  ccb_add ok.ts
  it "honours the control-byte-ok: allow declaration"
  eq "$(ccb)" "0"

  ccb_reset
  ccb_node 'require("fs").writeFileSync(process.argv[1], "// this file discusses control-byte-ok in prose" + String.fromCharCode(10) + "let a = " + String.fromCharCode(34,0,34) + ";")' prose.ts
  ccb_add prose.ts
  it "a bare MENTION of the marker does not disable the scan"
  eq "$(ccb)" "1"

  ccb_reset
  ccb_node 'require("fs").writeFileSync(process.argv[1], "content")' gone.txt
  ccb_add gone.txt
  ( cd "$CCB_REPO" && rm -f gone.txt )
  it "refuses a tracked file absent from disk rather than calling it clean"
  eq "$(ccb)" "1"

  it "rejects an unknown flag as could-not-run, not as clean"
  eq "$(ccb --nonsense)" "2"
fi

if [ "$FAIL" -gt 0 ]; then
  echo -e "${RED}$FAIL failed${NC}, $PASS passed${SKIP_NOTE}"
  exit 1
fi
if [ "$SKIP" -gt 0 ]; then
  echo -e "${GRN}$PASS passed${NC}${YEL}${SKIP_NOTE}${NC}"
else
  echo -e "${GRN}all $PASS passed${NC}"
fi
