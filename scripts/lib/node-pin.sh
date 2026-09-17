# shellcheck shell=bash
# =============================================================================
# The Node the conformance oracle runs as: ONE pinned version, on every leg.
# =============================================================================
# oam's parity claim is against one exact Node: the vendored node-suite corpus
# is a snapshot of that tag (conformance/vendor/node/manifest.json), and the
# node-differential cases, the builtin export-parity ratchet
# (conformance/surface-gaps.json) and every "probed on vX" comment in the tree
# were measured against it. The differential only means something when the
# `node` it compares with IS that version.
#
# Before this file each leg used whatever `node` its PATH happened to find:
#   - Windows dev box   scoop nodejs22 -- v22.22.2, the target, by luck
#   - MacBook Air       /usr/local/bin/node -- a hand-copied v22.23.1 binary
#                       (NOT Homebrew's node@22, which is keg-only and never on
#                       the non-interactive ssh PATH at all)
#   - GCP linux builder the image's Node 22 -- v22.23.1 when the linux
#                       surface-gaps section was recorded
# so the mac and linux receipts had never once compared against the target, and
# nothing said so: every leg printed a version, none of them checked it.
#
# The pin lives in `.node-version` at the repo root (the file nodenv, fnm, mise
# and asdf already read, so a developer's version manager picks up the same
# pin). Consumers:
#   scripts/build-remote.sh     provisions exactly that Node from nodejs.org
#                               (the official tarball, sha256-verified against
#                               the release's SHASUMS256.txt) into a cache under
#                               the remote $HOME, and puts it FIRST on PATH for
#                               every oracle dispatch (conformance,
#                               surface-gaps, bench). Fail-closed.
#   scripts/ci-local.sh         refuses a different `node` before step 1, not
#                               after the build and both test runs.
#   xtask conformance           refuses a different `node` (the Rust twin of
#                               node_pin_verdict, xtask/src/node_pin.rs).
#   xtask node-suite            refuses a corpus whose manifest names a
#                               different version (it never runs node itself).
#   scripts/gen-surface-gaps.mjs  refuses to record a ratchet section against a
#                               different node.
#
# Escape hatch, for ad-hoc local runs only: OAM_ALLOW_NODE_MISMATCH=1 turns each
# refusal into a loud warning, and the receipts then name the node that really
# ran. The remote orchestrators deliberately do NOT forward it.
#
# Same convention as lib/iap-helpers.sh and lib/src-sync.sh: sourced, never
# executed. Functions RETURN status and put the reason on stderr; the caller
# owns fail()/warn()/die().
# =============================================================================

# Where releases come from, and where provisioned installs are cached. The dist
# URL is overridable for a mirror (and for scripts/test-scripts.sh, which serves
# a planted release over file:// so the provisioning path runs for real with no
# network). The cache is outside any synced checkout on purpose: the
# orchestrators replace everything but target/ on every sync.
OAM_NODE_DIST_URL="${OAM_NODE_DIST_URL:-https://nodejs.org/dist}"
OAM_NODE_CACHE="${OAM_NODE_CACHE:-$HOME/.cache/oam-node}"

# --- the pin -----------------------------------------------------------------

# node_pin_parse <text>
# Echoes the version WITHOUT a leading v ("22.22.2"). Accepts surrounding
# whitespace, CRLF, and an optional leading "v" -- the spellings version
# managers accept -- and nothing else: exactly one MAJOR.MINOR.PATCH. A range or
# an alias ("22", "lts/jod") is refused rather than resolved, because the whole
# point is that every host lands on the SAME build.
node_pin_parse() {
  local v="$1"
  # Trim the ends only. Whitespace INSIDE stays and fails the pattern below, so
  # a second token on another line is refused loudly rather than dropped -- the
  # same rule as parse_pin in xtask/src/node_pin.rs, which str::trim()s.
  v="${v#"${v%%[![:space:]]*}"}"
  v="${v%"${v##*[![:space:]]}"}"
  v="${v#v}"
  [[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || return 1
  printf '%s' "$v"
}

# node_pin_read <repo-dir>
# node_pin_parse over <repo-dir>/.node-version, with the reason on stderr.
node_pin_read() {
  local file="$1/.node-version" v
  if [ ! -f "$file" ]; then
    echo "node-pin: $file is missing -- it pins the Node the conformance oracle must be" >&2
    return 1
  fi
  if ! v="$(node_pin_parse "$(cat "$file")")"; then
    echo "node-pin: $file must hold exactly one MAJOR.MINOR.PATCH version (e.g. 22.22.2), got: '$(head -c 80 "$file")'" >&2
    return 1
  fi
  printf '%s' "$v"
}

# node_pin_verdict <pinned-version> <found-version-or-empty> <allow-flag>
# The one decision every consumer makes. <found> is `node --version` verbatim
# ("v22.22.2"), empty when there is no node. Echoes one of
#   pinned | mismatch | absent | mismatch-allowed | absent-allowed
# and returns 0 exactly when the run may proceed. Only a literal "1" allows --
# the OAM_ALLOW_DOWNGRADE convention -- so a stray "0" or "false" cannot.
node_pin_verdict() {
  local want="v$1" got="$2" kind
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

# --- provisioning (remote legs) ----------------------------------------------

# node_dist_platform <uname -s> <uname -m>
# The platform suffix of nodejs.org's tarball names. Non-zero for a host Node
# publishes no official tarball for (the caller names it).
node_dist_platform() {
  local os arch
  case "$1" in
    Darwin) os=darwin ;;
    Linux) os=linux ;;
    *) return 1 ;;
  esac
  case "$2" in
    arm64 | aarch64) arch=arm64 ;;
    x86_64 | amd64) arch=x64 ;;
    *) return 1 ;;
  esac
  printf '%s-%s' "$os" "$arch"
}

# node_shasum_for <SHASUMS256.txt> <file-name>
# The sha256 recorded for EXACTLY <file-name>. Matched on the whole name field,
# not a substring: node-v22.22.2-linux-x64.tar.gz is a prefix-sibling of the
# .tar.xz beside it, and a grep would happily return the wrong line. Non-zero
# when absent or when the field is not a 64-hex digest.
node_shasum_for() {
  local sum
  [ -f "$1" ] || return 1
  sum="$(awk -v f="$2" '$2 == f { print $1; exit }' "$1")"
  case "$sum" in '' | *[!0-9a-f]*) return 1 ;; esac
  [ "${#sum}" -eq 64 ] || return 1
  printf '%s' "$sum"
}

# node_sha256 <file>
# Over stdin, not the path: GNU sha256sum backslash-prefixes its output line for
# a name with a backslash or newline in it, and `hash -` never has either.
node_sha256() {
  local out
  if command -v sha256sum >/dev/null 2>&1; then
    out="$(sha256sum <"$1")" || return 1
  elif command -v shasum >/dev/null 2>&1; then
    out="$(shasum -a 256 <"$1")" || return 1
  else
    echo "node-pin: neither sha256sum nor shasum is available to verify the download" >&2
    return 1
  fi
  printf '%s' "${out%% *}"
}

# node_fetch <url> <out-file>
# --proto-redir =https: a redirect may not downgrade the transport. The initial
# URL is not restricted, which is what lets a file:// or internal mirror work.
node_fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto-redir =https --retry 3 --retry-delay 2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    echo "node-pin: neither curl nor wget is available to fetch $1" >&2
    return 1
  fi
}

# node_pin_dir <version> <platform> -- the cached install root.
node_pin_dir() { printf '%s/node-v%s-%s' "$OAM_NODE_CACHE" "$1" "$2"; }

# node_pin_installed <install-root> <version>
# 0 when the install's own binary reports exactly v<version>. Asking the binary,
# not trusting the directory name, is what makes a truncated or hand-edited
# cache entry re-provision instead of being believed.
node_pin_installed() {
  [ -x "$1/bin/node" ] || return 1
  [ "$("$1/bin/node" --version 2>/dev/null)" = "v$2" ]
}

# node_pin_provision <version> <platform>
# Echoes the bin dir of a verified install of exactly v<version>, downloading it
# the first time. Everything lands in a private temp dir beside the cache and is
# only renamed into place once the checksum AND the binary's own version have
# checked out, so an interrupted download can never be mistaken for an install.
node_pin_provision() {
  local version="$1" platform="$2" dir name base tmp want got
  dir="$(node_pin_dir "$version" "$platform")"
  if node_pin_installed "$dir" "$version"; then
    printf '%s/bin' "$dir"
    return 0
  fi
  name="node-v$version-$platform"
  base="$OAM_NODE_DIST_URL/v$version"
  mkdir -p "$OAM_NODE_CACHE" || {
    echo "node-pin: cannot create the cache dir $OAM_NODE_CACHE" >&2
    return 1
  }
  tmp="$(mktemp -d "$OAM_NODE_CACHE/.partial-XXXXXX")" || {
    echo "node-pin: cannot create a staging dir under $OAM_NODE_CACHE" >&2
    return 1
  }
  if ! node_fetch "$base/SHASUMS256.txt" "$tmp/SHASUMS256.txt"; then
    echo "node-pin: could not fetch $base/SHASUMS256.txt -- is v$version a released Node, and is $OAM_NODE_DIST_URL reachable from this host?" >&2
    rm -rf "$tmp"
    return 1
  fi
  if ! want="$(node_shasum_for "$tmp/SHASUMS256.txt" "$name.tar.gz")"; then
    echo "node-pin: $base/SHASUMS256.txt lists no $name.tar.gz -- Node publishes no official build for this platform at v$version" >&2
    rm -rf "$tmp"
    return 1
  fi
  if ! node_fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz"; then
    echo "node-pin: could not fetch $base/$name.tar.gz" >&2
    rm -rf "$tmp"
    return 1
  fi
  got="$(node_sha256 "$tmp/$name.tar.gz")" || {
    rm -rf "$tmp"
    return 1
  }
  if [ "$got" != "$want" ]; then
    echo "node-pin: CHECKSUM MISMATCH for $name.tar.gz: SHASUMS256.txt says $want, the download hashes to $got -- refusing to install it" >&2
    rm -rf "$tmp"
    return 1
  fi
  if ! tar -xzf "$tmp/$name.tar.gz" -C "$tmp"; then
    echo "node-pin: could not extract $name.tar.gz" >&2
    rm -rf "$tmp"
    return 1
  fi
  if ! node_pin_installed "$tmp/$name" "$version"; then
    echo "node-pin: the extracted $name/bin/node does not run or does not report v$version" >&2
    rm -rf "$tmp"
    return 1
  fi
  # A previous entry that failed node_pin_installed above is broken; replace it.
  # Removed first because `mv src dst` onto an existing directory would nest
  # src INSIDE it rather than replace it.
  [ -e "$dir" ] && rm -rf "$dir"
  if ! mv "$tmp/$name" "$dir"; then
    echo "node-pin: could not move the verified install into $dir" >&2
    rm -rf "$tmp"
    return 1
  fi
  rm -rf "$tmp"
  node_pin_installed "$dir" "$version" || {
    echo "node-pin: $dir does not hold a working v$version after install (a concurrent install racing this one?)" >&2
    return 1
  }
  printf '%s/bin' "$dir"
}

# node_pin_use <repo-dir> [platform]
# Provision the pinned Node and put it first on PATH in the calling shell, then
# prove that `node` now resolves to it. The platform defaults to THIS host's
# (uname); the argument exists for scripts/test-scripts.sh, whose Windows box
# has no nodejs.org tarball of its own. Every step that can go wrong returns
# non-zero with its reason on stderr.
node_pin_use() {
  local version platform="${2:-}" bin
  version="$(node_pin_read "$1")" || return 1
  if [ -z "$platform" ] && ! platform="$(node_dist_platform "$(uname -s)" "$(uname -m)")"; then
    echo "node-pin: nodejs.org publishes no tarball this script knows for $(uname -s)/$(uname -m)" >&2
    return 1
  fi
  bin="$(node_pin_provision "$version" "$platform")" || return 1
  export PATH="$bin:$PATH"
  hash -r 2>/dev/null || true
  if [ "$(command -v node)" != "$bin/node" ]; then
    echo "node-pin: put $bin first on PATH but 'node' still resolves to $(command -v node)" >&2
    return 1
  fi
  [ "$(node --version 2>/dev/null)" = "v$version" ] || {
    echo "node-pin: $bin/node does not report v$version" >&2
    return 1
  }
}
