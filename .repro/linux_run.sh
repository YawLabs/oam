#!/bin/bash
# Repro the TLS FIN-gate script N times under a chosen oam/node binary.
cd "$(dirname "$0")"
export NODE_EXTRA_CA_CERTS="$PWD/ca.pem"
export OAM_ENABLE_NATIVE_ADDONS=1
export OAM_CACHE_DIR="$PWD/cache-$$"
unset FORCE_COLOR NO_COLOR NODE_DISABLE_COLORS
BIN="${BIN:-/home/jeff/oam-target-cc2/debug/oam}"
ARGS="${ARGS:-run --no-check}"
N="${N:-5}"
for i in $(seq 1 "$N"); do
  echo -n "[$i] "
  timeout 180 $BIN $ARGS fin_ready.mjs 2>&1 | tr '\n' ' '
  echo
done
