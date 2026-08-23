#!/bin/bash
# =============================================================================
# Tests for the release-orchestration shell scripts.
# =============================================================================
# scripts/ ships the binaries but had no automated verification of any kind:
# ci-local.sh`s nine gates are all Rust, and nothing in the workspace referenced
# these files. That is how gc-target.sh spent its whole life collecting NOTHING
# on Linux while reporting success -- three separate silent failures at once
# (mawk interval expressions, a dot-requiring family regex, and a `cd ""`
# no-op), none of which any gate could have caught.
#
# Everything here runs the REAL scripts against real temp directories. No mocks:
# the bugs that actually bit were external behaviour (mawk`s regex dialect,
# gcloud`s output buffering) differing from what the code assumed, which is
# precisely what a mock would have encoded wrong.
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

RED='\033[0;31m'; GRN='\033[0;32m'; CYA='\033[0;36m'; NC='\033[0m'
PASS=0; FAIL=0; CURRENT=""
group(){ echo -e "\n${CYA}== $* ==${NC}"; }
it(){ CURRENT="$1"; }
pass(){ PASS=$((PASS + 1)); [ "$VERBOSE" = "1" ] && echo -e "  ${GRN}ok${NC} $CURRENT"; return 0; }
fail(){ FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC} $CURRENT"; echo "       $*"; return 0; }
eq(){ [ "$1" = "$2" ] && pass || fail "expected '$2', got '$1'"; }

# A deps/ fixture. Files are given ascending mtimes in argument order, so the
# LAST one named in a family is that family`s current artifact.
mk_deps(){
  local dir="$1"; shift
  mkdir -p "$dir"
  local i=0 f
  for f in "$@"; do
    echo "artifact" > "$dir/$f"
    touch -t "2026080112$(printf '%02d' "$i")" "$dir/$f"
    i=$((i + 1))
  done
}

# A throwaway repo skeleton that gc-target.sh can run inside: it resolves its
# root from its own location, so it needs scripts/ and the lib it sources.
mk_repo(){
  local root; root="$(mktemp -d -t oamgc-XXXXXX)"
  mkdir -p "$root/scripts/lib"
  cp "$REPO_DIR/scripts/gc-target.sh"        "$root/scripts/"
  cp "$REPO_DIR/scripts/lib/build-locks.sh"  "$root/scripts/lib/"
  printf '%s' "$root"
}

# =============================================================================
group "gc-target.sh -- artifact selection"
# =============================================================================

# The 16GB blind spot: unix test/bin executables carry no extension, and a
# family regex that required a dot after the hash could not see any of them.
it "collects extensionless unix executables"
R="$(mk_repo)"
mk_deps "$R/target/debug/deps" \
  oam-aaaaaaaaaaaaaaa1 oam-aaaaaaaaaaaaaaa2 oam-aaaaaaaaaaaaaaa3
bash "$R/scripts/gc-target.sh" --keep 1 >/dev/null 2>&1
eq "$(ls "$R/target/debug/deps")" "oam-aaaaaaaaaaaaaaa3"

# Each extension is its own family: an .rlib must never be counted against the
# bare executable`s keep budget, or one of them gets evicted every run.
it "groups dotted artifacts per-extension, separately from the bare executable"
R="$(mk_repo)"
mk_deps "$R/target/debug/deps" \
  oam-bbbbbbbbbbbbbbb1 oam-bbbbbbbbbbbbbbb2 \
  oam-bbbbbbbbbbbbbbb1.d oam-bbbbbbbbbbbbbbb2.d \
  liboam-bbbbbbbbbbbbbbb1.rlib liboam-bbbbbbbbbbbbbbb2.rlib
bash "$R/scripts/gc-target.sh" --keep 1 >/dev/null 2>&1
eq "$(ls "$R/target/debug/deps" | sort | tr '\n' ' ')" \
   "liboam-bbbbbbbbbbbbbbb2.rlib oam-bbbbbbbbbbbbbbb2 oam-bbbbbbbbbbbbbbb2.d "

# The safety invariant of a delete tool. A loosened regex would start removing
# real files and the success message would look identical.
it "never touches files without a 16-hex hash"
R="$(mk_repo)"
mk_deps "$R/target/debug/deps" \
  build_script_build-notahash.txt README libfoo.rlib oam-short123
bash "$R/scripts/gc-target.sh" --keep 1 >/dev/null 2>&1
eq "$(ls "$R/target/debug/deps" | wc -l | tr -d ' ')" "4"

it "keeps exactly --keep per family and drops the oldest beyond it"
R="$(mk_repo)"
mk_deps "$R/target/debug/deps" \
  xtask-ccccccccccccccc1 xtask-ccccccccccccccc2 \
  xtask-ccccccccccccccc3 xtask-ccccccccccccccc4
bash "$R/scripts/gc-target.sh" --keep 2 >/dev/null 2>&1
eq "$(ls "$R/target/debug/deps" | sort | tr '\n' ' ')" \
   "xtask-ccccccccccccccc3 xtask-ccccccccccccccc4 "

it "a family at exactly --keep loses nothing"
R="$(mk_repo)"
mk_deps "$R/target/debug/deps" e2e-ddddddddddddddd1 e2e-ddddddddddddddd2
bash "$R/scripts/gc-target.sh" --keep 2 >/dev/null 2>&1
eq "$(ls "$R/target/debug/deps" | wc -l | tr -d ' ')" "2"

# The whole safety argument rests on newest-first ordering: if it inverts, the
# tool keeps stale artifacts and deletes the ones cargo is about to link.
it "survivors are the newest by mtime, not by name"
R="$(mk_repo)"
mkdir -p "$R/target/debug/deps"
for f in oam-eeeeeeeeeeeeeee9 oam-eeeeeeeeeeeeeee1; do echo x > "$R/target/debug/deps/$f"; done
touch -t 202608011200 "$R/target/debug/deps/oam-eeeeeeeeeeeeeee9"  # older
touch -t 202608011300 "$R/target/debug/deps/oam-eeeeeeeeeeeeeee1"  # newer
bash "$R/scripts/gc-target.sh" --keep 1 >/dev/null 2>&1
eq "$(ls "$R/target/debug/deps")" "oam-eeeeeeeeeeeeeee1"

# =============================================================================
group "gc-target.sh -- portability and safety"
# =============================================================================

# THE regression guard. mawk (Ubuntu`s default awk, and the GCP builder`s)
# matches NOTHING for /-[0-9a-f]{16}/ rather than erroring, so reintroducing an
# interval expression silently returns Linux collection to zero with a fully
# green run. Assert on the source, because the dev box has no mawk to run under.
it "the awk program uses no interval expressions"
# Strip comments BEFORE looking: the prose above the awk block necessarily
# quotes the very pattern it warns against. (Filtering `grep -n` output by a
# leading-# match cannot work -- grep -n prefixes the line number first.)
INTERVALS="$(sed 's/#.*//' scripts/gc-target.sh | grep -nE '\{[0-9]+\}' || true)"
if [ -n "$INTERVALS" ]; then
  fail "interval expression in live code -- mawk matches nothing for it: $INTERVALS"
else pass; fi

it "selection is identical under every awk on this host"
R="$(mk_repo)"
mk_deps "$R/target/debug/deps" oam-fffffffffffffff1 oam-fffffffffffffff2
for AWKBIN in mawk gawk awk; do
  command -v "$AWKBIN" >/dev/null 2>&1 || continue
  R2="$(mk_repo)"; mk_deps "$R2/target/debug/deps" oam-fffffffffffffff1 oam-fffffffffffffff2
  PATH_OUT="$(cd "$R2" && bash scripts/gc-target.sh --keep 1 >/dev/null 2>&1; ls target/debug/deps)"
  [ "$PATH_OUT" = "oam-fffffffffffffff2" ] || fail "$AWKBIN selected '$PATH_OUT'"
done
pass

it "--dry-run deletes nothing"
R="$(mk_repo)"
mk_deps "$R/target/debug/deps" oam-999999999999999a oam-999999999999999b oam-999999999999999c
bash "$R/scripts/gc-target.sh" --dry-run --keep 1 >/dev/null 2>&1
eq "$(ls "$R/target/debug/deps" | wc -l | tr -d ' ')" "3"

# The builder`s exact condition: tree arrives by tar with --exclude=./.git, and
# the caller`s cwd is $HOME. `git rev-parse` fails there and `cd ""` no-ops, so
# the script used to prune whatever directory it happened to be standing in.
it "resolves its own root from a foreign cwd with no .git"
R="$(mk_repo)"
mk_deps "$R/target/debug/deps" oam-1111111111111111 oam-2222222222222222
[ -d "$R/.git" ] && fail "fixture unexpectedly has .git"
( cd "$HOME" && bash "$R/scripts/gc-target.sh" --keep 1 >/dev/null 2>&1 )
eq "$(ls "$R/target/debug/deps")" "oam-2222222222222222"

it "an absent target/ is a clean no-op, not an error"
R="$(mk_repo)"
bash "$R/scripts/gc-target.sh" --keep 1 >/dev/null 2>&1
eq "$?" "0"

it "an absent deps/ is a clean no-op, not an error"
R="$(mk_repo)"
mkdir -p "$R/target/debug"
bash "$R/scripts/gc-target.sh" --keep 1 >/dev/null 2>&1
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
R="$(mktemp -d -t oamrg-XXXXXX)"; mkdir -p "$R/scripts"
cp scripts/build-remote.sh "$R/scripts/"
OUT="$(cd "$R" && bash scripts/build-remote.sh gc 2>&1)"; RC=$?
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
LOG="$(mktemp -t oamlog-XXXXXX)"
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
iap_parse_picked_port "/nonexistent/tunnel.log" >/dev/null 2>&1 && fail "parsed a missing file" || pass

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

# The builder`s real reading the day this was written: low enough to prune,
# high enough to still run. Both predicates must agree on that.
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
if [ "$FAIL" -gt 0 ]; then
  echo -e "${RED}$FAIL failed${NC}, $PASS passed"
  exit 1
fi
echo -e "${GRN}all $PASS passed${NC}"
