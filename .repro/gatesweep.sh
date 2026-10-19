#!/bin/bash
# Sweep OAM_FRESH_GATE_MS under the e2e workload (server+client in one process).
# Reports redir_max / final_doubles / get_ok distribution per gate value.
BIN=~/oam-build/target/debug/oam
GATE=${GATE:-1}
CONC=${CONC:-12}; ROUNDS=${ROUNDS:-6}
D=~/gate-$GATE-$$; mkdir -p $D
for r in $(seq 1 $ROUNDS); do
  pids=""
  for c in $(seq 1 $CONC); do
    w=$D/w$r-$c; mkdir -p $w
    cp ~/repro/fin_ready.mjs ~/repro/ca.pem ~/repro/leaf.pem ~/repro/leaf.key $w/
    ( cd $w && NODE_EXTRA_CA_CERTS=$w/ca.pem OAM_FRESH_GATE_MS=$GATE OAM_ENABLE_NATIVE_ADDONS=1 OAM_CACHE_DIR=$w/cache \
        timeout 180 $BIN run --no-check fin_ready.mjs > out.txt 2>&1 ) &
    pids="$pids $!"
  done
  wait $pids
  rm=$(grep -h 'redir_max=' $D/w$r-*/out.txt | sort | uniq -c | tr '\n' ' ')
  echo "[gate=$GATE r$r] $rm"
done
echo "=== gate=$GATE totals redir_max:"; grep -h 'redir_max=' $D/w*/out.txt | sort | uniq -c
echo "=== gate=$GATE totals get_ok:"; grep -h 'get_ok=' $D/w*/out.txt | sort | uniq -c
echo "=== gate=$GATE totals final_doubles:"; grep -h 'final_doubles=' $D/w*/out.txt | sort | uniq -c
echo DONE-$GATE
