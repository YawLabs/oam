# shellcheck shell=bash
# =============================================================================
# Pure decisions the build-host orchestrators share, lifted out so they can be
# TESTED, plus the repo's one process-tree reaper.
# =============================================================================
# The orchestrators (build-platforms-tailnet.sh, the mac legs, and
# build-platforms-mac-linux.sh, the linux leg in a VM on the same Mac) run
# top-to-bottom against a live host, so a test cannot source them. These are
# the parts that are pure -- a number or a captured text in, a verdict out --
# and that have been wrong in production:
#
#   disk_needs_reclaim  decides whether a release reclaims, proceeds, or aborts
#   disk_below_floor    -- i.e. whether a release runs at all.
#   disk_free_reading   a df probe's answer as a number, or the reason it is not
#                       one (#210: a probe that said nothing read as a skipped
#                       check nobody heard about).
#   last_nonblank_line  what a failed ssh said, for a failure message that has
#                       to carry it.
#   kill_proc_tree      not pure: here so the suite can run it against a real
#                       process tree (lib/signing.sh's timeouts use it).
#
# These lived beside the old cloud linux leg's own decisions until the linux
# leg moved to the build Mac; what nothing else used went with that leg.
#
# Same convention as lib/build-locks.sh: sourced, never executed. All functions
# RETURN status rather than exiting; the caller owns fail()/warn().
# =============================================================================

# --- builder disk headroom ---------------------------------------------------
#
# One clean debug+release build of this workspace needs ~7GB. Reclaim well above
# that so the prune happens BEFORE there is pressure, and abort only below the
# floor -- after the cheap fix has already run.
OAM_DISK_RECLAIM_GB="${OAM_DISK_RECLAIM_GB:-20}"
OAM_DISK_MIN_GB="${OAM_DISK_MIN_GB:-10}"

# disk_needs_reclaim <free-gb>  -- 0 when a prune is warranted.
# A non-numeric or empty reading is NOT treated as low: df failing over a flaky
# connection must not silently trigger a prune, and must not abort a release
# either.
disk_needs_reclaim() {
  case "$1" in '' | *[!0-9]*) return 1 ;; esac
  [ "$1" -lt "$OAM_DISK_RECLAIM_GB" ]
}

# disk_below_floor <free-gb>  -- 0 when a build cannot safely proceed.
disk_below_floor() {
  case "$1" in '' | *[!0-9]*) return 1 ;; esac
  [ "$1" -lt "$OAM_DISK_MIN_GB" ]
}

# disk_free_reading <probe-status> <probe-stdout>
# The probe is `df -BG --output=avail <dir>` run alone on the builder, so its
# status is df's (or ssh's), not that of a trailing `tr` that always succeeds.
# Echoes the whole GB free and returns 0 when the probe answered a size;
# otherwise echoes why the headroom check cannot run and returns 1. Never an
# empty success: a probe that says nothing must not read as a skipped check
# nobody hears about (#210). The caller owns what an unusable reading costs.
disk_free_reading() {
  local status="$1" last gb
  last="$(printf '%s\n' "$2" | tail -1 | tr -d '[:space:]')"
  if [ "$status" != 0 ]; then
    echo "the df probe failed (exit $status; ssh exits 255 when the connection is down)"
    return 1
  fi
  if [ -z "$last" ]; then
    echo "the df probe printed nothing"
    return 1
  fi
  gb="${last%G}"
  case "$gb" in
    "$last" | '' | *[!0-9]*)
      echo "the df probe answered '$last', not a size in GB"
      return 1 ;;
  esac
  echo "$gb"
}

# --- what a failed command said ----------------------------------------------

# last_nonblank_line <text>
# Echoes the last line of <text> with anything but whitespace on it, CRs
# stripped (ssh and other Windows tools end lines in \r\n). Non-zero when there
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

# --- background process reaping -----------------------------------------------
#
# On Windows a scoop shim is a /bin/sh script that runs `cmd.exe /C <tool>.cmd`,
# which runs the real program (python.exe, for the cloud CLI this was written
# against) -- so the pid `$!` names is the sh, and `kill` stops ONLY the sh. The
# program under it lives on: two tunnel processes from the 2026-09-25 03:52
# and 03:56 release runs were still alive ten hours later. `taskkill /T` walks
# the Windows process tree down from the sh's Windows pid (/proc/<pid>/winpid,
# which only MSYS/Cygwin have) and takes the whole chain.
#
# Linux and macOS have no winpid. A launcher that execs its program leaves one
# process -- but a plain kill of any job with children orphans them exactly as
# on Windows, which made "everything under it" true only by the luck of the
# job's shape. So the tree comes from ONE `ps -A -o pid= -o
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
