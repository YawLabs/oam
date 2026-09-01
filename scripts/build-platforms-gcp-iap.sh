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
#   OAM_GCP_BUILDER_ZONE      us-west1-b         (default -- moved from
#                                                us-central1-a 2026-08-06, that
#                                                region had no e2-highmem-4
#                                                capacity in ANY zone)
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
    --mode=release|--mode=measure|--mode=bench|--mode=surface-gaps) MODE="${arg#--mode=}" ;;
    -h|--help)
      awk 'NR==1{next} !/^#/{exit} {sub(/^# ?/, ""); print}' "$0"
      exit 0
      ;;
    *) echo "unknown arg: $arg (want --mode=release|measure|bench|surface-gaps)" >&2; exit 1 ;;
  esac
done

PROJECT="${OAM_GCP_PROJECT:-yaw-labs-prod}"
INSTANCE="${OAM_GCP_BUILDER_INSTANCE:-yaw-linux-builder}"
ZONE="${OAM_GCP_BUILDER_ZONE:-us-west1-b}"
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
# shellcheck source=lib/iap-helpers.sh
. "$SCRIPT_DIR/lib/iap-helpers.sh"
# shellcheck source=lib/src-sync.sh
. "$SCRIPT_DIR/lib/src-sync.sh"

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
  # `instances start` can fail with a zone-level `resource_availability` error
  # ("does not have enough resources available"), which is TRANSIENT -- the
  # same start succeeds minutes later. The old code neither retried nor
  # checked: it printed "VM started" unconditionally and the run then died in
  # the tunnel step with a misleading connection error. Retry with backoff,
  # and confirm RUNNING before believing it.
  VM_START_OK=0
  for VM_ATTEMPT in 1 2 3 4 5 6; do
    if gcloud compute instances start "$INSTANCE" --zone="$ZONE" --project="$PROJECT" >&2 2>/dev/null; then
      if [ "$(gcloud compute instances describe "$INSTANCE" --zone="$ZONE" --project="$PROJECT" \
              --format='value(status)' 2>/dev/null)" = "RUNNING" ]; then
        VM_START_OK=1
        break
      fi
    fi
    [ "$VM_ATTEMPT" -lt 6 ] && {
      warn "VM start failed (attempt $VM_ATTEMPT/6) -- usually zone capacity for $(gcloud compute instances describe "$INSTANCE" --zone="$ZONE" --project="$PROJECT" --format='value(machineType.basename())' 2>/dev/null). Retrying in 60s..."
      sleep 60
    }
  done
  [ "$VM_START_OK" = "1" ] || fail "could not start $INSTANCE in $ZONE after 6 attempts -- the zone is out of capacity for this machine type. Wait and re-run, or move the builder to another zone."
  WE_STARTED_VM=1
  ok "VM started"
  # `instances start` returning RUNNING means the API finished, NOT the guest:
  # sshd comes up ~15-60s later. gcloud`s start-iap-tunnel does a real backend
  # round-trip BEFORE it binds anything, and against a still-booting guest that
  # round-trip EXITS with a 4003 instead of retrying -- so the first tunnel
  # attempt was near-guaranteed to fail on a cold VM, burning ~30s and printing
  # a scary warning for an entirely expected condition.
  #
  # The serial console reads straight off the compute API -- no tunnel, no ssh --
  # so it is the one guest-side signal available before the chicken-and-egg
  # breaks. GCE resets the serial buffer each boot, so a match is from THIS boot.
  wait_for_guest_sshd() {
    local waited=0 max=180 serial
    while [ "$waited" -lt "$max" ]; do
      # Capture then match, rather than `gcloud | grep -q`: under `set -o
      # pipefail` grep short-circuits on a match, gcloud dies on SIGPIPE, and
      # the pipeline reports FAILURE -- turning a successful detection into a
      # false negative that would burn the whole ${max}s window every cold boot.
      #
      # Refetching the WHOLE buffer each poll is deliberate. gcloud offers
      # --start=<byte offset> to fetch only new output, but the offset would
      # have to be derived from the length of a command substitution, which
      # strips trailing newlines -- so the offset drifts and a poll can skip
      # the very line being matched. The buffer is ~150KB and sshd normally
      # lands on the first or second poll; trading a correct detection for
      # those bytes is not worth it.
      serial="$(gcloud compute instances get-serial-port-output "$INSTANCE" \
        --zone="$ZONE" --project="$PROJECT" 2>/dev/null || true)"
      if sshd_banner_seen "$serial"; then
        ok "guest sshd up (serial console, ${waited}s after start)"
        return 0
      fi
      sleep 10
      waited=$((waited + 10))
    done
    # Never fatal: the tunnel retry loop is still the real backstop.
    warn "no sshd banner on the serial console after ${max}s -- proceeding (the tunnel loop still retries)"
    return 0
  }
  step "Wait for guest sshd on $INSTANCE"
  wait_for_guest_sshd
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
# Returns 0 once the tunnel is bound AND accepting; non-zero for every failure
# mode, so start_iap_tunnel can retry all of them identically.
iap_tunnel_serving() {
  local waited=0
  # 90s, not 15s. gcloud does a real backend round-trip BEFORE it binds anything
  # (surface/compute/start_iap_tunnel.py -> IapTunnelProxyServerHelper.Run:
  # _TestConnection(), THEN _OpenLocalTcpSockets(), THEN the log line), and that
  # self-test takes well over 15s even against a warm, fully booted VM.
  local budget=90
  while ! grep -q "Listening on port" "$IAP_TUNNEL_LOG" 2>/dev/null; do
    sleep 1
    waited=$((waited + 1))
    if [ "$waited" -ge "$budget" ]; then
      warn "gcloud did not log 'Listening on port' within ${budget}s -- falling back to port-reachability probe"
      break
    fi
    kill -0 "$IAP_TUNNEL_PID" 2>/dev/null || return 1
  done
  IAP_TUNNEL_PORT="$(iap_parse_listening_port "$IAP_TUNNEL_LOG" || true)"
  if [ -n "$IAP_TUNNEL_PORT" ]; then
    ok "IAP tunnel up: localhost:$IAP_TUNNEL_PORT -> $INSTANCE:22 (waited ${waited}s)"
    return 0
  fi
  # Some gcloud versions hang in their internal tunnel self-test while the port
  # already serves fine, so a bound-but-unlogged port still counts as up.
  IAP_TUNNEL_PORT="$(iap_parse_picked_port "$IAP_TUNNEL_LOG" || true)"
  [ -n "$IAP_TUNNEL_PORT" ] || return 1
  local port_waited=0
  while ! timeout 2 bash -c "echo > /dev/tcp/127.0.0.1/$IAP_TUNNEL_PORT" 2>/dev/null; do
    sleep 1
    port_waited=$((port_waited + 1))
    [ "$port_waited" -ge 60 ] && return 1
    kill -0 "$IAP_TUNNEL_PID" 2>/dev/null || return 1
  done
  ok "IAP tunnel up (port-reachability): localhost:$IAP_TUNNEL_PORT -> $INSTANCE:22"
  return 0
}

start_iap_tunnel() {
  # sshd is NOT up the instant `instances start` returns: the guest keeps
  # booting for ~30-60s, IAP reports 4003 'failed to connect to backend', and
  # gcloud EXITS rather than retrying.
  #
  # EVERY failure mode retries, which is the whole point. The boot race does
  # not always kill the tunnel inside the first 15s -- it can survive that,
  # fail its self-test, and die during the port probe instead. That path used
  # to abort the leg outright, so a race the retry loop existed for took the
  # one branch that did not retry (observed twice, 2026-08-05).
  local attempt=1 max_attempts=8
  while :; do
    ok "starting IAP tunnel to $INSTANCE (gcloud start-iap-tunnel, attempt $attempt/$max_attempts)..."
    : >"$IAP_TUNNEL_LOG"
    # PYTHONUNBUFFERED is load-bearing, not a nicety. gcloud is Python, and of
    # everything it prints only `Listening on port [N].` goes to STDOUT
    # (log.out.Print); the rest -- `Picking local unused port`, `Testing if
    # tunnel connection works` -- is stderr (log.status.Print). Redirecting
    # stdout into a file makes Python block-buffer it, so the one line this
    # function waits for sat unflushed in the buffer indefinitely. Verified
    # 2026-08-22: a fully working tunnel served SSH for 160s+ having never
    # written that line to the log, so the fallback ran on EVERY invocation.
    PYTHONUNBUFFERED=1 gcloud compute start-iap-tunnel "$INSTANCE" 22 \
      --local-host-port=localhost:0 --zone="$ZONE" --project="$PROJECT" \
      >"$IAP_TUNNEL_LOG" 2>&1 &
    IAP_TUNNEL_PID=$!
    if iap_tunnel_serving; then
      return 0
    fi
    # Never leave a half-open tunnel behind for the next attempt to trip on.
    if kill -0 "$IAP_TUNNEL_PID" 2>/dev/null; then
      kill "$IAP_TUNNEL_PID" 2>/dev/null || true
      wait "$IAP_TUNNEL_PID" 2>/dev/null || true
    fi
    if [ "$attempt" -ge "$max_attempts" ]; then
      # Inline the log tail: cleanup rm's the file, so a path alone destroys
      # the evidence the operator needs.
      fail "IAP tunnel never served on any of $max_attempts attempts. Last output: $(tail -3 "$IAP_TUNNEL_LOG" 2>/dev/null | tr '
' ' ')"
    fi
    warn "tunnel attempt $attempt did not serve (sshd likely still booting) -- retrying in 15s"
    attempt=$((attempt + 1))
    sleep 15
  done
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

# Ship the WORKING TREE (uncommitted release fixes should build), asking git
# what the tree IS rather than hand-maintaining a list of what it is not -- see
# lib/src-sync.sh for why, and for the 11.6 GB release this cost. On the remote
# side, everything EXCEPT target/ is replaced: xtask resolves the oam binary
# repo-relative (target/<profile>/oam), and preserving target/ keeps the dep +
# rusty_v8 build cache warm -- a full cold build on the VM costs ~30 min, an
# incremental one minutes.
#
# Every step carries an explicit `|| fail`. Without them a tar or scp that died
# inside the subshell ended the run through errexit, which prints NOTHING: the
# 2026-08-31 operator saw only the EXIT trap's "stopping VM" line and had no
# indication of which step had failed, or that a step had failed at all.
sync_src(){
  ok "sync source -> $INSTANCE:$REMOTE_DIR (via IAP; remote target/ preserved)"
  local t; t="$(mktemp "$STAGE_DIR/src-XXXXXX.tar.gz")" \
    || fail "could not create a staging tarball under $STAGE_DIR"
  write_src_tarball "$REPO_DIR" "$t" \
    || fail "could not pack the source tree from $REPO_DIR -- is git healthy here? ('git ls-files' must work)"

  # Weigh it BEFORE pushing it. This is the cheap check that turns a 35-minute
  # upload of build junk into an immediate, self-diagnosing abort.
  local bytes; bytes="$(wc -c <"$t" 2>/dev/null | tr -dc '0-9')"
  if src_tarball_over_ceiling "$bytes"; then
    warn "biggest paths the sync would ship:"
    src_largest_paths "$REPO_DIR" 10 >&2 || true
    fail "source tarball is $((bytes / 1048576))MB, over the ${OAM_SRC_TARBALL_MAX_MB}MB ceiling -- something is shipping build output. The tree git reports should be a few MB; check the paths above, then either clean them up or raise OAM_SRC_TARBALL_MAX_MB if the growth is legitimate."
  fi
  # KB, not MB: the healthy tree is ~1.7MB, and an MB readout would round most
  # of that to a "0MB" that reads like a broken measurement.
  ok "source tarball: $((bytes / 1024))KB (ceiling ${OAM_SRC_TARBALL_MAX_MB}MB)"

  gcp_scp_to "$t" "oam-src.tar.gz" \
    || fail "scp of the source tarball to $INSTANCE failed (tunnel localhost:$IAP_TUNNEL_PORT)"
  gcp_ssh "mkdir -p $REMOTE_DIR && cd $REMOTE_DIR && find . -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} + && tar xzf ~/oam-src.tar.gz && rm -f ~/oam-src.tar.gz" \
    || fail "remote extract of the source tarball into $REMOTE_DIR failed"
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

# Disk headroom -- deliberately AFTER sync_src, not before. The reclaim below
# runs `build-remote.sh gc`, which only exists in a tree THIS script has synced:
# checking first would invoke the PREVIOUS run`s build-remote.sh, hit `unknown
# dispatch: gc` on any builder that predates that dispatch, and then hard-fail
# with a message claiming a reclaim that never happened. The sync is seconds and
# frees the old source tree before extracting, so ordering it first costs
# nothing and makes both remote scripts current.
#
# A full builder does not fail fast: cargo runs ~20 min and then dies with
# `rustc-LLVM ERROR: IO failure on output stream` / `os error 28`, which reads
# like a compiler bug. One clean debug+release build of this tree needs ~7GB,
# and target/ accretes across runs (observed 2026-08-22: 39GB, / at 10GB free).
step "Check builder disk headroom"
builder_free_gb(){ gcp_ssh "df -BG --output=avail / | tail -1 | tr -dc '0-9'" 2>/dev/null; }
DISK_FREE_GB="$(builder_free_gb)"
if [ -n "$DISK_FREE_GB" ]; then
  # Tight headroom used to just print advice telling the operator to ssh in and
  # delete things by hand -- a nag that never fixed anything, so the tree kept
  # growing until a run hard-failed here. Reclaim it instead; the build cache
  # survives, so this costs no build time.
  if disk_needs_reclaim "$DISK_FREE_GB"; then
    warn "builder has ${DISK_FREE_GB}GB free on / -- reclaiming accreted cargo output before building"
    gcp_ssh "cd $REMOTE_DIR && bash scripts/build-remote.sh gc" >&2 2>&1 \
      || warn "pre-build reclaim failed -- continuing to the threshold check"
    DISK_FREE_GB="$(builder_free_gb)"
  fi
  # Judge AFTER the reclaim: the cheap fix has already run, so anything still
  # short needs a real one.
  if disk_below_floor "$DISK_FREE_GB"; then
    fail "builder still has only ${DISK_FREE_GB}GB free on / after reclaiming prunable cargo output -- a build needs ~7GB. Grow the boot disk, or ssh in and look for space outside ~/${REMOTE_DIR}/target."
  fi
  [ -n "$DISK_FREE_GB" ] && ok "builder disk headroom: ${DISK_FREE_GB}GB free on /"
fi

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
  surface-gaps)
    remote_step surface-gaps
    step "Pull the ratchet"
    # The WHOLE file comes back, not a linux fragment. The generator merges --
    # it rewrites only platforms[linux] and leaves the sections that arrived in
    # the sync untouched -- so this is "what we sent, with linux re-measured".
    # That is also why the mac and linux legs must run ONE AT A TIME, each
    # starting from the previous one's output: run them concurrently and
    # whichever lands second overwrites the other's section with the stale copy
    # it was sent.
    gcp_scp_from "conformance/surface-gaps.json" "$REPO_DIR/conformance/"
    ;;
esac

# --- verify -------------------------------------------------------------------
step "Verify staged artifacts"
find "$ARTIFACTS_DIR" -type f -exec ls -lh {} \; >&2
case "$MODE" in
  release) [ -f "$ARTIFACTS_DIR/oam-x86_64-unknown-linux-gnu" ] || fail "no linux binary staged" ;;
  measure) [ -f "$ARTIFACTS_DIR/linux-x64/node-suite-scorecard.json" ] || fail "no scorecard staged" ;;
  bench)   [ -f "$ARTIFACTS_DIR/linux-x64/results.json" ] || fail "no bench results staged" ;;
  surface-gaps)
    # Lands in the repo, not the staging dir. Assert linux actually moved: a
    # silently-unchanged section is the exact failure this mode exists to
    # prevent, because the gate would then be reading a host that never ran.
    node -e '
      const g = require(process.argv[1]);
      const l = g.platforms && g.platforms.linux;
      if (!l) { console.error("no linux section in the pulled ratchet"); process.exit(1); }
      console.error(`linux: ${l.counts.present}/${l.counts.nodeExportNames} present, generated against ${l.generatedAgainst}`);
    ' "$REPO_DIR/conformance/surface-gaps.json" || fail "pulled ratchet has no linux section"
    ;;
esac
ok "all required artifacts staged under $ARTIFACTS_DIR"

# Reclaim AFTER the artifacts are staged, never before: this run`s outputs are
# already local, so a prune here cannot cost anything it needs. Doing it every
# run is what stops the tree accreting until the preflight check has to fail a
# release. Advisory -- the leg has already succeeded, and a failed cleanup must
# not retract that.
step "Reclaim builder disk"
gcp_ssh "cd $REMOTE_DIR && bash scripts/build-remote.sh gc" >&2 2>&1   || warn "post-build reclaim failed (non-blocking -- artifacts are already staged)"

# LAST stdout line = the artifact dir, for ART=$(...) capture.
echo "$ARTIFACTS_DIR"
