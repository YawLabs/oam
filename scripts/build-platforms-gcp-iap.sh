#!/bin/bash
# =============================================================================
# oam GCP platform-build orchestrator  (linux-x64 leg)
# =============================================================================
# Runs the Linux leg of the deleted GitHub workflows on the shared GCP Linux
# VM (the same yaw-linux-builder that builds yaw/vew). The VM is reached over
# plain OpenSSH to its EXTERNAL IP first; only when that does not answer does
# the run fall back, loudly, to a gcloud IAP TCP-forwarding tunnel (the only
# transport before 2026-09-25 -- see "ssh transport" below for why it is no
# longer the default). Mirrors yaw's scripts/build-platforms-gcp-iap.sh
# transport machinery; adapted for a Rust workspace and given VM lifecycle
# management (yaw's release.sh owns the VM start/stop there; oam is
# self-contained here). The file name predates direct ssh and is kept because
# release-local.sh, node-compat-measure.sh and bench-platforms.sh call it.
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
#   OAM_KEEP_VM_SCHEDULE=1    leave the VM's instance schedule(s) attached for
#                             the run (default: detach every stop schedule
#                             while the run is going and re-attach on exit --
#                             yaw-linux-builder-autostop stops the VM at 03:00
#                             America/Los_Angeles every day, mid-run or not).
#                             A scheduled stop that lands under a remote step
#                             is then answered like any stop under a step: the
#                             VM is started again (OAM_REMOTE_STEP_ATTEMPTS,
#                             below -- the walk can clone it into another
#                             zone) and is this run's to stop on exit; one
#                             that lands outside a remote step, or on the last
#                             attempt, ends the run.
#   OAM_GCP_FALLBACK_MACHINE_TYPES
#                             n2-highmem-4 n2d-highmem-4 c2d-highmem-4
#                             n1-highmem-4 e2-standard-8 t2d-standard-8
#                             n2-standard-8 n2d-standard-8 c2-standard-8
#                             n1-standard-8 e2-highmem-8
#                             (default -- same-zone machine types tried, in
#                             this order, when the zone has no capacity for
#                             the VM's own type: ZONE_RESOURCE_POOL_EXHAUSTED
#                             held us-west1-b's e2-highmem-4 for the whole
#                             2026-09-30 retry window. The VM is set to the
#                             fallback for the run and set back to its own
#                             type when this script stops it; a second Ctrl-C
#                             during that stop leaves it on the fallback, with
#                             the command to set it back printed first. Each
#                             must boot the VM's disk and NIC as they are --
#                             x86_64, pd-balanced, virtio -- so no ARM
#                             (t2a/c4a) and no c3/c3d/c4/c4d/n4, which need
#                             gVNIC or Hyperdisk; the separate hardware pools
#                             are the point, e2-standard-8 shares the E2 pool
#                             that ran out and sits late for it. Each family
#                             draws on its own regional CPU quota (E2_CPUS,
#                             N2_CPUS, N2D_CPUS, C2D_CPUS, T2D_CPUS; n1 on
#                             CPUS), all ample for one VM here. Empty
#                             disables the fallback.)
#   OAM_REMOTE_STEP_ATTEMPTS  3                  (default -- times a remote
#                                                step is run, in all, when the
#                                                ssh transport under it drops.
#                                                A VM still RUNNING is
#                                                reconnected to, direct again
#                                                or the IAP tunnel; one that is
#                                                TERMINATED or STOPPING --
#                                                stopped under the step by an
#                                                operator, a schedule this run
#                                                could not detach, a host event
#                                                -- is started again by the
#                                                same walk as at the top, once
#                                                a stop in progress has settled
#                                                (3 minutes at most) and the
#                                                postmortem has said who
#                                                stopped it, and is this run's
#                                                to stop on exit from then on.
#                                                Every attempt counts against
#                                                the same number. A step whose
#                                                command EXITS non-zero is
#                                                never retried.)
#   OAM_VM_START_BUDGET_S     1800               (default -- seconds after
#                                                which no further pass over
#                                                those types begins; a pass
#                                                in progress finishes, passes
#                                                are a minute apart, and the
#                                                run then clones the builder
#                                                into another zone (below) or
#                                                fails naming the zone move;
#                                                whole seconds)
#   OAM_GCP_ZONE_FALLBACK     1                  (default -- when the budget is
#                                                spent with every start refused
#                                                for capacity, the run moves to
#                                                another zone of the region
#                                                WITHOUT touching the builder:
#                                                a machine image of it
#                                                (<instance>-img-<run id>:
#                                                disks, metadata, service
#                                                account, scopes, tags, network
#                                                config; the boot disk's copy
#                                                keeps the cargo cache warm),
#                                                then an instance from that
#                                                image, <instance>-<zone>, in
#                                                each candidate zone and each
#                                                machine type of the walk,
#                                                until one boots. The original
#                                                stays stopped on its own type
#                                                and the next run finds it by
#                                                name as before. The clone is
#                                                stopped on exit (left RUNNING
#                                                with OAM_KEEP_VM=1, and the
#                                                stop command printed) and left
#                                                for the operator to delete (the
#                                                command is printed, at the
#                                                clone and again at exit); the
#                                                image is deleted on exit, or
#                                                kept and named, with the hand
#                                                move, when no zone took it. A
#                                                start refused for anything but
#                                                capacity (a backend error, a
#                                                timeout) keeps the run in its
#                                                zone: that is not a stockout.
#                                                A clone is never cloned again.
#                                                0: fail as before, naming the
#                                                zone move.)
#   OAM_GCP_FALLBACK_ZONES    (unset)            (the zones to clone into,
#                                                space-separated, in order;
#                                                default: the other UP zones of
#                                                the builder's region, in
#                                                `gcloud compute zones list`
#                                                order. Each must look like a
#                                                zone name, us-west1-c, and be
#                                                a zone of the builder's region
#                                                -- the clone keeps its network
#                                                config, which is regional; the
#                                                builder's own zone is skipped.)
#   OAM_IAP_SSH_MODE          auto               (default -- direct ssh to the
#                                                VM's external IP, falling back
#                                                to the IAP tunnel, with a
#                                                warning naming the direct-ssh
#                                                error, when that does not
#                                                answer)
#                             direct             (direct only: a direct path
#                                                that does not answer fails the
#                                                run instead of falling back)
#                             tunnel             (IAP tunnel only: the direct
#                                                path is never probed -- the
#                                                pre-2026-09-25 behaviour)
#
# Prereqs:
#   - gcloud CLI authenticated; identity has compute.instances.get (and
#     start/stop for the lifecycle step, instances.setMachineType for the
#     capacity fallback, compute.resourcePolicies.get +
#     instances.{add,remove}ResourcePolicies for the instance-schedule step,
#     and for the zone fallback compute.machineImages.{create,delete,
#     useReadOnly}, compute.instances.create in the other zones,
#     compute.zones.list, plus iam.serviceAccounts.actAs on the builder's
#     service account, which the clone runs as -- roles/compute.instanceAdmin.v1
#     with roles/iam.serviceAccountUser covers it).
#   - Direct path: the VM has an external IP, and TCP:22 on it is reachable
#     from this host (the default network's default-allow-ssh rule opens it
#     to 0.0.0.0/0); ~/.ssh/google_compute_engine is a key the VM accepts for
#     OAM_LINUX_USER (any earlier `gcloud compute ssh` published it).
#   - IAP fallback: roles/iap.tunnelResourceAccessor + compute.instances.use,
#     and a firewall rule allowing TCP:22 from 35.235.240.0/20 (IAP range).
#   - VM image: build-essential, curl, outbound HTTPS to nodejs.org.
#     rustup is auto-installed by scripts/build-remote.sh prep on first run,
#     and so is the conformance oracle: exactly the Node in .node-version,
#     cached under ~/.cache/oam-node (the image's own Node 22 is NOT used --
#     see scripts/lib/node-pin.sh).
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
SSH_MODE="${OAM_IAP_SSH_MODE:-auto}"

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

# Before anything touches the VM: a typo here must not cost a VM start.
ssh_mode_valid "$SSH_MODE" \
  || fail "invalid OAM_IAP_SSH_MODE='$SSH_MODE' (want auto|direct|tunnel)"
case "${OAM_VM_START_BUDGET_S:-1800}" in
  '' | *[!0-9]*) fail "invalid OAM_VM_START_BUDGET_S='${OAM_VM_START_BUDGET_S:-}' (want whole seconds, e.g. 1800)" ;;
esac
case "${OAM_REMOTE_STEP_ATTEMPTS:-3}" in
  '' | 0 | *[!0-9]*) fail "invalid OAM_REMOTE_STEP_ATTEMPTS='${OAM_REMOTE_STEP_ATTEMPTS:-}' (want a whole number of attempts, 1 or more)" ;;
esac
ZONE_FALLBACK="${OAM_GCP_ZONE_FALLBACK:-1}"
case "$ZONE_FALLBACK" in
  0 | 1) ;;
  *) fail "invalid OAM_GCP_ZONE_FALLBACK='$ZONE_FALLBACK' (want 0 or 1)" ;;
esac
# Empty and unset both mean "the region's other UP zones"; a list is checked
# name by name, since a typo would first show up as a refused clone half an
# hour into a walk.
FALLBACK_ZONES="${OAM_GCP_FALLBACK_ZONES:-}"
for fz in $FALLBACK_ZONES; do
  gce_zone_name_valid "$fz" \
    || fail "invalid zone '$fz' in OAM_GCP_FALLBACK_ZONES='$FALLBACK_ZONES' (want zone names like us-west1-c, space-separated)"
done
unset fz

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
# 1 when the VM came back RUNNING by someone else's hand after a stop under a
# step (restart_builder): a guest seconds into its boot, which the connect
# step then gives the boot budget it gives this run's own starts -- without
# the ownership. connect_builder clears it.
VM_JUST_BOOTED=0
# The builder has moved zones once (us-central1-a -> us-west1-b, 2026-08-06,
# for capacity), and the way out of a zone with none is to move it again --
# so a stale zone default finds the instance wherever it is, as yaw's sibling
# script does, instead of failing the release on a name that exists.
if ! DESCRIBE_ERR="$(gcloud compute instances describe "$INSTANCE" --zone="$ZONE" --project="$PROJECT" \
       --format='value(name)' 2>&1 >/dev/null)"; then
  # Only "not in this zone" goes looking; an expired credential, a 403 or a
  # network failure is reported in gcloud's words, not as a zone move.
  DESCRIBE_MSG="$(gcloud_error_message "$DESCRIBE_ERR" || echo '(gcloud printed nothing)')"
  case "${DESCRIBE_ERR//$'\r'/}" in
    *"instances/$INSTANCE' was not found"*) ;;
    *) fail "cannot read $INSTANCE in $ZONE ($PROJECT): $DESCRIBE_MSG" ;;
  esac
  # Anchored: `gcloud topic filters` documents `=` as equality for most APIs
  # and a deprecated pattern-match for some, so `~^NAME$` is the one spelling
  # that is exact either way, and a builder whose name is a prefix of
  # another's cannot find it.
  FOUND_ZONE="$(gcloud compute instances list --project="$PROJECT" --filter="name~^${INSTANCE}\$" \
    --format='value(zone.basename())' 2>"$STAGE_DIR/logs/instances-list.err" | head -1 | tr -d '\r' || true)"
  case "$FOUND_ZONE" in
    '' | */* | *[[:space:]]*)
      LIST_MSG="$(gcloud_error_message "$(cat "$STAGE_DIR/logs/instances-list.err" 2>/dev/null)" || true)"
      fail "instance $INSTANCE not found in $ZONE ($DESCRIBE_MSG), nor in any zone of $PROJECT${LIST_MSG:+ ($LIST_MSG)}${FOUND_ZONE:+ (zone reading: '$FOUND_ZONE')}" ;;
  esac
  warn "instance $INSTANCE is not in $ZONE but in $FOUND_ZONE -- using that (OAM_GCP_BUILDER_ZONE=$FOUND_ZONE silences this)"
  ZONE="$FOUND_ZONE"
fi
# The zone fallback clones within the builder's region: the clone keeps the
# builder's network config, which is regional, and the region's own zones are
# what `zones list` is asked for. A zone of another region in the operator's
# list would reach `instances create` and be refused there once per machine
# type of the walk, each refusal worded as the type's. Checked now that the
# builder's zone is final (the lookup above can move it), and still before
# anything touches the VM.
for fz in $FALLBACK_ZONES; do
  [ "${fz%-*}" = "${ZONE%-*}" ] \
    || fail "zone '$fz' in OAM_GCP_FALLBACK_ZONES='$FALLBACK_ZONES' is not in ${ZONE%-*}, the region of $INSTANCE's zone $ZONE -- the zone fallback clones within the region (want other zones of ${ZONE%-*})"
done
unset fz
# vm_describe <field> -- `describe --format=value(<field>)`, CR-stripped. A
# blank or failed answer is asked again, three tries two seconds apart, before
# it comes back blank. gcloud answers blank for a moment during an auth refresh
# or a 5xx, and one such answer used to read as "unreadable" and cost a whole
# pass -- with no budget left, the release. The script suite met the same
# shape: a stubbed read, racing the box's scanner under a full ci-local.sh
# run, answered blank once and failed the gate. The natIP reader below
# already retries this way. A field that is legitimately blank
# (resourcePolicies) does not go through here.
vm_describe() {
  local v try
  for try in 1 2 3; do
    v="$(gcloud compute instances describe "$INSTANCE" --zone="$ZONE" --project="$PROJECT" \
      --format="value($1)" 2>/dev/null | tr -d '\r' || true)"
    if [ -n "$v" ]; then printf '%s' "$v"; return 0; fi
    if [ "$try" -lt 3 ]; then sleep 2; fi
  done
  return 1
}
VM_STATUS="$(vm_describe status || true)"
[ -n "$VM_STATUS" ] || fail "could not read the status of $INSTANCE in $ZONE ($PROJECT)"
# A run whose zone had no capacity clones the builder into another zone as
# `<instance>-<zone>` (the zone fallback, below) and leaves the clone stopped
# for the operator to delete. Its disk costs money for as long as it exists,
# and nothing else would ever mention it again, so every run looks for one
# before it starts anything. Best effort: a list that fails must not stop a
# release.
LEFTOVER_CLONES="$(gcloud compute instances list --project="$PROJECT" \
  --filter="name~^${INSTANCE}-[a-z]+-[a-z]+[0-9]+-[a-z]\$" \
  --format='value(name,zone.basename(),status)' 2>/dev/null | tr -d '\r' || true)"
while IFS=$'\t' read -r clone_name clone_zone clone_status; do
  [ -n "$clone_name" ] || continue
  warn "a clone of $INSTANCE from an earlier run is still there: $clone_name in ${clone_zone:-?} (${clone_status:-?}) -- its disk costs money while it exists; when nothing needs it, delete it: gcloud compute instances delete $clone_name --zone=${clone_zone:-<zone>} --project=$PROJECT"
done <<<"$LEFTOVER_CLONES"
unset clone_name clone_zone clone_status
# The machine type this run found the VM with, and the one it is set to now.
# They differ only while a capacity fallback is in effect; restore_machine_type
# (on EXIT, from before the first change) puts the VM back.
ORIGINAL_MACHINE_TYPE=""
CURRENT_MACHINE_TYPE=""
# The type the VM is on NOW, from the API; CURRENT_MACHINE_TYPE is what this
# run believes, and a set-machine-type that gcloud reported as failed (a
# Ctrl-C while it polled, a timeout) can still have been applied.
vm_machine_type_now() {
  local t
  t="$(vm_describe 'machineType.basename()' || true)"
  printf '%s' "${t:-$CURRENT_MACHINE_TYPE}"
}
# restore_machine_type: 0 when the VM is on its own type (put back now, or
# never changed); non-zero, after a warning with the command, when it is left
# on another -- UNRESTORED_TYPE then says which, for a caller that is about to
# point INSTANCE elsewhere (the zone clone) and must carry the news itself.
# Under `set -e` a non-zero status inside an EXIT trap ends the trap right
# there, and the exit status with it, so the traps below call this `|| true`.
UNRESTORED_TYPE=""
restore_machine_type() {
  UNRESTORED_TYPE=""
  [ -n "$ORIGINAL_MACHINE_TYPE" ] || return 0
  local status now
  now="$(vm_machine_type_now)"
  [ "$now" != "$ORIGINAL_MACHINE_TYPE" ] || return 0
  status="$(vm_describe status || true)"
  if [ "$status" = "TERMINATED" ] \
     && gcloud compute instances set-machine-type "$INSTANCE" --zone="$ZONE" --project="$PROJECT" \
          --machine-type="$ORIGINAL_MACHINE_TYPE" >/dev/null 2>&1; then
    ok "set $INSTANCE back to $ORIGINAL_MACHINE_TYPE"
    CURRENT_MACHINE_TYPE="$ORIGINAL_MACHINE_TYPE"
  else
    UNRESTORED_TYPE="$now"
    warn "$INSTANCE is still $now (status ${status:-unknown}) -- set it back with: gcloud compute instances stop $INSTANCE --zone=$ZONE --project=$PROJECT && gcloud compute instances set-machine-type $INSTANCE --zone=$ZONE --project=$PROJECT --machine-type=$ORIGINAL_MACHINE_TYPE"
    return 1
  fi
}
# --- VM start: the machine-type walk, and the zone fallback -------------------
# start_vm_walk <status>: start a VM that is not RUNNING, and mark it this
# run's to stop (WE_STARTED_VM). Called once below, before the run connects,
# and again by restart_builder when the VM was stopped under a remote step; a
# second call keeps ORIGINAL_MACHINE_TYPE -- the type this run FOUND the VM
# on, the one it goes back to -- and reads the type it is on now afresh.
#
# `instances start` fails with ZONE_RESOURCE_POOL_EXHAUSTED when the zone
# has no capacity for the VM's machine type. The 06be680 loop retried that
# six times a minute apart, which covered the shortages of its week (the
# same start succeeded minutes later) and not the 2026-09-30 one, which
# outlasted the window and took the v0.17.1 release with it. It also threw
# gcloud's stderr away and retried every error alike, so the log guessed
# "usually zone capacity" whatever had happened.
#
# Now: the VM's own type first, then each OAM_GCP_FALLBACK_MACHINE_TYPES
# entry, set with `set-machine-type` (TERMINATED VMs only; this branch is
# one) -- a zone short of e2-highmem-4 usually has n2/n2d/n1 to spare, and
# nothing on the leg reads the vCPU count. Every failure is classified by
# lib/iap-helpers.sh (tested): capacity moves on to the next type, quota
# drops the type for the run, anything permanent fails now with gcloud's
# own text, and the walk repeats a minute apart until OAM_VM_START_BUDGET_S
# is spent. gcloud's progress ("Starting instance(s)...", the dots) is
# stderr too, so it is captured with the error and the operator sees one
# verdict line per attempt instead. A budget spent with every start refused
# for capacity hands over to zone_fallback_clone (below), which moves the run
# to another zone of the region, unless OAM_GCP_ZONE_FALLBACK=0.
CLEANUP_ARMED=0           # 1 once cleanup() is the EXIT trap (set beside it)
MACHINE_IMAGE=""          # the zone fallback's machine image; deleted at exit
CLONE_INSTANCE=""         # the clone this run builds on, once the walk moved to one
CLONE_ORIGIN=""           # "<instance> in <zone>" that clone was made from
VM_CLONE_NOTE=""          # what the zone fallback adds to a start failure
ORIGIN_UNRESTORED_NOTE="" # the original left on a fallback type behind a clone: the closing word
# The machine types a walk has dropped (a quota, or the API refusing the type):
# script scope, not per walk, so a restart under a step does not set the VM to
# each of them again and ask. Cleared when the run moves zone -- the API's
# refusal was the zone's; a quota is regional and costs one refused start per
# pass to find again, on the rare restart after a clone.
VM_UNUSABLE_TYPES=" "
# stop_vm: at exit, the VM this run started -- never one found RUNNING -- and
# not with OAM_KEEP_VM=1. Above start_vm_walk because the trap that walk arms
# names it: the first start runs before cleanup() is the EXIT trap, and a
# Ctrl-C between the two must not leave a VM this run started RUNNING.
stop_vm() {
  if [ "$WE_STARTED_VM" = "1" ] && [ "${OAM_KEEP_VM:-0}" != "1" ]; then
    if [ "$(vm_machine_type_now)" != "$ORIGINAL_MACHINE_TYPE" ]; then
      # set-machine-type wants TERMINATED, so this stop is not --async and
      # restore_machine_type runs right after it. A second Ctrl-C during the
      # stop ends the trap here, so the way back is printed before it.
      warn "stopping VM $INSTANCE (started by this run; OAM_KEEP_VM=1 to keep) and waiting, to set it back to $ORIGINAL_MACHINE_TYPE -- if this is interrupted, run: gcloud compute instances stop $INSTANCE --zone=$ZONE --project=$PROJECT && gcloud compute instances set-machine-type $INSTANCE --zone=$ZONE --project=$PROJECT --machine-type=$ORIGINAL_MACHINE_TYPE"
      gcloud compute instances stop "$INSTANCE" --zone="$ZONE" --project="$PROJECT" >&2 || true
    else
      warn "stopping VM $INSTANCE (started by this run; OAM_KEEP_VM=1 to keep)"
      gcloud compute instances stop "$INSTANCE" --zone="$ZONE" --project="$PROJECT" --async >&2 || true
    fi
  fi
}
# delete_machine_image: the zone fallback's image, at exit -- never the clone
# (the warm builder the operator may want again) and never the original. A
# failed delete only warns, with the command: the run's result is decided by
# then; an image that does not exist (a create a Ctrl-C landed on before the
# request went through) is nothing to delete. The clone's delete command is
# printed here too, as the last thing the operator sees, worded on what
# stop_vm did with it -- OAM_KEEP_VM=1 leaves it RUNNING -- and after it the
# original, should the restore before the clone have failed. Above
# start_vm_walk because the trap that walk arms names it.
delete_machine_image() {
  local del_err
  if [ -n "$CLONE_INSTANCE" ]; then
    if [ "${OAM_KEEP_VM:-0}" = "1" ]; then
      warn "$CLONE_INSTANCE in $ZONE, this run's clone of $CLONE_ORIGIN, is left RUNNING (OAM_KEEP_VM=1) -- it costs compute until: gcloud compute instances stop $CLONE_INSTANCE --zone=$ZONE --project=$PROJECT -- and is yours to delete when nothing needs it: gcloud compute instances delete $CLONE_INSTANCE --zone=$ZONE --project=$PROJECT"
    else
      warn "$CLONE_INSTANCE in $ZONE, this run's clone of $CLONE_ORIGIN, is left stopped and is yours to delete when nothing needs it: gcloud compute instances delete $CLONE_INSTANCE --zone=$ZONE --project=$PROJECT"
    fi
    [ -z "$ORIGIN_UNRESTORED_NOTE" ] || warn "$ORIGIN_UNRESTORED_NOTE"
  fi
  [ -n "$MACHINE_IMAGE" ] || return 0
  if del_err="$(gcloud compute machine-images delete "$MACHINE_IMAGE" --project="$PROJECT" --quiet 2>&1 >/dev/null)"; then
    ok "deleted machine image $MACHINE_IMAGE"
  else
    case "${del_err//$'\r'/}" in
      *"was not found"*) ok "machine image $MACHINE_IMAGE does not exist -- nothing to delete" ;;
      *) warn "could not delete machine image $MACHINE_IMAGE -- it costs storage until you run: gcloud compute machine-images delete $MACHINE_IMAGE --project=$PROJECT" ;;
    esac
  fi
  MACHINE_IMAGE=""
}
# clone_leftover <clone> <zone>: after an `instances create` whose answer was
# lost -- a Ctrl-C while gcloud polled it (gcloud then exits, see
# vm_start_verdict), a dropped reply -- the request can have gone through, and
# the clone then comes up RUNNING in the other zone while INSTANCE still names
# the original, so nothing at exit would stop or name it. One describe says.
# 0, with CLONE_LEFTOVER_NOTE naming the clone, its status and its delete
# command, when it exists -- a RUNNING one gets a stop (--async, best effort)
# first, since this run will not build on it; 1, with the note saying the
# describe found none, when it does not.
CLONE_LEFTOVER_NOTE=""
clone_leftover() {
  local clone="$1" zone="$2" status stopped=""
  status="$(gcloud compute instances describe "$clone" --zone="$zone" --project="$PROJECT" \
    --format='value(status)' 2>/dev/null | tr -d '\r' || true)"
  if [ -z "$status" ]; then
    CLONE_LEFTOVER_NOTE="a describe found no $clone in $zone"
    return 1
  fi
  if [ "$status" = "RUNNING" ] \
     && gcloud compute instances stop "$clone" --zone="$zone" --project="$PROJECT" --async >/dev/null 2>&1; then
    stopped="; a stop was issued"
  fi
  CLONE_LEFTOVER_NOTE="$clone EXISTS in $zone ($status$stopped) and is yours to delete: gcloud compute instances delete $clone --zone=$zone --project=$PROJECT"
  return 0
}
start_vm_walk() {
  local vm_type vm_now set_err start_err zones_with_it
  step "Start VM $INSTANCE (status: $1)"
  VM_START_BUDGET="${OAM_VM_START_BUDGET_S:-1800}"
  if [ -z "$ORIGINAL_MACHINE_TYPE" ]; then
    ORIGINAL_MACHINE_TYPE="$(vm_describe 'machineType.basename()' || true)"
    CURRENT_MACHINE_TYPE="$ORIGINAL_MACHINE_TYPE"
  else
    # A restart: whoever stopped the VM under the step may have set its type.
    CURRENT_MACHINE_TYPE="$(vm_machine_type_now)"
  fi
  VM_TYPES="$(vm_start_types "$ORIGINAL_MACHINE_TYPE" \
    "${OAM_GCP_FALLBACK_MACHINE_TYPES-n2-highmem-4 n2d-highmem-4 c2d-highmem-4 n1-highmem-4 e2-standard-8 t2d-standard-8 n2-standard-8 n2d-standard-8 c2-standard-8 n1-standard-8 e2-highmem-8}")" \
    || fail "could not read the machine type of $INSTANCE -- not starting it"
  # Armed before the first set-machine-type: a fail below must not leave the
  # VM stopped on a fallback type, a zone clone's machine image behind, or a
  # VM this run started RUNNING -- the first start runs before cleanup() is
  # the EXIT trap, and the zone fallback with it. cleanup() takes this over
  # further down -- and a restart under a step comes after that, so it must
  # not put this back in cleanup's place.
  [ "$CLEANUP_ARMED" = "1" ] || trap 'stop_vm; restore_machine_type || true; delete_machine_image' EXIT
  VM_START_OK=0; VM_START_PASS=0; VM_TRIED=""; VM_LAST_MSG=""
  # Stays 1 while every refused start was a capacity verdict -- the one shape
  # a zone clone answers. A backend error, or a start that returned to a VM
  # not RUNNING, clears it: that is not a stockout. A quota or an unsupported
  # type drops the type and says nothing about the zone; nor does a VM found
  # still STOPPING before a pass could start anything (the previous run's
  # --async stop, an operator's), which only waits for the next pass --
  # VM_TRIED covers a walk that never got to a start.
  VM_START_ALL_CAPACITY=1
  VM_START_DEADLINE=$((SECONDS + 10#$VM_START_BUDGET))   # 10#: "0900" is not octal
  while :; do
    VM_START_PASS=$((VM_START_PASS + 1))
    while IFS= read -r vm_type; do
      [ -n "$vm_type" ] || continue
      case "$VM_UNUSABLE_TYPES" in *" $vm_type "*) continue ;; esac
      # A start whose gcloud side failed can still have been accepted, and the
      # 03:00 schedule or an operator can be stopping or starting the VM under
      # this loop: only a TERMINATED VM takes a set-machine-type, and one that
      # is RUNNING is what the loop is after.
      vm_now="$(vm_describe status || true)"
      case "$vm_now" in
        RUNNING) ok "$INSTANCE is RUNNING (as $CURRENT_MACHINE_TYPE)"; VM_START_OK=1; break ;;
        TERMINATED) ;;
        SUSPENDED | SUSPENDING)
          fail "$INSTANCE is $vm_now, and \`instances start\` only starts a stopped VM -- run: gcloud compute instances resume $INSTANCE --zone=$ZONE --project=$PROJECT" ;;
        *) warn "$INSTANCE is ${vm_now:-unreadable}, not TERMINATED -- waiting for it to settle before the next pass"; break ;;
      esac
      if [ "$vm_type" != "$CURRENT_MACHINE_TYPE" ]; then
        if ! set_err="$(gcloud compute instances set-machine-type "$INSTANCE" --zone="$ZONE" --project="$PROJECT" \
               --machine-type="$vm_type" 2>&1 >/dev/null)"; then
          VM_LAST_MSG="$(gcloud_error_message "$set_err" || echo '(gcloud printed nothing)')"
          case "$(vm_start_verdict "$set_err")" in
            interrupted) fail "interrupted while setting $INSTANCE to $vm_type" ;;
            permanent)   fail "setting $INSTANCE to $vm_type failed, and retrying cannot help: $VM_LAST_MSG" ;;
            quota | unsupported)
              warn "$INSTANCE cannot be set to $vm_type -- not trying it again this run: $VM_LAST_MSG"
              VM_UNUSABLE_TYPES="$VM_UNUSABLE_TYPES$vm_type " ;;
            *) warn "setting $INSTANCE to $vm_type failed (pass $VM_START_PASS): $VM_LAST_MSG -- again next pass" ;;
          esac
          continue
        fi
        CURRENT_MACHINE_TYPE="$vm_type"
        ok "set $INSTANCE to $vm_type for this run (its own type is $ORIGINAL_MACHINE_TYPE)"
      fi
      case " $VM_TRIED " in *" $vm_type "*) ;; *) VM_TRIED="${VM_TRIED:+$VM_TRIED }$vm_type" ;; esac
      ok "starting $INSTANCE as $vm_type (pass $VM_START_PASS; gcloud's progress is captured, so this can be silent for a minute)..."
      if start_err="$(gcloud compute instances start "$INSTANCE" --zone="$ZONE" --project="$PROJECT" 2>&1 >/dev/null)"; then
        vm_now="$(vm_describe status || true)"
        [ "$vm_now" = "RUNNING" ] && { VM_START_OK=1; break; }
        warn "start of $INSTANCE ($vm_type) returned, but it reads ${vm_now:-unreadable} -- checking again next pass"
        VM_START_ALL_CAPACITY=0
        break
      fi
      VM_LAST_MSG="$(gcloud_error_message "$start_err" || echo '(gcloud printed nothing)')"
      case "$(vm_start_verdict "$start_err")" in
        interrupted)
          fail "interrupted while starting $INSTANCE ($vm_type)" ;;
        capacity)
          zones_with_it="$(vm_start_zones_available "$start_err" || true)"
          warn "no $vm_type capacity in $ZONE (pass $VM_START_PASS): $VM_LAST_MSG${zones_with_it:+ -- zones with capacity for it: $zones_with_it}" ;;
        quota | unsupported)
          warn "$vm_type is not usable here -- not trying it again this run: $VM_LAST_MSG"
          VM_UNUSABLE_TYPES="$VM_UNUSABLE_TYPES$vm_type " ;;
        permanent)
          fail "starting $INSTANCE ($vm_type) in $ZONE failed, and retrying cannot help: $VM_LAST_MSG" ;;
        *)
          warn "start of $INSTANCE ($vm_type) failed (pass $VM_START_PASS): $VM_LAST_MSG -- retrying"
          VM_START_ALL_CAPACITY=0 ;;
      esac
    done <<<"$VM_TYPES"
    [ "$VM_START_OK" = "1" ] && break
    VM_USABLE_LEFT=0
    while IFS= read -r vm_type; do
      [ -n "$vm_type" ] || continue
      case "$VM_UNUSABLE_TYPES" in *" $vm_type "*) ;; *) VM_USABLE_LEFT=1 ;; esac
    done <<<"$VM_TYPES"
    [ "$VM_USABLE_LEFT" = "1" ] \
      || fail "no machine type left to try for $INSTANCE in $ZONE (tried: ${VM_TRIED:-nothing}; last: ${VM_LAST_MSG:-no start was attempted})"
    VM_START_REMAINING=$((VM_START_DEADLINE - SECONDS))
    if [ "$VM_START_REMAINING" -le 0 ]; then
      VM_CLONE_NOTE=""
      if [ "$ZONE_FALLBACK" != "1" ]; then
        VM_CLONE_NOTE=" OAM_GCP_ZONE_FALLBACK=1 (the default) clones the builder into another zone of ${ZONE%-*} for the run instead."
      elif [ -n "$CLONE_INSTANCE" ]; then
        VM_CLONE_NOTE=" $INSTANCE is already this run's clone of $CLONE_ORIGIN; a clone is not cloned again."
      elif [ -z "$VM_TRIED" ] || [ "$VM_START_ALL_CAPACITY" != "1" ]; then
        VM_CLONE_NOTE=" Not cloning the builder into another zone: $ZONE was not shown to be out of capacity (a start was refused for something else, or none was attempted)."
      elif zone_fallback_clone; then
        break
      fi
      fail "could not start $INSTANCE in $ZONE within ${VM_START_BUDGET}s (tried: ${VM_TRIED:-nothing}; last: ${VM_LAST_MSG:-no start was attempted}). Wait and re-run (OAM_VM_START_BUDGET_S=<seconds> waits longer), or move the builder to a zone that has capacity: snapshot its boot disk, create a disk from the snapshot there, create the instance from that disk under the same name, delete the old one, and run again -- this script finds the moved instance by itself (OAM_GCP_BUILDER_ZONE pins it).$VM_CLONE_NOTE"
    fi
    VM_START_PAUSE=60
    [ "$VM_START_PAUSE" -le "$VM_START_REMAINING" ] || VM_START_PAUSE="$VM_START_REMAINING"
    warn "nothing started on pass $VM_START_PASS -- pass $((VM_START_PASS + 1)) in ${VM_START_PAUSE}s (${VM_START_REMAINING}s of the ${VM_START_BUDGET}s budget left)"
    sleep "$VM_START_PAUSE"
  done
  WE_STARTED_VM=1
  if [ "$CURRENT_MACHINE_TYPE" = "$ORIGINAL_MACHINE_TYPE" ]; then
    ok "VM started"
  else
    ok "VM started as $CURRENT_MACHINE_TYPE (its own type is $ORIGINAL_MACHINE_TYPE; set back when this run stops it)"
  fi
}

# zone_fallback_clone: the zone has refused every start of the walk for
# capacity for the whole budget, so this run moves to another zone of the
# region WITHOUT touching the builder. A machine image of the stopped builder
# first -- it carries the disks, metadata, service account, scopes, tags and
# network config, and the boot disk's copy keeps the cargo cache under
# ~/$REMOTE_DIR warm, where a fresh VM would spend ~30 min on deps -- then
# `<instance>-<zone>` from it in each candidate zone (OAM_GCP_FALLBACK_ZONES,
# else the region's other UP zones) and each machine type of the walk, until
# one boots. `instances create` boots what it creates, so no start follows,
# and a refused create is classified like a refused start: capacity moves on,
# quota drops the type (quotas are regional), anything permanent fails now.
# Returns 0 with INSTANCE, ZONE and the machine-type bookkeeping on the clone,
# which is this run's to stop; 1, with VM_CLONE_NOTE saying what to do by
# hand, when no image could be made or no zone took it -- the image is then
# KEPT, since the hand move needs it. The original is put back on its own
# type FIRST, while INSTANCE still names it, and is then left alone: stopped,
# found by name by the next run as before -- and when that restore fails (an
# API blip; the warning carries the command), the clone message and the
# closing word at exit say what type it is really on, since the exit-time
# restore acts on INSTANCE, which names the clone from here.
zone_fallback_clone() {
  local origin="$INSTANCE" origin_zone="$ZONE" origin_own="$ORIGINAL_MACHINE_TYPE" origin_left=""
  local region="${ZONE%-*}" img="$INSTANCE-img-$RUNID"
  local img_err zone_list list_err zones zone clone clone_type create_err msg unusable=" " origin_state keep_note
  step "Clone $origin into another zone of $region for this run"
  warn "$origin_zone refused every start for capacity for ${VM_START_BUDGET}s -- cloning $origin into another zone of $region for this run (OAM_GCP_ZONE_FALLBACK=0 fails instead). The original is not deleted and not started: it stays stopped, on its own type."
  restore_machine_type || origin_left="$UNRESTORED_TYPE"
  ok "creating machine image $img from $origin (gcloud's progress is captured; a large disk takes minutes)..."
  if ! img_err="$(gcloud compute machine-images create "$img" --source-instance="$origin" \
         --source-instance-zone="$origin_zone" --project="$PROJECT" 2>&1 >/dev/null)"; then
    # A Ctrl-C lands on gcloud, which exits rather than dies (vm_start_verdict),
    # with the create it was polling already accepted: the image exists, or is
    # about to, and is this run's to delete at exit like a finished one.
    if [ "$(vm_start_verdict "$img_err")" = "interrupted" ]; then
      MACHINE_IMAGE="$img"
      fail "interrupted while creating machine image $img from $origin -- the request may have gone through, so the exit deletes the image if it exists (should that fail, it costs storage until: gcloud compute machine-images delete $img --project=$PROJECT)"
    fi
    VM_CLONE_NOTE=" A clone into another zone was tried too, and the machine image could not be made: $(gcloud_error_message "$img_err" || echo '(gcloud printed nothing)')"
    return 1
  fi
  MACHINE_IMAGE="$img"
  ok "machine image $img created from $origin (deleted when this run exits)"
  zone_list=""
  if [ -z "$FALLBACK_ZONES" ]; then
    zone_list="$(gcloud compute zones list --project="$PROJECT" --filter="region:$region" \
      --format='value(name,status,region.basename())' 2>"$STAGE_DIR/logs/zones-list.err" | tr -d '\r' || true)"
  fi
  if ! zones="$(zone_fallback_candidates "$origin_zone" "$FALLBACK_ZONES" "$region" "$zone_list")"; then
    list_err="$(gcloud_error_message "$(cat "$STAGE_DIR/logs/zones-list.err" 2>/dev/null)" || true)"
    VM_CLONE_NOTE=" A clone into another zone was tried too, and there is no other UP zone of $region to clone into${FALLBACK_ZONES:+ (OAM_GCP_FALLBACK_ZONES='$FALLBACK_ZONES')}${list_err:+ (zones list: $list_err)}. Machine image $img is KEPT for a move by hand -- gcloud compute instances create $origin-<zone> --zone=<zone> --source-machine-image=$img --project=$PROJECT, then run again with OAM_GCP_BUILDER_INSTANCE=$origin-<zone> OAM_GCP_BUILDER_ZONE=<zone> -- and costs storage until: gcloud compute machine-images delete $img --project=$PROJECT"
    MACHINE_IMAGE=""
    return 1
  fi
  ok "zones to try, in order: $(tr '\n' ' ' <<<"$zones")"
  while IFS= read -r zone; do
    [ -n "$zone" ] || continue
    clone="$origin-$zone"
    while IFS= read -r clone_type; do
      [ -n "$clone_type" ] || continue
      case "$VM_UNUSABLE_TYPES$unusable" in *" $clone_type "*) continue ;; esac
      ok "creating $clone in $zone as $clone_type from $img (gcloud's progress is captured)..."
      if create_err="$(gcloud compute instances create "$clone" --zone="$zone" --source-machine-image="$img" \
             --machine-type="$clone_type" --project="$PROJECT" 2>&1 >/dev/null)"; then
        CLONE_INSTANCE="$clone"; CLONE_ORIGIN="$origin in $origin_zone"
        INSTANCE="$clone"; ZONE="$zone"
        ORIGINAL_MACHINE_TYPE="$clone_type"; CURRENT_MACHINE_TYPE="$clone_type"
        WE_STARTED_VM=1
        # The zone changed: the types the walk dropped in $origin_zone get
        # their try here, should a restart under a step walk again.
        VM_UNUSABLE_TYPES=" "
        if [ -n "$origin_left" ]; then
          ORIGIN_UNRESTORED_NOTE="$origin in $origin_zone is stopped but STILL ON $origin_left, not its own $origin_own: setting it back failed before the clone was made. Set it back with: gcloud compute instances stop $origin --zone=$origin_zone --project=$PROJECT && gcloud compute instances set-machine-type $origin --zone=$origin_zone --project=$PROJECT --machine-type=$origin_own"
          origin_state="$ORIGIN_UNRESTORED_NOTE The next run finds it by name as before, and reads that type as its own."
        else
          origin_state="$origin is untouched: stopped, on its own type, and the next run finds it by name as before."
        fi
        if [ "${OAM_KEEP_VM:-0}" = "1" ]; then
          keep_note="The clone is left RUNNING when this run exits (OAM_KEEP_VM=1) and is NOT deleted"
        else
          keep_note="The clone is stopped when this run exits and is NOT deleted"
        fi
        warn "THIS RUN NOW BUILDS ON $clone IN $zone (as $clone_type), a clone of $origin ($origin_zone) made from machine image $img. $origin_state $keep_note; the image is. When nothing needs the clone any more, delete it, and the image should this run's exit have failed to: gcloud compute instances delete $clone --zone=$zone --project=$PROJECT && gcloud compute machine-images delete $img --project=$PROJECT"
        return 0
      fi
      msg="$(gcloud_error_message "$create_err" || echo '(gcloud printed nothing)')"
      # The name is taken: an earlier run's clone, left for the operator (the
      # preflight names it). Not created over, not deleted -- the next zone.
      case "${create_err//$'\r'/}" in
        *"instances/$clone' already exists"*)
          warn "$clone already exists in $zone -- an earlier run's clone, left for you to delete; not creating over it. To use this zone, delete it first: gcloud compute instances delete $clone --zone=$zone --project=$PROJECT"
          break ;;
      esac
      case "$(vm_start_verdict "$create_err")" in
        interrupted)
          # The request can have gone through (clone_leftover): a clone that
          # came up is stopped and named here, since INSTANCE still names the
          # original and the exit would not touch it.
          clone_leftover "$clone" "$zone" || true
          fail "interrupted while creating $clone in $zone -- $CLONE_LEFTOVER_NOTE; machine image $img is deleted on exit" ;;
        permanent)   fail "creating $clone in $zone failed, and retrying cannot help: $msg (machine image $img is deleted on exit)" ;;
        capacity)    warn "no $clone_type capacity in $zone either: $msg" ;;
        quota)
          warn "$clone_type is not usable in $region -- not trying it in another zone: $msg"
          unusable="$unusable$clone_type " ;;
        unsupported) warn "$clone cannot be created as $clone_type in $zone: $msg" ;;
        *)
          # gcloud can lose the answer and not the request: one describe says
          # whether the clone came up anyway. One that did is not built on --
          # it may still be creating, on a type this run did not see
          # confirmed -- and holds the name, so the walk names it, with its
          # delete command, and goes to the next zone.
          if clone_leftover "$clone" "$zone"; then
            warn "creating $clone in $zone as $clone_type failed: $msg -- but $CLONE_LEFTOVER_NOTE. Not building on it; trying the next zone"
            break
          fi
          warn "creating $clone in $zone as $clone_type failed: $msg -- $CLONE_LEFTOVER_NOTE; trying the next type" ;;
      esac
    done <<<"$VM_TYPES"
  done <<<"$zones"
  VM_CLONE_NOTE=" A clone into another zone was tried too, and no zone of $region took it (tried: $(tr '\n' ' ' <<<"$zones" | sed 's/ *$//')). Machine image $img is KEPT for a move by hand -- gcloud compute instances create $origin-<zone> --zone=<zone> --source-machine-image=$img --project=$PROJECT, then run again with OAM_GCP_BUILDER_INSTANCE=$origin-<zone> OAM_GCP_BUILDER_ZONE=<zone> -- and costs storage until: gcloud compute machine-images delete $img --project=$PROJECT"
  MACHINE_IMAGE=""
  return 1
}

# `instances start` returning RUNNING means the API finished, NOT the guest:
# sshd comes up ~15-60s later -- whichever transport the run then uses. On
# the IAP fallback it is worse: gcloud`s start-iap-tunnel does a real backend
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
  # Never fatal: the connect step's own retries (the direct probe's boot
  # budget, then the tunnel loop) are still the real backstop.
  warn "no sshd banner on the serial console after ${max}s -- proceeding (the connect step still retries)"
  return 0
}

if [ "$VM_STATUS" != "RUNNING" ]; then
  start_vm_walk "$VM_STATUS"
  step "Wait for guest sshd on $INSTANCE"
  wait_for_guest_sshd
else
  ok "VM already RUNNING -- will leave it running on exit"
fi

# --- instance schedules ------------------------------------------------------
# yaw-linux-builder carries an instance schedule (`yaw-linux-builder-autostop`:
# stop at 03:00 America/Los_Angeles, every day) as the cost backstop for a VM
# left running. It fires whatever the VM is doing. The 2026-09-25 v0.17.0
# release leg started at 02:39, was 86s into node-suite at 03:00, and died
# with nothing but `Connection to localhost closed by remote host.` -- the
# guest journal (sshd SIGTERM, systemd shutdown.target one second after the
# session closed) and a `stop` by the Compute Engine system service account
# in the operations log were the only evidence, and this script's own cleanup
# had stopped the VM and deleted the tunnel log before anyone looked.
#
# So: detach every stop schedule for the run and re-attach on exit. The EXIT
# trap runs on failure too, so the backstop is back before this script is
# gone; if the re-attach itself fails, the warning carries the exact command.
# A VM found already RUNNING gets the same treatment -- the schedule does not
# care who started it. OAM_KEEP_VM_SCHEDULE=1 leaves the schedules alone; a
# stop that then lands under a remote step is answered by remote_step_run's
# restart path (the VM is started again, an attempt spent), one that lands
# anywhere else ends the run. The parsing lives in lib/iap-helpers.sh
# (iap_policy_*), where it is tested.
#
# The re-attach goes to the instance the policies came OFF, not to $INSTANCE:
# a run whose VM was stopped under a step can end up on a zone clone
# (restart_builder -> start_vm_walk -> zone_fallback_clone), and the original
# must get its backstop back, not the clone.
DETACHED_POLICIES=""
DETACHED_FROM_INSTANCE=""
DETACHED_FROM_ZONE=""
detach_stop_schedules() {
  if [ "${OAM_KEEP_VM_SCHEDULE:-0}" = "1" ]; then
    warn "OAM_KEEP_VM_SCHEDULE=1 -- leaving the VM's instance schedules attached. A scheduled stop under a remote step is followed by a start of the VM (the same walk as at the top, which can clone it into another zone) and the step again, counting against OAM_REMOTE_STEP_ATTEMPTS=${OAM_REMOTE_STEP_ATTEMPTS:-3}, and the VM is then this run's to stop on exit (OAM_KEEP_VM=1 keeps it); a stop outside a remote step, or on the last attempt, ends the run"
    return 0
  fi
  local reading url name region fields stop tz
  reading="$(gcloud compute instances describe "$INSTANCE" --zone="$ZONE" --project="$PROJECT" \
    --format='value(resourcePolicies)' 2>/dev/null || true)"
  while IFS= read -r url; do
    [ -n "$url" ] || continue
    name="$(iap_policy_name "$url")" || continue
    region="$(iap_policy_region "$url" || true)"
    [ -n "$region" ] || region="${ZONE%-*}"
    # Only a policy with a vmStopSchedule can kill the run; snapshot and
    # placement policies attach the same way and are left alone.
    fields="$(gcloud compute resource-policies describe "$name" --region="$region" --project="$PROJECT" \
      --format='value(instanceSchedulePolicy.vmStopSchedule.schedule,instanceSchedulePolicy.timeZone)' \
      2>/dev/null || true)"
    IFS=$'\t' read -r stop tz <<<"${fields//$'\r'/}"
    [ -n "$stop" ] || continue
    if gcloud compute instances remove-resource-policies "$INSTANCE" --resource-policies="$name" \
         --zone="$ZONE" --project="$PROJECT" >/dev/null 2>&1; then
      DETACHED_POLICIES="$DETACHED_POLICIES $name"
      DETACHED_FROM_INSTANCE="$INSTANCE"; DETACHED_FROM_ZONE="$ZONE"
      ok "detached instance schedule $name (stops $INSTANCE at '$stop' $tz) for this run -- re-attached on exit"
    else
      warn "could not detach instance schedule $name (stops $INSTANCE at '$stop' $tz) -- a run still going then will be killed"
    fi
  done <<<"$(iap_policy_urls "$reading")"
}
reattach_stop_schedules() {
  local name inst="${DETACHED_FROM_INSTANCE:-$INSTANCE}" zone="${DETACHED_FROM_ZONE:-$ZONE}"
  for name in $DETACHED_POLICIES; do
    if gcloud compute instances add-resource-policies "$inst" --resource-policies="$name" \
         --zone="$zone" --project="$PROJECT" >/dev/null 2>&1; then
      ok "re-attached instance schedule $name to $inst"
    else
      warn "could not re-attach instance schedule $name -- $inst has NO scheduled stop until you run: gcloud compute instances add-resource-policies $inst --resource-policies=$name --zone=$zone --project=$PROJECT"
    fi
  done
  DETACHED_POLICIES=""
}

# --- IAP tunnel machinery: the FALLBACK transport (mirrors yaw's, see that
# script for the full rationale) ----------------------------------------------
# Used only when direct ssh to the external IP does not answer, or when
# OAM_IAP_SSH_MODE=tunnel -- see "ssh transport" below. None of it runs on the
# direct path: no tunnel is started, so there is nothing to stop.
#
# Short version: gcloud's own ssh wrapper uses plink on Windows, which has no
# keepalive, and the IAP tunnel drops idle connections after ~5 min. So: run
# one long-lived `gcloud compute start-iap-tunnel` in the background for the
# whole build, parse the local port it picked, and drive plain OpenSSH
# (which has ServerAliveInterval) over localhost. Remote work is split into
# short per-step ssh invocations. The trap kills the tunnel -- the whole
# process tree, see kill_proc_tree in lib/iap-helpers.sh -- on any exit.
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
    # Never leave a half-open tunnel behind for the next attempt to trip on --
    # and reap the whole tree: on Windows $! is the scoop shim's sh, and a
    # plain kill leaves gcloud's python running under it (lib/iap-helpers.sh).
    kill_proc_tree "$IAP_TUNNEL_PID"
    if [ "$attempt" -ge "$max_attempts" ]; then
      # Inline the log tail: cleanup rm's the file, so a path alone destroys
      # the evidence the operator needs.
      fail "IAP tunnel never served on any of $max_attempts attempts. Last output: $(tail -3 "$IAP_TUNNEL_LOG" 2>/dev/null | tr '
' ' ')${DIRECT_SSH_ERR:+ (direct ssh was tried first and failed: $DIRECT_SSH_ERR)}"
    fi
    warn "tunnel attempt $attempt did not serve (sshd likely still booting) -- retrying in 15s"
    attempt=$((attempt + 1))
    sleep 15
  done
}
stop_iap_tunnel() {
  if [ -n "$IAP_TUNNEL_PID" ] && kill -0 "$IAP_TUNNEL_PID" 2>/dev/null; then
    kill_proc_tree "$IAP_TUNNEL_PID"
    ok "IAP tunnel stopped (pid $IAP_TUNNEL_PID, port $IAP_TUNNEL_PORT)"
  fi
  # Keep gcloud's own tunnel output next to the step logs. It is the only
  # record of a tunnel-side reset, and it used to be deleted here before
  # anyone could read it. -s, not -f: on the direct path no tunnel ran, and an
  # empty iap-tunnel.log among the logs would read as a tunnel that said
  # nothing.
  # Appended, not copied over: a reconnect after a reset relay stops one
  # tunnel and starts another, and the reset is in the FIRST one's output.
  if [ -s "$IAP_TUNNEL_LOG" ]; then
    { printf -- '--- tunnel on port %s, stopped %s ---\n' "$IAP_TUNNEL_PORT" "$(date +%Y-%m-%dT%H:%M:%S)"; cat "$IAP_TUNNEL_LOG"; } \
      >> "$STAGE_DIR/logs/iap-tunnel.log" 2>/dev/null || true
  fi
  rm -f "$IAP_TUNNEL_LOG"
}
# restore_machine_type after stop_vm: it needs the stop before it to have
# finished -- and `|| true`, since under `set -e` its non-zero status would
# end the trap, and the run's exit status, right there. delete_machine_image
# last: it is independent of the VM, and its closing word -- the clone left
# for the operator -- belongs at the end.
cleanup() { stop_iap_tunnel; reattach_stop_schedules; stop_vm; restore_machine_type || true; delete_machine_image; }
trap cleanup EXIT
CLEANUP_ARMED=1
# Only now that the trap is armed: a failure between detaching a schedule and
# arming the trap would leave the VM with no scheduled stop at all.
detach_stop_schedules

# --- ssh transport: direct first, the IAP tunnel as the fallback --------------
# Every remote command and file transfer goes through gcp_ssh / gcp_scp_to /
# gcp_scp_from, which dispatch on REMOTE_TRANSPORT, set once by
# connect_builder below:
#   direct  plain OpenSSH to the VM's external IP. ~1s per call from the
#           Windows orchestrator, no relay, no idle drop.
#   tunnel  plain OpenSSH to localhost:$IAP_TUNNEL_PORT, forwarded by the
#           long-lived `gcloud compute start-iap-tunnel` above.
# Why direct is the default: on 2026-09-25 the sibling yaw release died with
# "not reachable over IAP within 1269s. Last error: (empty stderr)" against a
# VM that was RUNNING and healthy the whole time. Each `gcloud compute ssh
# --tunnel-through-iap` probe cost 11-57s (gcloud's Python startup, then the
# websocket relay), only 4 of 20 reached sshd at all, and those 4
# authenticated fine and were killed by the client-side timeout. Plain
# OpenSSH to the external IP answered in ~1s, and default-allow-ssh
# already opens tcp:22 to 0.0.0.0/0 on the default network, so IAP bought no
# security here. The tunnel stays as the fallback for a host that cannot
# reach tcp:22 (no external IP, egress or firewall blocking it).
# OAM_IAP_SSH_MODE=direct|tunnel forces one; the pure selection logic lives
# in lib/iap-helpers.sh, where it is tested.
REMOTE_TRANSPORT=""   # direct | tunnel, set by connect_builder
DIRECT_IP=""          # re-read after the VM is RUNNING, never cached earlier
DIRECT_SSH_ERR=""     # last non-blank stderr line of the last direct probe
DIRECT_SSH_HINT=""

# _ssh_target [direct|tunnel]: fills the array SSH_OPTS and SSH_HOST for the
# transport (default: the one in use). Built at CALL time, not at script
# scope: DIRECT_IP and IAP_TUNNEL_PORT are only set once the connect step has
# run, and bash captures values at expansion time. An array rather than an
# unquoted $(echo ...), so a $HOME with a space in it cannot split an option.
# The Port flag is -o Port= (NOT -p/-P) -- the only spelling portable across
# ssh and scp including OpenSSH 10.2 on Windows.
_ssh_target() {
  SSH_OPTS=(
    -o "StrictHostKeyChecking=accept-new"
    -o "ServerAliveInterval=30"
    -o "ServerAliveCountMax=10"
    -o "IdentitiesOnly=yes"
    -o "UserKnownHostsFile=${HOME}/.ssh/google_compute_known_hosts"
    -i "$IAP_SSH_KEY"
  )
  if [ "${1:-$REMOTE_TRANSPORT}" = "direct" ]; then
    # BatchMode: a passphrase or password prompt has nobody to answer it and
    # would hang the run; fail instead, with the reason on stderr.
    SSH_OPTS+=(-o "BatchMode=yes" -o "ConnectTimeout=8")
    SSH_HOST="$DIRECT_IP"
  else
    SSH_OPTS+=(-o "ConnectTimeout=10" -o "Port=$IAP_TUNNEL_PORT")
    SSH_HOST="localhost"
  fi
}
gcp_ssh(){ _ssh_target; ssh "${SSH_OPTS[@]}" "${LINUX_USER}@${SSH_HOST}" "$@"; }
gcp_scp_to(){ _ssh_target; scp "${SSH_OPTS[@]}" "$1" "${LINUX_USER}@${SSH_HOST}:$2"; }
gcp_scp_from(){  # gcp_scp_from <remote-glob-under-REMOTE_DIR> <local-dir>
  # scp can't expand server-side globs; tar on the remote side can (the whole
  # command is one remote-shell invocation), piped back over the connection.
  local pattern="$1" local_dir="$2" parent_dir glob
  case "$pattern" in
    */*) parent_dir="${pattern%/*}"; glob="${pattern##*/}" ;;
    *)   parent_dir=".";             glob="$pattern" ;;
  esac
  _ssh_target
  # shellcheck disable=SC2029  # $REMOTE_DIR, $parent_dir and $glob are meant to expand here
  ssh "${SSH_OPTS[@]}" "${LINUX_USER}@${SSH_HOST}" "cd $REMOTE_DIR/$parent_dir && tar czf - $glob" \
    | tar xzf - -C "$local_dir"
}
# For messages: which path a failure travelled.
transport_desc(){
  if [ "$REMOTE_TRANSPORT" = "direct" ]; then printf 'direct ssh %s@%s' "$LINUX_USER" "$DIRECT_IP"
  else printf 'IAP tunnel localhost:%s' "$IAP_TUNNEL_PORT"; fi
}

# The external IP, read NOW -- an ephemeral one changes across stop/start, so
# a value read before this run started the VM would point at nobody. One
# repeat: an empty answer is indistinguishable from "this VM has no external
# IP", and a single blinked describe must not demote the run to the tunnel.
vm_external_ip(){
  local ip attempt
  for attempt in 1 2; do
    ip="$(gcloud compute instances describe "$INSTANCE" --zone="$ZONE" --project="$PROJECT" \
      --format='value(networkInterfaces[0].accessConfigs[0].natIP)' 2>/dev/null | tr -d '[:space:]' || true)"
    if [ -n "$ip" ]; then printf '%s' "$ip"; return 0; fi
    if [ "$attempt" -lt 2 ]; then sleep 3; fi
  done
  return 1
}

# Outer bound on one probe. ConnectTimeout covers the TCP connect and the key
# exchange but not authentication, and on a booting guest the OS Login /
# metadata key lookup behind auth can stall. GNU timeout only: Windows'
# own timeout.exe (if PATH puts it first) and a stock macOS have none, and
# then ConnectTimeout alone has to do.
_with_timeout(){
  local secs="$1"; shift
  if timeout --version >/dev/null 2>&1; then timeout "$secs" "$@"; else "$@"; fi
}

# direct_ssh_probe <budget-seconds>
# 0 once `ssh true` answers on the external IP, with exactly the options the
# run then uses. Non-zero when it has not within the budget -- or at once for
# a failure retrying cannot fix -- with DIRECT_SSH_ERR saying why: the LAST
# NON-BLANK line ssh wrote to stderr, never a placeholder.
direct_ssh_probe(){
  local budget="$1" started="$SECONDS" attempt=0 err rc line elapsed
  DIRECT_SSH_ERR=""; DIRECT_SSH_HINT=""
  if ! DIRECT_IP="$(vm_external_ip)"; then
    DIRECT_IP=""
    DIRECT_SSH_ERR="$INSTANCE has no external IP (networkInterfaces[0].accessConfigs[0].natIP is empty)"
    DIRECT_SSH_HINT="give it one with: gcloud compute instances add-access-config $INSTANCE --zone=$ZONE --project=$PROJECT"
    return 1
  fi
  _ssh_target direct
  while :; do
    attempt=$((attempt + 1))
    rc=0
    err="$(_with_timeout 30 ssh "${SSH_OPTS[@]}" "${LINUX_USER}@${DIRECT_IP}" true 2>&1 >/dev/null)" || rc=$?
    [ "$rc" -eq 0 ] && return 0
    if line="$(last_nonblank_line "$err")"; then
      DIRECT_SSH_ERR="$line"
    elif [ "$rc" -eq 124 ]; then
      DIRECT_SSH_ERR="ssh ${LINUX_USER}@${DIRECT_IP} gave no answer within 30s and was killed (nothing on stderr)"
    else
      DIRECT_SSH_ERR="ssh ${LINUX_USER}@${DIRECT_IP} exited $rc without writing anything to stderr"
    fi
    elapsed=$((SECONDS - started))
    if ssh_error_is_permanent "$err" || [ "$elapsed" -ge "$budget" ]; then
      DIRECT_SSH_HINT="$(direct_ssh_hint "$err" "$DIRECT_IP")"
      return 1
    fi
    warn "direct ssh to ${LINUX_USER}@${DIRECT_IP} not answering yet (attempt $attempt, ${elapsed}s of ${budget}s): $DIRECT_SSH_ERR -- retrying in 5s"
    sleep 5
  done
}

# connect_builder: pick the transport, prove it, and set REMOTE_TRANSPORT.
# Direct first (unless OAM_IAP_SSH_MODE=tunnel); the IAP tunnel only when that
# does not answer, and then loudly. The tunnel is not started at all on the
# direct path.
connect_builder(){
  # A VM this run just started gets a real boot budget: sshd comes up ~15-60s
  # after RUNNING, and the guest agent writes the ssh keys after that. So does
  # one that came back RUNNING by someone else's hand after a stop under a
  # step (VM_JUST_BOOTED, from restart_builder): a boot is a boot whoever asked
  # for it. One that was already RUNNING should answer at once, so a failure
  # there is a firewall or a key, and is not worth more than a few attempts.
  local budget=30 direct_ok=0
  if [ "$WE_STARTED_VM" = "1" ] || [ "$VM_JUST_BOOTED" = "1" ]; then budget=120; fi
  VM_JUST_BOOTED=0
  if [ "$SSH_MODE" = "tunnel" ]; then
    ok "OAM_IAP_SSH_MODE=tunnel -- not probing direct ssh; going through the IAP tunnel"
  elif direct_ssh_probe "$budget"; then
    direct_ok=1
  fi
  REMOTE_TRANSPORT="$(ssh_transport_pick "$SSH_MODE" "$direct_ok")" \
    || fail "OAM_IAP_SSH_MODE=direct, but direct ssh to $INSTANCE did not answer: $DIRECT_SSH_ERR${DIRECT_SSH_HINT:+ -- $DIRECT_SSH_HINT}. Unset OAM_IAP_SSH_MODE (auto) to fall back to the IAP tunnel."
  if [ "$REMOTE_TRANSPORT" = "direct" ]; then
    ok "direct ssh roundtrip ok: ${LINUX_USER}@${DIRECT_IP} (external IP; no IAP tunnel started)"
    return 0
  fi
  if [ "$SSH_MODE" = "auto" ]; then
    warn "FALLING BACK TO THE IAP TUNNEL: direct ssh to ${LINUX_USER}@${DIRECT_IP:-$INSTANCE} failed -- $DIRECT_SSH_ERR"
    if [ -n "$DIRECT_SSH_HINT" ]; then warn "  hint: $DIRECT_SSH_HINT"; fi
    warn "  every remote call now goes through gcloud's IAP relay, which is slower per call and drops idle connections; OAM_IAP_SSH_MODE=tunnel skips this probe next time"
  fi

  step "Start IAP tunnel + verify SSH on $INSTANCE"
  start_iap_tunnel
  # Retry the roundtrip probe: after a cold VM start, OS Login / metadata key
  # propagation can lag the tunnel by a few seconds. 10 x 6s covers a cold
  # boot; a real auth problem fails all ten.
  local probe_ok=0 probe_attempt probe_err="" probe_rc
  for probe_attempt in 1 2 3 4 5 6 7 8 9 10; do
    probe_rc=0
    probe_err="$(gcp_ssh "true" 2>&1 >/dev/null)" || probe_rc=$?
    if [ "$probe_rc" -eq 0 ]; then
      probe_ok=1
      break
    fi
    [ "$probe_attempt" -lt 10 ] && { warn "IAP SSH probe failed (attempt $probe_attempt/10) -- retrying in 6s..."; sleep 6; }
  done
  if [ "$probe_ok" -eq 1 ]; then
    ok "IAP SSH roundtrip ok (tunnel localhost:$IAP_TUNNEL_PORT)"
  else
    probe_err="$(last_nonblank_line "$probe_err" || echo "ssh exited $probe_rc without writing anything to stderr")"
    fail "IAP tunnel is up (port $IAP_TUNNEL_PORT) but ssh 'true' failed after 10 attempts: $probe_err. Check (1) $IAP_SSH_KEY exists, (2) OS Login accepted the key for $LINUX_USER, (3) google_compute_known_hosts is not stale."
  fi
}

# reconnect_builder: prove the ssh path again after a transport drop under a
# step. A tunnel that was up is torn down first -- a reset relay is not
# reused -- and the choice is made afresh: direct, else the tunnel.
reconnect_builder(){
  step "Reconnect to $INSTANCE after an ssh transport drop"
  stop_iap_tunnel
  REMOTE_TRANSPORT=""; DIRECT_IP=""
  connect_builder
}

# restart_builder <status>: the VM was stopped under a remote step -- by an
# operator, a schedule this run could not detach, a host event -- and the
# postmortem has already said who. A stop still in progress settles first
# (STOPPING lasts a minute or so; 3 minutes is the cap), then the same walk
# as at the top starts the VM again, the guest's sshd is waited for, and the
# ssh path is proved afresh -- the external IP is ephemeral and can have
# changed. From here the VM is this run's to stop on exit, whoever started it
# the first time: a run that starts a VM owns that start. One that came back
# RUNNING on its own (someone started it while this waited) is not started and
# not owned, but it is a guest seconds into its boot all the same: its sshd is
# waited for on the serial console (GCE resets the buffer each boot, so the
# banner is this boot's) and the connect step gets the boot budget.
restart_builder(){
  local status="$1" waited=0
  while [ "$waited" -lt 180 ]; do
    case "$status" in TERMINATED | RUNNING) break ;; esac
    warn "$INSTANCE is $status -- waiting for it to settle before starting it again (${waited}s of 180s)"
    sleep 10; waited=$((waited + 10))
    status="$(vm_describe status || echo UNKNOWN)"
  done
  case "$status" in
    TERMINATED)
      start_vm_walk "$status"
      step "Wait for guest sshd on $INSTANCE"
      wait_for_guest_sshd ;;
    RUNNING)
      warn "$INSTANCE is RUNNING again -- someone else started it; waiting for its sshd, then reconnecting without starting it"
      VM_JUST_BOOTED=1
      step "Wait for guest sshd on $INSTANCE"
      wait_for_guest_sshd ;;
    *)
      fail "$INSTANCE is $status ${waited}s after its ssh transport dropped, neither TERMINATED nor RUNNING -- not starting it again" ;;
  esac
  reconnect_builder
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
  ok "sync source -> $INSTANCE:$REMOTE_DIR (via $(transport_desc); remote target/ preserved)"
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
    || fail "scp of the source tarball to $INSTANCE failed ($(transport_desc))"
  gcp_ssh "mkdir -p $REMOTE_DIR && cd $REMOTE_DIR && find . -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} + && tar xzf ~/oam-src.tar.gz && rm -f ~/oam-src.tar.gz" \
    || fail "remote extract of the source tarball into $REMOTE_DIR failed"
  rm -f "$t"
}

# remote_step_postmortem <log>: a remote step whose ssh transport dropped ends
# with `Connection to <host> closed by remote host.` and no remote exit code
# -- which is what a scheduled VM stop, a host error, a network blip and an
# IAP tunnel reset all look like from here. The run's cleanup is about to stop
# the VM and delete the tunnel log, so ask the compute API NOW and put the
# answer next to the failure. Best effort: a postmortem must never mask the
# failure itself.
# remote_step_postmortem <log> [attempts-made] [reruns] [restarts]
# <attempts-made> counts every attempt so far, this one included; of the
# earlier ones, <reruns> dropped with the VM RUNNING (reconnected) and
# <restarts> with it stopped (started again). The RUNNING wording is about the
# transport, so it counts this drop and the reruns alone -- a VM stop that was
# recovered is not a path that is failing to hold -- and names the stops apart.
remote_step_postmortem() {
  local log="$1" attempts="${2:-1}" reruns="${3:-0}" restarts="${4:-0}" status last_stop restart_note=""
  ssh_transport_dropped "$(tail -5 "$log" 2>/dev/null || true)" || return 0
  status="$(vm_describe status || echo UNKNOWN)"
  if [ "$restarts" -gt 0 ] 2>/dev/null; then
    restart_note=" ($restarts of the $attempts attempts ended with $INSTANCE stopped under the step instead, and it was started again each time)"
  fi
  case "$status" in
    RUNNING)
      if [ "$reruns" -gt 0 ] 2>/dev/null; then
        warn "ssh transport dropped under this step $((reruns + 1)) times while $INSTANCE was RUNNING, each time reconnected$restart_note -- the path to the builder is not holding (last transport: $REMOTE_TRANSPORT${DIRECT_IP:+, direct IP $DIRECT_IP}). Try the other one: OAM_IAP_SSH_MODE=tunnel or =direct; OAM_REMOTE_STEP_ATTEMPTS raises the count. Tunnel log: $STAGE_DIR/logs/iap-tunnel.log"
      elif [ "$REMOTE_TRANSPORT" = "direct" ]; then
        warn "ssh transport dropped but $INSTANCE is still RUNNING -- the direct connection to ${DIRECT_IP} was cut under the step (a network blip or an sshd restart on the guest; transient -- re-run)$restart_note"
      else
        warn "ssh transport dropped but $INSTANCE is still RUNNING -- the IAP tunnel reset under the step (transient; re-run)$restart_note. Tunnel log tail: $(tail -3 "$IAP_TUNNEL_LOG" 2>/dev/null | tr -d '\r' | tr '\n' ' ')"
      fi
      ;;
    *)
      last_stop="$(gcloud compute operations list --project="$PROJECT" \
        --filter="targetLink~$INSTANCE AND operationType=stop" --sort-by=~insertTime --limit=1 \
        --format='value(insertTime,user)' 2>/dev/null | tr -d '\r' | tr '\t' ' ' || true)"
      warn "ssh transport dropped because $INSTANCE is $status -- last stop operation: ${last_stop:-none found}. A stop by service-<project-number>@compute-system.iam.gserviceaccount.com is an instance schedule firing (this script detaches those for the run unless OAM_KEEP_VM_SCHEDULE=1)$restart_note"
      ;;
  esac
}

# remote_step_run <dispatch>: one short ssh invocation per build-remote.sh
# dispatch, log captured per step, tail surfaced on failure. 0 when the step
# passed, 1 when it failed for good.
#
# A step whose ssh TRANSPORT dropped under it (OpenSSH's own closing line as
# the log's tail: a reset IAP relay, a cut direct connection, an sshd restart,
# a VM stopped under it) is run again, up to OAM_REMOTE_STEP_ATTEMPTS times in
# all. The VM's state says how (remote_step_verdict in lib/iap-helpers.sh,
# tested): one still RUNNING is reconnected to -- direct ssh is probed again
# and the IAP tunnel is the fallback, as at the start; one TERMINATED or
# STOPPING was stopped under the step, and the postmortem says who BEFORE
# restart_builder starts it again, by the same walk as at the top, and
# reconnects. Each dispatch is a fresh `build-remote.sh <dispatch>` on the
# synced tree, so running one again is safe (cargo waits on its own lock if
# the cut command is still finishing). A step whose command EXITED non-zero is
# a real failure and is never run again; a VM in any other state is left to
# the postmortem. Each earlier attempt's log is kept as <dispatch>.log.attemptN.
REMOTE_STEP_ATTEMPTS="${OAM_REMOTE_STEP_ATTEMPTS:-3}"
remote_step_run(){
  # attempt counts them all; reruns and restarts say how each earlier one
  # ended (the VM RUNNING and reconnected to, or stopped and started again),
  # for a postmortem that tells a flaky path from a stopped VM.
  local dispatch="$1" log="$STAGE_DIR/logs/$1.log" attempt=1 reruns=0 restarts=0 rc status verdict
  while :; do
    rc=0
    gcp_ssh "cd $REMOTE_DIR && bash scripts/build-remote.sh $dispatch" >"$log" 2>&1 || rc=$?
    [ "$rc" -eq 0 ] && return 0
    tail -30 "$log" >&2
    status="$(vm_describe status || echo UNKNOWN)"
    # ssh's own exit (255), not the remote command's: a step whose command
    # exited 1 with a log that mentions a reset is a real failure.
    verdict="$(remote_step_verdict "$attempt" "$REMOTE_STEP_ATTEMPTS" "$rc" "$(tail -5 "$log" 2>/dev/null || true)" "$status")"
    case "$verdict" in
      rerun)
        cp "$log" "$log.attempt$attempt" 2>/dev/null || true
        warn "remote '$dispatch' lost its ssh transport on attempt $attempt of $REMOTE_STEP_ATTEMPTS while $INSTANCE is RUNNING -- reconnecting and running it again (that attempt's log: $log.attempt$attempt)"
        reconnect_builder
        attempt=$((attempt + 1)); reruns=$((reruns + 1))
        continue ;;
      restart)
        cp "$log" "$log.attempt$attempt" 2>/dev/null || true
        # Who stopped it, from the operations log, before the start below
        # becomes the newest operation on the instance.
        remote_step_postmortem "$log" "$attempt" "$reruns" "$restarts"
        warn "remote '$dispatch' lost its ssh transport on attempt $attempt of $REMOTE_STEP_ATTEMPTS because $INSTANCE was stopped under it ($status) -- starting it again, reconnecting and running it again (that attempt's log: $log.attempt$attempt)"
        restart_builder "$status"
        attempt=$((attempt + 1)); restarts=$((restarts + 1))
        continue ;;
    esac
    remote_step_postmortem "$log" "$attempt" "$reruns" "$restarts"
    return 1
  done
}

# remote_step <dispatch>: a failure ends the run.
remote_step(){
  local dispatch="$1"
  remote_step_run "$dispatch" || fail "remote '$dispatch' failed -- see $STAGE_DIR/logs/$dispatch.log"
  ok "remote $dispatch ok"
}

# remote_step_advisory <dispatch>: same, but a failure warns instead of
# aborting. node-compat.yml/bench.yml parity: measurement scorecards and
# bench results upload even when a gate trips (they are written BEFORE the
# gate exits non-zero -- discarding them on failure loses the measurement).
remote_step_advisory(){
  local dispatch="$1"
  if remote_step_run "$dispatch"; then
    ok "remote $dispatch ok"
  else
    warn "remote '$dispatch' failed (advisory -- continuing; see $STAGE_DIR/logs/$dispatch.log)"
  fi
}

# --- preflight ----------------------------------------------------------------
step "Run $RUNID -- oam linux-x64 --mode=$MODE on $INSTANCE"
( cd "$REPO_DIR" && git diff --quiet HEAD ) || warn "working tree dirty -- uncommitted changes WILL ship"

step "Connect to $INSTANCE (OAM_IAP_SSH_MODE=$SSH_MODE)"
connect_builder

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
#
# The probe used to be `df ... | tail -1 | tr -dc '0-9'` with its stderr thrown
# away: tr's status hid df's, so a df that failed answered "" with exit 0 and
# the whole check below was skipped without a word; and an ssh failure ended
# the script under set -e, its diagnostic already discarded (#210). Now df runs
# alone, its stderr and ssh's reach the log, and an unusable answer is said
# out loud. It WARNS rather than fails: whether an unreadable builder should
# stop a release is the maintainer's call, and the mac leg warns too.
read_builder_disk(){  # sets DISK_FREE_GB; 1 (after a warning) when unreadable
  local out rc=0
  out="$(gcp_ssh "df -BG --output=avail /")" || rc=$?
  if DISK_FREE_GB="$(disk_free_reading "$rc" "$out")"; then return 0; fi
  warn "builder disk headroom NOT CHECKED: ${DISK_FREE_GB}. A full builder will not fail here -- it fails ~20 min into cargo with 'os error 28'."
  DISK_FREE_GB=""
  return 1
}
if read_builder_disk; then
  # Tight headroom used to just print advice telling the operator to ssh in and
  # delete things by hand -- a nag that never fixed anything, so the tree kept
  # growing until a run hard-failed here. Reclaim it instead; the build cache
  # survives, so this costs no build time.
  if disk_needs_reclaim "$DISK_FREE_GB"; then
    warn "builder has ${DISK_FREE_GB}GB free on / -- reclaiming accreted cargo output before building"
    gcp_ssh "cd $REMOTE_DIR && bash scripts/build-remote.sh gc" >&2 2>&1 \
      || warn "pre-build reclaim failed -- continuing to the threshold check"
    read_builder_disk || true
  fi
  # Judge AFTER the reclaim: the cheap fix has already run, so anything still
  # short needs a real one.
  if disk_below_floor "$DISK_FREE_GB"; then
    fail "builder still has only ${DISK_FREE_GB}GB free on / after reclaiming prunable cargo output -- a build needs ~7GB. Grow the boot disk, or ssh in and look for space outside ~/${REMOTE_DIR}/target."
  fi
  if [ -n "$DISK_FREE_GB" ]; then ok "builder disk headroom: ${DISK_FREE_GB}GB free on /"; fi
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
