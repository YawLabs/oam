#!/usr/bin/env bash
# The oam wedge demo, human edition (POSIX twin of demo.ps1).
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
cd "$work"
failed=0

echo
echo "ACT 1 - the typed loop: oam run main.ts"
echo "  (a classic silent bug: a string in a number slot. Node runs it"
echo "   silently. Bun runs it silently. Watch oam.)"
"$oam" run main.ts || { failed=1; echo "unexpected: run should succeed (types warn)"; }
echo "  ^ executed instantly (wrong answer: 410), AND the type error surfaced."

echo
echo "ACT 2 - the CI gate: oam run main.ts --check=block"
if "$oam" run main.ts --check=block; then failed=1; echo "unexpected: block mode should gate"; fi
echo "  ^ same bug, but CI never lets it execute."

echo
echo "ACT 3 - the daemon: repeat checks are served from cache"
"$oam" daemon stop . >/dev/null   # acts 1-2 already warmed it; show true cold
t0=$(date +%s%N); "$oam" check . >/dev/null 2>&1; t1=$(date +%s%N)
"$oam" check . >/dev/null 2>&1; t2=$(date +%s%N)
echo "  cold check: $(( (t1 - t0) / 1000000 )) ms  (daemon spawn + tsgo project load)"
echo "  warm check: $(( (t2 - t1) / 1000000 )) ms  (served from the fingerprint cache)"
[ $(( t2 - t1 )) -ge $(( t1 - t0 )) ] && { failed=1; echo "unexpected: warm should beat cold"; }

echo
echo "ACT 4 - machine mode: the same diagnostics, for agents"
"$oam" check . --json 2>&1 | head -1 | sed 's/^/  /'
echo "  ^ stable code + span + docs URL. The agent loop is: node agent-loop.mjs"

"$oam" daemon stop . >/dev/null
echo
[ "$failed" -ne 0 ] && { echo "DEMO FAILED"; exit 1; }
echo "DEMO OK"
