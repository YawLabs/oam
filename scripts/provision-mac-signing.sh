#!/bin/bash
# =============================================================================
# Runs ON the mac build host. Owns oam's STABLE self-signed code-signing
# identity, which the mac release leg signs both oam binaries with until a
# Yaw Labs Developer ID replaces it.
#
# WHAT IT BUYS, honestly: a named signature ("oam Code Signing (self-signed)")
# instead of an ad-hoc one, so `codesign -dv` says who signed the binary and the
# designated requirement names one certificate rather than a per-build cdhash.
# It buys users NOTHING at Gatekeeper: a self-signed identity is treated like
# ad-hoc, and a quarantined browser download is still blocked. That needs the
# Developer ID plus notarization, and the swap is: provision that identity into
# a build keychain, commit its SHA-1 to scripts/mac-signing-identity.sha1, add
# notarization. scripts/lib/mac-signing.sh already picks the right timestamp and
# verify rules from the certificate it finds.
#
# This is oam's OWN identity and keychain. It is never Yaw Terminal's: that
# certificate gates Yaw users' Keychain items, and a second signing path would
# widen its exposure for a cosmetic gain while mislabelling every oam binary.
#
# The identity lives in a DEDICATED keychain with its own password, which a
# headless SSH session can unlock -- the standard CI signing pattern. The login
# keychain is locked in an SSH session, and a codesign against it fails with
# errSecInternalComponent while `security find-identity` still lists the
# identity as fine. `codesign --keychain <file>` finds the key without touching
# the user's keychain search list, so nothing else on the host is affected.
#
# Generation is an explicit operator action, NEVER something a release does on
# its own (a release only ever runs `--check`). The certificate's SHA-1 is
# pinned in scripts/mac-signing-identity.sha1 and the release refuses to sign
# with anything else. Back up ~/.oam-signing/oam-codesign.p12 and
# oam-codesign.p12-password off this host; `--import` restores them onto a
# replacement host.
#
# Keychain password custody: the password sits in a 0600 file next to the
# keychain, read on this host by the release. The signing plan's preferred
# alternative is to `read -s` it once per release on the release box and send
# it over ssh stdin, so it is never at rest here. For a self-signed identity
# that buys nothing at Gatekeeper the file is accepted; revisit when the
# Developer ID moves in.
#
# bash 3.2 compatible (the build Mac's /bin/bash). The whole script is
# functions plus one `main "$@"` line, because the release box's preflight runs
# `--check` by piping THIS file to `ssh <mac> 'bash -s -- --check'` (it syncs
# nothing, so the Mac's copy may be stale). bash reads such a script from stdin
# as it goes; a command that read stdin mid-script would swallow the rest of
# it. Parsed whole before anything runs, it cannot.
#
# Usage:
#   bash scripts/provision-mac-signing.sh --check
#       Unlock the keychain and prove a signature can be made right now. Prints
#       `keychain=<path>` and `sha1=<fingerprint>` on success. Non-zero on any
#       problem. Generates nothing. This is what the release calls.
#   bash scripts/provision-mac-signing.sh --generate
#       First-time setup: create the keychain and a new identity. Refuses if one
#       already exists. Prints the SHA-1 to commit.
#   bash scripts/provision-mac-signing.sh --import <identity.p12> <password-file>
#       Restore a backed-up identity onto this host. Refuses if one already
#       exists.
#   OAM_SIGNING_DIR=<dir> overrides ~/.oam-signing (and must then be set for
#   the release too; scripts/lib/mac-signing.sh reads the same variable).
# =============================================================================
set -euo pipefail

MODE="${1:-}"
SIGN_DIR="${OAM_SIGNING_DIR:-$HOME/.oam-signing}"
KEYCHAIN="$SIGN_DIR/oam-codesign.keychain-db"
PW_FILE="$SIGN_DIR/oam-codesign.keychain-password"
P12_FILE="$SIGN_DIR/oam-codesign.p12"
P12_PW_FILE="$SIGN_DIR/oam-codesign.p12-password"
SHA_FILE="$SIGN_DIR/oam-codesign.sha1"
CERT_NAME="oam Code Signing (self-signed)"
# ~22 years: codesign refuses an expired certificate. Kept short of 2050 on
# purpose: X.509 switches notAfter from UTCTime to GeneralizedTime there, and a
# long-lived identity is the wrong place to find out which parser mishandles it.
CERT_DAYS=8000
# The system LibreSSL, by absolute path: a Homebrew OpenSSL 3 earlier on PATH
# writes PKCS#12 with algorithms `security import` rejects ("MAC verification
# failed") unless given -legacy, which LibreSSL in turn does not understand.
OPENSSL=/usr/bin/openssl

fail() { echo "[mac-signing] FATAL: $*" >&2; exit 1; }
note() { echo "[mac-signing] $*" >&2; }

usage() {
  cat >&2 <<'EOF'
usage: bash scripts/provision-mac-signing.sh --check
       bash scripts/provision-mac-signing.sh --generate
       bash scripts/provision-mac-signing.sh --import <identity.p12> <password-file>
EOF
  exit 2
}

# Normalise a fingerprint to the form codesign prints inside a designated
# requirement: 40 lowercase hex chars, no separators.
normalise_sha1() { tr -d ' :\n\r' | tr 'A-F' 'a-f'; }

keychain_sha1() {
  # The SHA-1 of the one code-signing IDENTITY (certificate + private key) in
  # this keychain -- not of whatever certificate lists first. A Developer ID
  # .p12 usually carries its intermediate CA too, and the order `security
  # find-certificate` lists them in is not guaranteed. No -v: a self-signed
  # certificate is not trusted, and -v lists only valid identities. Each
  # identity reads `  1) <40 HEX> "<name>" [(<trust error>)]`, and may appear
  # in both the "Matching" and the "Valid identities only" sections, hence
  # sort -u. Prints nothing and returns 1 unless there is exactly one; the
  # caller says why that is fatal.
  local all n
  all="$(security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null </dev/null \
    | awk '$1 ~ /^[0-9]+\)$/ && length($2) == 40 && $2 !~ /[^0-9A-Fa-f]/ { print $2 }' \
    | normalise_lines | sort -u)"
  n="$(printf '%s' "$all" | grep -c . || true)"
  if [ "$n" != "1" ]; then
    note "$KEYCHAIN holds $n code-signing identities; exactly one is expected${all:+: $(printf '%s' "$all" | tr '\n' ' ')}"
    return 1
  fi
  printf '%s\n' "$all"
}
# normalise_sha1 per line (it joins lines, for a single fingerprint).
normalise_lines() { tr -d ' :\r' | tr 'A-F' 'a-f'; }

unlock() {
  [ -f "$KEYCHAIN" ] || fail "signing keychain missing: $KEYCHAIN -- on this host run 'bash scripts/provision-mac-signing.sh --generate' (first time) or '--import <p12> <password-file>' (restore a backup)"
  [ -f "$PW_FILE" ] || fail "keychain password file missing: $PW_FILE"
  security unlock-keychain -p "$(cat "$PW_FILE")" "$KEYCHAIN" </dev/null \
    || fail "could not unlock $KEYCHAIN with the password in $PW_FILE"
}

# Sign a throwaway copy of a system binary, in the release's own shape (hardened
# runtime, no timestamp). This is the ONLY real proof that the private key is
# usable from this session: `security find-identity` happily lists an identity
# whose key it cannot use.
probe_sign() {
  local sha="$1" tmp rc=0 why
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/oam-sign-probe.XXXXXX")" || fail "mktemp failed"
  cp /usr/bin/true "$tmp/probe" || { rm -rf "$tmp"; fail "could not copy /usr/bin/true to probe with"; }
  codesign --force --keychain "$KEYCHAIN" --sign "$sha" --options runtime --timestamp=none \
    "$tmp/probe" >/dev/null 2>"$tmp/err" </dev/null || rc=$?
  if [ "$rc" -ne 0 ]; then
    why="$(cat "$tmp/err")"
    rm -rf "$tmp"
    fail "probe signature failed (codesign exit $rc): $why"
  fi
  rm -rf "$tmp"
}

create_keychain_with() {
  # $1 = p12 path, $2 = p12 password
  # Runs inside $(...), where bash 4.4+ clears errexit, so every step checks
  # its own status: a keychain that half-built must not reach the probe.
  local p12="$1" p12_pw="$2" kc_pw sha
  mkdir -p "$SIGN_DIR" || fail "could not create $SIGN_DIR"
  chmod 700 "$SIGN_DIR" || fail "could not chmod $SIGN_DIR"
  kc_pw="$("$OPENSSL" rand -hex 24)" || fail "could not generate a keychain password"
  ( umask 077; printf '%s' "$kc_pw" > "$PW_FILE" ) || fail "could not write $PW_FILE"
  security create-keychain -p "$kc_pw" "$KEYCHAIN" || fail "security create-keychain failed"
  # No arguments = no idle timeout and no lock-on-sleep. A release runs for
  # many minutes between the unlock and the signature (the leg re-unlocks
  # before every codesign anyway).
  security set-keychain-settings "$KEYCHAIN" || fail "security set-keychain-settings failed"
  security unlock-keychain -p "$kc_pw" "$KEYCHAIN" || fail "security unlock-keychain failed"
  security import "$p12" -k "$KEYCHAIN" -P "$p12_pw" -T /usr/bin/codesign >/dev/null \
    || fail "security import of $p12 failed (wrong p12 password?)"
  # Without the partition list, codesign's first use of the key raises a GUI
  # "allow access" dialog -- which in an SSH session means a hang or
  # errSecInternalComponent.
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kc_pw" "$KEYCHAIN" >/dev/null \
    || fail "security set-key-partition-list failed"
  sha="$(keychain_sha1)" || fail "imported identity not found in $KEYCHAIN as exactly one code-signing identity (a .p12 must hold one certificate + key; its CA chain may ride along)"
  ( umask 077; printf '%s\n' "$sha" > "$SHA_FILE" )
  probe_sign "$sha"
  echo "$sha"
}

refuse_if_provisioned() {
  if [ -e "$KEYCHAIN" ] || [ -e "$P12_FILE" ]; then
    fail "an identity already exists in $SIGN_DIR. Replacing it changes the certificate every oam release is pinned to (scripts/mac-signing-identity.sha1). If that is really intended, move $SIGN_DIR aside by hand first."
  fi
}

# Undo a --generate / --import that did not finish. Without this a failure
# partway (a wrong p12 password, a dropped ssh session, a probe signature that
# does not work) leaves a half-built keychain and a p12 behind, and the NEXT
# attempt dies in refuse_if_provisioned claiming "an identity already exists".
# It would also strand the freshly generated private key in a temp dir.
#
# Armed only AFTER refuse_if_provisioned has proved none of these files
# existed, so it can only ever delete what this run created. Disarmed on
# success. `fail` inside a $(...) exits only that subshell; this EXIT trap
# belongs to the parent and still runs when `set -e` then ends the script.
PARTIAL_WORK_DIR=""
cleanup_partial() {
  note "provisioning did not finish -- removing the partial identity so a retry starts clean"
  security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  rm -f "$KEYCHAIN" "$PW_FILE" "$P12_FILE" "$SHA_FILE" "$P12_PW_FILE"
  if [ -n "$PARTIAL_WORK_DIR" ] && [ -d "$PARTIAL_WORK_DIR" ]; then
    rm -f "$PARTIAL_WORK_DIR/key.pem" "$PARTIAL_WORK_DIR/cert.pem" "$PARTIAL_WORK_DIR/cert.cnf"
    rmdir "$PARTIAL_WORK_DIR" 2>/dev/null || true
  fi
}

do_check() {
  local want have
  unlock
  [ -f "$SHA_FILE" ] || fail "fingerprint file missing: $SHA_FILE"
  want="$(normalise_sha1 < "$SHA_FILE")"
  have="$(keychain_sha1)" || fail "no single code-signing identity found in $KEYCHAIN"
  [ "$have" = "$want" ] || fail "keychain identity $have does not match recorded fingerprint $want"
  probe_sign "$have"
  note "identity $have is usable from this session"
  echo "keychain=$KEYCHAIN"
  echo "sha1=$have"
}

do_generate() {
  local work p12_pw sha
  refuse_if_provisioned
  trap cleanup_partial EXIT
  mkdir -p "$SIGN_DIR"; chmod 700 "$SIGN_DIR"
  work="$(mktemp -d)"
  PARTIAL_WORK_DIR="$work"
  cat > "$work/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = v3
prompt = no
[dn]
CN = $CERT_NAME
O = Yaw Labs
[v3]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
  "$OPENSSL" req -x509 -newkey rsa:2048 -nodes -days "$CERT_DAYS" \
    -keyout "$work/key.pem" -out "$work/cert.pem" -config "$work/cert.cnf" >/dev/null 2>&1 \
    || fail "certificate generation failed"
  p12_pw="$("$OPENSSL" rand -hex 24)"
  ( umask 077
    "$OPENSSL" pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" \
      -name "$CERT_NAME" -passout "pass:$p12_pw" -out "$P12_FILE" )
  rm -f "$work/key.pem" "$work/cert.pem" "$work/cert.cnf"; rmdir "$work"
  PARTIAL_WORK_DIR=""
  # The backup p12 is protected by its own password, stored next to it. Both
  # files are needed to restore. Written BEFORE the keychain is built, so a
  # working keychain can never exist alongside a backup nobody can open.
  ( umask 077; printf '%s' "$p12_pw" > "$P12_PW_FILE" )
  sha="$(create_keychain_with "$P12_FILE" "$p12_pw")"
  trap - EXIT
  note "generated identity \"$CERT_NAME\""
  note "  sha1      $sha"
  note "  keychain  $KEYCHAIN"
  note "NEXT: (1) write that sha1 to scripts/mac-signing-identity.sha1 in the repo and commit it,"
  note "      (2) copy oam-codesign.p12 + oam-codesign.p12-password off this host."
  echo "sha1=$sha"
}

do_import() {
  local src_p12="$1" src_pw_file="$2" sha
  [ -f "$src_p12" ] && [ -f "$src_pw_file" ] || usage
  refuse_if_provisioned
  trap cleanup_partial EXIT
  mkdir -p "$SIGN_DIR"; chmod 700 "$SIGN_DIR"
  ( umask 077
    cp "$src_p12" "$P12_FILE"
    cp "$src_pw_file" "$P12_PW_FILE" )
  sha="$(create_keychain_with "$P12_FILE" "$(cat "$src_pw_file")")"
  trap - EXIT
  note "imported identity, sha1 $sha -- this must equal scripts/mac-signing-identity.sha1"
  echo "sha1=$sha"
}

main() {
  [ "$(uname -s)" = "Darwin" ] || fail "this script only runs on macOS (got $(uname -s))"
  case "$MODE" in
    --check)    do_check ;;
    --generate) do_generate ;;
    --import)   do_import "${2:-}" "${3:-}" ;;
    *)          usage ;;
  esac
}

main "$@"
