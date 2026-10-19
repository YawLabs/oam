#!/bin/bash
# Timed concurrent sweep: run CONC copies ROUNDS times at one GATE, report
# wall time and the redir_max / get_ok distributions.
BIN=~/oam-build/target/debug/oam
GATE=${GATE:-1}
CONC=${CONC:-12}; ROUNDS=${ROUNDS:-10}
D=~/gt-$GATE-$$; mkdir -p $D
start=$(date +%s)
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
done
end=$(date +%s)
echo "GATE=$GATE  wall=$((end-start))s  for $((ROUNDS*CONC)) runs"
echo "  redir_max: $(grep -h 'redir_max=' $D/w*/out.txt | sort | uniq -c | tr '\n' ' ')"
echo "  get_ok:    $(grep -h 'get_ok=' $D/w*/out.txt | sort | uniq -c | tr '\n' ' ')"
echo "  final_doubles: $(grep -h 'final_doubles=' $D/w*/out.txt | sort | uniq -c | tr '\n' ' ')"
echo DONE-$GATE
