#!/bin/sh
# oam installer (Linux / macOS). Canonical home: https://oam.sh/install.sh
#
#   curl -fsSL https://oam.sh/install.sh | sh
#
# Downloads the release binary for this OS/arch from GitHub Releases, verifies
# it against the published SHA256SUMS, and installs it to ~/.oam/bin. No sudo,
# no signing (the binaries are unsigned + checksummed -- see docs/design).
#
# Env overrides:
#   OAM_VERSION       install a specific tag (e.g. v0.7.0); default: latest
#   OAM_INSTALL_DIR   install location; default: $HOME/.oam/bin
#   OAM_INSTALL_BASE  asset base URL; default: GitHub Releases
#                     (oam.sh sets this to proxy downloads through the CDN)
set -eu

OWNER_REPO="YawLabs/oam"
INSTALL_DIR="${OAM_INSTALL_DIR:-$HOME/.oam/bin}"

say() { printf 'oam-install: %s\n' "$1"; }
die() { printf 'oam-install: error: %s\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"; }

need uname
# Prefer curl, fall back to wget.
if command -v curl >/dev/null 2>&1; then
  dl() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  dl() { wget -qO "$2" "$1"; }
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
      aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
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

say "downloading ${asset} from ${base}"
dl "${base}/${asset}" "${tmp}/${asset}" || die "download failed: ${base}/${asset}"
dl "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" || die "could not fetch SHA256SUMS"

# Verify the checksum for our asset only (the manifest covers all targets).
expected="$(grep " ${asset}\$" "${tmp}/SHA256SUMS" | awk '{print $1}')"
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
