#!/bin/bash
# =============================================================================
# oam GCP-IAP platform-build orchestrator  (linux-x64 leg)
# =============================================================================
# Runs the Linux leg of the deleted GitHub workflows on the shared GCP Linux
# VM (the same yaw-linux-builder that builds yaw/vew), reached via gcloud IAP
# TCP forwarding. Mirrors yaw's scripts/build-platforms-gcp-iap.sh tunnel
# machinery; adapted for a Rust workspace and given VM lifecycle management
# (yaw's release.sh owns the VM start/stop there; oam is self-contained here).
#
# Modes (per-mode remote steps; each is a short ssh sub-step -- see the
# tunnel rationale below):
#   --mode=release (default)
#       prep -> gate -> test -> conformance -> node-suite -> build
#       pulls:  $ART/oam-x86_64-unknown-linux-gnu
#       (OAM_LINUX_FAST=1 skips test+conformance -- escape hatch for a
#       rebuild-only iteration; a real release should never set it)
#   --mode=measure
#       prep -> conformance -> node-suite   (advisory: a tripped gate warns,
#                                            scorecards still pulled)
#       pulls:  $ART/linux-x64/{scorecard.json,CONFORMANCE.md,
#                               node-suite-scorecard.json,CONFORMANCE-NODE.md}
#   --mode=bench
#       prep -> bench -> io-uring-ab
#       pulls:  $ART/linux-x64/{BENCHMARKS.md,results.json,io-uring-ab.log}
#
# Usage (FAIL-CLOSED -- capture first, check the exit, THEN consume):
#   ART=$(./scripts/build-platforms-gcp-iap.sh) || { echo "linux leg failed"; exit 1; }
#   # The artifact dir path is the LAST stdout line; all progress goes to
#   # stderr. Do NOT inline the $(...) into a consumer's env assignment -- a
#   # failed build there yields an empty var and the consumer aborts on a
#   # misleading guard with the real error hidden.
#
# Host config (env):
#   OAM_GCP_PROJECT           yaw-labs-prod      (default)
#   OAM_GCP_BUILDER_INSTANCE  yaw-linux-builder  (default)
#   OAM_GCP_BUILDER_ZONE      us-central1-a      (default)
#   OAM_LINUX_USER            jeff               (default -- gcloud IAM user)
#   OAM_REMOTE_DIR            oam-build          (default -- under remote $HOME)
#   OAM_KEEP_VM=1             leave the VM running on exit even if this
#                             script started it (default: stop what we start;
#                             a VM found already RUNNING is always left alone)
#
# Prereqs (same as yaw's IAP path):
#   - gcloud CLI authenticated; identity has roles/iap.tunnelResourceAccessor
#     + compute.instances.get/use (and start/stop for the lifecycle step).
#   - Firewall allows TCP:22 from 35.235.240.0/20 (IAP range).
#   - VM image: build-essential + Node 22 (already there for yaw builds).
#     rustup is auto-installed by scripts/build-remote.sh prep on first run.
# =============================================================================

set -euo pipefail

MODE="release"
for arg in "$@"; do
  case "$arg" in
    --mode=release|--mode=measure|--mode=bench) MODE="${arg#--mode=}" ;;
    -h|--help)
      awk 'NR==1{next} !/^#/{exit} {sub(/^# ?/, ""); print}' "$0"
      exit 0
      ;;
    *) echo "unknown arg: $arg (want --mode=release|measure|bench)" >&2; exit 1 ;;
  esac
done

PROJECT="${OAM_GCP_PROJECT:-yaw-labs-prod}"
INSTANCE="${OAM_GCP_BUILDER_INSTANCE:-yaw-linux-builder}"
ZONE="${OAM_GCP_BUILDER_ZONE:-us-central1-a}"
LINUX_USER="${OAM_LINUX_USER:-jeff}"
REMOTE_DIR="${OAM_REMOTE_DIR:-oam-build}"
LINUX_FAST="${OAM_LINUX_FAST:-0}"

RED='\033[0;31m'; GRN='\033[0;32m'; YEL='\033[1;33m'; CYA='\033[1;36m'; NC='\033[0m'
ok()  { echo -e "${GRN}  [ok]${NC} $*" >&2; }
warn(){ echo -e "${YEL}  [warn]${NC} $*" >&2; }
fail(){ echo -e "${RED}  [fail]${NC} $*" >&2; exit 1; }
step(){ echo -e "\n${CYA}=== $* ===${NC}" >&2; }

command -v gcloud >/dev/null 2>&1 || fail "gcloud CLI not found on this box"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

RUNID="$(date +%Y%m%d-%H%M%S)"
STAGE_DIR="$(mktemp -d -t oam-iap-build-$RUNID-XXXXXX)"
ARTIFACTS_DIR="$STAGE_DIR/artifacts"
mkdir -p "$STAGE_DIR/logs" "$ARTIFACTS_DIR"

# --- VM lifecycle ------------------------------------------------------------
# Start the VM if it is stopped, and stop it again on exit ONLY if we were
# the ones who started it (a VM someone else is using stays up). OAM_KEEP_VM=1
# keeps it running either way (useful when iterating: the next run skips the
# ~30s boot).
WE_STARTED_VM=0
VM_STATUS="$(gcloud compute instances describe "$INSTANCE" --zone="$ZONE" --project="$PROJECT" \
  --format='value(status)' 2>/dev/null)" \
  || fail "instance $INSTANCE not found in $ZONE ($PROJECT)"
if [ "$VM_STATUS" != "RUNNING" ]; then
  step "Start VM $INSTANCE (status: $VM_STATUS)"
  gcloud compute instances start "$INSTANCE" --zone="$ZONE" --project="$PROJECT" >&2
  WE_STARTED_VM=1
  ok "VM started"
else
  ok "VM already RUNNING -- will leave it running on exit"
fi
stop_vm() {
  if [ "$WE_STARTED_VM" = "1" ] && [ "${OAM_KEEP_VM:-0}" != "1" ]; then
    warn "stopping VM $INSTANCE (started by this run; OAM_KEEP_VM=1 to keep)"
    gcloud compute instances stop "$INSTANCE" --zone="$ZONE" --project="$PROJECT" --async >&2 || true
  fi
}

# --- IAP tunnel machinery (mirrors yaw's, see that script for the full
# rationale) -------------------------------------------------------------------
# Short version: gcloud's own ssh wrapper uses plink on Windows, which has no
# keepalive, and the IAP tunnel drops idle connections after ~5 min. So: run
# one long-lived `gcloud compute start-iap-tunnel` in the background for the
# whole build, parse the local port it picked, and drive plain OpenSSH
# (which has ServerAliveInterval) over localhost. Remote work is split into
# short per-step ssh invocations. The trap kills the tunnel on any exit.
IAP_TUNNEL_LOG="$(mktemp -t oam-iap-tunnel-XXXXXX.log)"
IAP_TUNNEL_PID=""
IAP_TUNNEL_PORT=""
IAP_SSH_KEY="${HOME}/.ssh/google_compute_engine"
start_iap_tunnel() {
  # sshd is NOT up the instant `instances start` returns: the guest keeps
  # booting for ~30-60s, IAP reports 4003 'failed to connect to backend',
  # and gcloud EXITS rather than retrying. Respawn the tunnel across the
  # boot window (observed 2026-08-04: two consecutive leg failures from
  # this race, then first-try success ~60s after boot).
  local attempt=1 max_attempts=8 waited=0
  while :; do
    ok "starting IAP tunnel to $INSTANCE (gcloud start-iap-tunnel, attempt $attempt/$max_attempts)..."
    : >"$IAP_TUNNEL_LOG"
    gcloud compute start-iap-tunnel "$INSTANCE" 22 \
      --local-host-port=localhost:0 --zone="$ZONE" --project="$PROJECT" \
      >"$IAP_TUNNEL_LOG" 2>&1 &
    IAP_TUNNEL_PID=$!
    # Wait for "Listening on port [NNNN]." -- the canonical bound-and-serving
    # signal -- with a 15s ceiling, then fall back to a port-reachability probe
    # ("Picking local unused port [NNNN]" + TCP connect poll). Some gcloud
    # versions hang in their internal tunnel self-test while the port already
    # serves fine.
    local died=0
    waited=0
    while ! grep -q "Listening on port" "$IAP_TUNNEL_LOG" 2>/dev/null; do
      sleep 1
      waited=$((waited + 1))
      if [ "$waited" -ge 15 ]; then
        warn "gcloud did not log 'Listening on port' within 15s -- falling back to port-reachability probe"
        break
      fi
      if ! kill -0 "$IAP_TUNNEL_PID" 2>/dev/null; then
        died=1
        break
      fi
    done
    if [ "$died" = "1" ]; then
      if [ "$attempt" -ge "$max_attempts" ]; then
        # Inline the log tail: cleanup rm's the file, so a path alone
        # destroys the evidence the operator needs.
        fail "IAP tunnel process exited before listening on all $max_attempts attempts. Last output: $(tail -3 "$IAP_TUNNEL_LOG" 2>/dev/null | tr '\n' ' ')"
      fi
      warn "tunnel process exited before listening (sshd likely still booting) -- retrying in 15s"
      attempt=$((attempt + 1))
      sleep 15
      continue
    fi
    break
  done
  IAP_TUNNEL_PORT=$(awk -F'[][]' '/Listening on port/ {print $2; exit}' "$IAP_TUNNEL_LOG")
  if [ -z "$IAP_TUNNEL_PORT" ]; then
    IAP_TUNNEL_PORT=$(awk -F'[][]' '/Picking local unused port/ {print $2; exit}' "$IAP_TUNNEL_LOG")
    [ -n "$IAP_TUNNEL_PORT" ] || fail "could not parse port from $IAP_TUNNEL_LOG"
    local port_waited=0
    while ! timeout 2 bash -c "echo > /dev/tcp/127.0.0.1/$IAP_TUNNEL_PORT" 2>/dev/null; do
      sleep 1
      port_waited=$((port_waited + 1))
      if [ "$port_waited" -ge 60 ]; then
        fail "IAP tunnel port $IAP_TUNNEL_PORT did not become reachable within 60s. Log: $IAP_TUNNEL_LOG"
      fi
      if ! kill -0 "$IAP_TUNNEL_PID" 2>/dev/null; then
        fail "IAP tunnel process exited before port became reachable. Log: $IAP_TUNNEL_LOG"
      fi
    done
    ok "IAP tunnel up (port-reachability): localhost:$IAP_TUNNEL_PORT -> $INSTANCE:22"
  else
    ok "IAP tunnel up: localhost:$IAP_TUNNEL_PORT -> $INSTANCE:22 (waited ${waited}s)"
  fi
}
stop_iap_tunnel() {
  if [ -n "$IAP_TUNNEL_PID" ] && kill -0 "$IAP_TUNNEL_PID" 2>/dev/null; then
    kill "$IAP_TUNNEL_PID" 2>/dev/null || true
    wait "$IAP_TUNNEL_PID" 2>/dev/null || true
    ok "IAP tunnel stopped (pid $IAP_TUNNEL_PID, port $IAP_TUNNEL_PORT)"
  fi
  rm -f "$IAP_TUNNEL_LOG"
}
cleanup() { stop_iap_tunnel; stop_vm; }
trap cleanup EXIT

# Built INSIDE the helpers, not at script scope: $IAP_TUNNEL_PORT is set by
# start_iap_tunnel() and bash captures values at expansion time. Port flag is
# -o Port= (NOT -p/-P) -- the only spelling portable across ssh and scp
# including OpenSSH 10.2 on Windows.
_ssh_opts() {
  echo \
    -o "StrictHostKeyChecking=accept-new" \
    -o "ServerAliveInterval=30" \
    -o "ServerAliveCountMax=10" \
    -o "ConnectTimeout=10" \
    -o "IdentitiesOnly=yes" \
    -o "UserKnownHostsFile=${HOME}/.ssh/google_compute_known_hosts" \
    -i "$IAP_SSH_KEY" \
    -o "Port=$IAP_TUNNEL_PORT"
}
gcp_ssh(){  ssh $(_ssh_opts) "${LINUX_USER}@localhost" "$@"; }
gcp_scp_to(){ scp $(_ssh_opts) "$1" "${LINUX_USER}@localhost:$2"; }
gcp_scp_from(){  # gcp_scp_from <remote-glob-under-REMOTE_DIR> <local-dir>
  # scp can't expand server-side globs; tar on the remote side can (the whole
  # command is one remote-shell invocation), piped back over the tunnel.
  local pattern="$1" local_dir="$2" parent_dir glob
  case "$pattern" in
    */*) parent_dir="${pattern%/*}"; glob="${pattern##*/}" ;;
    *)   parent_dir=".";             glob="$pattern" ;;
  esac
  ssh $(_ssh_opts) "${LINUX_USER}@localhost" "cd $REMOTE_DIR/$parent_dir && tar czf - $glob" \
    | tar xzf - -C "$local_dir"
}

# Ship the WORKING TREE via tar (uncommitted release fixes should build),
# excluding the bulky/host-local dirs. On the remote side, everything EXCEPT
# target/ is replaced: xtask resolves the oam binary repo-relative
# (target/<profile>/oam), and preserving target/ keeps the dep + rusty_v8
# build cache warm -- a full cold build on the VM costs ~30 min, an
# incremental one minutes.
TAR_EXCLUDES=(--exclude=./.git --exclude=./target --exclude=./node_modules --exclude=./dist --exclude='*.log')
sync_src(){
  ok "sync source -> $INSTANCE:$REMOTE_DIR (via IAP; remote target/ preserved)"
  local t; t="$(mktemp "$STAGE_DIR/src-XXXXXX.tar.gz")"
  ( cd "$REPO_DIR" && tar czf "$t" "${TAR_EXCLUDES[@]}" . )
  gcp_scp_to "$t" "oam-src.tar.gz"
  gcp_ssh "mkdir -p $REMOTE_DIR && cd $REMOTE_DIR && find . -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} + && tar xzf ~/oam-src.tar.gz && rm -f ~/oam-src.tar.gz"
  rm -f "$t"
}

# remote_step <dispatch>: one short ssh invocation per build-remote.sh
# dispatch, log captured per step, tail surfaced on failure.
remote_step(){
  local dispatch="$1"
  gcp_ssh "cd $REMOTE_DIR && bash scripts/build-remote.sh $dispatch" \
    > "$STAGE_DIR/logs/$dispatch.log" 2>&1 \
    || { tail -30 "$STAGE_DIR/logs/$dispatch.log" >&2; fail "remote '$dispatch' failed -- see $STAGE_DIR/logs/$dispatch.log"; }
  ok "remote $dispatch ok"
}

# remote_step_advisory <dispatch>: same, but a failure warns instead of
# aborting. node-compat.yml/bench.yml parity: measurement scorecards and
# bench results upload even when a gate trips (they are written BEFORE the
# gate exits non-zero -- discarding them on failure loses the measurement).
remote_step_advisory(){
  local dispatch="$1"
  if gcp_ssh "cd $REMOTE_DIR && bash scripts/build-remote.sh $dispatch" \
    > "$STAGE_DIR/logs/$dispatch.log" 2>&1; then
    ok "remote $dispatch ok"
  else
    tail -30 "$STAGE_DIR/logs/$dispatch.log" >&2
    warn "remote '$dispatch' failed (advisory -- continuing; see $STAGE_DIR/logs/$dispatch.log)"
  fi
}

# --- preflight ----------------------------------------------------------------
step "Run $RUNID -- oam linux-x64 --mode=$MODE on $INSTANCE via IAP"
( cd "$REPO_DIR" && git diff --quiet HEAD ) || warn "working tree dirty -- uncommitted changes WILL ship"

step "Start IAP tunnel + verify SSH on $INSTANCE"
start_iap_tunnel
# Retry the roundtrip probe: after a cold VM start, OS Login / metadata key
# propagation can lag the tunnel by a few seconds. 10 x 6s covers a cold
# boot; a real auth problem fails all ten.
IAP_PROBE_OK=0
for IAP_PROBE_ATTEMPT in 1 2 3 4 5 6 7 8 9 10; do
  if gcp_ssh "true" >/dev/null 2>&1; then
    IAP_PROBE_OK=1
    break
  fi
  [ "$IAP_PROBE_ATTEMPT" -lt 10 ] && { warn "IAP SSH probe failed (attempt $IAP_PROBE_ATTEMPT/10) -- retrying in 6s..."; sleep 6; }
done
if [ "$IAP_PROBE_OK" -eq 1 ]; then
  ok "IAP SSH roundtrip ok (tunnel localhost:$IAP_TUNNEL_PORT)"
else
  fail "IAP tunnel is up (port $IAP_TUNNEL_PORT) but ssh 'true' failed after 10 attempts. Check (1) $IAP_SSH_KEY exists, (2) OS Login accepted the key for $LINUX_USER, (3) google_compute_known_hosts is not stale."
fi

# --- build --------------------------------------------------------------------
sync_src
remote_step prep

case "$MODE" in
  release)
    if [ "$LINUX_FAST" = "1" ]; then
      warn "OAM_LINUX_FAST=1 -- skipping remote gate/test/conformance (rebuild-only; NOT for a real release)"
    else
      remote_step gate
      remote_step test
      remote_step conformance
      # node-compat.yml's ubuntu skip-ratchet job was GATING; with GHA gone,
      # this release leg is the one place Linux node-suite still gates.
      remote_step node-suite
    fi
    remote_step build
    step "Pull artifacts"
    gcp_scp_from "dist/oam-x86_64-unknown-linux-gnu" "$ARTIFACTS_DIR/"
    ;;
  measure)
    # Advisory (node-compat.yml measure parity, continue-on-error): xtask
    # writes scorecards before exiting non-zero, so pull them even when a
    # gate trips -- the numbers ARE the product of this mode.
    remote_step_advisory conformance
    remote_step_advisory node-suite
    step "Pull scorecards"
    mkdir -p "$ARTIFACTS_DIR/linux-x64"
    gcp_scp_from "conformance/scorecard.json"            "$ARTIFACTS_DIR/linux-x64/"
    gcp_scp_from "conformance/node-suite-scorecard.json" "$ARTIFACTS_DIR/linux-x64/"
    gcp_scp_from "CONFORMANCE.md"                        "$ARTIFACTS_DIR/linux-x64/"
    gcp_scp_from "CONFORMANCE-NODE.md"                   "$ARTIFACTS_DIR/linux-x64/"
    ;;
  bench)
    remote_step bench
    # Advisory: an io_uring A/B failure must not discard the bench results
    # already written (bench.yml uploaded artifacts if: always()).
    remote_step_advisory io-uring-ab
    step "Pull bench results"
    mkdir -p "$ARTIFACTS_DIR/linux-x64"
    gcp_scp_from "BENCHMARKS.md"      "$ARTIFACTS_DIR/linux-x64/"
    gcp_scp_from "bench/results.json" "$ARTIFACTS_DIR/linux-x64/"
    cp "$STAGE_DIR/logs/io-uring-ab.log" "$ARTIFACTS_DIR/linux-x64/io-uring-ab.log"
    ;;
esac

# --- verify -------------------------------------------------------------------
step "Verify staged artifacts"
find "$ARTIFACTS_DIR" -type f -exec ls -lh {} \; >&2
case "$MODE" in
  release) [ -f "$ARTIFACTS_DIR/oam-x86_64-unknown-linux-gnu" ] || fail "no linux binary staged" ;;
  measure) [ -f "$ARTIFACTS_DIR/linux-x64/node-suite-scorecard.json" ] || fail "no scorecard staged" ;;
  bench)   [ -f "$ARTIFACTS_DIR/linux-x64/results.json" ] || fail "no bench results staged" ;;
esac
ok "all required artifacts staged under $ARTIFACTS_DIR"

# LAST stdout line = the artifact dir, for ART=$(...) capture.
echo "$ARTIFACTS_DIR"
