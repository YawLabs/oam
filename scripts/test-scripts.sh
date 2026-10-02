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

# --- wiring: the orchestrator must decide through the lib ----------------------
# Comment-stripped: the block above the loop describes the old loop, and a
# negative grep must not match a description.
VMS_SRC="$(sed 's/#.*//' scripts/build-platforms-gcp-iap.sh)"

it "build-platforms-gcp-iap.sh starts the VM through the lib's verdicts and type walk"
VMS_WIRE=""
for want in 'vm_start_types "$ORIGINAL_MACHINE_TYPE"' 'vm_start_verdict "$start_err"' \
            'vm_start_verdict "$set_err"' 'gcloud_error_message "$start_err"' \
            'vm_start_zones_available "$start_err"' \
            '${OAM_GCP_FALLBACK_MACHINE_TYPES-' 'OAM_VM_START_BUDGET_S:-900' \
            '--filter="name~^${INSTANCE}\$"' 'trap restore_machine_type EXIT' \
            'cleanup() { stop_iap_tunnel; reattach_stop_schedules; stop_vm; restore_machine_type; }'; do
  grep -qF -- "$want" <<<"$VMS_SRC" || VMS_WIRE="$VMS_WIRE [$want]"
done
if [ -z "$VMS_WIRE" ]; then pass; else fail "build-platforms-gcp-iap.sh no longer carries:$VMS_WIRE"; fi

it "the VM start neither discards gcloud's stderr nor retries a fixed six times"
if grep -E 'instances start ' <<<"$VMS_SRC" | grep -q '2>/dev/null' \
   || ! grep -E 'instances start ' <<<"$VMS_SRC" | grep -q '2>&1' \
   || grep -q 'for VM_ATTEMPT in 1 2 3 4 5 6' <<<"$VMS_SRC"; then
  fail "the blind six-attempt start loop is back"
else pass; fi

# The restore needs the VM TERMINATED, so the type change has to be undone by an
# EXIT trap armed BEFORE the first set-machine-type runs, and in cleanup() only
# after the stop. Asserted by line order, since that is the property.
it "the restore trap is armed before the first set-machine-type, and cleanup stops before it restores"
VMS_TRAP_LINE="$(grep -n 'trap restore_machine_type EXIT' scripts/build-platforms-gcp-iap.sh | head -1 | cut -d: -f1)"
VMS_SET_LINE="$(grep -n -- '--machine-type="$vm_type"' scripts/build-platforms-gcp-iap.sh | head -1 | cut -d: -f1)"
if [ -n "$VMS_TRAP_LINE" ] && [ -n "$VMS_SET_LINE" ] && [ "$VMS_TRAP_LINE" -lt "$VMS_SET_LINE" ] \
   && grep -q 'stop_vm; restore_machine_type; }' scripts/build-platforms-gcp-iap.sh; then pass
else fail "order is trap@${VMS_TRAP_LINE:-?} set-machine-type@${VMS_SET_LINE:-?}; cleanup must run stop_vm, then restore_machine_type"; fi

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
# An instance that moved zones: nothing answers in the old zone but the list.
if [ -e "\$S/moved" ]; then
  case "\$a" in
    *"instances list"*) printf 'us-west1-c\r\n'; exit 0 ;;
    *"--zone=us-west1-b"*)
      printf 'ERROR: (gcloud.compute.instances.describe) Could not fetch resource:\r\n - The resource '"'"'projects/yaw-labs-prod/zones/us-west1-b/instances/yaw-linux-builder'"'"' was not found\r\n\r\n' >&2; exit 1 ;;
  esac
fi
case "\$a" in
  *"instances describe"*"value(name)"*)                 echo yaw-linux-builder ;;
  *"instances describe"*"value(status)"*)               cat "\$S/status" ;;
  *"instances describe"*"machineType.basename()"*)      cat "\$S/type" ;;
  *"instances describe"*"resourcePolicies"*)            echo ;;
  *"instances describe"*"natIP"*)                       echo 203.0.113.9 ;;
  *"instances set-machine-type"*)
    t="\${a##*--machine-type=}"; t="\${t%% *}"
    # reject-set-<type>: the API refuses the type. interrupt-set-<type>: the
    # change is applied, then gcloud reports a Ctrl-C the way Windows sees it.
    if [ -e "\$S/reject-set-\$t" ]; then
      printf "ERROR: (gcloud.compute.instances.set-machine-type) Could not fetch resource:\r\n - Invalid value for field 'resource.machineType': 'zones/us-west1-b/machineTypes/\$t'.\r\n\r\n" >&2; exit 1
    fi
    echo "\$t" > "\$S/type"
    if [ -e "\$S/interrupt-set-\$t" ]; then printf '\n\nCommand killed by keyboard interrupt\n' >&2; exit 2; fi ;;
  *"instances start"*)
    if [ "\$(cat "\$S/type")" = "\$(cat "\$S/good-type")" ]; then echo RUNNING > "\$S/status"
    elif [ -e "\$S/start-quota" ]; then cat "\$S/quota.txt" >&2; exit 1
    else cat "\$S/exhausted.txt" >&2; exit 1; fi ;;
  *"instances stop"*)              echo 'Stopping instance(s) yaw-linux-builder...' >&2; echo TERMINATED > "\$S/status" ;;
  *"get-serial-port-output"*)      echo 'Started ssh.service - OpenBSD Secure Shell server.' ;;
  *) echo "stub gcloud: unexpected call: \$a" >&2; exit 97 ;;
esac
EOF
cat > "$VMS_BIN/ssh" <<'EOF'
#!/bin/bash
echo 'Host key verification failed.' >&2
exit 255
EOF
chmod +x "$VMS_BIN/gcloud" "$VMS_BIN/ssh"
# vms_run <good-type|none> [flag...]  -- the orchestrator against the stubs,
# from a TERMINATED e2-highmem-4, with n2-highmem-4 the one fallback and no
# budget for a second pass. Flags are state files the stub reads: moved,
# start-quota, reject-set-<type>, interrupt-set-<type>. Every OAM_* knob the
# orchestrator reads is pinned, so a shell that exports OAM_KEEP_VM=1 or
# another project cannot turn a run red. Stdout to VMS_OUT, stderr to VMS_ERR,
# status to VMS_RC, the stub's call log to VMS_LOG.
vms_run(){
  rm -f "$VMS_STATE/log" "$VMS_STATE/moved" "$VMS_STATE/start-quota" "$VMS_STATE"/reject-set-* "$VMS_STATE"/interrupt-set-*
  echo TERMINATED > "$VMS_STATE/status"; echo e2-highmem-4 > "$VMS_STATE/type"
  echo "$1" > "$VMS_STATE/good-type"; shift
  local flag; for flag in "$@"; do : > "$VMS_STATE/$flag"; done
  VMS_OUT="$(PATH="$VMS_BIN:$PATH" TMPDIR="$VMS_TMP" OAM_GCP_FALLBACK_MACHINE_TYPES=n2-highmem-4 \
    OAM_VM_START_BUDGET_S=0 OAM_IAP_SSH_MODE=direct OAM_GCP_BUILDER_ZONE=us-west1-b \
    OAM_GCP_PROJECT=yaw-labs-prod OAM_GCP_BUILDER_INSTANCE=yaw-linux-builder OAM_LINUX_USER=jeff \
    OAM_KEEP_VM=0 OAM_KEEP_VM_SCHEDULE=0 OAM_LINUX_FAST=0 \
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
# stderr to MACPF_ERR, status to MACPF_RC.
macpf(){
  MACPF_OUT="$(PATH="$MACPF_BIN:$PATH" TMPDIR="$MACPF_TMP" OAM_MAC_KEY="$SUITE_TMP/macpf-key" \
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

it "no key committed + OAM_SIGN_REQUIRED=1: fatal"
SG_D="$(OAM_SIGN_REQUIRED=1 sg "$SG_BOOT" release_signing_decision)"
case "$SG_D" in fail:*"OAM_SIGN_REQUIRED=1"*) pass ;; *) fail "got '$SG_D'" ;; esac

it "OAM_SIGN_REQUIRED takes 0 or 1 and nothing else"
SG_D="$(OAM_SIGN_REQUIRED=yes sg "$SG_BOOT" release_signing_decision)"
case "$SG_D" in fail:*"0 or 1"*) pass ;; *) fail "got '$SG_D'" ;; esac

SG_ONE="$SG/one"
sg_keys "$SG_ONE" "oam-release-k1 $SG_NS ssh-ed25519 $SG_K"$'\n' $'k1 v0.18.0 -\n'
it "a committed key makes signing mandatory, whatever OAM_SIGN_REQUIRED says"
SG_D="$(OAM_SIGN_REQUIRED=0 sg "$SG_ONE" release_signing_decision) $(OAM_SIGN_REQUIRED=1 sg "$SG_ONE" release_signing_decision)"
eq "$SG_D" "sign sign"

it "the lib has no knob that skips the manifest"
if grep -v '^[[:space:]]*#' scripts/lib/signing.sh | grep -qE 'OAM_SKIP|OAM_NO_SIGN|SKIP_SIGN|SKIP_MANIFEST'; then
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

it "release-local.sh: trap, then the agent and signing preflight, all before the dirty-tree check, the bump and the tag"
sg_order scripts/release-local.sh 'trap release_on_exit EXIT' 'release_agent_start || fail' \
  'release_signing_preflight "$TAG" || fail' 'restore_gate_artifacts "preflight"' \
  'bumping Cargo.toml to' 'git tag -a "$TAG" -m "$TAG"'

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
sg_order scripts/release-upload-local-arm64.sh 'git rev-parse "${TAG}^{commit}"' \
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
  # stranger to allowed_signers.
  for k in k1 k2 k3; do
    ssh-keygen -q -t ed25519 -N '' -C "oam-release-$k" -f "$IN/$k" </dev/null >/dev/null 2>&1 || break
  done
  printf 'probe\n' >"$IN/probe"
  if [ -f "$IN/k3" ] && ssh-keygen -Y sign -f "$IN/k1" -n oam-release "$IN/probe" </dev/null >/dev/null 2>&1 \
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
    printf 'oam-release-k2 namespaces="oam-release" %s\n' "$(cut -d' ' -f1,2 "$IN/k2.pub")"; } >"$IN/allowed_signers"
  printf '# fixture ranges\nk1 v0.18.0 v0.19.5\n' >"$IN/ranges"

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
  it "a re-install replaces the binary by rename and leaves no temp file"
  in_sh good v0.18.0 "$IN_PATH_KG"
  printf 'old\n' >"$IN/dest/oam"
  IN_RC=0
  IN_OUT="$(env -u GH_TOKEN -u GITHUB_TOKEN HOME="$IN" PATH="$IN_PATH_KG" OAM_INSTALL_BASE="$(in_url "$IN/rel/good")" \
    OAM_VERSION=v0.18.0 OAM_INSTALL_DIR="$IN/dest" "$IN_SH" "$IN/install.sh" 2>&1)" || IN_RC=$?
  in_installed good 'installed oam v0.18.0'
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
  # in_ps <powershell> <script> <release> <tag> [VAR=value...] -- run it into a
  # fresh $IN/pdest. PowerShell wraps long error lines at the console width,
  # mid-word, so the output is joined back up before any needle is looked for.
  in_ps(){
    local ps="$1" script="$2" rel="$3" tag="$4"; shift 4
    rm -rf "$IN/pdest"
    IN_RC=0
    IN_OUT="$(env -u GH_TOKEN -u GITHUB_TOKEN -u OAM_INSECURE_SKIP_SIGNATURE -u OAM_GH_API \
      PATH="$IN_WINPATH" OAM_INSTALL_BASE="$(in_url "$IN/rel/$rel")" OAM_VERSION="$tag" \
      OAM_INSTALL_DIR="$(cygpath -w "$IN/pdest")" "$@" \
      "$ps" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(cygpath -w "$script")" 2>&1 | tr -d '\r\n')" || IN_RC=$?
  }
  in_ps_refused(){
    if [ "$IN_RC" != "0" ] && grep -qF -- "$1" <<<"$IN_OUT" && [ ! -e "$IN/pdest/oam.exe" ]; then pass
    else fail "rc=$IN_RC, wanted a refusal saying '$1' and no oam.exe: $IN_OUT"; fi
  }
  in_ps_installed(){
    if [ "$IN_RC" = "0" ] && grep -qF -- "$1" <<<"$IN_OUT" && cmp -s "$IN/pdest/oam.exe" "$IN_WIN_EXE"; then pass
    else fail "rc=$IN_RC, wanted '$1' and the fixture oam.exe in place: $IN_OUT"; fi
  }

  it "ps1 good: verifies with System32's inbox ssh-keygen and installs"
  in_ps "$IN_PS64" "$IN/install.ps1" good v0.18.0
  if grep -qF 'System32\OpenSSH\ssh-keygen.exe' <<<"$IN_OUT"; then in_ps_installed 'signature ok: v0.18.0, signed by oam-release-k1'
  else fail "did not verify with the System32 ssh-keygen: $IN_OUT"; fi
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
fi

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
