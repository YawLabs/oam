#!/bin/bash
BIN=~/oam-build/target/debug/deps/e2e-b4447e3eb8dea017
NAME=the_fin_gate_holds_over_tls_for_a_get_hop_and_a_post_after_a_fin
CONC=${CONC:-16}; ROUNDS=${ROUNDS:-12}
D=~/verify-$$; mkdir -p $D
bad=0; tot=0
for r in $(seq 1 $ROUNDS); do
  pids=""
  for c in $(seq 1 $CONC); do
    ( cd ~/oam-build && $BIN $NAME --nocapture > $D/r$r-c$c.log 2>&1 ) &
    pids="$pids $!"
  done
  wait $pids
  for f in $D/r$r-c*.log; do
    tot=$((tot+1))
    if grep -q "test result: FAILED" "$f"; then
      bad=$((bad+1))
      echo "FAILED $(basename $f)"
      grep -E 'redir_doubles|final_doubles|get_ok|panicked|assertion|left:|right:' "$f"
    fi
  done
  echo "[round $r] tot=$tot bad=$bad"
done
echo "TOTAL=$tot FAILED=$bad"
