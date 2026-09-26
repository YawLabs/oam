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
#   ssh_transport_pick  direct ssh or the IAP tunnel -- the choice that decides
#   last_nonblank_line  whether a run reaches the builder at all, and what it
#                       says when it cannot.
#
# Same convention as lib/build-locks.sh: sourced, never executed. All functions
# RETURN status rather than exiting; the caller owns fail()/warn(). The one
# helper that is not pure, kill_proc_tree, is here so the suite can run it
# against a real process tree.
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

# --- ssh transport: direct first, the IAP tunnel as the fallback --------------
#
# The builder has an external IP, and the default network's default-allow-ssh
# rule opens tcp:22 on it to 0.0.0.0/0 -- so plain OpenSSH straight at that IP
# works, and while that rule stands IAP adds no security. Measured
# from the Windows orchestrator on 2026-09-25: ~1s per direct `ssh true`,
# against 11-57s per `gcloud compute ssh --tunnel-through-iap` call (gcloud's
# Python startup, then the IAP websocket relay). That day the sibling yaw
# release died with "not reachable over IAP within 1269s" against a VM that
# was up and healthy throughout: only 4 of 20 IAP probes even reached sshd, and
# those 4 authenticated fine. So the orchestrator goes direct and keeps the
# tunnel for a host that cannot reach tcp:22. OAM_IAP_SSH_MODE forces either.

# ssh_mode_valid <mode>   -- 0 for a value OAM_IAP_SSH_MODE accepts.
ssh_mode_valid() {
  case "$1" in auto | direct | tunnel) return 0 ;; *) return 1 ;; esac
}

# ssh_transport_pick <mode> <direct-answered: 1 or anything else>
# Echoes the transport the run uses: `direct` or `tunnel`. Non-zero, echoing
# nothing, when there is none: OAM_IAP_SSH_MODE=direct forbids the fallback,
# so a direct path that did not answer ends the run. `tunnel` never probes the
# direct path, so its second argument is ignored.
ssh_transport_pick() {
  case "$1" in
    tunnel) printf 'tunnel' ;;
    direct) [ "$2" = "1" ] || return 1; printf 'direct' ;;
    auto) if [ "$2" = "1" ]; then printf 'direct'; else printf 'tunnel'; fi ;;
    *) return 1 ;;
  esac
}

# last_nonblank_line <text>
# Echoes the last line of <text> with anything but whitespace on it, CRs
# stripped (ssh and gcloud on Windows end lines in \r\n). Non-zero when there
# is no such line, so the caller has to say what that MEANS instead of printing
# a placeholder: "Last error: (empty stderr)" is all 1269s of failed probes
# left the operator on 2026-09-25.
last_nonblank_line() {
  local text="${1//$'\r'/}" line last=""
  while IFS= read -r line; do
    [[ "$line" =~ [^[:space:]] ]] && last="$line"
  done <<<"$text"
  [ -n "$last" ] || return 1
  printf '%s' "$last"
}

# ssh_error_is_permanent <ssh-stderr>   -- 0 when retrying cannot help.
# A changed host key is the one direct-ssh failure no amount of polling fixes:
# ephemeral external IPs are recycled across stop/start, so an address this box
# once recorded for another host can come back on this one, and OpenSSH refuses
# it until the stale entry goes. Everything else a probe sees on a booting VM --
# refused, timed out, publickey denied while the guest agent is still writing
# keys -- is expected for the first minute.
ssh_error_is_permanent() {
  grep -qE 'Host key verification failed|REMOTE HOST IDENTIFICATION HAS CHANGED' <<<"$1"
}

# direct_ssh_hint <ssh-stderr> <ip>
# One actionable sentence for the failure the text shows; nothing for a shape
# it does not recognise (the stderr line itself then has to carry it).
direct_ssh_hint() {
  local t="$1" ip="$2"
  case "$t" in
    *'Host key verification failed'* | *'REMOTE HOST IDENTIFICATION HAS CHANGED'*)
      printf 'the known_hosts entry for %s is for another host (ephemeral IPs are recycled); remove it with: ssh-keygen -R %s -f ~/.ssh/google_compute_known_hosts' "$ip" "$ip" ;;
    *'Permission denied'*)
      printf 'sshd refused ~/.ssh/google_compute_engine; running gcloud compute ssh against the VM once publishes that key for this user' ;;
    *'timed out'* | *'No route to host'* | *'Network is unreachable'*)
      printf 'tcp:22 on %s did not answer from this host; a firewall rule must allow it (default-allow-ssh does, unless it was removed) and this network must allow outbound ssh' "$ip" ;;
    *'Connection refused'*)
      printf 'nothing is listening on %s:22 -- sshd is down or still starting' "$ip" ;;
  esac
  return 0
}

# --- background process reaping -----------------------------------------------
#
# On Windows the scoop gcloud shim is a /bin/sh script that runs
# `cmd.exe /C gcloud.cmd`, which runs python.exe -- so the pid `$!` names is
# the sh, and `kill` stops ONLY the sh. The python under it lives on: two
# `gcloud compute start-iap-tunnel` processes from the 2026-09-25 03:52 and
# 03:56 runs were still alive ten hours later. `taskkill /T` walks the Windows
# process tree down from the sh's Windows pid (/proc/<pid>/winpid, which only
# MSYS/Cygwin have) and takes the whole chain.
#
# Linux and macOS have no winpid. gcloud's launcher execs python there, so the
# tunnel is one process today -- but a plain kill of any job with children
# orphans them exactly as on Windows, which made "everything under it" true only
# by the luck of the job's shape. So the tree comes from ONE `ps -A -o pid= -o
# ppid=` snapshot (procps and BSD ps agree on it), taken before anything is
# signalled: a child whose parent dies first is reparented and can no longer be
# found by walking. Then every process is SIGKILLed, parents first, so a shell
# cannot start its next command as its child dies; KILL rather than TERM because
# taskkill /F forces too, and a hung process that ignores TERM would otherwise
# survive, with the `wait` below blocking on it if it is the job itself. Never
# `kill -- -<pgid>`: with no job control the job shares the caller's process
# group, so that would kill the caller too. MSYS ps rejects -A, so a Windows job
# whose winpid is already gone just has the job itself killed.
#
# Path conversion is switched off for the one call rather than spelling the
# flags //F: under an exported MSYS_NO_PATHCONV=1, //F reaches taskkill
# literally and it rejects it.

# kill_proc_tree <pid>
# Kills a background job and everything under it, then reaps it. Always
# returns 0: a process that has already gone is the goal, not an error.
kill_proc_tree() {
  local pid="${1:-}" winpid="" desc="" p
  # Only a real job pid. 0 or 1 would make the walk below return nearly every
  # process on the host, and -1 makes `kill -KILL -1` signal all of them.
  case "$pid" in '' | *[!0-9]* | 0 | 1) return 0 ;; esac
  if [ -r "/proc/$pid/winpid" ] && command -v taskkill >/dev/null 2>&1; then
    winpid="$(cat "/proc/$pid/winpid" 2>/dev/null || true)"
    winpid="${winpid//[!0-9]/}"
    if [ -n "$winpid" ]; then
      MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' taskkill /F /T /PID "$winpid" >/dev/null 2>&1 || true
    fi
    kill "$pid" 2>/dev/null || true
  else
    desc="$(ps -A -o pid= -o ppid= 2>/dev/null | awk -v root="$pid" '
      { parent[$1] = $2 }
      END { n = 1; q[1] = root
            for (i = 1; i <= n; i++) for (c in parent) if (parent[c] == q[i]) { q[++n] = c; print c } }' || true)"
    for p in "$pid" $desc; do kill -KILL "$p" 2>/dev/null || true; done
  fi
  wait "$pid" 2>/dev/null || true
  return 0
}
