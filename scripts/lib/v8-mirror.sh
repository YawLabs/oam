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
# start costs seconds instead of failing deep inside a gate run.
#
# Same convention as lib/build-locks.sh: sourced, never executed. All
# functions RETURN status rather than exiting; the caller owns fail()/ok().
# The assert_ variant prints one ok() or nothing at all, so it is safe under
# ci-local.sh's `set -e` as a bare statement (set -e stops on an UNCAUGHT
# non-zero; an `if !` test is caught by definition).
# =============================================================================

# v8_mirror_base  -- the base URL .cargo/config.toml points builds at, read
# from the config itself (the file the hedge lives in) with an env override
# (cargo [env] does not override an already-set variable, so the env WINS when
# set -- which is exactly the bypass the sum check has to see).
v8_mirror_base() {
  local cfg=".cargo/config.toml" line
  if [ -n "${RUSTY_V8_MIRROR:-}" ]; then
    printf '%s\n' "$RUSTY_V8_MIRROR"
    return 0
  fi
  [ -f "$cfg" ] || return 1
  line="$(sed -n 's/^RUSTY_V8_MIRROR *= *"\(.*\)"[[:space:]]*$/\1/p' "$cfg")" || return 1
  [ -n "$line" ] || return 1
  printf '%s\n' "$line"
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
  local version="$1" target lib
  shift
  for target in "$@"; do
    case "$target" in
      *windows*) lib="rusty_v8_release_${target}.lib" ;;
      *)         lib="librusty_v8_release_${target}.a" ;;
    esac
    printf '%s\n' "${lib}.gz" "src_binding_release_${target}.rs"
  done
  : "${version:?}"  # version is part of the URL, asserted non-empty
}

# v8_mirror_asset_urls <version> <target>... -- the full URLs, base from
# v8_mirror_base.
v8_mirror_asset_urls() {
  local base version
  base="$(v8_mirror_base)" || return 1
  version="$1"
  v8_mirror_asset_names "$@" | while IFS= read -r name; do
    printf '%s/v%s/%s\n' "$base" "$version" "$name"
  done
}

# assert_v8_mirror_sum [receipt] -- 0 with one ok() when the receipt records
# a fetch from the mirror base, 1 with nothing printed otherwise. The receipt
# is target/<profile>/build/gn_out/obj/rusty_v8.sum, which v8's build.rs
# rewrites with the URL it downloaded (it is also what stops re-downloads, so
# a forced re-download is what refreshes it). Multiple receipts (both
# profiles) pass if ANY records the mirror.
assert_v8_mirror_sum() {
  local base f url found=1
  base="$(v8_mirror_base)" || return 1
  for f in target/debug/build/gn_out/obj/rusty_v8.sum \
           target/release/build/gn_out/obj/rusty_v8.sum; do
    [ -f "$f" ] || continue
    url="$(head -n1 "$f")"
    case "$url" in
      "$base"/*)
        ok "v8 fetched from the mirror: $url"
        found=0
        ;;
    esac
  done
  return "$found"
}
