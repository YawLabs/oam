#!/bin/sh
# oam installer (Linux / macOS). Canonical home: https://oamjs.org/install.sh
#
#   curl -fsSL https://oamjs.org/install.sh | sh
#
# Downloads the release binary for this OS/arch from GitHub Releases, verifies
# it against the published SHA256SUMS, and installs it to ~/.oam/bin. No sudo,
# no signing (the binaries are unsigned + checksummed -- see docs/design).
#
# Env overrides:
#   OAM_VERSION       install a specific tag (e.g. v0.7.0); default: latest
#   OAM_INSTALL_DIR   install location; default: $HOME/.oam/bin
#   OAM_INSTALL_BASE  asset base URL; default: GitHub Releases
#                     (oamjs.org sets this to proxy downloads through the CDN)
#   GH_TOKEN          GitHub token for private-repo installs (GITHUB_TOKEN is
#                     also accepted). Needed on headless hosts -- CI, Docker, a
#                     fresh VM -- that have a token but no gh CLI. While the
#                     repo is private, unauthenticated asset URLs return 404.
#   OAM_GH_API        GitHub API base; default https://api.github.com
#                     (set this for GitHub Enterprise)
set -eu

OWNER_REPO="YawLabs/oam"
INSTALL_DIR="${OAM_INSTALL_DIR:-$HOME/.oam/bin}"
GH_API="${OAM_GH_API:-https://api.github.com}"
# gh CLI convention first, then the Actions-provided name.
TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"

say() { printf 'oam-install: %s\n' "$1"; }
die() { printf 'oam-install: error: %s\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"; }

need uname
# Prefer curl, fall back to wget.
# The token must never reach argv: /proc/<pid>/cmdline is world-readable on
# Linux, so `-H "Authorization: Bearer ..."` leaks the secret to every local
# user for the life of the request. curl reads directives from stdin via
# `--config -`; wget has no stdin equivalent, so it gets a mode-600 rc file
# (WGETRC), which is at least owner-only rather than world-readable.
if command -v curl >/dev/null 2>&1; then
  dl() { curl -fsSL "$1" -o "$2"; }
  dl_auth() {
    printf 'header = "Authorization: Bearer %s"\nheader = "Accept: %s"\n' "$TOKEN" "$3" \
      | curl -fsSL --config - "$1" -o "$2"
  }
  api_get() {
    printf 'header = "Authorization: Bearer %s"\nheader = "Accept: application/vnd.github+json"\n' "$TOKEN" \
      | curl -fsSL --config - "$1"
  }
elif command -v wget >/dev/null 2>&1; then
  dl() { wget -qO "$2" "$1"; }
  _wgetrc() {
    _rc="${tmp:-${TMPDIR:-/tmp}}/oam-wgetrc.$$"
    (umask 077; printf 'header = Authorization: Bearer %s\nheader = Accept: %s\n' "$TOKEN" "$1" > "$_rc")
    echo "$_rc"
  }
  dl_auth() {
    _rc="$(_wgetrc "$3")"; WGETRC="$_rc" wget -qO "$2" "$1"; _s=$?; rm -f "$_rc"; return $_s
  }
  api_get() {
    _rc="$(_wgetrc 'application/vnd.github+json')"
    WGETRC="$_rc" wget -qO- "$1"; _s=$?; rm -f "$_rc"; return $_s
  }
else
  die "need curl or wget to download"
fi

# Map uname -> Rust target triple (must match release.yml asset names).
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      # No aarch64-unknown-linux-gnu asset has shipped yet (needs a native ARM
      # build host; V8 snapshot generation cannot cross-compile). Re-add the
      # mapping when the release leg exists -- see install/README.md.
      aarch64|arm64) die "no published oam binary for Linux $arch yet (aarch64-unknown-linux-gnu is unreleased; use an x86_64 host or build from source)" ;;
      *) die "unsupported Linux arch: $arch" ;;
    esac ;;
  Darwin)
    case "$arch" in
      x86_64|amd64) target="x86_64-apple-darwin" ;;
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      *) die "unsupported macOS arch: $arch" ;;
    esac ;;
  *) die "unsupported OS: $os (use install.ps1 on Windows)" ;;
esac

asset="oam-${target}"

# Resolve the asset base. Default to GitHub Releases: a pinned OAM_VERSION uses
# the /download/<tag>/ path; otherwise /latest/download/ resolves the newest.
if [ -n "${OAM_INSTALL_BASE:-}" ]; then
  base="$OAM_INSTALL_BASE"
elif [ -n "${OAM_VERSION:-}" ]; then
  base="https://github.com/${OWNER_REPO}/releases/download/${OAM_VERSION}"
else
  base="https://github.com/${OWNER_REPO}/releases/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Token fallback: works on headless hosts with no gh CLI (CI, Docker, a fresh
# VM). A private-repo asset is NOT reachable via its browser_download_url even
# with a token -- GitHub only serves the bytes from the assets endpoint with
# Accept: application/octet-stream -- so resolve the numeric asset id first.
rel_json=""
token_dl() {
  [ -n "$TOKEN" ] || return 1
  if [ -z "$rel_json" ]; then
    if [ -n "${OAM_VERSION:-}" ]; then
      rel_url="${GH_API}/repos/${OWNER_REPO}/releases/tags/${OAM_VERSION}"
    else
      rel_url="${GH_API}/repos/${OWNER_REPO}/releases/latest"
    fi
    rel_json="$(api_get "$rel_url")" || return 1
  fi
  # Strip ALL whitespace first: the REST API pretty-prints (`"name": "x"`, one
  # field per line) while `gh api` returns compact JSON, and a parser written
  # against either shape alone breaks on the other. Safe here because we only
  # ever match asset names (target triples -- no spaces) and read back digits.
  # Then split on '{' so each object is one line: GitHub emits "id" before
  # "name" within an asset, and the nested uploader object (which carries its
  # own "id") starts a LATER segment, so the id on the matching line is the
  # asset's own. `tr` rather than `sed s/../\n/` -- BSD sed rejects \n in a RHS.
  asset_id="$(printf '%s' "$rel_json" | tr -d ' \n\r' | tr '{' '\n' \
    | grep "\"name\":\"$1\"," | grep -o '"id":[0-9][0-9]*' | head -1 | cut -d: -f2)"
  [ -n "$asset_id" ] || return 1
  dl_auth "${GH_API}/repos/${OWNER_REPO}/releases/assets/${asset_id}" "$2" \
    "application/octet-stream"
}

# Authenticated fallback: while the repo is private, unauthenticated release
# URLs 404. If the direct download fails and the gh CLI is available (internal
# machines), fetch the same assets through the caller's GitHub auth.
gh_dl() {
  command -v gh >/dev/null 2>&1 || return 1
  gh_tag="${OAM_VERSION:-}"
  if [ -z "$gh_tag" ]; then
    gh_tag="$(gh release view --repo "$OWNER_REPO" --json tagName -q .tagName 2>/dev/null)" || return 1
  fi
  gh release download "$gh_tag" --repo "$OWNER_REPO" --pattern "$1" \
    --output "$2" --clobber 2>/dev/null
}

# Direct first (public releases + the oamjs.org CDN), then token, then gh CLI.
fetch_asset() {
  dl "${base}/$1" "$2" && return 0
  if [ -n "$TOKEN" ]; then
    say "direct download failed; retrying with \$GH_TOKEN"
    token_dl "$1" "$2" && return 0
  fi
  say "retrying via gh CLI (private repo needs auth)"
  gh_dl "$1" "$2"
}

say "downloading ${asset} from ${base}"
fetch_asset "${asset}" "${tmp}/${asset}" || die "download failed: ${asset} (private repo? set GH_TOKEN or install the gh CLI)"
fetch_asset "SHA256SUMS" "${tmp}/SHA256SUMS" || die "could not fetch SHA256SUMS"

# Verify the checksum for our asset only (the manifest covers all targets).
# Match field 2 exactly, stripping sha256sum's binary-mode "*" marker -- the
# manifest is written as "<hash> *<asset>", so a plain `grep " <asset>$"` never
# matches and every install would die with "no checksum for ...".
expected="$(awk -v a="$asset" '{ f = $2; sub(/^\*/, "", f); if (f == a) { print $1; exit } }' "${tmp}/SHA256SUMS")"
[ -n "$expected" ] || die "no checksum for ${asset} in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${tmp}/${asset}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${tmp}/${asset}" | awk '{print $1}')"
else
  die "need sha256sum or shasum to verify the download"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for ${asset} (expected ${expected}, got ${actual})"
say "checksum ok"

mkdir -p "$INSTALL_DIR"
chmod +x "${tmp}/${asset}"
mv -f "${tmp}/${asset}" "${INSTALL_DIR}/oam"
say "installed oam to ${INSTALL_DIR}/oam"

# PATH guidance: only nudge if the install dir isn't already on PATH.
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    say "add it to your PATH:"
    # $PATH is intentionally literal here -- it's printed for the user to copy.
    # shellcheck disable=SC2016
    printf '    export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    say "(append that line to your shell profile, e.g. ~/.bashrc or ~/.zshrc)"
    ;;
esac

"${INSTALL_DIR}/oam" --version 2>/dev/null || true
