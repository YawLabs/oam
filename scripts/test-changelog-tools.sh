#!/bin/bash
# =============================================================================
# Tests for the changelog tooling: release-local.sh's changelog gate and
# scripts/changelog-release.sh.
# =============================================================================
# Two pieces of shell decide whether a release ships with a changelog and where
# its entries land, and neither had a test: the gate in release-local.sh
# checked only that [Unreleased] was non-empty, so every release from 0.15.1 to
# 0.16.4 shipped with its notes still under [Unreleased] and no heading of its
# own -- the second time that needed a hand-written backfill (d2632ae did
# 0.12.0 through 0.15.0). The helper that now promotes them shipped with a
# claim ("fails rather than continues if a link rewrite does not happen") its
# code did not keep. Each of those is the kind of defect that reports success.
#
# What runs here:
#   - the gate block, SLICED VERBATIM out of scripts/release-local.sh and
#     sourced the way the release sources it (TAG set, fail() defined, in a
#     subshell). A re-implementation would test the re-implementation.
#   - scripts/changelog-release.sh, a copy run against fixture repos.
#   - the two together on ONE fixture, so the helper's output is what the gate
#     accepts and the gate's refusals are what the helper acts on.
#   - the gate's POSITION in release-local.sh: above the auto-bump and the tag.
#     An earlier version fired after the tag and left a half-released state.
#
# Separate from test-scripts.sh, which is spawn-bound and minutes long: this
# suite is fixture-driven, needs no build, and belongs next to the two scripts
# it covers. Portable on purpose -- the release runs from Git Bash on Windows,
# bash on the GCP Linux VM and bash on the Mac -- so: bash, POSIX awk, grep,
# cmp and mktemp only. No `sed -i` (BSD sed wants `-i ''`), no md5sum (absent
# on stock macOS), no bats.
#
# Usage:
#   ./scripts/test-changelog-tools.sh      # run all, non-zero exit on any failure
#   ./scripts/test-changelog-tools.sh -v   # also print each passing assertion
# =============================================================================

set -uo pipefail

VERBOSE=0
[ "${1:-}" = "-v" ] && VERBOSE=1

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
# Guarded: every path below is repo-relative, and a failed cd would slice the
# gate out of whatever release-local.sh the caller happened to stand next to.
cd "$REPO_DIR" || { echo "cannot cd to $REPO_DIR" >&2; exit 1; }

# One temp root, removed on exit -- the same shape as test-scripts.sh.
SUITE_TMP="$(mktemp -d -t oamchangelog-XXXXXX)"
trap 'rm -rf "$SUITE_TMP"' EXIT

RED='\033[0;31m'; GRN='\033[0;32m'; CYA='\033[0;36m'; NC='\033[0m'
PASS=0; FAIL=0; CURRENT=""
group(){ echo -e "\n${CYA}== $* ==${NC}"; }
it(){ CURRENT="$1"; }
pass(){ PASS=$((PASS + 1)); [ "$VERBOSE" = "1" ] && echo -e "  ${GRN}ok${NC} $CURRENT"; return 0; }
fail(){ FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC} $CURRENT"; echo "       $*"; return 0; }
eq(){ [ "$1" = "$2" ] && pass || fail "expected '$2', got '$1'"; }
# Echo the predicate on failure, so a failed chain names the link that gave way.
ck(){ if "$@"; then pass; else fail "assertion failed: $*"; fi; }

RELEASE="scripts/release-local.sh"
HELPER_SRC="scripts/changelog-release.sh"

# =============================================================================
group "the gate block, as release-local.sh runs it"
# =============================================================================
# Sliced by its two anchors: the `if [ -f CHANGELOG.md ]; then` that opens it
# and the `# origin/main is load-bearing` comment that follows its `fi`.
GATE="$SUITE_TMP/gate.sh"
GATE_START="$(grep -nF 'if [ -f CHANGELOG.md ]; then' "$RELEASE" | head -1 | cut -d: -f1)"
GATE_END="$(grep -n '^# origin/main is load-bearing' "$RELEASE" | head -1 | cut -d: -f1)"
if [ -n "$GATE_START" ] && [ -n "$GATE_END" ] && [ "$GATE_START" -lt "$GATE_END" ]; then
  sed -n "${GATE_START},$((GATE_END - 1))p" "$RELEASE" > "$GATE"
else
  : > "$GATE"
fi

# Asserted before anything is sourced from it: an EMPTY gate.sh sources
# cleanly, and every fixture below that expects PASS would then read PASS --
# the suite would report coverage of a gate it never ran.
it "the gate block is sliced out of release-local.sh by its two anchors"
if [ -s "$GATE" ] && grep -qF 'fail "CHANGELOG.md' "$GATE" && grep -q '^fi$' "$GATE"; then pass
else
  fail "anchors start=${GATE_START:-none} end=${GATE_END:-none} -- the block moved, or an anchor line was reworded"
  echo -e "${RED}$FAIL failed${NC}, $PASS passed -- nothing below can run without the gate block"
  exit 1
fi

# gate <dir> <tag> -- runs the block in <dir> against its CHANGELOG.md the way
# release-local.sh does: TAG set, fail() defined, sourced in a subshell. Prints
# PASS, or the gate's own failure message.
gate(){
  ( cd "$1" && TAG="$2" && fail(){ echo "FAIL: $*"; exit 1; } && . "$GATE" && echo PASS ) 2>&1
}
# gate_says <dir> <tag> <fragment>... -- one run; the verdict must carry every
# <fragment>. (Each run is a handful of spawns, which on Windows is the cost.)
gate_says(){
  local dir="$1" tag="$2" out missing="" f; shift 2
  out="$(gate "$dir" "$tag")"
  for f in "$@"; do case "$out" in *"$f"*) ;; *) missing="$missing [$f]" ;; esac; done
  if [ -z "$missing" ]; then pass; else fail "the verdict lacks$missing -- gate said: $out"; fi
}

FX="$SUITE_TMP/fx"; mkdir -p "$FX"
mk(){ printf '%s\n' "$@" > "$FX/CHANGELOG.md"; }

it "promoted with entries and an empty [Unreleased]: passes"
mk "# Changelog" "" "## [Unreleased]" "" "## [0.16.5] - 2026-09-21" "" "### Fixed" "" "- **a fix.**" "" "## [0.16.4] - 2026-09-20" "" "- old"
eq "$(gate "$FX" v0.16.5)" PASS

it "promoted, but the section is a bare Keep-a-Changelog skeleton: refused"
mk "# Changelog" "" "## [Unreleased]" "" "## [0.16.5] - 2026-09-21" "" "### Fixed" "" "## [0.16.4] - 2026-09-20" "" "- old"
gate_says "$FX" v0.16.5 "section has no entries"

it "promoted, with entries left under [Unreleased]: refused as leftover"
mk "# Changelog" "" "## [Unreleased]" "" "- **late entry.**" "" "## [0.16.5] - 2026-09-21" "" "- a fix" "" "## [0.16.4] - 2026-09-20"
gate_says "$FX" v0.16.5 "AND entries under [Unreleased]"

it "entries under [Unreleased] and no heading for the tag: refused, naming the exact helper command"
mk "# Changelog" "" "## [Unreleased]" "" "- **a fix.**" "" "## [0.16.4] - 2026-09-20"
gate_says "$FX" v0.16.5 "no '## [0.16.5]' heading" "scripts/changelog-release.sh 0.16.5"

it "nothing written at all: refused as an empty Unreleased section"
mk "# Changelog" "" "## [Unreleased]" "" "## [0.16.4] - 2026-09-20" "" "- old"
gate_says "$FX" v0.16.5 "Unreleased section has no entries"

# The dot trap: a version spliced into a regex reads 0.16.5 as 0?16?5, and
# this heading would then count as the tag's.
it "a look-alike heading ([0x16y5]) is not taken for the tag's"
mk "# Changelog" "" "## [Unreleased]" "" "- **a fix.**" "" "## [0x16y5] - 2026-09-21" "" "- decoy"
gate_says "$FX" v0.16.5 "no '## [0.16.5]' heading"

# The real file, for the version the workspace is at. Between releases
# [Unreleased] legitimately carries the next release's entries, and the gate
# run for the SHIPPED version reads that as leftover -- correctly, for a
# release, but this suite runs on every push. So the pending entries are set
# aside on a copy, and what is asserted is the invariant that holds all cycle:
# the shipped version has its own heading, and that section carries its log.
it "CHANGELOG.md has the workspace version promoted, with entries"
WS_VERSION="$(awk -F'"' '
  /^\[workspace\.package\]/ { inside = 1; next }
  inside && /^\[/ { exit }
  inside && /^version[[:space:]]*=/ { print $2; exit }
' Cargo.toml)"
REAL="$SUITE_TMP/real"; mkdir -p "$REAL"
awk '
  !inside && /^#+[[:space:]]*\[?[Uu]nreleased\]?/ { inside = 1; print; next }
  inside && /^#+[[:space:]]/ && !/^###[[:space:]]/ { inside = 0 }
  !inside { print }
' CHANGELOG.md > "$REAL/CHANGELOG.md"
if [ -z "$WS_VERSION" ]; then fail "could not read [workspace.package] version from Cargo.toml"
else eq "$(gate "$REAL" "v$WS_VERSION")" PASS; fi

# =============================================================================
group "where the gate sits in release-local.sh"
# =============================================================================
# The block's own comment records why: an earlier version ran AFTER the tag was
# created and pushed, so a refusal left exactly the half-released state a
# preflight exists to prevent, and recovering meant hand-deleting a remote tag.
# Asserted by line order, since that is the property -- a correct block moved
# below the bump or the tag is the bug again.
it "the changelog gate runs BEFORE the auto-bump lands on main and BEFORE the tag is created"
BUMP_LINE="$(grep -nF 'land_on_main "chore(release): bump' "$RELEASE" | head -1 | cut -d: -f1)"
TAG_LINE="$(grep -nF 'git tag -a "$TAG"' "$RELEASE" | head -1 | cut -d: -f1)"
if [ -n "$BUMP_LINE" ] && [ -n "$TAG_LINE" ] \
   && [ "$GATE_START" -lt "$BUMP_LINE" ] && [ "$GATE_START" -lt "$TAG_LINE" ]; then pass
else fail "order is gate@$GATE_START bump@${BUMP_LINE:-none} tag@${TAG_LINE:-none} -- want gate < bump and gate < tag"; fi

# =============================================================================
group "changelog-release.sh -- promoting [Unreleased] to a version heading"
# =============================================================================
# A copy of the helper in a fixture repo: it resolves CHANGELOG.md from its own
# location, so the fixture needs scripts/ beside the file.
HR="$SUITE_TMP/repo"; mkdir -p "$HR/scripts"
cp "$HELPER_SRC" "$HR/scripts/changelog-release.sh"
H="$HR/scripts/changelog-release.sh"; C="$HR/CHANGELOG.md"

# helper <args> -- runs the copy, output (both streams) on stdout, status kept.
helper(){ bash "$H" "$@" 2>&1; }
unreleased_link(){ echo "[Unreleased]: https://github.com/YawLabs/oam/compare/v$1...HEAD"; }
version_link(){ echo "[$1]: https://github.com/YawLabs/oam/compare/v0.16.3...v$1"; }
# fixture <unreleased-heading> <prev-version> <entry-or-empty> <link line>...
fixture(){
  local head="$1" prev="$2" entry="$3"; shift 3
  {
    printf '%s\n' "# Changelog" "" "$head" "" "### Fixed" ""
    [ -n "$entry" ] && printf '%s\n' "$entry" ""
    printf '%s\n' "## [$prev] - 2026-09-20" "" "- old" ""
    [ $# -gt 0 ] && printf '%s\n' "$@"
  } > "$C"
}
# The file must come out byte-identical when the helper refuses: a snapshot
# before, cmp after. (No md5sum: stock macOS has none.)
snapshot(){ cp "$C" "$C.before"; }
untouched(){ cmp -s "$C" "$C.before"; }
# count_line <line> <file> -- how many lines equal <line>, matched literally.
count_line(){ awk -v want="$1" '$0 == want { n++ } END { print n + 0 }' "$2"; }

fixture "## [Unreleased]" 0.16.4 "- **a fix.**" "$(unreleased_link 0.16.4)" "$(version_link 0.16.4)"
HELPER_OUT="$(helper 0.16.5 2026-09-21)"; HELPER_RC=$?
it "promotes: exits 0"
eq "$HELPER_RC" 0
it "promotes: the dated heading is inserted"
ck grep -qxF "## [0.16.5] - 2026-09-21" "$C"
it "promotes: the entry now sits under the new heading, and [Unreleased] is empty"
eq "$(awk '/^## \[Unreleased\]/ { s = "u" } /^## \[0\.16\.5\]/ { s = "v" } /a fix/ { print s; exit }' "$C")" v
it "promotes: the version's compare link is inserted"
ck grep -qxF "[0.16.5]: https://github.com/YawLabs/oam/compare/v0.16.4...v0.16.5" "$C"
it "promotes: the [Unreleased] link moves on to compare from the new version"
ck grep -qxF "[Unreleased]: https://github.com/YawLabs/oam/compare/v0.16.5...HEAD" "$C"
it "promotes: the old [Unreleased] link is gone, not duplicated"
eq "$(count_line "$(unreleased_link 0.16.4)" "$C")" 0
it "promotes: says what it did and names the commit to make"
case "$HELPER_OUT" in *"[Unreleased] -> ## [0.16.5] - 2026-09-21"*"docs(changelog): release 0.16.5"*) pass ;; *) fail "output: $HELPER_OUT" ;; esac

snapshot
HELPER_OUT="$(helper 0.16.5 2026-09-21)"; HELPER_RC=$?
it "re-run on a promoted file: exits 0 and says there is nothing to do"
if [ "$HELPER_RC" -eq 0 ]; then case "$HELPER_OUT" in *"nothing to do"*) pass ;; *) fail "output: $HELPER_OUT" ;; esac
else fail "rc=$HELPER_RC output: $HELPER_OUT"; fi
it "re-run on a promoted file: the file is byte-identical"
ck untouched
it "re-run on a promoted file: still exactly one [0.16.5] link"
eq "$(count_line "[0.16.5]: https://github.com/YawLabs/oam/compare/v0.16.4...v0.16.5" "$C")" 1

it "a '### Unreleased' heading (wrong depth, no brackets) is found and promoted under"
fixture "### Unreleased" 0.16.4 "- **a fix.**" "$(unreleased_link 0.16.4)" "$(version_link 0.16.4)"
helper 0.16.5 2026-09-21 >/dev/null
ck grep -qxF "## [0.16.5] - 2026-09-21" "$C"

it "a pre-release predecessor (0.17.0-rc.1) is kept whole in the compare link"
fixture "## [Unreleased]" 0.17.0-rc.1 "- **a fix.**" "$(unreleased_link 0.17.0-rc.1)" "$(version_link 0.17.0-rc.1)"
helper 0.17.0 2026-09-21 >/dev/null
ck grep -qxF "[0.17.0]: https://github.com/YawLabs/oam/compare/v0.17.0-rc.1...v0.17.0" "$C"

printf '%s\n' "# Changelog" "" "- stray" "" "## [0.16.4] - 2026-09-20" "" "$(unreleased_link 0.16.4)" "$(version_link 0.16.4)" > "$C"
snapshot
HELPER_OUT="$(helper 0.16.5)"; HELPER_RC=$?
it "no [Unreleased] heading: fails"
ck [ "$HELPER_RC" -ne 0 ]
it "no [Unreleased] heading: the file is untouched"
ck untouched

fixture "## [Unreleased]" 0.16.4 "" "$(unreleased_link 0.16.4)" "$(version_link 0.16.4)"
snapshot
HELPER_OUT="$(helper 0.16.5)"; HELPER_RC=$?
it "empty [Unreleased] (a bare ### Fixed skeleton): fails, saying so"
if [ "$HELPER_RC" -ne 0 ]; then case "$HELPER_OUT" in *"has no entries"*) pass ;; *) fail "output: $HELPER_OUT" ;; esac
else fail "rc=0 output: $HELPER_OUT"; fi
it "empty [Unreleased]: the file is untouched"
ck untouched

# --- strict link rewrites ---------------------------------------------------
# awk exits 0 whether or not a line matched. Before the rewrites checked their
# effect, a file missing either link line got its heading, no link, and a clean
# exit -- while the commit that shipped the helper claimed it "fails rather than
# continues if a link rewrite does not happen".
fixture "## [Unreleased]" 0.16.4 "- **a fix.**" "$(unreleased_link 0.16.4)"
snapshot
HELPER_OUT="$(helper 0.16.5 2026-09-21)"; HELPER_RC=$?
it "no '[0.16.4]:' link line to insert the version link above: fails, naming it"
if [ "$HELPER_RC" -ne 0 ]; then case "$HELPER_OUT" in *"no '[0.16.4]:' link reference"*) pass ;; *) fail "output: $HELPER_OUT" ;; esac
else fail "rc=0 -- the heading went in with no link, output: $HELPER_OUT"; fi
it "no '[0.16.4]:' link line: the file is untouched (no heading either)"
ck untouched

fixture "## [Unreleased]" 0.16.4 "- **a fix.**" "$(version_link 0.16.4)"
snapshot
HELPER_OUT="$(helper 0.16.5 2026-09-21)"; HELPER_RC=$?
it "no '[Unreleased]:' link line to move on: fails, naming it"
if [ "$HELPER_RC" -ne 0 ]; then case "$HELPER_OUT" in *"no '[Unreleased]:' link reference"*) pass ;; *) fail "output: $HELPER_OUT" ;; esac
else fail "rc=0 -- the heading went in with no link, output: $HELPER_OUT"; fi
it "no '[Unreleased]:' link line: the file is untouched (no heading either)"
ck untouched

# =============================================================================
group "gate and helper agree, on one fixture"
# =============================================================================
# The gate's refusal names the helper; the helper's output is what the gate
# accepts; and once the next cycle's entries arrive the helper stands aside
# and the gate refuses again. Each tool tested alone could pass while the two
# disagreed on what "promoted" looks like.
fixture "## [Unreleased]" 0.16.4 "- **a fix.**" "$(unreleased_link 0.16.4)" "$(version_link 0.16.4)"
it "gate: unpromoted entries are refused, pointing at the helper"
gate_says "$HR" v0.16.5 "no '## [0.16.5]' heading"
it "helper: promotes them"
helper 0.16.5 2026-09-21 >/dev/null; eq "$?" 0
it "gate: the promoted file passes"
eq "$(gate "$HR" v0.16.5)" PASS

# A late entry under [Unreleased], written after the promotion. Through a temp
# file rather than `sed -i`, which BSD sed spells differently.
awk '
  { print }
  !done && /^## \[Unreleased\]/ { print ""; print "- **a late entry.**"; done = 1 }
' "$C" > "$C.next" && mv "$C.next" "$C"
snapshot
HELPER_OUT="$(helper 0.16.5 2026-09-21)"; HELPER_RC=$?
it "helper: with the version already promoted it has nothing to do, and touches nothing"
if [ "$HELPER_RC" -eq 0 ] && untouched; then case "$HELPER_OUT" in *"nothing to do"*) pass ;; *) fail "output: $HELPER_OUT" ;; esac
else fail "rc=$HELPER_RC untouched=$(untouched && echo yes || echo no) output: $HELPER_OUT"; fi
it "gate: the late entry is refused as leftover"
gate_says "$HR" v0.16.5 "AND entries under [Unreleased]"

echo ""
if [ "$FAIL" -gt 0 ]; then
  echo -e "${RED}$FAIL failed${NC}, $PASS passed"
  exit 1
fi
echo -e "${GRN}all $PASS passed${NC}"
