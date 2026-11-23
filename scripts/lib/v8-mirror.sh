# shellcheck shell=bash
# =============================================================================
# The two assertions behind .cargo/config.toml's RUSTY_V8_MIRROR hedge:
#
#   assert_v8_mirror_sum   -- a just-built tree REALLY fetched from the
#                             YawLabs mirror (reads the .sum receipt that
#                             v8's build.rs writes beside each artifact).
#   v8_mirror_asset_urls   -- the exact URLs a build of a given target-triple
#                             set would fetch for a given locked v8 version.
#
# Why they exist: the config [env] entry does NOT override a pre-set
# environment variable, so one stray RUSTY_V8_MIRROR in a leg's environment
# silently bypasses the hedge while every build stays green -- the .sum receipt
# is the only place the truth lands. And the mirror path embeds CARGO_PKG_VERSION
# (the LOCKED v8 version, per Cargo.lock), so a `cargo update -p v8` invalidates
# the fork's release with no Cargo.toml edit; checking the fork BEFORE the legs
# start costs seconds instead of failing deep inside a gate run. Both compare
# against the value COMMITTED in .cargo/config.toml, never the environment's,
# and both refuse when the environment overrides it.
#
# Same convention as lib/build-locks.sh: sourced, never executed. All
# functions RETURN status rather than exiting; the caller owns fail()/ok().
# The assert_ variant prints ok() lines on success and nothing on failure, so
# it is safe under ci-local.sh's `set -e` as a bare statement (set -e stops on an UNCAUGHT
# non-zero; an `if !` test is caught by definition).
# =============================================================================

# v8_mirror_config_base -- the base URL committed in .cargo/config.toml (the
# file the hedge lives in), and nothing else. This is the EXPECTED value every
# check compares against: an env-derived "expected" value would match a stray
# env var's own receipt and wave the bypass through.
v8_mirror_config_base() {
  local cfg=".cargo/config.toml" line
  [ -f "$cfg" ] || return 1
  line="$(sed -n 's/^RUSTY_V8_MIRROR *= *"\(.*\)"[[:space:]]*$/\1/p' "$cfg")" || return 1
  [ -n "$line" ] || return 1
  printf '%s\n' "$line"
}

# v8_mirror_base  -- the base URL a cargo build in this environment actually
# fetches from: the config value, unless RUSTY_V8_MIRROR is pre-set (cargo
# [env] does not override an already-set variable, so the env WINS when set --
# which is exactly the bypass the checks below have to refuse).
v8_mirror_base() {
  if [ -n "${RUSTY_V8_MIRROR:-}" ]; then
    printf '%s\n' "$RUSTY_V8_MIRROR"
    return 0
  fi
  v8_mirror_config_base
}

# v8_mirror_env_bypass <config-base> -- 0 when RUSTY_V8_MIRROR is set in this
# environment to something other than <config-base> (builds here would fetch
# from it instead of the committed mirror), 1 otherwise.
v8_mirror_env_bypass() {
  [ -n "${RUSTY_V8_MIRROR:-}" ] && [ "$RUSTY_V8_MIRROR" != "$1" ]
}

# v8_locked_version [manifest] -- the v8 version builds actually use, from
# Cargo.lock (CARGO_PKG_VERSION is the LOCKED version, not Cargo.toml's
# requirement). First `name = "v8"` stanza, first `version = "..."` after it.
v8_locked_version() {
  local lock="${1:-Cargo.lock}" ver
  [ -f "$lock" ] || return 1
  ver="$(awk '
    $0 == "name = \"v8\"" { inv8 = 1; next }
    inv8 && /^version = "/ {
      sub(/^version = "/, ""); sub(/"$/, ""); print; exit
    }
    /^\[\[package\]\]/ { inv8 = 0 }
  ' "$lock")" || return 1
  # awk exits 0 on a stanza-free file too; an empty answer is a failure, not a
  # version -- a caller building a URL from "" would 404 on the base path.
  [ -n "$ver" ] || return 1
  printf '%s\n' "$ver"
}

# v8_mirror_asset_names <version> <target>... -- the asset file names v8's
# build.rs derives for each target (build.rs:590-600, :894-898): the static
# lib in the `release` profile with no feature suffix (prebuilt_profile is
# `release` unless V8_FORCE_DEBUG on non-Windows; oam enables no v8 features)
# and the matching src_binding. Keep the shape in step with a v8 upgrade.
v8_mirror_asset_names() {
  local version="${1:-}" target lib
  # Checked first, and returned rather than `${version:?}` (which EXITS a
  # non-interactive shell): the version is part of every URL.
  [ -n "$version" ] || { echo "v8_mirror_asset_names: version required" >&2; return 1; }
  shift
  for target in "$@"; do
    case "$target" in
      *windows*) lib="rusty_v8_release_${target}.lib" ;;
      *)         lib="librusty_v8_release_${target}.a" ;;
    esac
    printf '%s\n' "${lib}.gz" "src_binding_release_${target}.rs"
  done
}

# v8_mirror_asset_urls <version> <target>... -- the full URLs under the
# COMMITTED base (v8_mirror_config_base). Refuses, with the reason on stderr,
# when RUSTY_V8_MIRROR in this environment points elsewhere: a build here would
# not fetch these URLs at all, so probing them would vouch for the wrong origin.
v8_mirror_asset_urls() {
  local base version="${1:-}"
  [ -n "$version" ] || { echo "v8_mirror_asset_urls: version required" >&2; return 1; }
  base="$(v8_mirror_config_base)" || { echo "v8_mirror_asset_urls: no RUSTY_V8_MIRROR in .cargo/config.toml" >&2; return 1; }
  if v8_mirror_env_bypass "$base"; then
    echo "v8_mirror_asset_urls: RUSTY_V8_MIRROR is set in this environment ($RUSTY_V8_MIRROR) and overrides .cargo/config.toml's $base -- unset it" >&2
    return 1
  fi
  v8_mirror_asset_names "$@" | while IFS= read -r name; do
    printf '%s/v%s/%s\n' "$base" "$version" "$name"
  done
}

# assert_v8_mirror_sum [profile]... -- 0 with one ok() per receipt when EVERY
# existing receipt for the named profiles (default: debug release) records a
# fetch from the committed mirror base, 1 with nothing printed otherwise --
# including when none of those receipts exists, and when RUSTY_V8_MIRROR is
# set in this environment to anything but the committed base. A receipt is
# ${CARGO_TARGET_DIR:-target}/<profile>/build/gn_out/obj/rusty_v8.sum, which
# v8's build.rs writes with the URL it downloaded. It is rewritten only on a
# re-download (it is also what stops re-downloads), so it records the origin
# of the prebuilt this tree links, not of the latest build; a forced
# re-download is what refreshes it. Pass only the profiles just built: a
# stale receipt for another profile proves nothing about this build.
assert_v8_mirror_sum() {
  local base f url root="${CARGO_TARGET_DIR:-target}" profile seen=0
  local -a oks=()
  base="$(v8_mirror_config_base)" || return 1
  v8_mirror_env_bypass "$base" && return 1
  [ "$#" -gt 0 ] || set -- debug release
  for profile in "$@"; do
    f="$root/$profile/build/gn_out/obj/rusty_v8.sum"
    [ -f "$f" ] || continue
    seen=1
    url="$(head -n1 "$f")"
    case "$url" in
      "$base"/*) oks+=("$url") ;;
      *) return 1 ;;
    esac
  done
  [ "$seen" = "1" ] || return 1
  for url in "${oks[@]}"; do
    ok "v8 fetched from the mirror: $url"
  done
}
