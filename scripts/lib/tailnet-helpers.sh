# shellcheck shell=bash
# =============================================================================
# Pure decisions lifted out of the mac orchestrator so they can be TESTED.
# =============================================================================
# scripts/build-platforms-tailnet.sh runs against the MacBook Air, so a test
# cannot execute its build. These are the parts of its host preflight that are
# pure -- captured text in, a verdict out -- and they are what decides what the
# operator is told when the Air cannot be used.
#
# They exist because that answer was wrong on 2026-09-30. The release box had
# signed in to a different tailnet since the last release (its node there dates
# from 2026-09-27) and the Air had not been signed in to it, so the address
# that built v0.17.0 had no device behind it, and the preflight reported
#
#   mac host '<the Air's tailnet IP>' does not resolve via DNS. Use the
#   Tailscale MagicDNS name (run 'tailscale status') or the tailnet IP.
#
# for a host that WAS given as a tailnet IP. The check behind that message ran
# `nslookup <host>` on Windows, which is wrong twice over: for an IP literal it
# is a REVERSE lookup, which an address only passes while something serves a
# PTR record for it; and nslookup queries the adapter's DNS server directly,
# skipping the Windows name-resolution policy rules Tailscale installs for
# MagicDNS. It had passed for that address on the releases before; on the
# tailnet the box is on now it cannot pass for anything. Probed there the same
# day: nslookup failed this machine's own tailnet IP and its own MagicDNS name,
# and, once the Air had joined, the Air's new address and name too -- while ssh
# reached both machines. So the orchestrator now asks ssh alone -- the resolver
# and the route every later scp uses -- and classifies what ssh says.
#
# Same convention as lib/iap-helpers.sh: sourced, never executed. All functions
# RETURN status rather than exiting; the caller owns fail()/warn().
# =============================================================================

# tailnet_ssh_failure <ssh-output>
# Echoes the class of failure a failed `ssh ... true` printed:
#
#   auth         sshd answered and refused authentication (the key is not
#                enrolled, the account is wrong, or ssh could not load the key)
#   hostkey      the host answered with a key other than the recorded one
#   resolve      the name did not resolve
#   refused      the host is up and nothing listens on tcp:22
#   unreachable  connect() got no answer from that address
#   unknown      anything else -- the caller prints ssh's own text
#
# `auth` needs the method list sshd's refusal carries ("Permission denied
# (publickey)."): connect() can fail with the same two words and no list
# ("ssh: connect to host ... port 22: Permission denied"), and that is a
# blocked connection, not an authentication refusal.
#
# `unreachable` needs connect()'s own line. ConnectTimeout also bounds the
# banner exchange, and "Connection timed out during banner exchange" means
# something DID accept the connection: that one stays `unknown`.
tailnet_ssh_failure() {
  case "$1" in
    *'Host key verification failed'* | *'REMOTE HOST IDENTIFICATION HAS CHANGED'*) printf 'hostkey' ;;
    *'Permission denied ('*) printf 'auth' ;;
    *'Could not resolve hostname'*) printf 'resolve' ;;
    *'Connection refused'*) printf 'refused' ;;
    *'connect to host'*'timed out'* | *'No route to host'* | *'Network is unreachable'* | *'Host is down'*) printf 'unreachable' ;;
    *) printf 'unknown' ;;
  esac
  return 0
}

# --- `tailscale status` ---------------------------------------------------------
#
# One row per device, this machine first, then its peers:
#
#   100.80.0.1  release-box  owner@  windows  -
#   100.90.0.6  macbook-air  owner@  macOS    offline, last seen 3d ago
#
# address, name, owner, OS, state. A row is a line whose first field is an
# address: blank lines, the `#` lines tailscale appends (health warnings) and
# anything the CLI wrote to stderr -- the callers capture both streams -- are
# not rows. The name is the device's MagicDNS label; a device shared in from
# another tailnet is listed by its full DNS name. Only the device's first
# address is shown, which is its IPv4 one.
#
# The row test, repeated in each awk program below (mawk has no interval
# expressions, so no {1,3}):
#   $1 ~ /^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$/ || $1 ~ /^[0-9A-Fa-f:]*:[0-9A-Fa-f:]*$/

# tailnet_peer_state <host> <`tailscale status` output>
# Echoes what the table says about <host>, given as an IPv4 address, a MagicDNS
# name or that name's first label:
#
#   absent    no row for it
#   offline   a row whose state says offline
#   listed    a row, not marked offline. NOT "reachable": the offline mark is
#             the coordination server's view of whether the peer is connected
#             to it, not whether the peer answers on tcp:22.
#
# Echoes nothing for an IPv6 address, which the table cannot answer for.
tailnet_peer_state() {
  local host="$1" text="${2//$'\r'/}"
  case "$host" in *:*) return 0 ;; esac
  awk -v want="$host" '
    BEGIN {
      want = tolower(want); sub(/\.$/, "", want)
      isip = (want ~ /^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$/)
      label = want; if (!isip) sub(/\..*$/, "", label)
      state = "absent"
    }
    !($1 ~ /^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$/ || $1 ~ /^[0-9A-Fa-f:]*:[0-9A-Fa-f:]*$/) { next }
    {
      name = tolower($2); sub(/\.$/, "", name)
      first = name; sub(/\..*$/, "", first)
      if ($1 == want || name == want || (!isip && first == label)) {
        # From field 4, not 5: a peer that reported no OS has an empty OS cell,
        # which moves its state one field left. No OS name contains "offline".
        rest = ""; for (i = 4; i <= NF; i++) rest = rest " " $i
        state = (rest ~ /offline/) ? "offline" : "listed"
        exit
      }
    }
    END { printf "%s", state }
  ' <<<"$text"
}

# tailnet_peer_rows <`tailscale status` output>
# Echoes one "address  name  OS" line, with "  offline" appended where the row
# says so, for each device the table lists besides this machine -- nothing when
# this machine is alone. At most 8, then "... and N more": this is for a
# failure message, where the operator needs to spot the Air, not read a tailnet.
# The free-form state column (endpoints, traffic counters) is left out.
tailnet_peer_rows() {
  local text="${1//$'\r'/}"
  awk '
    !($1 ~ /^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$/ || $1 ~ /^[0-9A-Fa-f:]*:[0-9A-Fa-f:]*$/) { next }
    ++n == 1 { next }
    n > 9 { next }
    {
      rest = ""; for (i = 4; i <= NF; i++) rest = rest " " $i
      printf "%s  %s  %s%s\n", $1, $2, $4, (rest ~ /offline/ ? "  offline" : "")
    }
    END { if (n > 9) printf "... and %d more\n", n - 9 }
  ' <<<"$text"
}
