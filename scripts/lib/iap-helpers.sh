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
