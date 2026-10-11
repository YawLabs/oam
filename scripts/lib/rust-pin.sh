# shellcheck shell=bash
# =============================================================================
# The Rust toolchain every leg gates with: ONE exact version, on every host.
# =============================================================================
# The gate is "cargo clippy --workspace --all-targets -- -D warnings" (ci-local
# step 3, and run_gate on both remote legs), and clippy's lint set moves
# BETWEEN rustc versions. The v0.18.1 run on 2026-10-10 shipped on exactly
# that trap: rust-toolchain.toml said channel = "stable", which rustup
# resolves PER HOST at install time -- the Windows box had been stable long
# enough to sit at 1.96, the GCP builder at 1.97. The builder's 1.97 clippy
# carries manual_clamp; the local 1.96 did not -- so the tree the local gate
# called clean failed the linux leg's `gate` dispatch, an hour into the
# release, on a line that had never been touched by the release's commits.
#
# The fix has two halves:
#   1. rust-toolchain.toml pins an EXACT version ("1.99.0"), never a floating
#      channel. rustup then resolves the same rustc on every host that reads
#      the file -- which is every leg, because build-remote.sh runs
#      `rustup toolchain install` in the repo root and cargo consults the
#      file for every command run inside it.
#   2. This lib, the assert half: rust_pin_read parses the pin (and refuses a
#      floating channel outright), rust_pin_verdict is the same
#      pinned/mismatch/absent decision node-pin.sh makes for the Node oracle.
#      "The toolchain is consistent" is checked, not assumed -- a host whose
#      rustc is not the pin fails in seconds, at prep, not an hour in.
#
# The x86_64-apple-darwin (Rosetta) build is pinned to the same version on
# purpose: `rustup toolchain install stable-x86_64-apple-darwin` would put a
# DIFFERENT rustc on that leg than the arm64 one the toml resolves to.
# build-remote.sh builds it with `cargo +<pin>-x86_64-apple-darwin`.
#
# Consumers:
#   scripts/ci-local.sh       refuses a different rustc before step 1 (the
#                             toolchain is already an input of the step
#                             ledger's key; this is the human-readable half)
#   scripts/build-remote.sh   remote_prep asserts it after `rustup toolchain
#                             install`; build_mac_x64 derives the Rosetta
#                             toolchain name from the pin
#   scripts/release-local.sh  preflight asserts the release box itself,
#                             before the slow signing half
#
# Escape hatch, for ad-hoc local runs only: OAM_ALLOW_RUST_MISMATCH=1 turns
# each refusal into a loud warning. The remote orchestrators deliberately do
# NOT forward it -- a leg that gates on the wrong lint set is the bug.
#
# Same convention as lib/node-pin.sh and lib/src-sync.sh: sourced, never
# executed. Functions RETURN status and put the reason on stderr; the caller
# owns fail()/warn()/die().
# =============================================================================

# rust_pin_read <repo-dir>
# Echoes the EXACT version rust-toolchain.toml pins ("1.99.0"), and refuses
# anything else loudly: a floating channel ("stable", "nightly") is the
# per-host skew this gate exists to prevent, and a missing/blank channel
# would leave every host on whatever its default happens to be.
rust_pin_read() {
  local file="$1/rust-toolchain.toml" line
  if [ ! -f "$file" ]; then
    echo "rust-pin: $file is missing -- it pins the toolchain every leg gates with" >&2
    return 1
  fi
  # The LAST `channel =` line wins, matching toml semantics (a later duplicate
  # key replaces an earlier one) rather than whichever line grep saw first.
  line="$(grep -E '^[[:space:]]*channel[[:space:]]*=' "$file" | tail -n 1)" || line=""
  line="${line#*=}"
  line="${line%%#*}"
  # Trim, THEN strip one pair of quotes, then trim again: `channel = "1.99.0"`
  # leaves a leading SPACE after the = split, and a quote stripped before that
  # trim would not fire (the first character is the space), stranding it.
  line="${line#"${line%%[![:space:]]*}"}"
  line="${line%"${line##*[![:space:]]}"}"
  line="${line%\"}"; line="${line#\"}"
  line="${line%\'}"; line="${line#\'}"
  line="${line#"${line%%[![:space:]]*}"}"
  line="${line%"${line##*[![:space:]]}"}"
  # Same shape as node_pin_parse: exactly one MAJOR.MINOR.PATCH. A host triple
  # ("1.99.0-x86_64-apple-darwin") is refused -- it would be wrong on every
  # other host, which is the skew this gate exists to prevent.
  [[ "$line" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    echo "rust-pin: rust-toolchain.toml must pin an EXACT x.y.z version (channel = \"1.99.0\"), got '$line' -- a floating channel resolves per host, which is the toolchain skew between legs this gate exists to prevent (see the header)." >&2
    return 1
  }
  printf '%s' "$line"
}

# rust_pin_verdict <pinned> <found-version-or-empty> <allow-flag>
# The one decision every consumer makes -- the same contract as
# node_pin_verdict. <found> is `rustc --version` with its "rustc " prefix
# already stripped ("1.99.0"), empty when there is no rustc. Echoes one of
#   pinned | mismatch | absent | mismatch-allowed | absent-allowed
# and returns 0 exactly when the run may proceed. Only a literal "1" allows,
# so a stray "0" or "false" cannot.
rust_pin_verdict() {
  local want="$1" got="$2" kind
  if [ -n "$got" ] && [ "$got" = "$want" ]; then
    echo pinned
    return 0
  fi
  kind=mismatch
  [ -z "$got" ] && kind=absent
  if [ "$3" = "1" ]; then
    echo "$kind-allowed"
    return 0
  fi
  echo "$kind"
  return 1
}
