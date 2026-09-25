# shellcheck shell=bash
# =============================================================================
# Pure decisions lifted out of the IAP orchestrator so they can be TESTED.
# =============================================================================
# scripts/build-platforms-gcp-iap.sh runs top-to-bottom against live GCP, so a
# test cannot source it. These are the parts of it that are pure -- a number or
# a captured log in, a verdict out -- and they are also the parts that have
# actually been wrong in production:
#
#   iap_parse_*_port    the port parsing is what the gcloud stdout-buffering
#                       bug hid behind; gcloud`s output format is external and
#                       can change under us.
#   sshd_banner_seen    a distro or unit-name change turns guest-boot detection
#                       into a silent 180s stall on every cold VM start.
#   disk_needs_reclaim  decides whether a release reclaims, proceeds, or aborts
#   disk_below_floor    -- i.e. whether a release runs at all.
#
# Same convention as lib/build-locks.sh: sourced, never executed. All functions
# RETURN status rather than exiting; the caller owns fail()/warn().
# =============================================================================

# --- gcloud tunnel log parsing -----------------------------------------------
#
# gcloud prints the local port in two different shapes, and which one you got
# changes what it MEANS (surface/compute/start_iap_tunnel.py + command_lib/
# compute/iap_tunnel.py):
#
#   "Picking local unused port [N]."   stderr. DetermineLocalPort() bound an
#                                      ephemeral socket, read N, and CLOSED it
#                                      again. N is reserved by convention only
#                                      -- nothing is listening yet, and any
#                                      other process on the box could take it.
#   "Testing if tunnel connection works."   stderr. The backend round-trip.
#   "Listening on port [N]."           stdout. _OpenLocalTcpSockets() has now
#                                      actually bound N. This is the only line
#                                      that proves the tunnel is up.
#
# So "Listening" is authoritative and "Picking" is a hint that still needs a
# reachability probe. Keep them as separate functions -- collapsing them into
# one "get the port" would erase exactly that distinction.

# iap_parse_listening_port <tunnel-log>
# Echoes the port gcloud has BOUND. Non-zero if it has not logged that yet.
iap_parse_listening_port() {
  local log="$1" port
  [ -f "$log" ] || return 1
  port="$(awk -F'[][]' '/Listening on port/ {print $2; exit}' "$log")"
  [ -n "$port" ] || return 1
  printf '%s' "$port"
}

# iap_parse_picked_port <tunnel-log>
# Echoes the port gcloud RESERVED but may not have bound yet. Non-zero if absent.
iap_parse_picked_port() {
  local log="$1" port
  [ -f "$log" ] || return 1
  port="$(awk -F'[][]' '/Picking local unused port/ {print $2; exit}' "$log")"
  [ -n "$port" ] || return 1
  printf '%s' "$port"
}

# --- guest boot detection ----------------------------------------------------

# sshd_banner_seen <serial-console-text>
# 0 when the text shows sshd came up. GCE resets the serial buffer on each boot,
# so a match is necessarily from the CURRENT boot and needs no timestamp check.
# Matches the systemd unit line on Debian/Ubuntu ("Started ssh.service - OpenBSD
# Secure Shell server.") plus the RHEL-family and older unit namings.
sshd_banner_seen() {
  grep -qE 'Started (ssh|sshd)\.service|Started OpenBSD Secure Shell|Started OpenSSH' <<<"$1"
}

# --- builder disk headroom ---------------------------------------------------
#
# One clean debug+release build of this workspace needs ~7GB. Reclaim well above
# that so the prune happens BEFORE there is pressure, and abort only below the
# floor -- after the cheap fix has already run.
OAM_DISK_RECLAIM_GB="${OAM_DISK_RECLAIM_GB:-20}"
OAM_DISK_MIN_GB="${OAM_DISK_MIN_GB:-10}"

# disk_needs_reclaim <free-gb>  -- 0 when a prune is warranted.
# A non-numeric or empty reading is NOT treated as low: df failing over a flaky
# tunnel must not silently trigger a prune, and must not abort a release either.
disk_needs_reclaim() {
  case "$1" in '' | *[!0-9]*) return 1 ;; esac
  [ "$1" -lt "$OAM_DISK_RECLAIM_GB" ]
}

# disk_below_floor <free-gb>  -- 0 when a build cannot safely proceed.
disk_below_floor() {
  case "$1" in '' | *[!0-9]*) return 1 ;; esac
  [ "$1" -lt "$OAM_DISK_MIN_GB" ]
}

# --- instance schedules ------------------------------------------------------
#
# A GCE instance schedule (a resource policy with a vmStopSchedule) stops the
# VM at a clock time whatever it is running. yaw-linux-builder carries
# `yaw-linux-builder-autostop` -- `0 3 * * *` America/Los_Angeles, the cost
# backstop for a VM left running -- and on 2026-09-25 the v0.17.0 release leg
# started at 02:39, was 86s into node-suite at 03:00, and died with nothing
# but `Connection to localhost closed by remote host.` The orchestrator now
# detaches stop schedules for the run and re-attaches them on exit; these are
# the pure parts of that.
#
# gcloud's `value(resourcePolicies)` prints the attached policies as selfLink
# URLs joined with `;` on one line, CRLF-terminated on Windows:
#   https://www.googleapis.com/compute/v1/projects/P/regions/R/resourcePolicies/NAME
# The region is in the URL and is what `resource-policies describe` needs. The
# \r matters: a name carrying one would look up a policy that does not exist.

# iap_policy_urls <value-output>
# Echoes one policy URL per line from a `value(resourcePolicies)` reading, and
# nothing at all for an empty reading (a VM with no policies attached).
iap_policy_urls() {
  local v="${1//$'\r'/}"
  [ -n "$v" ] || return 0
  printf '%s\n' "${v//;/$'\n'}"
}

# iap_policy_name <url-or-name>   -- the bare policy name. Non-zero if empty.
iap_policy_name() {
  local p="${1//$'\r'/}"
  p="${p##*/}"
  [ -n "$p" ] || return 1
  printf '%s' "$p"
}

# iap_policy_region <url>   -- the region segment of a policy selfLink.
# Non-zero for a bare name (nothing to read); the caller falls back to the
# zone's region.
iap_policy_region() {
  local p="${1//$'\r'/}" r
  case "$p" in */regions/*/*) ;; *) return 1 ;; esac
  r="${p##*/regions/}"
  r="${r%%/*}"
  [ -n "$r" ] || return 1
  printf '%s' "$r"
}

# --- ssh transport drops -----------------------------------------------------
#
# A remote step that ends because the CONNECTION died, not because the remote
# command exited, leaves one of OpenSSH's transport messages as the last line
# of its log. A scheduled VM stop, a host error and an IAP tunnel reset all
# look identical from here; the orchestrator uses this to know when to go and
# ask the compute API which one it was.

# ssh_transport_dropped <log-tail-text>   -- 0 when the text carries one.
ssh_transport_dropped() {
  grep -qE 'closed by remote host|Connection reset by peer|Broken pipe|Connection closed by|client_loop: send disconnect|Connection timed out' <<<"$1"
}
