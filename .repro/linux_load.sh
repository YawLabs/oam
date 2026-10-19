#!/bin/bash
# Run the TLS FIN-gate script under CPU load, to reproduce the load-induced
# redir_max=2 the release Linux leg hit.
cd "$(dirname "$0")"
export NODE_EXTRA_CA_CERTS="$PWD/ca.pem"
export OAM_ENABLE_NATIVE_ADDONS=1
export OAM_CACHE_DIR="$PWD/cache-$$"
unset FORCE_COLOR NO_COLOR NODE_DISABLE_COLORS
BIN="${BIN:-/home/jeff/oam-target-cc2/debug/oam}"
N="${N:-40}"
LOAD="${LOAD:-12}"

pids=""
for i in $(seq 1 "$LOAD"); do
  ( while :; do :; done ) &
  pids="$pids $!"
done
trap 'kill $pids 2>/dev/null' EXIT

fails=0
for i in $(seq 1 "$N"); do
  out=$(timeout 180 $BIN run --no-check fin_ready.mjs 2>&1 | tr '\n' ' ')
  echo "[$i] $out"
  case "$out" in
    *"redir_max=1 "*) ;;
    *) fails=$((fails+1)) ;;
  esac
done
echo "=== redir_max!=1 runs: $fails/$N"
