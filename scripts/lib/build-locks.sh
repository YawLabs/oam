# shellcheck shell=bash
# =============================================================================
# Build-output lock helpers, shared by ci-local.sh, release-local.sh and
# release-upload-local-arm64.sh.
# =============================================================================
# cargo's link step intermittently dies on Windows with one of:
#
#   error: failed to remove file `target\release\oam.exe`
#   Caused by: Access is denied. (os error 5)
#   LINK : fatal error LNK1104: cannot open file '...\deps\e2e-<hash>.exe'
#
# The usual explanation -- "Windows won't let you delete a running .exe" -- is
# WRONG on Windows 11, and believing it sends the fix the wrong way. Measured on
# this box 2026-08-06, 60 trials each:
#
#   steady-state running image, delete   -> SUCCEEDS (POSIX unlink; the mapped
#                                           section keeps the file alive)
#   image being LAUNCHED, delete         -> 59/60 ACCESS DENIED
#   image being LAUNCHED, rename         -> 60/60 OK
#
# The deny window is process STARTUP: while the loader maps an image it holds
# the file without FILE_SHARE_DELETE. Two consequences, and both are why this
# file exists:
#
#   1. RENAME is the correct primitive and DELETE is not. Renaming succeeded in
#      every trial that denied a delete, and holders never notice -- they keep
#      their handle to the renamed file.
#   2. `taskkill //F //IM oam.exe` (what these scripts used to do) is not just
#      hostile, it CAUSES the failure. Killing the operator's typed-cli panes
#      makes all of them restart on --resume within seconds, so it opens a burst
#      of startup windows at exactly the moment the link step needs the path.
#
# oam trips the startup window more than most Rust projects because it SPAWNS
# ITSELF: the type-check daemon re-execs `std::env::current_exe()` with
# DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP (crates/oam_ts/src/daemon.rs), so
# it outlives its parent, is unreachable from that parent's process group, and
# idles for OAM_DAEMON_IDLE_MS -- 45s under the e2e suite, THIRTY MINUTES by
# default. Every one of those is a future startup window on a build output.
#
# Two tools, deliberately separate:
#
#   oam_park_*  -- make a path writable WITHOUT killing anything. Windows
#                  allows RENAMING a running image: holders keep their handle
#                  to the renamed file and never notice. This is the default,
#                  because target/release/oam.exe is what live typed-cli
#                  sessions run and `taskkill //F //IM oam.exe` (what these
#                  scripts used to do) kills the operator's other agent panes
#                  -- and loses the race anyway, since those sessions restart
#                  on --resume within seconds.
#   oam_kill_under -- kill processes by IMAGE PATH PREFIX, never by image name.
#                  Used only on target/debug after the test step, where every
#                  process is by definition a leftover of the run that just
#                  ended. Path-scoping is what makes it structurally incapable
#                  of touching a target/release session.
#
# All functions RETURN status rather than exiting: ci-local.sh fails via ko(),
# release-local.sh via fail(), and neither is available here.
# =============================================================================

# oam_park_file <path>
# Frees <path> if anything holds it. Returns 0 if the path is free afterwards.
oam_park_file() {
  local path="$1" parked
  [ -e "$path" ] || return 0
  parked="$path.inuse-$$-$(date +%s)"
  mv "$path" "$parked" 2>/dev/null || true
  # Deleting the parked copy usually works (a fully-mapped image unlinks fine),
  # but not if something is mid-launch from it. Failing is fine -- a parked copy
  # is some MB inside gitignored target/, and oam_reap_parked collects it on a
  # later run.
  rm -f "$parked" 2>/dev/null || true
  [ -e "$path" ] && return 1
  return 0
}

# oam_park_glob <glob>...
# Parks every existing match. Unmatched globs and already-parked copies are
# skipped. Returns 1 if any path could not be freed.
oam_park_glob() {
  local pattern hit rc=0
  for pattern in "$@"; do
    for hit in $pattern; do
      [ -e "$hit" ] || continue
      case "$hit" in *.inuse-*) continue ;; esac
      oam_park_file "$hit" || rc=1
    done
  done
  return "$rc"
}

# oam_reap_parked <dir>
# Deletes parked copies left by earlier runs whose holder has since exited.
# Always succeeds: one still-locked leftover must never fail a build.
oam_reap_parked() {
  local dir="$1"
  [ -d "$dir" ] || return 0
  find "$dir" -maxdepth 1 -name '*.inuse-*' -exec rm -f {} + 2>/dev/null || true
  return 0
}

# oam_kill_under <abs-dir>
# Kills every process whose EXECUTABLE IMAGE lives under <abs-dir>. Matching is
# on the image path, never the image name, so passing target/debug cannot reach
# a target/release process no matter what it is called. Always succeeds.
oam_kill_under() {
  local dir="$1" win_dir
  [ -d "$dir" ] || return 0
  case "$(uname -s 2>/dev/null || echo unknown)" in
    MINGW* | MSYS* | CYGWIN*)
      win_dir="$(cygpath -w "$dir" 2>/dev/null || echo "$dir")"
      # StartsWith, not -like: a path is data, and -like would read [ ] * ? in
      # it as wildcards. -NonInteractive so this can never block a headless run.
      powershell.exe -NoProfile -NonInteractive -Command \
        "\$d = '$win_dir'; Get-CimInstance Win32_Process | Where-Object { \$_.ExecutablePath -and \$_.ExecutablePath.StartsWith(\$d, [System.StringComparison]::OrdinalIgnoreCase) } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force -ErrorAction SilentlyContinue }" \
        >/dev/null 2>&1 || true
      ;;
    *)
      # Unix can unlink a running image, so the LOCK half of this file is moot
      # there -- but a detached daemon still leaks, and pkill -f matches the
      # argv[0] path the daemon was re-exec'd with.
      command -v pkill >/dev/null 2>&1 && pkill -f "^$dir/" >/dev/null 2>&1
      ;;
  esac
  return 0
}

# oam_dir_gb <dir> -- size in whole GB, "0" when absent. Used for the
# target/-bloat advisory; du is fine here, nothing depends on precision.
oam_dir_gb() {
  local dir="$1" kb
  [ -d "$dir" ] || { echo 0; return 0; }
  kb="$(du -sk "$dir" 2>/dev/null | awk '{print $1; exit}')"
  [ -n "$kb" ] || { echo 0; return 0; }
  echo $((kb / 1024 / 1024))
}
