#!/usr/bin/env bash
# The oam wedge demo, human edition (POSIX twin of demo.ps1). Self-verifying:
# exits non-zero if any act doesn't behave as advertised. Works on a temp copy
# of project/, which is removed on exit along with the daemon it started.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
oam="${1:-${OAM_BIN:-}}"
if [ -z "$oam" ]; then
  for c in "$here/../../target/release/oam" "$here/../../target/debug/oam"; do
    [ -x "$c" ] && oam="$c" && break
  done
fi
[ -z "$oam" ] && oam="oam"

work="$(mktemp -d "${TMPDIR:-/tmp}/oam-wedge-XXXXXX")"
cp -r "$here/project/." "$work/"
export OAM_CACHE_DIR="$work/.oam-cache"
# A binary given as a relative path must survive the cd below.
case "$oam" in
  */*) [ -e "$oam" ] && oam="$(cd "$(dirname "$oam")" && pwd)/$(basename "$oam")" ;;
esac
cd "$work" || { echo "cannot enter $work"; exit 1; }
failed=0

# Every way out stops the project's daemon and removes the temp copy (the
# cache dir lives inside it).
cleanup() {
  "$oam" daemon stop "$work" >/dev/null 2>&1
  cd / && rm -rf "$work"
}
trap cleanup EXIT

# Milliseconds since the epoch. `date +%s%N` is GNU; a date without %N prints
# the N literally, so anything non-numeric falls back to whole seconds.
now_ms() {
  ns="$(date +%s%N 2>/dev/null)"
  case "$ns" in
    ''|*[!0-9]*) echo "$(( $(date +%s) * 1000 ))" ;;
    *) echo "$(( ns / 1000000 ))" ;;
  esac
}

echo
echo "ACT 1 - the typed loop: oam run main.ts --check=warn"
echo "  (a classic silent bug: a string in a number slot. Node runs it"
echo "   silently. Bun runs it silently. Watch oam.)"
# The run waits OAM_CHECK_WAIT_MS (default 10 s) for the checker after the
# program ends; a longer bound keeps the assertion below off the clock.
act1="$(OAM_CHECK_WAIT_MS="${OAM_CHECK_WAIT_MS:-60000}" "$oam" run main.ts --check=warn 2>&1)" || { failed=1; echo "unexpected: run should succeed (types warn)"; }
printf '%s\n' "$act1" | sed 's/^/  /'
case "$act1" in *"total: 410"*) ;; *) failed=1; echo "unexpected: the program should have printed total: 410" ;; esac
case "$act1" in *TS2322*) ;; *) failed=1; echo "unexpected: the type error (TS2322) should have surfaced" ;; esac
echo "  ^ executed instantly (wrong answer: 410), AND the type error surfaced."

echo
echo "ACT 2 - the CI gate: oam run main.ts --check=block"
if "$oam" run main.ts --check=block; then failed=1; echo "unexpected: block mode should gate"; fi
echo "  ^ same bug, but CI never lets it execute."

echo
echo "ACT 3 - the daemon: repeat checks are served from cache"
"$oam" daemon stop . >/dev/null   # acts 1-2 already warmed it; show true cold
t0=$(now_ms); "$oam" check . >/dev/null 2>&1; t1=$(now_ms)
"$oam" check . >/dev/null 2>&1; t2=$(now_ms)
# The claim is "served from cache", so that is what is asserted: the daemon's
# own hit counter. The timings are shown, not gated -- on a busy box a warm
# check can lose a wall-clock race it still won on work done.
daemon_hits() {
  "$oam" daemon status . 2>/dev/null | sed -n 's/.*"cache_hits":\([0-9][0-9]*\).*/\1/p'
}
hits="$(daemon_hits)"
# A cold check falls back to a one-shot tsgo when the daemon is slow to come
# up, which leaves the second check as the daemon's first (a fill, not a
# hit). One more check settles it either way.
if [ "${hits:-0}" -lt 1 ]; then
  "$oam" check . >/dev/null 2>&1
  hits="$(daemon_hits)"
fi
echo "  cold check: $(( t1 - t0 )) ms  (daemon spawn + tsgo project load)"
echo "  warm check: $(( t2 - t1 )) ms  (cache_hits=${hits:-none}; floor is process+ipc)"
[ "${hits:-0}" -ge 1 ] || { failed=1; echo "unexpected: the repeat check should be a daemon cache hit"; }

echo
echo "ACT 4 - machine mode: the same diagnostics, for agents"
act4="$("$oam" check . --json 2>&1 | grep 'OAM-TS2322' | head -1)"
printf '%s\n' "$act4" | sed 's/^/  /'
case "$act4" in
  "{"*) ;;
  *) failed=1; echo "unexpected: --json should carry the OAM-TS2322 diagnostic as a JSON line" ;;
esac
echo "  ^ stable code + span + docs URL. The agent loop is: node agent-loop.mjs"

echo
[ "$failed" -ne 0 ] && { echo "DEMO FAILED"; exit 1; }
echo "DEMO OK"
