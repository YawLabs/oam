#!/usr/bin/env bash
#
# check-vendor.sh -- prove every crate under vendor/ is its crates.io release
# plus the reviewed patch next to it, and nothing else.
#
# Why this exists: a vendored crate is outside every other gate. The root
# Cargo.toml excludes vendor/ from the workspace, so fmt, clippy and the tests
# never look at it; the unsafe-budget scan walks crates/ and xtask only; and
# Cargo.lock records no checksum for a path dependency. Without this check,
# any later edit under vendor/ -- new `unsafe` included -- would pass every
# gate. And the recipe the first vendoring documented (`git diff <commit>`
# against the pristine import) named a commit that a squash merge does not
# keep.
#
# For each vendor/<name>-<version>/:
#   1. the published .crate is taken from cargo's download cache, or fetched
#      from static.crates.io, and must match OAM-PATCH.sha256 (the checksum
#      Cargo.lock recorded before the crate was vendored);
#   2. it is unpacked, OAM-PATCH.diff is applied, and the result must equal
#      the vendored directory byte for byte (the OAM-PATCH.* files aside);
#   3. with --build, the vendored crate must also compile, warning-free, for
#      each feature set listed in OAM-PATCH.features, one per line. That is
#      where a patch that compiles only in oam's own feature set shows up
#      (the hyper patch once broke client + http2 without http1).
#
# After changing a vendored crate, `--regen` rewrites its OAM-PATCH.diff from
# the vendored copy; review the result and commit it with the change.
#
# Usage:
#   scripts/check-vendor.sh            # checksum + pristine-plus-diff check
#   scripts/check-vendor.sh --build    # also the feature-set compile check
#   scripts/check-vendor.sh --regen    # rewrite each OAM-PATCH.diff
#
# Exit 0 clean, 1 on a finding, 2 when the check could not RUN (the crate not
# in cargo's cache and no network, a tool missing). A 2 is not a clean result.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

BUILD=0
REGEN=0
for arg in "$@"; do
  case "$arg" in
    --build) BUILD=1 ;;
    --regen) REGEN=1 ;;
    -h|--help)
      awk 'NR==1{next} !/^#/{exit} {sub(/^# ?/, ""); print}' "$0"
      exit 0
      ;;
    *) echo "check-vendor: unknown argument $arg" >&2; exit 2 ;;
  esac
done

fail() { echo "check-vendor: $*" >&2; exit 1; }
cannot() { echo "check-vendor: cannot run: $*" >&2; exit 2; }

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    cannot "no sha256sum or shasum on PATH"
  fi
}

# A path a native (non-MSYS) tool reads from inside a file: on Git Bash,
# /c/... must be written as C:/...; elsewhere it is already right.
native_path() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s\n' "$1"; fi
}

# The published .crate for <name> <version>: cargo's download cache first
# (cargo fetched it for the lockfile before the crate was vendored), then
# static.crates.io.
find_crate() {
  local name=$1 version=$2 dest=$3 cached
  local home="${CARGO_HOME:-$HOME/.cargo}"
  for cached in "$home"/registry/cache/*/"$name-$version.crate"; do
    if [ -f "$cached" ]; then
      cp "$cached" "$dest"
      return 0
    fi
  done
  command -v curl >/dev/null 2>&1 || cannot "$name-$version.crate is not in cargo's cache and there is no curl"
  curl -fsSL "https://static.crates.io/crates/$name/$name-$version.crate" -o "$dest" \
    || cannot "$name-$version.crate is not in cargo's cache and could not be downloaded"
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

shopt -s nullglob
dirs=(vendor/*/)
[ "${#dirs[@]}" -gt 0 ] || { echo "check-vendor: nothing vendored"; exit 0; }

for dir in "${dirs[@]}"; do
  dir="${dir%/}"
  crate="$(basename "$dir")"
  name="${crate%-*}"
  version="${crate##*-}"
  [ -f "$dir/OAM-PATCH.sha256" ] || fail "$dir has no OAM-PATCH.sha256 -- every vendored crate records its upstream checksum"
  want="$(tr -d ' \r\n' < "$dir/OAM-PATCH.sha256")"

  find_crate "$name" "$version" "$WORK/$crate.crate"
  got="$(sha256_of "$WORK/$crate.crate")"
  [ "$got" = "$want" ] || fail "$crate.crate has sha256 $got, OAM-PATCH.sha256 says $want"
  # a/ is the release, b/ the vendored copy without its OAM-PATCH.* files.
  mkdir -p "$WORK/$crate/a" "$WORK/$crate/b"
  tar -xzf "$WORK/$crate.crate" -C "$WORK/$crate/a" --strip-components=1
  cp -R "$dir/." "$WORK/$crate/b/"
  rm -f "$WORK/$crate/b"/OAM-PATCH.*

  if [ "$REGEN" = 1 ]; then
    # diff exits 1 when the trees differ, which is the point here; the sed
    # drops the timestamps from the file headers so the output is stable.
    (cd "$WORK/$crate" && diff -ruN a b || true) \
      | sed -E 's#^((---|\+\+\+) [^\t]*)\t.*#\1#' > "$dir/OAM-PATCH.diff"
    echo "check-vendor: rewrote $dir/OAM-PATCH.diff -- review it before committing"
    continue
  fi

  [ -f "$dir/OAM-PATCH.diff" ] || fail "$dir has no OAM-PATCH.diff -- every vendored crate carries its patch"
  # Applied outside any repository: inside one, git apply would resolve the
  # paths against the enclosing work tree. core.autocrlf off: a global
  # autocrlf=true (the Git for Windows default) would write the patched
  # files with CRLF, and every one of them would then differ.
  patch_file="$PWD/$dir/OAM-PATCH.diff"
  (cd "$WORK/$crate/a" && GIT_CEILING_DIRECTORIES="$WORK" git -c core.autocrlf=false -c core.eol=lf apply --whitespace=nowarn "$patch_file") \
    || fail "$dir/OAM-PATCH.diff does not apply to the published $crate"
  if ! drift="$(cd "$WORK/$crate" && diff -r a b)"; then
    printf '%s\n' "$drift" | head -40 >&2
    fail "$dir differs from the published $crate plus OAM-PATCH.diff (above) -- an edit under vendor/ must land in OAM-PATCH.diff too ('scripts/check-vendor.sh --regen', then review)"
  fi
  echo "check-vendor: $dir = crates.io $crate (sha256 ${want:0:12}...) + OAM-PATCH.diff"

  if [ "$BUILD" = 1 ] && [ -f "$dir/OAM-PATCH.features" ]; then
    vendored_native="$(native_path "$PWD/$dir")"
    probe="$WORK/probe"
    while IFS= read -r features || [ -n "$features" ]; do
      case "$features" in ''|'#'*) continue ;; esac
      rm -rf "$probe"
      mkdir -p "$probe/src"
      printf 'pub fn probe() {}\n' > "$probe/src/lib.rs"
      cat > "$probe/Cargo.toml" <<EOF
[package]
name = "vendor-probe"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
$name = { version = "=$version", default-features = false, features = [$features] }

[patch.crates-io]
$name = { path = "$vendored_native" }
EOF
      # The workspace lockfile pins every dependency to a version cargo has
      # already downloaded, so --offline resolves.
      cp Cargo.lock "$probe/Cargo.lock"
      out="$(cargo check --offline --quiet --manifest-path "$(native_path "$probe/Cargo.toml")" \
        --target-dir target/vendor-check 2>&1)" \
        || { printf '%s\n' "$out" >&2; fail "$crate does not compile with features [$features] (above)"; }
      if printf '%s\n' "$out" | grep -q '^warning'; then
        printf '%s\n' "$out" >&2
        fail "$crate warns with features [$features] (above)"
      fi
      echo "check-vendor: $crate builds warning-free with [$features]"
    done < "$dir/OAM-PATCH.features"
  fi
done
