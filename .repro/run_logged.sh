#!/bin/bash
# Run the logging fixture under concurrency with pool tracing; keep the
# arrivals + trace of any instance that doubled a /redir.
BIN=~/oam-build/target/debug/oam
CONC=${CONC:-12}; ROUNDS=${ROUNDS:-8}
D=${D:-~/logged-$$}; mkdir -p $D
found=0
for r in $(seq 1 $ROUNDS); do
  pids=""
  for c in $(seq 1 $CONC); do
    w=$D/w$r-$c; mkdir -p $w
    cp ~/repro/fin_ready_log.mjs ~/repro/ca.pem ~/repro/leaf.pem ~/repro/leaf.key $w/
    ( cd $w && NODE_EXTRA_CA_CERTS=$w/ca.pem ARRIVALS=$w/arrivals.log OAM_TRACE_POOL=1 \
        OAM_ENABLE_NATIVE_ADDONS=1 OAM_CACHE_DIR=$w/cache \
        timeout 180 $BIN run --no-check fin_ready_log.mjs > out.txt 2> trace.txt ) &
    pids="$pids $!"
  done
  wait $pids
  for w in $D/w$r-*; do
    if grep -q 'redir_max=2' $w/out.txt 2>/dev/null; then
      echo "FAIL $w: $(tr '\n' ' ' < $w/out.txt)"
      found=$((found+1))
    fi
  done
  echo "[round $r] done, failures so far: $found"
done
echo "D=$D"
echo DONE
