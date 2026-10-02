#!/bin/sh
# oam installer (Linux / macOS). Canonical home: https://oamjs.org/install.sh
#
#   curl -fsSL https://oamjs.org/install.sh | sh
#
# Downloads the release binary for this OS/arch from GitHub Releases, verifies
# it, and installs it to ~/.oam/bin. No sudo. The binaries themselves are not
# code-signed yet (no Apple Developer ID); what is signed is the release:
#
#   v0.18.0 and later   RELEASE-MANIFEST ("oam-release-manifest v1", "tag
#                       <tag>", then the SHA256SUMS lines) must carry a
#                       RELEASE-MANIFEST.sig that `ssh-keygen -Y verify`
#                       accepts against the release keys EMBEDDED below, be
#                       signed for exactly the tag being installed, by a key
#                       whose range covers that tag. The binary's sha256 comes
#                       from that signed manifest.
#   before v0.18.0      cut before signing existed. The release's SHA256SUMS
#                       must hash to the digest pinned below for its tag, and
#                       the binary's sha256 comes from it. A pre-v0.18.0 tag
#                       that is not pinned is refused.
#
# A signature that is present but does not verify always fails; so does a
# v0.18.0+ release with no manifest. release-keys/README.md is the runbook.
#
# Env overrides:
#   OAM_VERSION       install a specific tag (e.g. v0.18.0); default: latest
#   OAM_INSTALL_DIR   install location; default: $HOME/.oam/bin
#   OAM_INSTALL_BASE  asset base URL for a mirror or CDN; default: GitHub
#                     Releases. Requires OAM_VERSION: the signature is checked
#                     against the tag you asked for, and a mirror cannot be
#                     asked which tag it serves.
#   OAM_INSECURE_SKIP_SIGNATURE=1
#                     install a v0.18.0+ release on a host with no usable
#                     ssh-keygen, WITHOUT checking its signature. Only a
#                     missing tool is skipped: with ssh-keygen present the
#                     signature is always checked, and a bad one always fails.
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

# The first tag cut with a signed RELEASE-MANIFEST. Every tag from here on must
# carry one that verifies; every tag before it is checked against the pinned
# table instead. install.ps1 carries the same cutoff, and
# scripts/test-scripts.sh holds both to the first tag release-keys/ranges opens.
FIRST_MANIFEST_SIG_TAG="v0.18.0"
SIGN_NAMESPACE="oam-release"
MANIFEST_HEADER="oam-release-manifest v1"
PRINCIPAL_PREFIX="oam-release-"

say() { printf 'oam-install: %s\n' "$1"; }
warn() { printf 'oam-install: warning: %s\n' "$1" >&2; }
die() { printf 'oam-install: error: %s\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"; }

# --- The trust root, embedded ---------------------------------------------------
# Byte-identical copies of release-keys/allowed_signers, release-keys/ranges and
# release-keys/presigning-sums; scripts/test-scripts.sh fails on any drift.
# Embedded, never fetched: a key list downloaded from where the release came from
# is whatever whoever controls the release says it is. There is deliberately no
# environment variable that points these anywhere else -- that is exactly the
# knob an attacker would set. (The test suite swaps these blocks in a COPY of
# this file.) Each heredoc's delimiter is quoted, so its body is verbatim.
embedded_allowed_signers() {
  cat <<'OAM_EMBED_ALLOWED_SIGNERS'
# oam release keys -- the PUBLIC halves only (ssh-keygen allowed_signers format).
#
# Every oam release from the first signed one on carries RELEASE-MANIFEST and
# RELEASE-MANIFEST.sig; this file names the keys allowed to have made that
# signature. Read by scripts/lib/signing.sh (release_verify_manifest), and the
# source of the key set the installers and `oam self-update` will embed.
#
# One line per key:
#
#   oam-release-<id> namespaces="oam-release" ssh-ed25519 <base64> [comment]
#
#   - the principal is exactly "oam-release-<id>"; <id> is the name its line in
#     ./ranges uses (k1, k2, ...)
#   - namespaces="oam-release" is required: it stops a signature this key made
#     for anything else from verifying as a release
#   - ssh-ed25519 (or sk-ssh-ed25519@openssh.com for a hardware key)
#
# k1 is the current release key (signs from v0.18.0, see ./ranges); k2 is the
# staged next key, held offline, with no range yet. Because key lines exist,
# every release must sign; there is no knob that skips it.
#
#   k1  SHA256:zB7Aq4Ky/U90VJ4sAEp0e2A65KfQpiyQXJI4FuT2oss
#   k2  SHA256:Uy7nugF5mDzfM/8/fcUti9K+sQMbn/LdRAk+sIbWYs4
#
# Never remove a key line to retire a key: old tags it signed must keep
# verifying. Close its range in ./ranges instead.
oam-release-k1 namespaces="oam-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJuD0+abFvf+zbUclRzT4uqdebLr6CU8Ps2PE+Qev0/k
oam-release-k2 namespaces="oam-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDdFy96BmdBfcHwXjWDTod9i5sfvJk7wu5MI7j4SnyoR
OAM_EMBED_ALLOWED_SIGNERS
}
embedded_ranges() {
  cat <<'OAM_EMBED_RANGES'
# Which tags each release key may sign. Read by scripts/lib/signing.sh
# (release_tag_in_range); the installers and `oam self-update` will embed it.
#
# At most one line per key in ./allowed_signers:
#
#   <id> <from-tag> <to-tag>
#
#   - <id> is the principal's suffix: "k1" for oam-release-k1
#   - <from-tag> is inclusive; <to-tag> is inclusive, or "-" for open-ended
#   - tags are plain vMAJOR.MINOR.PATCH, compared numerically per field
#   - a key with no line (the staged "next" key) may sign nothing yet
#
# Example: k1 signs from v0.18.0 on; at rotation k1 is closed at the last tag
# it signed and k2 opens at the next one:
#
#   k1 v0.18.0 v0.21.3
#   k2 v0.21.4 -
#
# A verifier accepts a manifest only when the key that signed it has a range
# covering the tag INSIDE the signed manifest. That is why retiring a key is an
# edit here, not a deletion from allowed_signers, and why it never breaks the
# old tags the key legitimately signed.
#
# k1 signs from v0.18.0, the first signed release. k2 is staged: no line yet.
k1 v0.18.0 -
OAM_EMBED_RANGES
}
embedded_presigning_sums() {
  cat <<'OAM_EMBED_PRESIGNING_SUMS'
# SHA-256 of each pre-signing release's published SHA256SUMS (tags before v0.18.0).
# Bounded and immutable: captured 2026-10-02 from the published releases.
# A pre-v0.18.0 tag not listed here is refused. Format: <tag> <sha256-hex>
v0.6.1 2d0082d28b70b171468bd2562300a7b33b847bf0bf3c9e93d0c2e0cc3613f5d3
v0.7.0 28e52f688c4269240d3baebbc4376ea03d21091fb2b5b9ae9068870e5354ba6c
v0.8.0 a6bfd6e74f1a8c0710c7846be9ec67206caa28fee7cdeba359ddd1f7af1eb0af
v0.8.1 d3e806ca0f5b20ec3bd0a480e55d3b2941b687358261196bf00fd7877d86d403
v0.8.2 fb85c1c2e0b514e50748b72937dc1f9d5f675ad120cce57ceb27f93611ab5f75
v0.8.3 b142260e172646b3e704e4a1a85e1a810cdc590e6d4a4f1fcb8633e753d417da
v0.9.0 6a7cbbc21b464525885254ff729f1f927546c3c88952d2f867c3d2167fa4743e
v0.9.1 abc9607cf134c0a3123618700d892eda5a20b7759f90efd68c5b70e91bc37c36
v0.10.2 1623ff13605ca4181e7262489e0d2a01baf9e99040cb1bb60fae926d9c339bd9
v0.11.0 149fc07a00aefe35f8730195cdc9199a26e4d45cda6c9b884dab7ce5ae16e118
v0.12.0 0942ec0eeb56074c71129cb857f05104194e9c98ae100bb023e037780b937581
v0.12.1 3bd908edc17737a68dacbe26e4dd28fdfdf7ce779971ade1b6cc7dcfd14cbde7
v0.13.0 f28c6aa95c7a173e5318ef809ecdf69cbf1c84945d04e6d9f3011765146f8265
v0.13.1 78e1eeea71a51c4076677d344859fe0cbe905196e852a14992adc8bdae5a7e45
v0.14.0 a92fc06ab7bfde6c15bada68dedd2964630f2ed09c5c1f48ffa8b097e07fa6e4
v0.15.0 7d3f57342cea6e96a72f61e25690f56a3b052baf3ee4b8a391d493a7f3285e7c
v0.15.1 ddb1ea652623ac8e74cf7b173ea9d6460883a06d7605907075a00f758e98182c
v0.15.2 1a1e6d63bd49d0741eea7cd9ffdca06780b2cf00c496595b71f3755a8b9b5137
v0.15.3 653760ba45a1dc636662dde47f951bbbfd714aa5f658dcc0a8ac8e8b0efc7afd
v0.16.1 a35db0c44321f4f003a259989eb12efc522fdeb1871b53835ef28f561921fe39
v0.16.2 2b50575f440e747bd69b30af08a1113431b70823cf7a801a594164836c41f540
v0.16.3 ea11b31eb8b92aa8576545de66a0e1f9b65db3b8deba4428f0f9b2b1d14087f6
v0.16.4 28f7e3485703f7381111d8c49dba096dc0714e018eb264ac3d0d29d61c3ec19f
v0.17.0 78c4b2249f42e1959f6f6d2ca29d613c32a8808360c170702d211d26cc104765
v0.17.1 d13c587307519995fabe05a999abdfae9292921b261e5808082b53e7acbe79a9
OAM_EMBED_PRESIGNING_SUMS
}

# Comment and blank lines out. [ \t], not [[:space:]]: Debian's older mawk has
# no POSIX character classes and would match nothing.
data_lines() { awk '!/^[ \t]*(#|$)/'; }

# is_plain_tag <tag> -- vMAJOR.MINOR.PATCH and nothing else, the only shape
# this repo releases and the only one the key ranges can place. awk's ^ and $
# anchor the whole string, so an embedded newline cannot sneak a second line in.
is_plain_tag() { awk -v t="$1" 'BEGIN { exit !(t ~ /^v[0-9]+\.[0-9]+\.[0-9]+$/) }'; }

# tag_le <a> <b> -- 0 when plain tag a <= b, numerically per field: a string
# compare would put v0.10.0 before v0.9.0.
tag_le() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    sub(/^v/, "", a); sub(/^v/, "", b); split(a, x, "."); split(b, y, ".")
    for (i = 1; i <= 3; i++) { if (x[i] + 0 < y[i] + 0) exit 0; if (x[i] + 0 > y[i] + 0) exit 1 }
    exit 0
  }'
}

need uname
need awk
# Prefer curl, fall back to wget.
# The token must never reach argv: /proc/<pid>/cmdline is world-readable on
# Linux, so `-H "Authorization: Bearer ..."` leaks the secret to every local
# user for the life of the request. curl reads directives from stdin via
# `--config -`; wget has no stdin equivalent, so it gets a mode-600 rc file
# (WGETRC), which is at least owner-only rather than world-readable.
if command -v curl >/dev/null 2>&1; then
  dl() { curl -fsSL "$1" -o "$2"; }
  # The URL a redirect chain ends at; HEAD, so no page body is fetched.
  final_url() { curl -fsSLI -o /dev/null -w '%{url_effective}' "$1"; }
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
  # wget prints each hop's Location: with -S; the last one is where it ended.
  final_url() {
    wget -S --spider "$1" 2>&1 | tr -d '\r' \
      | awk '$1 == "Location:" || $1 == "location:" { u = $2 } END { if (u == "") exit 1; print u }'
  }
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
if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "need sha256sum or shasum to verify the download"
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

tmp="$(mktemp -d)"
bin_tmp=""
trap 'rm -rf "$tmp"; if [ -n "$bin_tmp" ]; then rm -f "$bin_tmp"; fi' EXIT
# dash and friends skip the EXIT trap on a signal; route signals through exit.
trap 'exit 1' HUP INT TERM

# Resolve the tag ONCE. Every asset below comes from /download/<tag>/, never
# from /latest/download/: two separate "latest" fetches can straddle a release
# and pair one tag's binary with another's checksums -- and the signature check
# needs to know which tag it is checking.
tag_from_redirect() {
  _u="$(final_url "https://github.com/${OWNER_REPO}/releases/latest" 2>/dev/null)" || return 1
  case "$_u" in
    */releases/tag/*) printf '%s\n' "${_u##*/releases/tag/}" ;;
    *) return 1 ;;
  esac
}
# Private repo: /releases/latest 404s unauthenticated, so ask the API with the
# token, then the gh CLI. Strip whitespace first, as token_dl explains.
tag_from_api() {
  [ -n "$TOKEN" ] || return 1
  api_get "${GH_API}/repos/${OWNER_REPO}/releases/latest" | tr -d ' \n\r' \
    | grep -o '"tag_name":"[^"]*"' | head -1 | cut -d'"' -f4
}
tag_from_gh() {
  command -v gh >/dev/null 2>&1 || return 1
  gh release view --repo "$OWNER_REPO" --json tagName -q .tagName 2>/dev/null
}

if [ -n "${OAM_VERSION:-}" ]; then
  tag="$OAM_VERSION"
elif [ -n "${OAM_INSTALL_BASE:-}" ]; then
  die "OAM_INSTALL_BASE needs OAM_VERSION too: the signature is checked against the tag you ask for, and a mirror cannot be asked which tag it serves (e.g. OAM_VERSION=${FIRST_MANIFEST_SIG_TAG})"
else
  tag="$(tag_from_redirect || true)"
  [ -n "$tag" ] || tag="$(tag_from_api 2>/dev/null || true)"
  [ -n "$tag" ] || tag="$(tag_from_gh || true)"
  [ -n "$tag" ] || die "could not resolve the latest release tag (private repo? set GH_TOKEN or install the gh CLI; or set OAM_VERSION)"
fi
is_plain_tag "$tag" || die "'$tag' is not a release tag -- oam tags look like ${FIRST_MANIFEST_SIG_TAG}"

if [ -n "${OAM_INSTALL_BASE:-}" ]; then
  base="$OAM_INSTALL_BASE"
else
  base="https://github.com/${OWNER_REPO}/releases/download/${tag}"
fi

# Token fallback: works on headless hosts with no gh CLI (CI, Docker, a fresh
# VM). A private-repo asset is NOT reachable via its browser_download_url even
# with a token -- GitHub only serves the bytes from the assets endpoint with
# Accept: application/octet-stream -- so resolve the numeric asset id first.
rel_json=""
token_dl() {
  [ -n "$TOKEN" ] || return 1
  if [ -z "$rel_json" ]; then
    rel_json="$(api_get "${GH_API}/repos/${OWNER_REPO}/releases/tags/${tag}")" || return 1
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
  gh release download "$tag" --repo "$OWNER_REPO" --pattern "$1" \
    --output "$2" --clobber 2>/dev/null
}

# Direct first (public releases + a mirror), then token, then gh CLI. All three
# fetch the same tag, and everything fetched is verified the same way.
fetch_asset() {
  dl "${base}/$1" "$2" && return 0
  if [ -n "$TOKEN" ]; then
    say "direct download of $1 failed; retrying with \$GH_TOKEN"
    token_dl "$1" "$2" && return 0
  fi
  if command -v gh >/dev/null 2>&1; then
    say "retrying $1 via gh CLI (private repo needs auth)"
    gh_dl "$1" "$2" && return 0
  fi
  return 1
}

# hash_from_sums <file> -- the sha256 <file> lists for $asset, when it lists it
# exactly once. Field 2 is matched exactly, with sha256sum's binary-mode "*"
# stripped: the release writes "<hash> *<asset>". Two entries are refused
# rather than resolved: the first of a duplicate pair has been the stale one.
hash_from_sums() {
  awk -v a="$asset" '{ f = $2; sub(/^\*/, "", f); if (f == a) { n++; h = $1 } }
    END { if (n != 1) exit 1; print tolower(h) }' "$1"
}

# find_ssh_keygen -- KEYGEN set to an ssh-keygen that understands -Y (OpenSSH
# 8.1+), or empty with KEYGEN_WHY saying what is wrong. The probe asks the tool
# itself, as scripts/lib/signing.sh does: one with -Y answers an unknown
# operation with "Unsupported operation for -Y"; one without rejects the flag.
KEYGEN=""; KEYGEN_WHY=""
find_ssh_keygen() {
  _kg="$(command -v ssh-keygen 2>/dev/null)" || _kg=""
  if [ -z "$_kg" ]; then KEYGEN_WHY="ssh-keygen is not installed"; return 1; fi
  _probe="$("$_kg" -Y oam-probe </dev/null 2>&1)" || true
  case "$_probe" in
    *'Unsupported operation for -Y'*) KEYGEN="$_kg"; return 0 ;;
  esac
  KEYGEN_WHY="$_kg has no -Y (it is older than OpenSSH 8.1)"
  return 1
}
KEYGEN_FIX="install OpenSSH 8.1 or later -- Debian/Ubuntu: apt-get install openssh-client; Alpine: apk add openssh-keygen; Fedora/RHEL 9/Amazon Linux: dnf install openssh (RHEL/Alma/Rocky 8 ship 8.0, which has no -Y); macOS ships it"

# verify_manifest -- the signed-era chain, on $tmp/RELEASE-MANIFEST{,.sig}.
# Leaves the signed SHA256SUMS section in $tmp/SUMS.verified.
verify_manifest() {
  _m="$tmp/RELEASE-MANIFEST"; _s="$tmp/RELEASE-MANIFEST.sig"
  _principal=""
  if [ -n "$KEYGEN" ]; then
    # 1. The signature, against each embedded principal in turn with -I: the
    #    exact command a person verifying by hand runs.
    embedded_allowed_signers >"$tmp/allowed_signers"
    for _p in $(embedded_allowed_signers | data_lines | awk '{ print $1 }'); do
      case "$_p" in "$PRINCIPAL_PREFIX"*) ;; *) continue ;; esac
      if "$KEYGEN" -Y verify -f "$tmp/allowed_signers" -I "$_p" -n "$SIGN_NAMESPACE" \
           -s "$_s" <"$_m" >"$tmp/verify.out" 2>&1; then
        _principal="$_p"; break
      fi
    done
    [ -n "$_principal" ] \
      || die "RELEASE-MANIFEST.sig for $tag does not verify against any oam release key ($(head -1 "$tmp/verify.out" 2>/dev/null)) -- this is not a release we signed; refusing it"
  fi
  # 2. The header, only now that the bytes are known to be ours: line 1 the v1
  #    header, line 2 exactly "tag <the tag we resolved>". awk reads the raw
  #    bytes, so a CR is seen rather than stripped.
  _hdr="$(awk -v h="$MANIFEST_HEADER" -v t="tag $tag" '
    NR == 1 { if (index($0, "\r")) { r = "crlf" } else if ($0 != h) { r = "header" } }
    NR == 2 { if (r == "" && index($0, "\r")) r = "crlf"; else if (r == "" && $0 != t) r = ($0 ~ /^tag / ? "tag:" substr($0, 5) : "header"); exit }
    END { if (NR < 2 && r == "") r = "header"; print (r == "" ? "ok" : r) }' "$_m")"
  case "$_hdr" in
    ok) ;;
    tag:*) die "RELEASE-MANIFEST is signed for tag '${_hdr#tag:}', not $tag -- a replayed or misfiled release; refusing it" ;;
    crlf) die "RELEASE-MANIFEST for $tag has CR line endings -- it is LF-only, byte for byte; refusing it" ;;
    *) die "RELEASE-MANIFEST for $tag does not start with '$MANIFEST_HEADER' / 'tag $tag' -- refusing it" ;;
  esac
  # 3. The signing key's range covers the tag. A key with no range line (the
  #    staged next key) signs nothing.
  if [ -n "$_principal" ]; then
    _id="${_principal#"$PRINCIPAL_PREFIX"}"
    _range="$(embedded_ranges | data_lines | awk -v id="$_id" '$1 == id { print $2 " " $3; exit }')"
    [ -n "$_range" ] || die "RELEASE-MANIFEST for $tag is signed by $_principal, which has no range in release-keys/ranges -- a staged key signs nothing yet; refusing it"
    _from="${_range%% *}"; _to="${_range#* }"
    tag_le "$_from" "$tag" || die "RELEASE-MANIFEST for $tag is signed by $_principal, which may sign only from $_from on; refusing it"
    if [ "$_to" != "-" ] && ! tag_le "$tag" "$_to"; then
      die "RELEASE-MANIFEST for $tag is signed by $_principal, which was retired after $_to; refusing it"
    fi
    say "signature ok: $tag, signed by $_principal"
  fi
  tail -n +3 "$_m" >"$tmp/SUMS.verified"
}

# --- Verify the release BEFORE the binary is downloaded -------------------------
if tag_le "$FIRST_MANIFEST_SIG_TAG" "$tag"; then
  find_ssh_keygen || true
  if [ -z "$KEYGEN" ]; then
    if [ "${OAM_INSECURE_SKIP_SIGNATURE:-}" = "1" ]; then
      warn "$KEYGEN_WHY, so the signature of $tag CANNOT be checked."
      warn "OAM_INSECURE_SKIP_SIGNATURE=1: installing WITHOUT signature verification. The checksum below only proves the download matches what the release says, not that we published it."
      warn "to verify instead: $KEYGEN_FIX"
    else
      die "$KEYGEN_WHY, and oam $tag is verified by its signature (ssh-keygen -Y verify). Fix: $KEYGEN_FIX. (To install anyway, unverified: OAM_INSECURE_SKIP_SIGNATURE=1.)"
    fi
  fi
  fetch_asset RELEASE-MANIFEST "$tmp/RELEASE-MANIFEST" \
    || die "could not fetch RELEASE-MANIFEST for $tag -- every release from $FIRST_MANIFEST_SIG_TAG on is signed, so a missing manifest means the download failed or the release was tampered with"
  fetch_asset RELEASE-MANIFEST.sig "$tmp/RELEASE-MANIFEST.sig" \
    || die "could not fetch RELEASE-MANIFEST.sig for $tag -- every release from $FIRST_MANIFEST_SIG_TAG on is signed, so a missing signature means the download failed or the release was tampered with"
  verify_manifest
  sums="$tmp/SUMS.verified"
else
  # Cut before signing existed: the pinned digest of its SHA256SUMS is the
  # proof, and ssh-keygen plays no part. Said anyway when it is missing, so the
  # next (signed) install does not come as a surprise.
  if ! find_ssh_keygen; then
    warn "$KEYGEN_WHY. Not needed for $tag (verified by its pinned digest), but releases from $FIRST_MANIFEST_SIG_TAG on are verified by signature: $KEYGEN_FIX"
  fi
  pinned="$(embedded_presigning_sums | data_lines | awk -v t="$tag" '$1 == t { print $2; exit }')"
  [ -n "$pinned" ] || die "$tag predates signed releases ($FIRST_MANIFEST_SIG_TAG) and is not in the pinned table of pre-signing releases -- there is no such oam release; refusing it"
  fetch_asset SHA256SUMS "$tmp/SHA256SUMS" || die "could not fetch SHA256SUMS for $tag"
  got="$(sha256_of "$tmp/SHA256SUMS")"
  [ "$got" = "$pinned" ] || die "SHA256SUMS for $tag hashes to $got, but $tag's pinned digest is $pinned -- it is not the file that release published; refusing it"
  say "SHA256SUMS ok: $tag matches its pinned digest (released before signing)"
  sums="$tmp/SHA256SUMS"
fi
expected="$(hash_from_sums "$sums")" || die "the verified checksums for $tag list ${asset} $(awk -v a="$asset" '{ f = $2; sub(/^\*/, "", f); if (f == a) n++ } END { print n + 0 }' "$sums") times, not once"

# --- macOS code-signature hook (INACTIVE) ---------------------------------------
# Developer ID signing is not live yet (signing plan 4.2 / rollout step 6), and
# an ad-hoc signature proves nothing about who built a binary, so today this
# checks nothing. When it goes live: set FIRST_DEVID_TAG to the first tag
# signed with it and DEVID_TEAM_ID to the pinned TeamIdentifier (public in every
# signature), and every tag from FIRST_DEVID_TAG on must pass
# `codesign --verify --strict` and carry that team.
FIRST_DEVID_TAG=""
DEVID_TEAM_ID=""
macos_signature_check() {
  [ "$os" = "Darwin" ] && [ -n "$FIRST_DEVID_TAG" ] || return 0
  tag_le "$FIRST_DEVID_TAG" "$tag" || return 0
  command -v codesign >/dev/null 2>&1 || die "codesign not found -- cannot check the Developer ID signature of $tag"
  codesign --verify --strict "$1" 2>/dev/null || die "the $tag binary fails codesign --verify --strict; refusing it"
  _team="$(codesign -dv "$1" 2>&1 | awk -F= '$1 == "TeamIdentifier" { print $2 }')"
  [ "$_team" = "$DEVID_TEAM_ID" ] || die "the $tag binary is signed by team '${_team}', not oam's ${DEVID_TEAM_ID}; refusing it"
}

# --- Download, check, and replace by rename -------------------------------------
# The binary lands in a temp file IN the install dir, then a rename replaces
# the old one. Never written in place: overwriting a running binary fails with
# ETXTBSY on Linux, and macOS SIGKILLs a signed Mach-O modified in place. A
# temp file elsewhere would make that `mv` a cross-filesystem copy -- the same
# in-place write, under another name.
mkdir -p "$INSTALL_DIR"
bin_tmp="$(mktemp "$INSTALL_DIR/.oam.XXXXXX")" || die "could not create a temp file in $INSTALL_DIR"
say "downloading ${asset} ${tag} from ${base}"
fetch_asset "${asset}" "$bin_tmp" || die "download failed: ${asset} (private repo? set GH_TOKEN or install the gh CLI)"
actual="$(sha256_of "$bin_tmp")"
[ "$expected" = "$actual" ] || die "checksum mismatch for ${asset} (expected ${expected}, got ${actual})"
say "checksum ok"
macos_signature_check "$bin_tmp"
chmod 755 "$bin_tmp"
mv -f "$bin_tmp" "${INSTALL_DIR}/oam"
bin_tmp=""
say "installed oam ${tag} to ${INSTALL_DIR}/oam"

# The binary is a binary redistribution of V8, ICU, the Node streams port and
# ~380 Rust crates, whose licenses require their notices travel with it. Put
# them beside the binary so the copy on THIS machine carries its attribution,
# not just the repo it came from. Best-effort: a missing notice is not worth
# failing an otherwise good install over, and it is reported rather than
# swallowed.
got_licenses=0
mkdir -p "${INSTALL_DIR}/licenses"
for f in LICENSE NOTICE THIRD_PARTY_LICENSES.md; do
  if fetch_asset "$f" "${tmp}/$f" 2>/dev/null; then
    mv -f "${tmp}/$f" "${INSTALL_DIR}/licenses/$f"
    got_licenses=1
  fi
done
if [ "$got_licenses" = "1" ]; then
  say "license and attribution files in ${INSTALL_DIR}/licenses"
else
  # Releases before v0.8.1 shipped no license assets. Leaving an empty
  # directory behind would imply they were installed, so remove it and say
  # plainly where to read them instead.
  rmdir "${INSTALL_DIR}/licenses" 2>/dev/null || true
  say "note: this release ships no license assets; see https://github.com/${OWNER_REPO}"
fi

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
