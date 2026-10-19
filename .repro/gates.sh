#!/bin/bash
set -o pipefail
source ~/.cargo/env
cd ~/oam-build
echo "=== cargo fmt --check"
cargo fmt --all --check 2>&1 | head -20
rc1=$?
echo "fmt rc=$rc1"
echo "=== clippy (oam_cli tests)"
cargo clippy -p oam_cli --tests -- -D warnings 2>&1 | tail -5
rc2=$?
echo "clippy rc=$rc2"
echo "=== full e2e suite"
B=target/debug/deps/e2e-b4447e3eb8dea017
$B > ~/suite-final.log 2>&1
rc3=$?
echo "e2e rc=$rc3 $(grep 'test result' ~/suite-final.log | tail -1)"
echo DONE