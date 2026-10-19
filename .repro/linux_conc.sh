#!/bin/bash
# Mirror the e2e harness: many concurrent copies of the FIN-gate script, the
# way cargo test oversubscribes the box, and report any redir_max != 1.
cd "$(dirname "$0")"
export NODE_EXTRA_CA_CERTS="$PWD/ca.pem"
export OAM_ENABLE_NATIVE_ADDONS=1
unset FORCE_COLOR NO_COLOR NODE_DISABLE_COLORS
BIN="${BIN:-/home/jeff/oam-target-cc2/debug/oam}"
CONC="${CONC:-12}"
ROUNDS="${ROUNDS:-5}"
RES="$PWD/results-$$"
mkdir -p "$RES"

for r in $(seq 1 "$ROUNDS"); do
  pids=""
  for c in $(seq 1 "$CONC"); do
    OAM_CACHE_DIR="$PWD/cache-$$-$r-$c" \
      timeout 180 $BIN run --no-check fin_ready.mjs \
      > "$RES/r$r-c$c.out" 2>&1 &
    pids="$pids $!"
  done
  wait $pids
  echo "=== round $r done"
done

echo "=== summary"
grep -h . "$RES"/*.out | tr '\n' ' ' | tr -s ' '
echo
echo "--- any redir_max != 1:"
grep -h 'redir_max=' "$RES"/*.out | grep -v 'redir_max=1 ' || echo "(none)"
echo "--- distinct redir_max values:"
grep -ho 'redir_max=[0-9]*' "$RES"/*.out | sort | uniq -c
rm -rf "$RES"
