# shellcheck shell=bash
# =============================================================================
# mac-signing.sh -- code-signing the macOS release binaries, and proving it.
# =============================================================================
# Sourced by scripts/build-remote.sh on the build Mac (the mac-release leg) and
# by scripts/build-platforms-tailnet.sh on the release box (its preflight and
# the hash hand-back). Defines functions and defaults only; sourcing has no side
# effects. bash 3.2 compatible (the build Mac's /bin/bash). Every message goes
# to stderr: the tailnet preflight's contract is a silent stdout.
#
# Ported from Yaw Terminal's mac signing machinery -- the real-signature probe,
# the locked-keychain remediation, the pinned-identity selection and the verify
# gate -- but with oam's OWN identity and keychain (scripts/provision-mac-
# signing.sh, ~/.oam-signing), never Yaw Terminal's.
#
# What is signed, and how (mac_sign_binary):
#   codesign --force --sign <identity> [--keychain <kc>] --options runtime
#            --timestamp=none --identifier org.oamjs.oam
#            [-r='designated => identifier "org.oamjs.oam" and
#                 certificate leaf = H"<pin>"']
#            --entitlements scripts/macos/oam.entitlements.plist <binary>
#   * A self-signed identity STATES its designated requirement (-r=) rather
#     than taking the one codesign derives: for a self-signed certificate the
#     derived one was measured (Yaw Terminal, same build Mac, macOS 26) to come
#     out as `certificate leaf = H"..."` for one binary and `certificate root =
#     H"..."` for another, and the verify gate accepts only the leaf form.
#     Ad-hoc and Developer ID keep the derived requirement (cdhash; `anchor
#     apple generic` + team, which is what notarization expects).
#   * The hardened runtime and the three entitlements go on EVERY signature,
#     ad-hoc included. An ad-hoc signature can carry both (only the restricted
#     com.apple.developer.* entitlements need a provisioning profile, and these
#     are not those), so the binaries exercise the runtime the Developer ID
#     build will ship, and scripts/fixtures/jit-smoke.js proves V8 still JITs
#     under it now rather than on the day the certificate arrives.
#   * --timestamp=none for ad-hoc and self-signed: Apple's timestamp server is
#     the default TSA, and codesign(1) warns it "may not support signatures made
#     with identities not furnished by Apple" -- a TSA refusal fails the
#     signature outright. A Developer ID Application certificate gets the real
#     --timestamp (notarization requires it); the switch keys off the
#     certificate's own Authority line, so moving to the Developer ID needs no
#     edit here.
#
# Which identity (mac_sign_decision, then mac_signing_setup):
#   OAM_SKIP_MAC_SIGN=1       skip   ship the binaries as the linker left them.
#                                    Loud, and honored even under
#                                    OAM_SIGN_REQUIRED=1 (the signing plan's
#                                    rule for every binary-signing skip knob).
#   no pin committed          adhoc  BOOTSTRAP: ad-hoc + runtime + entitlements,
#                                    with a loud warning. Fatal instead under
#                                    OAM_SIGN_REQUIRED=1.
#   a pin committed           identity  the pinned certificate is MANDATORY:
#                                    `provision-mac-signing.sh --check` must
#                                    unlock the keychain, find that SHA-1 and
#                                    make a real probe signature, or the leg
#                                    fails. Same rule as release-keys/: once
#                                    the trust anchor is committed there is no
#                                    quiet fallback to the weaker thing.
#   The pin lives in scripts/mac-signing-identity.sha1.
#
# OAM_SIGN_REQUIRED is ONE knob for every signing bootstrap. It is the same
# variable scripts/lib/signing.sh reads for the release manifest and for
# Windows Authenticode, and here it gates the mac pin bootstrap: with it set to
# 1, an empty pin file fails the mac preflight, before anything is tagged. The
# pin is committed now (as is release key k1), so for the mac leg the knob
# changes nothing: the pinned identity is mandatory either way, and only
# OAM_SKIP_MAC_SIGN=1 skips it. The knob still matters for Windows, where it
# makes an unset OAM_WIN_SIGN_METADATA fatal, so release-local.sh defaults it
# to 1 (an explicit 0 still wins). The bootstrap branch stays for a checkout
# whose pin file holds no SHA-1, such as a fork's.
#
# The verify gate (mac_verify_binary) re-reads the pin from the file rather
# than trusting what setup chose, so a selection step that was skipped or lost
# its state still cannot let a weaker signature through.
#
# Contract: functions print a precise line on stderr and RETURN non-zero; they
# never exit. The caller decides what a failure ends.
# =============================================================================

MAC_SIGNING_PIN_FILE="${MAC_SIGNING_PIN_FILE:-scripts/mac-signing-identity.sha1}"
MAC_PROVISION_SCRIPT="${MAC_PROVISION_SCRIPT:-scripts/provision-mac-signing.sh}"
MAC_ENTITLEMENTS="${MAC_ENTITLEMENTS:-scripts/macos/oam.entitlements.plist}"
# Same directory and variable as provision-mac-signing.sh.
MAC_SIGN_DIR="${OAM_SIGNING_DIR:-$HOME/.oam-signing}"
MAC_SIGN_PW_FILE="${MAC_SIGN_PW_FILE:-$MAC_SIGN_DIR/oam-codesign.keychain-password}"
# What the probe signs a copy of. Overridable only so the suite can probe on a
# host whose /usr/bin/true is not a plain file (Git Bash: true.exe).
MAC_PROBE_SOURCE="${MAC_PROBE_SOURCE:-/usr/bin/true}"
# The code-signing identifier both binaries carry.
MAC_CODESIGN_IDENTIFIER="org.oamjs.oam"

# Set by mac_signing_setup.
MAC_SIGN_MODE=""
MAC_SIGN_IDENTITY=""
MAC_SIGN_KEYCHAIN=""
MAC_SIGN_AUTHORITY=""
# Set by mac_codesign_identity_usable.
MAC_CODESIGN_PROBE_ERR=""
MAC_CODESIGN_PROBE_AUTHORITY=""

mac_say(){ echo "[mac-signing] $*" >&2; }
mac_warn(){ echo "[mac-signing] WARN: $*" >&2; }
mac_fatal(){ echo "[mac-signing] FATAL: $*" >&2; }

# The fix for a locked signing keychain, printed wherever that is the diagnosis.
mac_keychain_remediation(){
  cat >&2 <<'EOF'
    The signing key's keychain is locked in this SSH session (codesign: errSecInternalComponent).
    A non-GUI session cannot show the unlock prompt, so codesign cannot use the key. To fix, on the
    build Mac: keep the signing identity in a dedicated build keychain with its own password, have the
    build unlock it in this session (security unlock-keychain), and allow codesign to use the key
    without a prompt (security set-key-partition-list -S apple-tool:,apple:,codesign: -s ...).
    scripts/provision-mac-signing.sh does exactly that for oam's identity; `--check` proves it.
EOF
}

# mac_pinned_sha1 -- the committed pin, normalised to what codesign prints in a
# designated requirement (40 lowercase hex), or nothing during bootstrap.
# '#' comments and blank lines are ignored; colons, spaces and case are not
# significant. Returns 1 (with the reason) when what remains is not exactly one
# fingerprint: a truncated or doubled pin must never read as "no pin".
mac_pinned_sha1(){
  local f="$MAC_SIGNING_PIN_FILE" pin
  if [ ! -f "$f" ]; then
    mac_fatal "no pin file at $f -- it is committed (holding only comments during bootstrap); restore it"
    return 1
  fi
  pin="$(sed 's/#.*//' "$f" | tr -d ' :\t\r\n' | tr 'A-F' 'a-f')"
  [ -z "$pin" ] && return 0
  case "$pin" in
    *[!0-9a-f]*)
      mac_fatal "$f does not hold a hex SHA-1 (after comments): '$pin'"
      return 1 ;;
  esac
  if [ "${#pin}" -ne 40 ]; then
    mac_fatal "$f must hold exactly one 40-hex-digit SHA-1, got ${#pin} hex digits"
    return 1
  fi
  printf '%s\n' "$pin"
}

# mac_sign_decision -- what the mac leg will do, as one line on stdout:
#   skip:<reason>       OAM_SKIP_MAC_SIGN=1
#   adhoc:<reason>      bootstrap (no pin), OAM_SIGN_REQUIRED is not 1
#   identity:<sha1>     a pin is committed: that certificate is mandatory
#   fail:<reason>       a malformed knob or pin, or bootstrap under
#                       OAM_SIGN_REQUIRED=1
# A pure function of the knobs and the pin file, so the release box's preflight
# and the Mac reach the same answer, and the suite drives it with fixtures.
mac_sign_decision(){
  local req="${OAM_SIGN_REQUIRED:-0}" skip="${OAM_SKIP_MAC_SIGN:-0}" pin err
  case "$req" in 0|1) ;; *) printf 'fail:OAM_SIGN_REQUIRED must be 0 or 1, not %s\n' "$req"; return 0 ;; esac
  case "$skip" in 0|1) ;; *) printf 'fail:OAM_SKIP_MAC_SIGN must be 0 or 1, not %s\n' "$skip"; return 0 ;; esac
  if ! pin="$(mac_pinned_sha1 2>&1)"; then
    err="$(printf '%s' "$pin" | sed 's/^\[mac-signing\] FATAL: //' | head -n 1)"
    printf 'fail:%s\n' "$err"
    return 0
  fi
  if [ "$skip" = "1" ]; then
    if [ "$req" = "1" ]; then
      printf 'skip:OAM_SKIP_MAC_SIGN=1 -- the mac binaries ship UNSIGNED by this release (as the linker left them), even though OAM_SIGN_REQUIRED=1. Never a normal release\n'
    else
      printf 'skip:OAM_SKIP_MAC_SIGN=1 -- the mac binaries ship UNSIGNED by this release (as the linker left them). Never a normal release\n'
    fi
  elif [ -n "$pin" ]; then
    printf 'identity:%s\n' "$pin"
  elif [ "$req" = "1" ]; then
    printf 'fail:OAM_SIGN_REQUIRED=1 but %s holds no SHA-1 yet -- provision the identity (ssh <mac> '\''bash -s -- --generate'\'' < scripts/provision-mac-signing.sh) and commit its sha1, or set OAM_SIGN_REQUIRED=0 for a bootstrap release (release-local.sh defaults it to 1)\n' "$MAC_SIGNING_PIN_FILE"
  else
    printf 'adhoc:%s holds no SHA-1 yet, so the mac binaries are signed AD-HOC (hardened runtime + entitlements, no certificate). Bootstrap only: once a SHA-1 is committed the pinned identity is mandatory\n' "$MAC_SIGNING_PIN_FILE"
  fi
  return 0
}

# mac_codesign_identity_usable <identity> [keychain] -- 0 if codesign can
# ACTUALLY sign with <identity> from this session, in the release's own shape.
# codesign's error lands in MAC_CODESIGN_PROBE_ERR; the certificate's leaf
# Authority line (e.g. "oam Code Signing (self-signed)", or "Developer ID
# Application: ...") in MAC_CODESIGN_PROBE_AUTHORITY. `security find-identity`
# is not proof: it lists an identity whose key a locked keychain will not hand
# over. A throwaway signature takes ~50ms and tells the truth.
mac_codesign_identity_usable(){
  local id="$1" kc="${2:-}" d rc=0
  MAC_CODESIGN_PROBE_ERR=""
  MAC_CODESIGN_PROBE_AUTHORITY=""
  d="$(mktemp -d "${TMPDIR:-/tmp}/oam-sign-probe.XXXXXX")" \
    || { MAC_CODESIGN_PROBE_ERR="mktemp failed"; return 1; }
  if ! cp "$MAC_PROBE_SOURCE" "$d/probe" 2>/dev/null; then
    rm -rf "$d"
    MAC_CODESIGN_PROBE_ERR="could not copy $MAC_PROBE_SOURCE to probe with"
    return 1
  fi
  if [ -n "$kc" ]; then
    MAC_CODESIGN_PROBE_ERR="$(codesign --force --sign "$id" --keychain "$kc" --options runtime --timestamp=none "$d/probe" 2>&1 </dev/null)" || rc=$?
  else
    MAC_CODESIGN_PROBE_ERR="$(codesign --force --sign "$id" --options runtime --timestamp=none "$d/probe" 2>&1 </dev/null)" || rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    MAC_CODESIGN_PROBE_AUTHORITY="$(codesign -dvv "$d/probe" 2>&1 </dev/null | sed -n 's/^Authority=//p' | head -n 1)"
  fi
  rm -rf "$d"
  return "$rc"
}

# mac_report_probe_failure <what> -- the probe's error, indented, plus the
# remediation when the error names a locked keychain.
mac_report_probe_failure(){
  mac_fatal "$1 cannot sign from this session:"
  printf '%s\n' "${MAC_CODESIGN_PROBE_ERR:-<codesign printed nothing>}" | sed 's/^/    /' >&2
  case "$MAC_CODESIGN_PROBE_ERR" in
    *errSecInternalComponent*|*"User interaction is not allowed"*) mac_keychain_remediation ;;
  esac
}

# mac_unlock_keychain -- unlock the dedicated keychain with the password file
# provision-mac-signing.sh wrote. Called IMMEDIATELY before every codesign
# with the identity, not once per leg: prep, gate, test and conformance run
# first and take long enough for a keychain to relock, which reads as
# errSecInternalComponent. (The keychain is provisioned with no idle timeout;
# this is the belt to that brace.)
#
# The password goes to `security unlock-keychain -p` as an argument, so for
# the instant that command runs it is visible to other local users of the
# build Mac in the process list. Accepted, with the at-rest custody the
# provision script's header describes: security(1) offers no documented way
# to take it on stdin or from a file for a non-interactive unlock, and the
# identity it guards is self-signed. Revisit with the Developer ID.
mac_unlock_keychain(){
  local kc="${1:-$MAC_SIGN_KEYCHAIN}"
  if [ -z "$kc" ] || [ ! -f "$kc" ]; then
    mac_fatal "signing keychain missing: '${kc:-<none>}' -- bash $MAC_PROVISION_SCRIPT --check on the build Mac says why"
    return 1
  fi
  if [ ! -f "$MAC_SIGN_PW_FILE" ]; then
    mac_fatal "keychain password file missing: $MAC_SIGN_PW_FILE"
    return 1
  fi
  if ! security unlock-keychain -p "$(cat "$MAC_SIGN_PW_FILE")" "$kc" </dev/null; then
    mac_fatal "could not unlock $kc with the password in $MAC_SIGN_PW_FILE"
    return 1
  fi
}

# mac_signing_setup -- decide, and for the pinned identity prove it usable
# BEFORE the long build. Sets MAC_SIGN_MODE (skip | adhoc | identity) and, for
# identity, MAC_SIGN_IDENTITY / MAC_SIGN_KEYCHAIN / MAC_SIGN_AUTHORITY. Returns
# 1 when the leg must not go on.
mac_signing_setup(){
  local dec info rc=0 keychain sha pin
  MAC_SIGN_MODE=""; MAC_SIGN_IDENTITY=""; MAC_SIGN_KEYCHAIN=""; MAC_SIGN_AUTHORITY=""
  dec="$(mac_sign_decision)"
  case "$dec" in
    skip:*)  mac_warn "${dec#skip:}"; MAC_SIGN_MODE=skip; return 0 ;;
    adhoc:*) mac_warn "${dec#adhoc:}"; MAC_SIGN_MODE=adhoc; return 0 ;;
    fail:*)  mac_fatal "${dec#fail:}"; return 1 ;;
    identity:*) pin="${dec#identity:}" ;;
    *) mac_fatal "mac_sign_decision returned '$dec' -- refusing to guess how to sign"; return 1 ;;
  esac

  # `--check` unlocks the dedicated keychain, confirms the certificate there is
  # the one the host recorded, and makes a REAL probe signature.
  info="$(bash "$MAC_PROVISION_SCRIPT" --check </dev/null)" || rc=$?
  if [ "$rc" -ne 0 ]; then
    mac_fatal "the pinned signing identity $pin is not usable from this session ($MAC_PROVISION_SCRIPT --check exited $rc; its reason is above)."
    mac_keychain_remediation
    echo "    First-time setup on the build Mac:  bash $MAC_PROVISION_SCRIPT --generate   (then commit the printed sha1 to $MAC_SIGNING_PIN_FILE)" >&2
    echo "    Replacement build Mac:              bash $MAC_PROVISION_SCRIPT --import <oam-codesign.p12> <oam-codesign.p12-password>" >&2
    echo "    To ship without signing on purpose: OAM_SKIP_MAC_SIGN=1" >&2
    return 1
  fi
  keychain="$(printf '%s\n' "$info" | sed -n 's/^keychain=//p' | head -n 1)"
  sha="$(printf '%s\n' "$info" | sed -n 's/^sha1=//p' | head -n 1)"
  if [ -z "$keychain" ] || [ -z "$sha" ]; then
    mac_fatal "could not parse '$MAC_PROVISION_SCRIPT --check' output (want keychain= and sha1= lines), got:"
    printf '%s\n' "$info" | sed 's/^/    /' >&2
    return 1
  fi
  if [ "$sha" != "$pin" ]; then
    mac_fatal "this host's signing certificate is $sha but the repo pins $pin ($MAC_SIGNING_PIN_FILE)."
    echo "    Restore the pinned identity with:  bash $MAC_PROVISION_SCRIPT --import <p12> <password-file>" >&2
    echo "    Only if the identity was rotated ON PURPOSE, update the pin in the same change that says so." >&2
    return 1
  fi
  mac_unlock_keychain "$keychain" || return 1
  if ! mac_codesign_identity_usable "$sha" "$keychain"; then
    mac_report_probe_failure "the pinned identity $sha"
    return 1
  fi
  MAC_SIGN_MODE=identity
  MAC_SIGN_IDENTITY="$sha"
  MAC_SIGN_KEYCHAIN="$keychain"
  MAC_SIGN_AUTHORITY="$MAC_CODESIGN_PROBE_AUTHORITY"
  mac_say "signing with the pinned identity $sha (${MAC_SIGN_AUTHORITY:-authority unknown}; keychain $keychain)"
  return 0
}

# mac_sign_binary <binary> -- sign it per MAC_SIGN_MODE (mac_signing_setup
# first). Rewrites the file, so it runs before every execution gate and before
# anything hashes the bytes.
mac_sign_binary(){
  local bin="$1" ts="--timestamp=none" out dr
  if [ ! -f "$bin" ]; then mac_fatal "no binary to sign at $bin"; return 1; fi
  case "$MAC_SIGN_MODE" in
    skip)
      mac_warn "OAM_SKIP_MAC_SIGN=1 -- $bin left as the linker signed it"
      return 0 ;;
    adhoc)
      if ! out="$(codesign --force --sign - --options runtime --timestamp=none \
            --identifier "$MAC_CODESIGN_IDENTIFIER" --entitlements "$MAC_ENTITLEMENTS" "$bin" 2>&1 </dev/null)"; then
        mac_fatal "ad-hoc codesign of $bin failed:"; printf '%s\n' "$out" | sed 's/^/    /' >&2
        return 1
      fi
      mac_warn "$bin signed AD-HOC (bootstrap: no pinned identity yet)" ;;
    identity)
      # Unlocked and re-probed right here, not trusted from setup: the build
      # between setup and this line is long enough for the keychain to relock.
      mac_unlock_keychain || return 1
      if ! mac_codesign_identity_usable "$MAC_SIGN_IDENTITY" "$MAC_SIGN_KEYCHAIN"; then
        mac_report_probe_failure "the pinned identity $MAC_SIGN_IDENTITY"
        return 1
      fi
      # Self-signed: the designated requirement spelled out (see the header),
      # in the same pass -- a Mach-O is one signature, nothing nested.
      # Developer ID: a secure timestamp, and the requirement codesign derives.
      dr="designated => identifier \"$MAC_CODESIGN_IDENTIFIER\" and certificate leaf = H\"$MAC_SIGN_IDENTITY\""
      case "$MAC_SIGN_AUTHORITY" in
        "Developer ID Application:"*) ts="--timestamp"; dr="" ;;
      esac
      if ! out="$(codesign --force --sign "$MAC_SIGN_IDENTITY" --keychain "$MAC_SIGN_KEYCHAIN" \
            --options runtime "$ts" --identifier "$MAC_CODESIGN_IDENTIFIER" \
            ${dr:+"-r=$dr"} --entitlements "$MAC_ENTITLEMENTS" "$bin" 2>&1 </dev/null)"; then
        mac_fatal "codesign of $bin with $MAC_SIGN_IDENTITY failed:"; printf '%s\n' "$out" | sed 's/^/    /' >&2
        case "$out" in *errSecInternalComponent*) mac_keychain_remediation ;; esac
        return 1
      fi
      mac_say "$bin signed with $MAC_SIGN_IDENTITY" ;;
    *)
      mac_fatal "mac_sign_binary called with no signing mode -- run mac_signing_setup first"
      return 1 ;;
  esac
}

# mac_entitlement_pairs -- read a plist on stdin, print "<key> <value>" per
# top-level entitlement, sorted, where <value> is "true" for <true/> and
# "other" for anything else. XML comments are dropped first, so a comment that
# names a key does not count as one. Enough XML for an entitlements plist
# (flat dict, boolean values); not a general plist parser.
mac_entitlement_pairs(){
  tr '\r\n\t' '   ' | awk '{
    s = $0
    while ((i = index(s, "<!--")) > 0) {
      rest = substr(s, i + 4); j = index(rest, "-->")
      if (j == 0) { s = substr(s, 1, i - 1); break }
      s = substr(s, 1, i - 1) substr(rest, j + 3)
    }
    gsub(/[ ]+/, "", s)
    n = split(s, parts, "<key>")
    for (k = 2; k <= n; k++) {
      e = index(parts[k], "</key>"); if (e == 0) continue
      key = substr(parts[k], 1, e - 1); val = substr(parts[k], e + 6)
      print key, (substr(val, 1, 7) == "<true/>" ? "true" : "other")
    }
  }' | LC_ALL=C sort
}

# mac_verify_binary <binary> -- the gate between signing and every later use:
#   1. codesign --verify --strict
#   2. Identifier is org.oamjs.oam, and the CodeDirectory carries the hardened
#      runtime flag
#   3. the embedded entitlements are EXACTLY the committed file's keys, all true
#   4. the signer is the one the pin says: ad-hoc while no pin is committed;
#      with a pin, a designated requirement naming `certificate leaf =
#      H"<pin>"` (self-signed) or `anchor apple generic` plus a Timestamp
#      (Developer ID), AND on both, the extracted leaf certificate's own SHA-1
#      equal to the pin
# Under OAM_SKIP_MAC_SIGN=1 there is nothing of ours to verify: warn, return 0.
mac_verify_binary(){
  local bin="$1" out dv flags want got pin dr dr_lc auth leaf
  if [ "$MAC_SIGN_MODE" = "skip" ]; then
    mac_warn "OAM_SKIP_MAC_SIGN=1 -- signature checks skipped for $bin"
    return 0
  fi
  if [ ! -f "$bin" ]; then mac_fatal "no binary to verify at $bin"; return 1; fi

  if ! out="$(codesign --verify --strict --verbose=2 "$bin" 2>&1 </dev/null)"; then
    mac_fatal "the signature on $bin does not verify. codesign --verify --strict said:"
    printf '%s\n' "$out" | sed 's/^/    /' >&2
    return 1
  fi

  dv="$(codesign -dv --verbose=2 "$bin" 2>&1 </dev/null)"
  if [ "$(printf '%s\n' "$dv" | sed -n 's/^Identifier=//p' | head -n 1)" != "$MAC_CODESIGN_IDENTIFIER" ]; then
    mac_fatal "$bin is not signed as $MAC_CODESIGN_IDENTIFIER:"
    printf '%s\n' "$dv" | sed 's/^/    /' >&2
    return 1
  fi
  flags="$(printf '%s\n' "$dv" | sed -n 's/^CodeDirectory .*flags=\(0x[0-9a-fA-F]*([^)]*)\).*/\1/p' | head -n 1)"
  case "$flags" in
    *runtime*) ;;
    *) mac_fatal "$bin has no hardened runtime flag (CodeDirectory flags: '${flags:-<none>}')"; return 1 ;;
  esac

  want="$(mac_entitlement_pairs < "$MAC_ENTITLEMENTS")"
  got="$(codesign -d --entitlements - --xml "$bin" 2>/dev/null </dev/null | mac_entitlement_pairs)"
  if [ -z "$want" ] || [ "$got" != "$want" ]; then
    mac_fatal "$bin carries the wrong entitlements. Want (from $MAC_ENTITLEMENTS):"
    printf '%s\n' "${want:-<none>}" | sed 's/^/    /' >&2
    echo "  got:" >&2
    printf '%s\n' "${got:-<none>}" | sed 's/^/    /' >&2
    return 1
  fi
  case "$want" in
    *" other"*) mac_fatal "$MAC_ENTITLEMENTS holds a non-true entitlement"; return 1 ;;
  esac

  pin="$(mac_pinned_sha1)" || return 1
  # codesign prints an explicit requirement as `designated => ...` and a derived
  # one (every ad-hoc signature) as `# designated => ...`; accept both. Only the
  # hex digits are lowercased for the pin match.
  dr="$(codesign -d -r- "$bin" 2>&1 </dev/null | sed -En 's/^(# )?designated => //p' | head -n 1)"
  dr_lc="$(printf '%s' "$dr" | tr 'A-F' 'a-f')"
  if [ -z "$pin" ]; then
    if ! printf '%s\n' "$dv" | grep -qx 'Signature=adhoc'; then
      mac_fatal "no pin is committed, so $bin should be signed ad-hoc, but codesign does not say Signature=adhoc"
      return 1
    fi
    mac_warn "$bin verifies, AD-HOC (requirement: ${dr:-<none>})"
    return 0
  fi
  auth="$(printf '%s\n' "$dv" | sed -n 's/^Authority=//p' | head -n 1)"
  case "$auth" in
    "Developer ID Application:"*)
      case "$dr" in
        *"identifier \"$MAC_CODESIGN_IDENTIFIER\""*"anchor apple generic"*) ;;
        *) mac_fatal "$bin is Developer ID signed but its designated requirement is not the Developer ID shape: ${dr:-<none>}"; return 1 ;;
      esac
      if ! printf '%s\n' "$dv" | grep -q '^Timestamp='; then
        mac_fatal "$bin is Developer ID signed without a secure timestamp (notarization rejects that)"
        return 1
      fi ;;
    *)
      case "$dr_lc" in
        *cdhash*)
          mac_fatal "$bin is ad-hoc signed (cdhash requirement), but the repo pins $pin"
          return 1 ;;
        # The H of H"..." is outside A-F, so lowercasing left it uppercase.
        *"certificate leaf = H\"$pin\""*) ;;
        *)
          mac_fatal "the designated requirement of $bin does not name the pinned certificate $pin: ${dr:-<none>}"
          return 1 ;;
      esac
      case "$dr" in
        *"identifier \"$MAC_CODESIGN_IDENTIFIER\""*) ;;
        *) mac_fatal "the designated requirement of $bin does not name $MAC_CODESIGN_IDENTIFIER: $dr"; return 1 ;;
      esac ;;
  esac
  # On BOTH certificate paths, the certificate that actually signed is the
  # pinned one. A Developer ID requirement names only Apple's anchor and a
  # team, which any certificate of that team (or, in a weaker requirement, any
  # team) satisfies; this ties the signature to the pin itself.
  leaf="$(mac_leaf_sha1 "$bin")"
  if [ "$leaf" != "$pin" ]; then
    mac_fatal "$bin is signed by certificate ${leaf:-<none extracted>}, not the pinned $pin ($MAC_SIGNING_PIN_FILE)"
    return 1
  fi
  mac_say "$bin verifies: $auth (leaf $leaf), requirement $dr"
  return 0
}

# mac_leaf_sha1 <binary> -- the SHA-1 of the certificate that signed it (the
# leaf, which codesign extracts as <prefix>0, DER), lowercase hex; nothing when
# there is none (ad-hoc). The same fingerprint `security find-identity` and a
# designated requirement's H"..." use.
#
# When no certificate comes out, codesign's exit status and its own output go
# to stderr: the caller's "<none extracted>" alone gives nothing to diagnose.
# The work runs in a subshell so its traps are its own: the temp dir (public
# certificate DER only) is removed on an interrupt too, and the caller's traps
# -- build-remote.sh's, the probe's -- are neither replaced nor reset.
mac_leaf_sha1(){
  (
    d=""
    trap '[ -z "$d" ] || rm -rf "$d"' EXIT
    trap 'exit 130' INT TERM HUP
    d="$(mktemp -d "${TMPDIR:-/tmp}/oam-sign-leaf.XXXXXX")" \
      || { d=""; mac_warn "mktemp failed -- cannot extract the certificate of $1"; exit 0; }
    rc=0
    codesign -d --extract-certificates="$d/cert" "$1" >"$d/out" 2>&1 </dev/null || rc=$?
    if [ -s "$d/cert0" ]; then
      mac_sha1 "$d/cert0"
    else
      mac_warn "codesign extracted no certificate from $1 (exit $rc):"
      if [ -s "$d/out" ]; then sed 's/^/    /' "$d/out" >&2; else echo "    <codesign printed nothing>" >&2; fi
    fi
  )
}

# mac_sha1 <file> -- the hex SHA-1 alone, as mac_sha256 does it.
mac_sha1(){
  if command -v sha1sum >/dev/null 2>&1; then
    sha1sum "$1" | awk '{ print tolower($1) }'
  else
    shasum -a 1 "$1" | awk '{ print tolower($1) }'
  fi
}

# mac_sha256 <file> -- the hex digest alone, from whichever tool this host has
# (sha256sum on the release box, shasum on the Mac).
mac_sha256(){
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print tolower($1) }'
  else
    shasum -a 256 "$1" | awk '{ print tolower($1) }'
  fi
}

# mac_handback_check <hash-file> <dir> <name>... -- the release box's half of
# the hash hand-back. The Mac writes dist/mac-sha256.txt over the bytes it
# signed and gated; this compares every pulled binary against it. The file
# must list EXACTLY <name>..., so a binary that was swapped, truncated or
# re-signed between the Mac's gate and the release box is caught before it is
# checksummed into SHA256SUMS. ssh stdout cannot carry the hashes: the leg's
# output goes to its log.
mac_handback_check(){
  local hf="$1" dir="$2" n want have listed bad=""
  shift 2
  if [ ! -s "$hf" ]; then mac_fatal "no hash hand-back at $hf"; return 1; fi
  listed="$(awk 'NF { n = $2; sub(/^\*/, "", n); print n }' "$hf" | LC_ALL=C sort)"
  if [ "$listed" != "$(printf '%s\n' "$@" | LC_ALL=C sort)" ]; then
    mac_fatal "the Mac's hand-back lists '$(printf '%s' "$listed" | tr '\n' ' ')' but this run expects '$*'"
    return 1
  fi
  for n in "$@"; do
    want="$(awk -v n="$n" '{ m = $2; sub(/^\*/, "", m) } m == n { print tolower($1); exit }' "$hf")"
    if [ ! -f "$dir/$n" ]; then bad="$bad $n(not pulled)"; continue; fi
    have="$(mac_sha256 "$dir/$n")"
    if [ -z "$want" ] || [ "$want" != "$have" ]; then
      bad="$bad $n(mac=${want:-none} here=$have)"
    fi
  done
  if [ -n "$bad" ]; then
    mac_fatal "pulled mac binaries differ from what the Mac signed and gated:$bad"
    return 1
  fi
  return 0
}
