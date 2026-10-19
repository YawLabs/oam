#!/bin/bash
# Isolate client vs server: run the FIN-gate workload with the server and the
# client from chosen runtimes (oam = instrumented HEAD build, node = /usr/bin).
OAM=~/oam-build/target/debug/oam
NODE=/usr/bin/node
MODE=${MODE:-nn}
CONC=${CONC:-12}; ROUNDS=${ROUNDS:-4}
case "$MODE" in
  nn) SRV=$NODE; CLI=$NODE;;
  on) SRV=$OAM;  CLI=$NODE;;
  no) SRV=$NODE; CLI=$OAM;;
  oo) SRV=$OAM;  CLI=$OAM;;
esac
D=~/iso-$MODE-$$; mkdir -p $D

for r in $(seq 1 $ROUNDS); do
  pids=""
  for c in $(seq 1 $CONC); do
    port=$(( 20000 + RANDOM % 20000 ))
    w=$D/w$r-$c; mkdir -p $w
    cp ~/repro/iso_srv.mjs ~/repro/iso_cli.mjs ~/repro/ca.pem ~/repro/leaf.pem ~/repro/leaf.key $w/
    (
      cd $w
      export NODE_EXTRA_CA_CERTS=$w/ca.pem PORT=$port OAM_CACHE_DIR=$w/cache OAM_ENABLE_NATIVE_ADDONS=1
      if [ "$SRV" = "$NODE" ]; then
        "$NODE" iso_srv.mjs > srv.txt 2>&1 & sp=$!
      else
        "$OAM" run --no-check iso_srv.mjs > srv.txt 2>&1 & sp=$!
      fi
      for i in $(seq 1 80); do grep -q "server-on" srv.txt && break; sleep 0.1; done
      if [ "$CLI" = "$NODE" ]; then
        "$NODE" iso_cli.mjs > cli.txt 2>&1
      else
        "$OAM" run --no-check iso_cli.mjs > cli.txt 2>&1
      fi
      kill -TERM $sp 2>/dev/null
      wait $sp 2>/dev/null
    ) &
    pids="$pids $!"
  done
  wait $pids
  echo "[$MODE round $r] redir_max: $(grep -h 'redir_max=' $D/w$r-*/srv.txt | sort | uniq -c | tr '\n' ' ')"
  echo "            final_doubles: $(grep -h 'final_doubles=' $D/w$r-*/srv.txt | sort | uniq -c | tr '\n' ' ')"
  echo "            get_ok: $(grep -h 'get_ok=' $D/w$r-*/cli.txt | sort | uniq -c | tr '\n' ' ')"
done
echo DONE-$MODE
