#!/bin/bash
# =============================================================================
# Release signing: a tag-bound RELEASE-MANIFEST, signed with an ssh ed25519 key
# =============================================================================
# SHA256SUMS proves a download matches the release it came from. It cannot
# prove the release came from us: whoever can upload to the release (a stolen
# gh token, a compromised box) uploads a matching SHA256SUMS beside the binary.
# So every release also carries
#
#   RELEASE-MANIFEST        "oam-release-manifest v1\n" + "tag <tag>\n" + the
#                           SHA256SUMS bytes, verbatim
#   RELEASE-MANIFEST.sig    an SSHSIG over it (ssh-keygen -Y sign, namespace
#                           "oam-release"), made with a dedicated release key
#
# The tag line is the point of the extra file. A signature over SHA256SUMS
# alone is valid forever for whatever it covers, so an attacker could serve an
# OLD, correctly signed release as the latest one (a downgrade to a known-bad
# version) or replay it under another tag. Binding the tag makes the verifier's
# question "is this the release I asked for?", not just "did we ever sign
# this?". SHA256SUMS itself is untouched, so every existing consumer keeps
# working; the manifest's name sits outside the `oam-*` globs that pick
# binaries, and it is never listed IN SHA256SUMS (that file covers binaries).
#
# Trust is by TAG RANGE, not by time. release-keys/allowed_signers names the
# keys (public material only), and release-keys/ranges says which tags each may
# sign ("k1 v0.18.0 -": from v0.18.0 inclusive, open-ended). ssh-keygen's own
# valid-before= option would be checked against the VERIFY time, so retiring a
# key would break every pinned old tag it signed; a tag range retires a key
# for new releases only. release-keys/README.md is the runbook.
#
# Custody: the private half lives at $OAM_RELEASE_SIGNING_KEY, passphrase
# protected. release_agent_start loads it into a PRIVATE ssh-agent -- its own
# process, its own 0700 socket directory, a 6-hour key lifetime -- with one
# passphrase prompt, and release_agent_stop kills it. SSH_AUTH_SOCK is NEVER
# exported: every ssh-add / ssh-keygen call that needs the agent gets it as a
# per-command prefix. Exporting it would hand the release key to every child of
# the release -- including the ssh sessions the remote build legs open to the
# Mac and the GCP VM, which would then offer it to (and, with agent forwarding,
# lend it to) hosts that have no business seeing it.
#
# Bootstrap: until a key is committed to allowed_signers there is nothing to
# sign with, and the manifest step is skipped with a loud warning (fatal under
# OAM_SIGN_REQUIRED=1). The moment ANY key is committed, signing is mandatory:
# there is deliberately no knob that skips it, because a skip knob is exactly
# what an attacker holding the release box would set.
#
# Contract for callers (release-local.sh, release-upload-local-arm64.sh,
# test-scripts.sh):
#   - functions print a precise "[fail]" line on stderr and RETURN non-zero;
#     they never exit. The caller decides what a failure ends.
#   - nothing here sets a trap. The caller owns its ONE EXIT trap (a second
#     `trap ... EXIT` silently replaces the first -- see ci-local.sh) and must
#     call release_agent_stop from it.
#   - call release_agent_start in the shell that will sign, never inside
#     $(...): the agent's pid and socket live in shell variables, and a
#     subshell's copies die with it, leaving the caller with no agent to sign
#     through -- and an agent nobody stops. (The agent's own stdio goes to
#     /dev/null, so a substitution would not hang; it would just be wrong.)
#   - callers run under `set -e`, which `release_x || fail` disables inside the
#     function, so every step here checks its own status explicitly.
# =============================================================================

RELEASE_SIGN_NAMESPACE="oam-release"
RELEASE_MANIFEST_HEADER="oam-release-manifest v1"
# Principals in allowed_signers are "oam-release-<id>"; ranges names the <id>.
RELEASE_PRINCIPAL_PREFIX="oam-release-"
# 6 hours: one release, with headroom. If a long run outlives it anyway,
# release_sign_manifest re-adds the key (one more prompt) instead of failing
# after the builds are done.
RELEASE_KEY_LIFETIME=21600

# Resolved from this file's own location, NOT from the environment: the trust
# root a release verifies against is the committed one, and an env knob that
# could point it elsewhere is a knob that could make a bad key verify. Two
# things reassign it after sourcing: release-upload-local-arm64.sh, which
# patches an OLD tag's release and points it at origin/main's copy instead
# (release_keys_from_commit -- the current trust root, not the one frozen into
# that tag, nor a local checkout that may lag origin), and the test suite, in
# a subshell.
RELEASE_KEYS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/release-keys"
# The Windows inbox OpenSSH client's ssh-add, used only to ask the agent
# SERVICE whether it holds the release key, and reg.exe, used to look in that
# service's at-rest key store. Both reassigned by the test suite.
RELEASE_INBOX_SSH_ADD="/c/Windows/System32/OpenSSH/ssh-add.exe"
RELEASE_INBOX_REG="/c/Windows/System32/reg.exe"
# Where Win32-OpenSSH's agent service keeps added keys (DPAPI-encrypted), one
# subkey per key, with the public key blob in a "pub" REG_BINARY value.
RELEASE_INBOX_REG_KEY='HKCU\Software\OpenSSH\Agent\Keys'
# Longest agent socket path release_agent_start will use. sun_path is 104
# bytes on macOS and 108 on Linux, NUL included; ssh-agent refuses a longer
# path ("too long for Unix domain socket") and exits. macOS's per-user
# $TMPDIR (/var/folders/xx/<30 chars>/T/) leaves little room under it.
RELEASE_SOCK_MAX=100

RELEASE_SSH_KEYGEN=""
RELEASE_SSH_ADD=""
RELEASE_SSH_AGENT=""
RELEASE_AGENT_PID=""
RELEASE_AGENT_DIR=""
RELEASE_AGENT_SOCK=""
RELEASE_SIGNING_KEY=""
RELEASE_SIGNING_FP=""

_rs_fail(){ printf '  [fail] %s\n' "$*" >&2; return 1; }
_rs_warn(){ printf '  [warn] %s\n' "$*" >&2; return 0; }
_rs_ok(){   printf '  [ok] %s\n' "$*" >&2; return 0; }

# release_ssh_tools -- resolve ssh-keygen, ssh-add and ssh-agent ONCE, from
# PATH, and insist they come from the same directory.
#
# Same directory, because the halves must speak the same agent protocol over
# the same kind of socket: a Git Bash (MSYS) ssh-agent listens on an MSYS unix
# socket that the Windows inbox ssh-add.exe cannot open, and the inbox agent is
# a named-pipe service. A PATH that mixes the two installs fails with "could
# not connect to agent" at best, and at worst talks to an agent other than the
# one this lib started. On this repo's release box that directory is Git for
# Windows' /usr/bin.
#
# -Y: sign/verify arrived in OpenSSH 8.1; the floor here is 8.2 (plan 4.3).
# The version comes from the sibling `ssh -V`, since ssh-keygen has no version
# flag, and the functional half -- does this ssh-keygen parse -Y at all -- is
# asked of ssh-keygen itself: given an operation it does not know, one with -Y
# answers "Unsupported operation for -Y" (measured on 10.2p1), while one
# without -Y rejects the option before any operation is looked at.
release_ssh_tools() {
  [ -z "$RELEASE_SSH_KEYGEN" ] || return 0
  local kg add agent dir ver major minor probe
  kg="$(command -v ssh-keygen 2>/dev/null)" || kg=""
  add="$(command -v ssh-add 2>/dev/null)" || add=""
  agent="$(command -v ssh-agent 2>/dev/null)" || agent=""
  [ -n "$kg" ] && [ -n "$add" ] && [ -n "$agent" ] \
    || _rs_fail "ssh-keygen, ssh-add and ssh-agent must all be on PATH (found: ${kg:-no ssh-keygen}, ${add:-no ssh-add}, ${agent:-no ssh-agent}) -- install OpenSSH >= 8.2 (on Windows: Git for Windows ships it in /usr/bin)" || return 1
  dir="$(dirname "$kg")"
  [ "$(dirname "$add")" = "$dir" ] && [ "$(dirname "$agent")" = "$dir" ] \
    || _rs_fail "ssh-keygen ($kg), ssh-add ($add) and ssh-agent ($agent) come from different OpenSSH installs -- they cannot share an agent socket. Put one install first on PATH (Git for Windows: /usr/bin)" || return 1
  if [ -x "$dir/ssh" ] || [ -x "$dir/ssh.exe" ]; then
    ver="$("$dir/ssh" -V 2>&1)"
    if [[ "$ver" =~ OpenSSH_([0-9]+)\.([0-9]+) ]]; then
      major="${BASH_REMATCH[1]}"; minor="${BASH_REMATCH[2]}"
      if [ "$major" -lt 8 ] || { [ "$major" -eq 8 ] && [ "$minor" -lt 2 ]; }; then
        _rs_fail "$dir/ssh-keygen is OpenSSH $major.$minor ('$ver') -- release signing needs ssh-keygen -Y from OpenSSH >= 8.2"
        return 1
      fi
    else
      _rs_warn "could not read an OpenSSH version from '$dir/ssh -V' ('$ver') -- relying on the -Y probe alone"
    fi
  fi
  probe="$("$kg" -Y oam-probe </dev/null 2>&1)"
  case "$probe" in
    *'Unsupported operation for -Y'*) ;;
    *) _rs_fail "$kg does not support -Y (OpenSSH >= 8.2 needed); it said: $(printf '%s' "$probe" | head -1)"; return 1 ;;
  esac
  RELEASE_SSH_KEYGEN="$kg"; RELEASE_SSH_ADD="$add"; RELEASE_SSH_AGENT="$agent"
  return 0
}

# --- release-keys/ parsing ----------------------------------------------------

# release_key_lines -- the non-comment, non-blank lines of allowed_signers.
release_key_lines() {
  awk '!/^[[:space:]]*(#|$)/' "$RELEASE_KEYS_DIR/allowed_signers"
}

# _rs_plain_tag <tag> -- 0 for a plain vMAJOR.MINOR.PATCH, the only tag shape
# this repo releases (release-local.sh refuses the rest).
_rs_plain_tag() { [[ "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; }

# _rs_tag_le <a> <b> -- 0 when tag a <= tag b, both plain. Numeric per field
# (10# so a leading zero is not read as octal); a string compare would put
# v0.10.0 before v0.9.0.
_rs_tag_le() {
  local a b i
  IFS=. read -r -a a <<<"${1#v}"
  IFS=. read -r -a b <<<"${2#v}"
  for i in 0 1 2; do
    if [ $((10#${a[i]})) -lt $((10#${b[i]})) ]; then return 0; fi
    if [ $((10#${a[i]})) -gt $((10#${b[i]})) ]; then return 1; fi
  done
  return 0
}

# release_keys_lint -- the committed key files are well-formed. Checked before
# anything trusts them, because both are hand-edited and a malformed line must
# not quietly degrade into "this key is trusted for everything" or "nothing
# verifies, skip". Rules:
#   allowed_signers  principal oam-release-<id> (one, no patterns/commas),
#                    namespaces="oam-release" exactly, an ed25519 key type,
#                    a base64 blob; each principal once.
#   ranges           "<id> <from> <to>": plain tags, "-" for an open end,
#                    from <= to; one line per id.
# A key with NO range line is legal: that is the staged "next" key (k2),
# committed early so its public half travels ahead of the rotation. It signs
# nothing until a range opens for it; release_tag_in_range says so by name.
release_keys_lint() {
  local as="$RELEASE_KEYS_DIR/allowed_signers" rg="$RELEASE_KEYS_DIR/ranges"
  [ -f "$as" ] || { _rs_fail "$as is missing -- it is committed; restore it from git"; return 1; }
  [ -f "$rg" ] || { _rs_fail "$rg is missing -- it is committed; restore it from git"; return 1; }
  local line principal opts ktype blob extra id from to seen_p="" seen_r=""
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    read -r principal opts ktype blob extra <<<"$line"
    case "$principal" in
      "$RELEASE_PRINCIPAL_PREFIX"*) ;;
      *) _rs_fail "allowed_signers: principal '$principal' does not start with '$RELEASE_PRINCIPAL_PREFIX'"; return 1 ;;
    esac
    id="${principal#"$RELEASE_PRINCIPAL_PREFIX"}"
    [[ "$id" =~ ^[A-Za-z0-9._-]+$ ]] \
      || { _rs_fail "allowed_signers: principal '$principal' must be a single literal name (no patterns or lists)"; return 1; }
    [ "$opts" = "namespaces=\"$RELEASE_SIGN_NAMESPACE\"" ] \
      || { _rs_fail "allowed_signers: '$principal' must carry namespaces=\"$RELEASE_SIGN_NAMESPACE\" as its only option (got '$opts')"; return 1; }
    case "$ktype" in
      ssh-ed25519 | sk-ssh-ed25519@openssh.com) ;;
      *) _rs_fail "allowed_signers: '$principal' has key type '$ktype' -- release keys are ssh-ed25519 (or sk-ssh-ed25519@openssh.com)"; return 1 ;;
    esac
    [[ "$blob" =~ ^[A-Za-z0-9+/]+=*$ ]] \
      || { _rs_fail "allowed_signers: '$principal' has no base64 key blob"; return 1; }
    # $extra is the key comment, if any; ssh-keygen ignores it.
    case " $seen_p " in *" $id "*) _rs_fail "allowed_signers: principal '$principal' appears twice"; return 1 ;; esac
    seen_p="$seen_p $id"
  done <<<"$(release_key_lines)"
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    read -r id from to extra <<<"$line"
    [ -n "$to" ] && [ -z "$extra" ] \
      || { _rs_fail "ranges: '$line' must be exactly '<id> <from-tag> <to-tag|->'"; return 1; }
    _rs_plain_tag "$from" || { _rs_fail "ranges: '$id' starts at '$from', not a plain vX.Y.Z tag"; return 1; }
    if [ "$to" != "-" ]; then
      _rs_plain_tag "$to" || { _rs_fail "ranges: '$id' ends at '$to', neither '-' nor a plain vX.Y.Z tag"; return 1; }
      _rs_tag_le "$from" "$to" || { _rs_fail "ranges: '$id' runs backwards ($from > $to)"; return 1; }
    fi
    case " $seen_r " in *" $id "*) _rs_fail "ranges: '$id' has more than one line -- one range per key, or a forgotten open line keeps a closed key alive"; return 1 ;; esac
    seen_r="$seen_r $id"
  done <<<"$(awk '!/^[[:space:]]*(#|$)/' "$rg")"
  return 0
}

# release_tag_in_range <principal> <tag> -- 0 when ranges lets <principal>
# sign <tag>. Assumes release_keys_lint passed.
release_tag_in_range() {
  local principal="$1" tag="$2" id from to
  id="${principal#"$RELEASE_PRINCIPAL_PREFIX"}"
  _rs_plain_tag "$tag" || { _rs_fail "'$tag' is not a plain vX.Y.Z tag -- key ranges cannot place it"; return 1; }
  read -r _ from to <<<"$(awk -v id="$id" '!/^[[:space:]]*(#|$)/ && $1 == id { print; exit }' "$RELEASE_KEYS_DIR/ranges")"
  [ -n "${from:-}" ] || { _rs_fail "key $principal has no line in release-keys/ranges -- a staged next key signs nothing until its range opens"; return 1; }
  if ! _rs_tag_le "$from" "$tag"; then
    _rs_fail "key $principal may sign $from onward (release-keys/ranges), and $tag is before that"
    return 1
  fi
  if [ "$to" != "-" ] && ! _rs_tag_le "$tag" "$to"; then
    _rs_fail "key $principal was retired after $to (release-keys/ranges) -- it may not sign $tag"
    return 1
  fi
  return 0
}

# release_tag_predates_signing <tag> -- 0 when <tag> comes before the start of
# every range in release-keys/ranges: no committed key was ever allowed to
# sign it, so a release of it without a manifest was cut before signing
# existed. 1 for a tag at or after any range's start: that release was cut in
# the signing era, and a missing manifest means it was removed or never made.
# A closed range counts -- a tag in a gap between ranges is still after
# signing began. Assumes release_keys_lint passed.
release_tag_predates_signing() {
  local tag="$1" from
  _rs_plain_tag "$tag" || { _rs_fail "'$tag' is not a plain vX.Y.Z tag -- key ranges cannot place it"; return 1; }
  while read -r _ from _; do
    [ -n "$from" ] || continue
    if _rs_tag_le "$from" "$tag"; then return 1; fi
  done <<<"$(awk '!/^[[:space:]]*(#|$)/' "$RELEASE_KEYS_DIR/ranges")"
  return 0
}

# release_keys_from_commit <commit> <dir> -- copy release-keys/allowed_signers
# and ranges as committed at <commit> into <dir> (which the caller owns and
# removes) and point RELEASE_KEYS_DIR at it. For a caller whose checkout is not
# the current trust root: release-upload-local-arm64.sh patches an old tag,
# whose release-keys/ predates any later range close or rotation, and
# verifying against that would accept a key the project has since retired --
# or refuse the key that replaced it. Run in the caller's shell, never in
# $(...): the assignment is the point.
release_keys_from_commit() {
  local commit="$1" dir="$2" f
  git cat-file -e "${commit}^{commit}" 2>/dev/null \
    || { _rs_fail "commit $commit is not in this clone -- fetch it first"; return 1; }
  for f in allowed_signers ranges; do
    git show "${commit}:release-keys/$f" >"$dir/$f" 2>/dev/null \
      || { _rs_fail "release-keys/$f does not exist at $commit -- that commit carries no signing trust root"; return 1; }
  done
  RELEASE_KEYS_DIR="$dir"
  return 0
}

# release_signing_decision -- whether this run signs, as one line on stdout:
#   sign            a key is committed: signing is mandatory
#   skip:<reason>   bootstrap (no key yet) and OAM_SIGN_REQUIRED is not 1
#   fail:<reason>   bootstrap under OAM_SIGN_REQUIRED=1, a malformed knob, or
#                   malformed key files
# Pure function of the committed files and the knob, so the suite drives it
# with fixtures. Note what is NOT an input: anything that could turn "sign"
# into "skip" once a key exists.
release_signing_decision() {
  local req="${OAM_SIGN_REQUIRED:-0}" lint_err
  [[ "$req" =~ ^[01]$ ]] || { printf 'fail:OAM_SIGN_REQUIRED must be 0 or 1, not %s\n' "$req"; return 0; }
  if ! lint_err="$(release_keys_lint 2>&1)"; then
    printf 'fail:release-keys/ is malformed: %s\n' "$(printf '%s' "$lint_err" | sed 's/^ *\[fail\] //' | head -1)"
    return 0
  fi
  if [ -n "$(release_key_lines)" ]; then
    printf 'sign\n'
  elif [ "$req" = "1" ]; then
    printf 'fail:OAM_SIGN_REQUIRED=1 but release-keys/allowed_signers holds no key yet -- generate and commit one (release-keys/README.md), or unset OAM_SIGN_REQUIRED for a bootstrap release\n'
  else
    printf 'skip:release-keys/allowed_signers holds no key yet, so this release ships WITHOUT a signed RELEASE-MANIFEST (bootstrap; see release-keys/README.md). Once a key is committed this step is mandatory\n'
  fi
  return 0
}

# --- the private agent ----------------------------------------------------------

# release_inbox_agent_holds <fingerprint> <public-key-base64> -- 0 when the
# Windows OpenSSH agent SERVICE holds the key, live or at rest. That service
# keeps added keys in the registry ($RELEASE_INBOX_REG_KEY), DPAPI-encrypted,
# across reboots and while it is STOPPED -- decryptable by anything running as
# this user, for good: the opposite of a 6-hour private agent. So two looks:
#   1. the running service's listing (ssh-add.exe -l). SSH_AUTH_SOCK is
#      cleared for the call: the inbox client honors it, and pointed at an
#      MSYS socket it would ask the wrong agent.
#   2. the at-rest store, which answers whether or not the service runs: a
#      value holding this key's public blob. reg.exe prints a REG_BINARY as
#      one hex string, so the blob is matched as hex.
# Neither binary present (not Windows) reads as "not held", and so does no
# store at all. A STOPPED service whose store holds keys that look 2 cannot
# match is warned about rather than silently passed: the check could not ask
# the service itself, and the line says so.
release_inbox_agent_holds() {
  local fp="$1" blob="$2" listing rc=0 store hex n
  if [ -x "$RELEASE_INBOX_SSH_ADD" ]; then
    listing="$( unset SSH_AUTH_SOCK; "$RELEASE_INBOX_SSH_ADD" -l 2>/dev/null )" || rc=$?
    if awk -v fp="$fp" '$2 == fp { found = 1 } END { exit !found }' <<<"$listing"; then
      return 0
    fi
  fi
  [ -x "$RELEASE_INBOX_REG" ] || return 1
  # MSYS2_ARG_CONV_EXCL: Git Bash would otherwise rewrite "/s" into a path.
  store="$(MSYS2_ARG_CONV_EXCL='*' "$RELEASE_INBOX_REG" query "$RELEASE_INBOX_REG_KEY" /s 2>/dev/null)" \
    || return 1
  hex="$(printf '%s' "$blob" | base64 -d 2>/dev/null | od -An -v -tx1 | tr -d ' \n' | tr 'a-f' 'A-F')"
  if [ -z "$hex" ]; then
    _rs_warn "could not decode the release key's public blob, so the Windows agent's key store ($RELEASE_INBOX_REG_KEY) was not searched"
    return 1
  fi
  if tr -d ' \t\r\n' <<<"$store" | tr 'a-f' 'A-F' | grep -qF -- "$hex"; then
    return 0
  fi
  if [ "$rc" -eq 2 ]; then
    # Subkey header lines: the store's own path plus "\<name>".
    n="$(tr -d '\r' <<<"$store" | grep -ciE '^HKEY_CURRENT_USER.+agent.keys.[^[:space:]]')"
    if [ "${n:-0}" -gt 0 ]; then
      _rs_warn "the Windows OpenSSH agent service is not running, and its key store ($RELEASE_INBOX_REG_KEY) holds $n key(s) this check did not match to the release key ($fp). If the release key was ever added there, start the service and remove it (ssh-add.exe -d '<key>.pub')"
    fi
  fi
  return 1
}

# release_agent_holds_key -- 0 when the private agent holds the release key.
release_agent_holds_key() {
  [ -n "$RELEASE_AGENT_SOCK" ] || return 1
  SSH_AUTH_SOCK="$RELEASE_AGENT_SOCK" "$RELEASE_SSH_ADD" -l 2>/dev/null \
    | awk -v fp="$RELEASE_SIGNING_FP" '$2 == fp { found = 1 } END { exit !found }'
}

# _rs_add_key -- one ssh-add with the lifetime; the passphrase prompt is
# ssh-add's own (it reads the terminal, so stdin is left alone).
_rs_add_key() {
  _rs_warn "loading the release key ($RELEASE_SIGNING_FP) into a private agent for $((RELEASE_KEY_LIFETIME / 3600))h -- enter its passphrase if asked"
  SSH_AUTH_SOCK="$RELEASE_AGENT_SOCK" "$RELEASE_SSH_ADD" -t "$RELEASE_KEY_LIFETIME" "$RELEASE_SIGNING_KEY" >&2 \
    || { _rs_fail "ssh-add could not load $RELEASE_SIGNING_KEY (wrong passphrase, or not a private key)"; return 1; }
  release_agent_holds_key \
    || { _rs_fail "ssh-add reported success but the private agent does not list $RELEASE_SIGNING_FP"; return 1; }
  return 0
}

# release_agent_start -- validate $OAM_RELEASE_SIGNING_KEY, start the private
# agent and add the key once. Idempotent while the agent lives.
#
# `ssh-agent -D -a <sock> &`, not `eval "$(ssh-agent -s)"`: -D keeps it in the
# foreground of a background job, so $! IS the agent and stopping it is a
# kill, not a parse of its output, and no eval of anything an external
# program printed; -a puts the socket in a directory this function created
# 0700.
release_agent_start() {
  release_ssh_tools || return 1
  if [ -n "$RELEASE_AGENT_PID" ] && kill -0 "$RELEASE_AGENT_PID" 2>/dev/null; then
    return 0
  fi
  # An agent that died under us still has a socket directory to clear.
  release_agent_stop
  local key="${OAM_RELEASE_SIGNING_KEY:-}" ktype kblob i rc base
  [ -n "$key" ] \
    || { _rs_fail "OAM_RELEASE_SIGNING_KEY is not set -- point it at the release key's PRIVATE half (passphrase-protected ed25519, '<path>.pub' beside it); release-keys/README.md has the runbook"; return 1; }
  # A Windows-style path from the environment works for the MSYS tools once
  # translated; cygpath exists only where that translation is needed.
  if command -v cygpath >/dev/null 2>&1; then key="$(cygpath -u "$key")"; fi
  [ -f "$key" ] || { _rs_fail "OAM_RELEASE_SIGNING_KEY=$key does not exist"; return 1; }
  [ -f "$key.pub" ] || { _rs_fail "$key.pub does not exist -- signing names the key by its public half (ssh-keygen -Y sign -f <key>.pub)"; return 1; }
  read -r ktype kblob _ <"$key.pub"
  case "$ktype" in
    ssh-ed25519 | sk-ssh-ed25519@openssh.com) ;;
    *) _rs_fail "$key.pub is a '$ktype' key -- release keys are ssh-ed25519"; return 1 ;;
  esac
  RELEASE_SIGNING_FP="$("$RELEASE_SSH_KEYGEN" -lf "$key.pub" 2>/dev/null | awk '{print $2; exit}')"
  [ -n "$RELEASE_SIGNING_FP" ] || { _rs_fail "could not fingerprint $key.pub"; return 1; }
  if release_inbox_agent_holds "$RELEASE_SIGNING_FP" "$kblob"; then
    _rs_fail "the Windows OpenSSH agent SERVICE holds the release key ($RELEASE_SIGNING_FP), live or in its registry store -- it keeps keys across reboots and while stopped. Remove it there (start the service, then '$RELEASE_INBOX_SSH_ADD' -d '$key.pub', or -D for all) and re-run; release signing uses only its own private agent"
    return 1
  fi
  # An unencrypted key on disk is the key itself, at rest. The runbook says
  # passphrase-protected; this warns rather than refuses because a refusal
  # would need a test-only bypass for the suite's throwaway keys, and a bypass
  # in the signing path is worse than a loud line in the release log.
  if [ "$ktype" = "ssh-ed25519" ] && "$RELEASE_SSH_KEYGEN" -y -P '' -f "$key" >/dev/null 2>&1; then
    _rs_warn "$key has NO passphrase -- anyone who reads that file can sign oam releases. Add one: ssh-keygen -p -f '$key'"
  fi
  RELEASE_SIGNING_KEY="$key"

  # $TMPDIR first (per-user on macOS), /tmp when the socket path would not
  # fit under it; 22 is "/oam-sign.XXXXXX/agent". /tmp is shared, but the
  # directory is 0700 either way. The final length is checked too, so a path
  # that still does not fit fails here, by name, rather than as an agent that
  # "did not come up".
  base="${TMPDIR:-/tmp}"
  if [ $(( ${#base} + 22 )) -gt "$RELEASE_SOCK_MAX" ]; then base=/tmp; fi
  RELEASE_AGENT_DIR="$(mktemp -d "$base/oam-sign.XXXXXX")" \
    || { _rs_fail "could not create the agent's socket directory"; return 1; }
  chmod 700 "$RELEASE_AGENT_DIR" || { _rs_fail "could not chmod 700 $RELEASE_AGENT_DIR"; release_agent_stop; return 1; }
  RELEASE_AGENT_SOCK="$RELEASE_AGENT_DIR/agent"
  if [ "${#RELEASE_AGENT_SOCK}" -gt "$RELEASE_SOCK_MAX" ]; then
    _rs_fail "the agent socket path $RELEASE_AGENT_SOCK is ${#RELEASE_AGENT_SOCK} bytes, over the $RELEASE_SOCK_MAX a unix socket path can safely hold -- set TMPDIR to a shorter directory"
    release_agent_stop
    return 1
  fi
  "$RELEASE_SSH_AGENT" -D -a "$RELEASE_AGENT_SOCK" </dev/null >/dev/null 2>&1 &
  RELEASE_AGENT_PID=$!
  # ssh-add -l: 0 = keys, 1 = reachable and empty, 2 = cannot connect. Poll
  # for "reachable", ~5s, and notice an agent that died instead of listening.
  rc=2
  for ((i = 1; i <= 50; i++)); do
    SSH_AUTH_SOCK="$RELEASE_AGENT_SOCK" "$RELEASE_SSH_ADD" -l >/dev/null 2>&1 && rc=0 || rc=$?
    [ "$rc" -eq 2 ] || break
    kill -0 "$RELEASE_AGENT_PID" 2>/dev/null || break
    sleep 0.1
  done
  if [ "$rc" -eq 2 ]; then
    _rs_fail "the private ssh-agent did not come up on $RELEASE_AGENT_SOCK (after $i polls)"
    release_agent_stop
    return 1
  fi
  _rs_add_key || { release_agent_stop; return 1; }
  _rs_ok "release key $RELEASE_SIGNING_FP held by a private agent (pid $RELEASE_AGENT_PID, ${RELEASE_KEY_LIFETIME}s lifetime)"
  return 0
}

# release_agent_stop -- kill the agent, remove its socket directory. Safe to
# call any number of times and from an EXIT trap; always returns 0.
#
# Residual, stated rather than hidden: a release killed so hard that its EXIT
# trap never runs (taskkill /F, a power cut) orphans the agent with the key in
# it until the lifetime runs out. The 0700 directory and that lifetime are the
# bound on it.
release_agent_stop() {
  if [ -n "$RELEASE_AGENT_PID" ]; then
    kill "$RELEASE_AGENT_PID" 2>/dev/null || true
    wait "$RELEASE_AGENT_PID" 2>/dev/null || true
  fi
  if [ -n "$RELEASE_AGENT_DIR" ]; then rm -rf "$RELEASE_AGENT_DIR"; fi
  RELEASE_AGENT_PID=""; RELEASE_AGENT_DIR=""; RELEASE_AGENT_SOCK=""
  return 0
}

# --- sign and verify ------------------------------------------------------------

# _rs_sign_file <file> -- <file>.sig through the private agent. A stale .sig
# goes first: ssh-keygen stops to ask before overwriting one.
_rs_sign_file() {
  local file="$1" out
  if [ -z "$RELEASE_AGENT_PID" ] || ! kill -0 "$RELEASE_AGENT_PID" 2>/dev/null; then
    _rs_fail "the private signing agent is not running (release_agent_start first)"
    return 1
  fi
  if ! release_agent_holds_key; then
    _rs_warn "the release key's ${RELEASE_KEY_LIFETIME}s agent lifetime ran out -- adding it again"
    _rs_add_key || return 1
  fi
  rm -f "$file.sig"
  out="$(SSH_AUTH_SOCK="$RELEASE_AGENT_SOCK" "$RELEASE_SSH_KEYGEN" -Y sign \
           -f "$RELEASE_SIGNING_KEY.pub" -n "$RELEASE_SIGN_NAMESPACE" "$file" </dev/null 2>&1)" \
    || { _rs_fail "ssh-keygen -Y sign failed for $file: $out"; return 1; }
  [ -s "$file.sig" ] || { _rs_fail "ssh-keygen -Y sign wrote no $file.sig ($out)"; return 1; }
  return 0
}

# _rs_verify_sig <file> <sig> -- try every committed principal with -I; print
# the one the signature verifies for. `-I` per principal rather than
# find-principals: it is the exact command a consumer runs, so a pass here is
# a pass there.
_rs_verify_sig() {
  local file="$1" sig="$2" principal out last=""
  while read -r principal _; do
    [ -n "$principal" ] || continue
    if out="$("$RELEASE_SSH_KEYGEN" -Y verify -f "$RELEASE_KEYS_DIR/allowed_signers" \
                -I "$principal" -n "$RELEASE_SIGN_NAMESPACE" -s "$sig" <"$file" 2>&1)"; then
      printf '%s\n' "$principal"
      return 0
    fi
    last="$out"
  done <<<"$(release_key_lines)"
  printf '%s\n' "${last:-no key in allowed_signers}" | tr '\n' ' ' >&2
  return 1
}

# release_signing_preflight <tag> -- prove, BEFORE anything is bumped or
# tagged, that this box can produce a signature a consumer will accept for
# <tag>: a real throwaway signature, in a temp dir OUTSIDE the repo (the
# dirty-tree check runs right after), verified against the committed
# allowed_signers, by a key whose range covers <tag>. Each of those is a way a
# release used to be able to get all the way to the manifest step and die
# there with the tag already public.
release_signing_preflight() {
  local tag="$1" d principal err
  _rs_plain_tag "$tag" || { _rs_fail "'$tag' is not a plain vX.Y.Z tag"; return 1; }
  release_keys_lint || return 1
  d="$(mktemp -d "${TMPDIR:-/tmp}/oam-sign-probe.XXXXXX")" || { _rs_fail "could not create a temp dir"; return 1; }
  printf 'oam release signing preflight for %s -- not a release manifest\n' "$tag" >"$d/probe"
  if ! _rs_sign_file "$d/probe"; then rm -rf "$d"; return 1; fi
  if ! principal="$(_rs_verify_sig "$d/probe" "$d/probe.sig" 2>"$d/err")"; then
    err="$(cat "$d/err")"; rm -rf "$d"
    _rs_fail "a fresh signature by OAM_RELEASE_SIGNING_KEY ($RELEASE_SIGNING_FP) does not verify against release-keys/allowed_signers -- it is not a committed release key ($err)"
    return 1
  fi
  rm -rf "$d"
  release_tag_in_range "$principal" "$tag" || return 1
  _rs_ok "signing preflight: $principal ($RELEASE_SIGNING_FP) signs and verifies, and may sign $tag"
  return 0
}

# release_write_manifest <dir> <tag> -- <dir>/RELEASE-MANIFEST from
# <dir>/SHA256SUMS. Any old .sig beside it goes too: it signs bytes that no
# longer exist.
release_write_manifest() {
  local dir="$1" tag="$2"
  _rs_plain_tag "$tag" || { _rs_fail "'$tag' is not a plain vX.Y.Z tag"; return 1; }
  [ -s "$dir/SHA256SUMS" ] || { _rs_fail "$dir/SHA256SUMS is missing or empty -- the manifest wraps it"; return 1; }
  # SHA256SUMS covers binaries. A manifest listed inside it would be a hash of
  # the thing that carries the hash -- and a sign the two got confused.
  if grep -q 'RELEASE-MANIFEST' "$dir/SHA256SUMS"; then
    _rs_fail "$dir/SHA256SUMS lists RELEASE-MANIFEST -- it must cover the oam-* binaries only"
    return 1
  fi
  rm -f "$dir/RELEASE-MANIFEST.sig"
  { printf '%s\n' "$RELEASE_MANIFEST_HEADER" && printf 'tag %s\n' "$tag" && cat "$dir/SHA256SUMS"; } \
      >"$dir/RELEASE-MANIFEST" \
    || { rm -f "$dir/RELEASE-MANIFEST"; _rs_fail "could not write $dir/RELEASE-MANIFEST"; return 1; }
  return 0
}

# release_sign_manifest <dir> -- <dir>/RELEASE-MANIFEST.sig.
release_sign_manifest() {
  local dir="$1"
  [ -s "$dir/RELEASE-MANIFEST" ] || { _rs_fail "$dir/RELEASE-MANIFEST is missing -- write it first"; return 1; }
  _rs_sign_file "$dir/RELEASE-MANIFEST"
}

# release_verify_manifest <dir> <expected-tag> -- everything a consumer will
# check, read back from DISK (never from what the caller believes it wrote):
#   1. the signature verifies for a committed principal, namespace oam-release
#   2. line 1 is exactly the v1 header
#   3. line 2 is exactly "tag <expected-tag>"
#   4. the rest is byte-identical to <dir>/SHA256SUMS
#   5. the signing key's range covers the tag
# The content checks come AFTER the signature: until it verifies, the content
# is attacker-controlled and its parse is not worth reporting.
release_verify_manifest() {
  local dir="$1" tag="$2" m s sums principal err line1 line2 prefix_len
  # Byte lengths below, not character lengths: the header and a plain tag are
  # ASCII, but a UTF-8 locale is no reason to find out.
  local LC_ALL=C
  m="$dir/RELEASE-MANIFEST"; s="$dir/RELEASE-MANIFEST.sig"; sums="$dir/SHA256SUMS"
  release_ssh_tools || return 1
  _rs_plain_tag "$tag" || { _rs_fail "expected tag '$tag' is not a plain vX.Y.Z tag"; return 1; }
  [ -f "$m" ] || { _rs_fail "$m is missing"; return 1; }
  [ -f "$s" ] || { _rs_fail "$s is missing"; return 1; }
  [ -f "$sums" ] || { _rs_fail "$sums is missing"; return 1; }
  release_keys_lint || return 1
  [ -n "$(release_key_lines)" ] \
    || { _rs_fail "release-keys/allowed_signers holds no key -- nothing can verify $m"; return 1; }
  # _rs_verify_sig prints the principal on stdout and ssh-keygen's last
  # complaint on stderr; each is captured where it belongs.
  err="$(mktemp)" || { _rs_fail "could not create a temp file"; return 1; }
  if ! principal="$(_rs_verify_sig "$m" "$s" 2>"$err")"; then
    _rs_fail "RELEASE-MANIFEST.sig does not verify against any key in release-keys/allowed_signers (namespace $RELEASE_SIGN_NAMESPACE): $(cat "$err")"
    rm -f "$err"
    return 1
  fi
  rm -f "$err"
  # The two header lines are compared as BYTES against the expected ones, and
  # only diagnosed line by line when that fails. A line-level compare alone is
  # not enough: Git Bash strips CRs from $(...) output, so a CRLF header reads
  # back there as the right text, and the misplaced boundary would surface as
  # a confusing SUMS mismatch -- or, with lengths that happened to line up,
  # not at all.
  prefix_len=$(( ${#RELEASE_MANIFEST_HEADER} + 1 + ${#tag} + 5 ))
  if ! printf '%s\ntag %s\n' "$RELEASE_MANIFEST_HEADER" "$tag" | cmp -s - <(head -c "$prefix_len" "$m"); then
    if [ "$(head -n 2 "$m" | tr -dc '\r' | wc -c)" -gt 0 ]; then
      _rs_fail "RELEASE-MANIFEST's header has CR line endings -- it is LF-only, byte for byte"
      return 1
    fi
    line1="$(sed -n 1p "$m")"
    [ "$line1" = "$RELEASE_MANIFEST_HEADER" ] \
      || { _rs_fail "RELEASE-MANIFEST line 1 is '$line1', not '$RELEASE_MANIFEST_HEADER'"; return 1; }
    line2="$(sed -n 2p "$m")"
    case "$line2" in
      "tag $tag") _rs_fail "RELEASE-MANIFEST's header reads right but is not byte-exact"; return 1 ;;
      "tag "*) _rs_fail "RELEASE-MANIFEST is signed for tag '${line2#tag }', not $tag -- a replayed or misfiled release"; return 1 ;;
      *) _rs_fail "RELEASE-MANIFEST line 2 is '$line2', not 'tag $tag'"; return 1 ;;
    esac
  fi
  # Then SHA256SUMS, to the last byte.
  tail -c +"$((prefix_len + 1))" "$m" | cmp -s - "$sums" \
    || { _rs_fail "RELEASE-MANIFEST's SUMS section is not byte-identical to $sums"; return 1; }
  release_tag_in_range "$principal" "$tag" || return 1
  _rs_ok "RELEASE-MANIFEST verifies: $tag, signed by $principal, SUMS section identical to SHA256SUMS"
  return 0
}

# =============================================================================
# Windows: Authenticode through Azure Artifact Signing
# =============================================================================
# The manifest above proves a release came from us; it does nothing for the
# person who double-clicks oam.exe, or for Smart App Control, which judges the
# binary alone. So each Windows asset also carries an Authenticode signature,
# made by Azure Artifact Signing: the key lives in Microsoft's HSM and never
# touches this box, the certificate is issued to the validated organization
# name and lives three days, and signtool reaches the service through a
# client "dlib" plugin. Everything account-specific (endpoint, account name,
# certificate profile) sits in a metadata.json OUTSIDE the repo, at
# $OAM_WIN_SIGN_METADATA, never committed: this is a public repo. The
# publisher name the signature must carry is $OAM_WIN_SIGN_PUBLISHER; the name
# itself is public (it is printed on every signed asset, and the docs state
# it), but the script takes it from the environment so a renamed or
# re-validated organization is a config change, not a code change.
# release-keys/README.md ("Windows Authenticode") is the setup runbook.
#
# Three-day certificates make the RFC 3161 timestamp the signature's real
# lifetime: an untimestamped signature dies with its certificate. So the TSA
# is not optional, a TSA outage fails the release, and the preflight's real
# throwaway signature probes it.
#
# The verify gate is the point, not the sign call. signtool with this dlib can
# report success and sign nothing (Microsoft's FAQ: "No error codes, SignTool
# silently fails" without the matching .NET runtime), so win_verify re-reads
# the file from disk twice, independently: `signtool verify /pa`, then
# verify-authenticode.ps1 through Get-AuthenticodeSignature (status Valid, a
# timestamp, signer CN and O == $OAM_WIN_SIGN_PUBLISHER, the Artifact Signing
# intermediate in the chain). The second half shares no code with signtool, so
# a signtool that lies -- or a stub on PATH -- cannot vouch for itself.
#
# Bootstrap, like the manifest: with $OAM_WIN_SIGN_METADATA unset the Windows
# assets ship unsigned, loudly (fatal under OAM_SIGN_REQUIRED=1). Once it is
# set, signing both .exe assets is mandatory. The one skip knob,
# OAM_SKIP_WIN_SIGN, is read by the CALLER and handed in as
# win_sign_decision's argument: this lib reads no skip knob, so nothing here
# can be talked into skipping the manifest.
#
# Tooling on the arm64 release box: the dlib ships x86 and x64 builds only,
# so it is the x64 signtool, the x64 dlib and the x64 .NET 8 runtime, all
# under the OS's x64 emulation. Same caller contract as above: [fail] lines on
# stderr, return non-zero, never exit, no traps.
# =============================================================================

WIN_SIGN_TSA="http://timestamp.acs.microsoft.com"
# The token audience the dlib asks Entra for. A token for it proves the az
# session is live; it does NOT prove the Signer role (only a signature does).
WIN_SIGN_RESOURCE="https://codesigning.azure.net"
# Every Artifact Signing (Public Trust) leaf chains through this intermediate
# (learn.microsoft.com/azure/artifact-signing/faq). Requiring it pins the
# signature to the service, not merely to "some CA Windows trusts".
WIN_SIGN_INTERMEDIATE="Microsoft ID Verified Code Signing PCA 2021"
# signtool floor, as a Windows SDK version. Microsoft's integration page says
# "10.0.2261.755", which is no SDK that exists; the client tools' own README
# says "Windows SDK 10.0.22621.0 or higher" -- the docs number with a dropped
# digit. 10.0.20348.* (Server 2022's SDK) is called out as unsupported by the
# dlib and is skipped whatever its number.
WIN_SIGNTOOL_FLOOR="10.0.22621"
# Seconds any one signtool, verify or az call may take (1..3600). signtool +
# the dlib do not time out on their own: against an unreachable endpoint they
# print "Submitting digest for signing..." and wait forever (measured, dlib
# 1.0.119). A stalled service mid-release must be a clean failure, not a hang.
WIN_SIGN_TIMEOUT="${OAM_WIN_SIGN_TIMEOUT:-300}"
WIN_SIGN_TIMEOUT_MAX=3600
# Seconds `dotnet --list-runtimes` may take: a local probe, no network.
WIN_DOTNET_PROBE_TIMEOUT=60
# Search roots and the external programs, reassigned by the test suite.
WIN_SDK_BIN_ROOT="/c/Program Files (x86)/Windows Kits/10/bin"
WIN_DOTNET_CANDIDATES=("/c/Program Files/dotnet/x64/dotnet.exe" "/c/Program Files/dotnet/dotnet.exe")
WIN_AZ="az"
WIN_POWERSHELL="powershell.exe"
WIN_VERIFY_PS1="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/verify-authenticode.ps1"
# Where `winget install Microsoft.Azure.ArtifactSigningClientTools` (0.1.128,
# measured) puts the dlib: per-user, unversioned. The second name is the one
# that installer's own signtool.bat still refers to.
WIN_DLIB_DIRS=()
if [ -n "${LOCALAPPDATA:-}" ]; then
  _ws_lad="$LOCALAPPDATA"
  if command -v cygpath >/dev/null 2>&1; then _ws_lad="$(cygpath -u "$_ws_lad")"; fi
  WIN_DLIB_DIRS=("$_ws_lad/Microsoft/MicrosoftArtifactSigningClientTools" "$_ws_lad/Microsoft/ArtifactSigningTools")
  unset _ws_lad
fi

WIN_SIGNTOOL=""
WIN_SIGN_DLIB=""
WIN_DOTNET_X64=""

# kill_proc_tree <pid>: the repo's one process-tree reaper, tested where it
# lives. iap-helpers.sh defines functions and two OAM_DISK_* defaults only.
# shellcheck source=iap-helpers.sh
. "$(dirname "${BASH_SOURCE[0]}")/iap-helpers.sh"

# _ws_winpath <path> -- the native Windows spelling, for arguments a Windows
# program reads (signtool, powershell). $RELEASE_DIR is an MSYS /tmp path that
# no Windows program can open as written.
_ws_winpath() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s\n' "$1"; fi
}

# _ws_run_for <seconds> <command...> -- run with stdin from /dev/null and
# stdout + stderr captured, then printed on stdout; returns the command's rc,
# or 124 when it ran out of time and was killed WITH everything it started (a
# command's OWN 124 comes back as 1, so it is never read as a timeout).
#
# Not timeout(1): on Windows it kills only its direct child. `az` is a bash
# script around python.exe, a stand-in may be cmd.exe around ping.exe, and the
# grandchild lives on holding the output pipe, so the caller blocks anyway
# (measured: `timeout 3` on a script running `cmd //c ping -n 593` returned
# only when ping did). So kill_proc_tree (lib/iap-helpers.sh: taskkill /T on
# Windows, the ps-snapshot tree elsewhere) does the killing, and the output
# goes through a file, not a pipe, so nothing that escaped could hold the
# caller's $(...) open either.
#
# The command is the background job itself, never wrapped: Cygwin's exec
# starts a NEW Windows process, so a wrapper subshell's children hang off a
# process that is already gone, and taskkill /T from the wrapper misses them
# (measured: the ping survived). The wait spawns nothing per tick (a spawn
# costs ~0.5s on the Windows release box): the watchdog is one `sleep` whose
# stdout this shell reads with `read -t`, a builtin pause that ends early, at
# EOF, when the sleep does; bash reaps its children as they exit, so
# `kill -0` turns false the moment the command is done. Not `wait -n`: a job
# that exits before `wait -n` is entered can already be marked notified, and
# then `wait -n` waits out the watchdog instead (measured: a stub that exited
# at once was reported as timed out after the full 300s).
_ws_run_for() {
  local secs="$1" log pid wfd wpid r rc=0 late=0
  shift
  log="$(mktemp "${TMPDIR:-/tmp}/oam-ws-run.XXXXXX")" || { echo "could not create a temp file for the output of $1"; return 1; }
  "$@" </dev/null >"$log" 2>&1 &
  pid=$!
  exec {wfd}< <(exec sleep "$secs")
  wpid=$!
  while kill -0 "$pid" 2>/dev/null; do
    r=0
    read -r -t 0.2 -u "$wfd" _ || r=$?
    # 0 or 1 (data, or EOF): the watchdog's sleep ended. > 128: just a tick.
    if [ "$r" -le 128 ]; then
      if kill -0 "$pid" 2>/dev/null; then late=1; fi
      break
    fi
  done
  exec {wfd}<&-
  if [ "$late" = "1" ]; then
    kill_proc_tree "$pid"
    rc=124
  else
    kill "$wpid" 2>/dev/null
    wait "$pid"
    rc=$?
    if [ "$rc" = "124" ]; then rc=1; fi
  fi
  cat "$log"
  rm -f "$log"
  return "$rc"
}

# _ws_run <command...> -- _ws_run_for WIN_SIGN_TIMEOUT.
_ws_run() { _ws_run_for "$WIN_SIGN_TIMEOUT" "$@"; }

# _ws_timed_out <rc> -- whether rc is _ws_run killing the command for time.
_ws_timed_out() { [ "$1" = "124" ]; }

# _ws_ci <var> <word> -- set <var> to <word> as a case-insensitive ERE
# ("ab" -> "[Aa][Bb]"): sed's I flag is GNU-only.
_ws_ci() {
  local _w="$2" _i _c _o=""
  for ((_i = 0; _i < ${#_w}; _i++)); do
    _c="${_w:_i:1}"
    if [[ "$_c" == [A-Za-z] ]]; then _o="${_o}[${_c^^}${_c,,}]"; else _o="$_o$_c"; fi
  done
  printf -v "$1" '%s' "$_o"
}

# _ws_redact [metadata.json] -- stdin to stdout (CRs dropped) with everything
# that identifies the account or the operator replaced: every value
# metadata.json holds, in any case; the endpoint / account / profile shapes
# the service echoes back; email-shaped tokens (az names the signed-in UPN:
# "User '...' does not exist in MSAL token cache"); GUIDs (tenant and
# subscription IDs, AADSTS trace and correlation IDs); *.onmicrosoft.com
# tenants and quoted tenant / subscription / directory names; and the user
# profile directory in every spelling ($HOME, $USERPROFILE, C:\Users\<name>,
# C:/Users/<name>, /c/Users/<name>) -> <home>. Every path that prints an
# external tool's output goes through here: that output is exactly what an
# operator pastes into a public issue.
_ws_redact() {
  local m="${1:-}" k v vals="" homes="" acct prof q="'"
  if [ -n "$m" ] && [ -f "$m" ]; then
    for k in Endpoint CodeSigningAccountName CertificateProfileName; do
      v="$(sed -nE "s/.*\"$k\"[[:space:]]*:[[:space:]]*\"([^\"]*)\".*/\1/p" "$m" | head -1)"
      if [ -n "$v" ]; then vals="$vals$v"$'\n'; fi
    done
  fi
  for v in "${HOME:-}" "${USERPROFILE:-}"; do
    [ "${#v}" -ge 4 ] || continue
    homes="$homes$v"$'\n'"${v//\\//}"$'\n'
  done
  _ws_ci acct codesigningaccounts
  _ws_ci prof certificateprofiles
  # In this order: the profile directory as spelled in this environment (a
  # literal: it may hold spaces), then the shapes -- any other profile path,
  # emails, GUIDs, tenants, Artifact Signing hosts -- and only then the
  # metadata values, case-insensitively and as data, never as a regex (after
  # the shapes, so a value like "example" cannot break up an email first).
  # mawk has no [[:classes:]] or {n} intervals, hence the spelled-out forms.
  # The three rules that need a back-reference follow in sed.
  WS_REDACT_VALS="$vals" WS_REDACT_HOMES="$homes" awk '
    function swap(line, needle, with,   low, out, p, n) {
      n = length(needle); out = ""; low = tolower(line)
      while ((p = index(low, needle)) > 0) {
        out = out substr(line, 1, p - 1) with
        line = substr(line, p + n); low = substr(low, p + n)
      }
      return out line
    }
    BEGIN {
      nv = split(ENVIRON["WS_REDACT_VALS"], raw, "\n")
      for (i = 1; i <= nv; i++) if (length(raw[i]) >= 3) val[++kv] = tolower(raw[i])
      nh = split(ENVIRON["WS_REDACT_HOMES"], raw, "\n")
      for (i = 1; i <= nh; i++) if (length(raw[i]) >= 4) home[++kh] = tolower(raw[i])
      h = "[0-9A-Fa-f]"; h4 = h h h h
      guid = h4 h4 "-" h4 "-" h4 "-" h4 "-" h4 h4 h4
    }
    {
      sub(/\r$/, "")
      for (i = 1; i <= kh; i++) $0 = swap($0, home[i], "<home>")
      gsub(/[A-Za-z]:(\\\\|\\|\/)[Uu][Ss][Ee][Rr][Ss](\\\\|\\|\/)[^\\\/ \t"\047<>|:*?]+/, "<home>")
      gsub(/\/[A-Za-z]\/[Uu][Ss][Ee][Rr][Ss]\/[^\/ \t"\047<>|:*?]+/, "<home>")
      gsub(/[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z][A-Za-z]+/, "<email>")
      gsub(guid, "<guid>")
      gsub(/[A-Za-z0-9-]+\.[Oo][Nn][Mm][Ii][Cc][Rr][Oo][Ss][Oo][Ff][Tt]\.[Cc][Oo][Mm]/, "<tenant>.onmicrosoft.com")
      gsub(/[A-Za-z0-9-]+\.[Cc][Oo][Dd][Ee][Ss][Ii][Gg][Nn][Ii][Nn][Gg]\.[Aa][Zz][Uu][Rr][Ee]\.[Nn][Ee][Tt]/, "<redacted>.codesigning.azure.net")
      for (i = 1; i <= kv; i++) $0 = swap($0, val[i], "<redacted>")
      print
    }' | sed -E \
    -e 's/("(Endpoint|CodeSigningAccountName|CertificateProfileName)"[[:space:]]*:[[:space:]]*)"[^"]*"/\1"<redacted>"/g' \
    -e "s#($acct|$prof)/[^/[:space:]\"]+#\\1/<redacted>#g" \
    -e "s#([Tt]enant|[Ss]ubscription|[Dd]irectory)([[:space:]]+[Ii][Dd])?([[:space:]]*:?[[:space:]]*)${q}[^${q}]*${q}#\\1\\2\\3$q<redacted>$q#g"
}

# _ws_fail / _ws_warn / _ws_ok -- the Windows section's own status lines, run
# through _ws_redact like every tool output: a message that names a path
# (metadata.json, a probe in TMPDIR) would otherwise print the user profile
# directory, and with it the operator's Windows account name.
_ws_fail(){ printf '  [fail] %s\n' "$*" | _ws_redact >&2; return 1; }
_ws_warn(){ printf '  [warn] %s\n' "$*" | _ws_redact >&2; return 0; }
_ws_ok(){   printf '  [ok] %s\n' "$*" | _ws_redact >&2; return 0; }

# _ws_show <label> [metadata.json] -- a failed tool's output (stdin), redacted,
# onto stderr: first the lines that say WHY (HTTP status, error codes,
# SignerSign(), exception messages, AADSTS codes; stack frames skipped), then
# the last 12 lines. A .NET failure from the dlib runs to 30+ lines, and its
# "Status: 403 (Forbidden)" sits near the top, where a bare tail drops it.
_ws_show() {
  local label="$1" m="${2:-}"
  _ws_redact "$m" | awk -v p="        $label: " -v n=12 '
    { l[NR] = $0 }
    END {
      start = NR - n + 1
      if (start < 1) start = 1
      k = 0
      for (i = 1; i < start && k < 12; i++) {
        lo = tolower(l[i])
        if (lo ~ /^[ \t]*at /) continue
        if (lo ~ /status:|error|fail|exception|denied|forbidden|unauthori|aadsts|signersign|0x[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]/) { print p l[i]; k++ }
      }
      if (start > 1) print p "... (the last " n " of " NR " lines:)"
      for (i = start; i <= NR; i++) print p l[i]
    }' >&2
}

# _ws_pe_machine <file> -- the COFF Machine field as 4 lowercase hex digits
# (8664 = x64, aa64 = arm64, 014c = x86); empty for anything that is not a
# PE. Read with od, so it answers the same for a real binary and a fixture.
_ws_pe_machine() {
  local f="$1" mz lfanew sig
  [ -f "$f" ] || return 0
  mz="$(od -An -tx1 -N2 "$f" 2>/dev/null | tr -d ' \n')"
  [ "$mz" = "4d5a" ] || return 0
  lfanew="$(od -An -tu4 -j60 -N4 "$f" 2>/dev/null | tr -d ' \n')"
  [[ "$lfanew" =~ ^[0-9]+$ ]] || return 0
  sig="$(od -An -tx1 -j"$lfanew" -N4 "$f" 2>/dev/null | tr -d ' \n')"
  [ "$sig" = "50450000" ] || return 0
  od -An -tx1 -j"$((lfanew + 4))" -N2 "$f" 2>/dev/null | awk '{ print $2 $1 }'
}

# _ws_ver_ge <a> <b> -- 0 when dotted version a >= b, numeric per field,
# missing fields read as 0; 1 for anything non-numeric.
_ws_ver_ge() {
  local -a a b
  local i n x y
  IFS=. read -r -a a <<<"$1"
  IFS=. read -r -a b <<<"$2"
  n=${#a[@]}
  if [ "${#b[@]}" -gt "$n" ]; then n=${#b[@]}; fi
  for ((i = 0; i < n; i++)); do
    x="${a[i]:-0}"; y="${b[i]:-0}"
    [[ "$x" =~ ^[0-9]+$ && "$y" =~ ^[0-9]+$ ]] || return 1
    if [ $((10#$x)) -gt $((10#$y)) ]; then return 0; fi
    if [ $((10#$x)) -lt $((10#$y)) ]; then return 1; fi
  done
  return 0
}

# locate_signtool_x64 -- WIN_SIGNTOOL: the x64 signtool from the newest
# Windows SDK at or above the floor, skipping 10.0.20348.*. The SDK directory
# names the version; the binary's own PE header must say x64.
locate_signtool_x64() {
  [ -z "$WIN_SIGNTOOL" ] || return 0
  local d v best="" bestv="" seen=""
  for d in "$WIN_SDK_BIN_ROOT"/10.*/; do
    d="${d%/}"; v="${d##*/}"
    [ -f "$d/x64/signtool.exe" ] || continue
    seen="$seen $v"
    case "$v" in 10.0.20348.*) continue ;; esac
    _ws_ver_ge "$v" "$WIN_SIGNTOOL_FLOOR" || continue
    if [ -z "$bestv" ] || ! _ws_ver_ge "$bestv" "$v"; then best="$d/x64/signtool.exe"; bestv="$v"; fi
  done
  [ -n "$best" ] \
    || { _ws_fail "no x64 signtool.exe from a Windows SDK >= $WIN_SIGNTOOL_FLOOR (and not 10.0.20348) under $WIN_SDK_BIN_ROOT (found:${seen:- none}) -- install the Windows SDK signing tools"; return 1; }
  [ "$(_ws_pe_machine "$best")" = "8664" ] \
    || { _ws_fail "$best is not an x64 binary -- the Artifact Signing dlib ships x86/x64 builds only, and this lib pairs it with the x64 signtool"; return 1; }
  WIN_SIGNTOOL="$best"
  return 0
}

# locate_artifact_signing_dlib -- WIN_SIGN_DLIB: Azure.CodeSigning.Dlib.dll
# from the client tools install, and x64 (it must match signtool's arch).
locate_artifact_signing_dlib() {
  [ -z "$WIN_SIGN_DLIB" ] || return 0
  local d f
  for d in "${WIN_DLIB_DIRS[@]}"; do
    f="$d/Azure.CodeSigning.Dlib.dll"
    [ -f "$f" ] || continue
    if [ "$(_ws_pe_machine "$f")" != "8664" ]; then
      _ws_warn "$f is not the x64 build of the dlib -- skipped"
      continue
    fi
    WIN_SIGN_DLIB="$f"
    return 0
  done
  _ws_fail "no x64 Azure.CodeSigning.Dlib.dll in: ${WIN_DLIB_DIRS[*]:-<no LOCALAPPDATA>} -- install it: winget install -e --id Microsoft.Azure.ArtifactSigningClientTools"
  return 1
}

# probe_dotnet_x64 -- WIN_DOTNET_X64: an x64 dotnet host that lists a
# Microsoft.NETCore.App runtime >= 8 (the dlib's runtimeconfig rolls forward
# across majors). On an arm64 box the x64 host lives in dotnet/x64/ and the
# top-level dotnet.exe is the arm64 one, whose runtimes are no use to an x64
# signtool -- hence the PE check rather than trusting either path.
probe_dotnet_x64() {
  [ -z "$WIN_DOTNET_X64" ] || return 0
  local c out
  for c in "${WIN_DOTNET_CANDIDATES[@]}"; do
    [ -f "$c" ] || continue
    [ "$(_ws_pe_machine "$c")" = "8664" ] || continue
    # Bounded and tree-killed like every other external call here.
    out="$(_ws_run_for "$WIN_DOTNET_PROBE_TIMEOUT" "$c" --list-runtimes)" || continue
    if grep -qE '^Microsoft\.NETCore\.App ([89]|[1-9][0-9])\.' <<<"$out"; then
      WIN_DOTNET_X64="$c"
      return 0
    fi
  done
  _ws_fail "no x64 .NET runtime >= 8 (checked: ${WIN_DOTNET_CANDIDATES[*]}) -- without it signtool + the dlib can exit 0 and sign NOTHING. Install the x64 .NET 8 runtime (winget install -e --id Microsoft.DotNet.Runtime.8 --architecture x64)"
  return 1
}

# win_sign_decision <skip-knob> -- whether this run Authenticode-signs, as one
# line on stdout, in release_signing_decision's vocabulary:
#   sign            OAM_WIN_SIGN_METADATA and OAM_WIN_SIGN_PUBLISHER are set
#   skip:<reason>   the caller's OAM_SKIP_WIN_SIGN=1 (honored even when
#                   required -- it is the loud, deliberate override), or
#                   bootstrap: nothing configured, not required
#   fail:<reason>   bootstrap under OAM_SIGN_REQUIRED=1, half a config, or a
#                   malformed knob
# Pure function of the environment and its argument, so the suite drives it
# directly.
win_sign_decision() {
  local skip="${1:-0}" req="${OAM_SIGN_REQUIRED:-0}"
  local meta="${OAM_WIN_SIGN_METADATA:-}" pub="${OAM_WIN_SIGN_PUBLISHER:-}"
  [[ "$req" =~ ^[01]$ ]] || { printf 'fail:OAM_SIGN_REQUIRED must be 0 or 1, not %s\n' "$req"; return 0; }
  [[ "$skip" =~ ^[01]$ ]] || { printf 'fail:OAM_SKIP_WIN_SIGN must be 0 or 1, not %s\n' "$skip"; return 0; }
  if [ "$skip" = "1" ]; then
    printf 'skip:OAM_SKIP_WIN_SIGN=1 -- the Windows assets ship WITHOUT Authenticode signatures (SmartScreen and Smart App Control will treat them as unsigned)\n'
  elif [ -n "$meta" ] && [ -n "$pub" ]; then
    printf 'sign\n'
  elif [ -n "$meta" ] || [ -n "$pub" ]; then
    printf 'fail:Windows signing is half configured -- set BOTH OAM_WIN_SIGN_METADATA (the metadata.json path) and OAM_WIN_SIGN_PUBLISHER (the validated publisher name), or neither\n'
  elif [ "$req" = "1" ]; then
    printf 'fail:OAM_SIGN_REQUIRED=1 but OAM_WIN_SIGN_METADATA is not set -- configure Windows Authenticode signing (release-keys/README.md), or set OAM_SKIP_WIN_SIGN=1 to ship unsigned Windows assets deliberately\n'
  else
    printf 'skip:OAM_WIN_SIGN_METADATA is not set, so the Windows assets ship WITHOUT Authenticode signatures (bootstrap; see release-keys/README.md). OAM_SIGN_REQUIRED=1 makes this fatal\n'
  fi
  return 0
}

# _ws_meta_path -- $OAM_WIN_SIGN_METADATA in Unix spelling, unchecked (empty
# when unset): for redaction, which must work on a half-broken config too.
_ws_meta_path() {
  local m="${OAM_WIN_SIGN_METADATA:-}"
  if [ -n "$m" ] && command -v cygpath >/dev/null 2>&1; then m="$(cygpath -u "$m")"; fi
  printf '%s\n' "$m"
}

# _ws_metadata -- the metadata.json path, Unix spelling, checked for the three
# fields the dlib needs. Values are never printed: they identify the account.
_ws_metadata() {
  local m="${OAM_WIN_SIGN_METADATA:-}" k
  [ -n "$m" ] || { _ws_fail "OAM_WIN_SIGN_METADATA is not set"; return 1; }
  if command -v cygpath >/dev/null 2>&1; then m="$(cygpath -u "$m")"; fi
  [ -f "$m" ] || { _ws_fail "OAM_WIN_SIGN_METADATA=$m does not exist"; return 1; }
  for k in Endpoint CodeSigningAccountName CertificateProfileName; do
    grep -qE "\"$k\"[[:space:]]*:[[:space:]]*\"[^\"<]+\"" "$m" \
      || { _ws_fail "$m has no \"$k\" value (shape: {\"Endpoint\": ..., \"CodeSigningAccountName\": ..., \"CertificateProfileName\": ...})"; return 1; }
  done
  printf '%s\n' "$m"
}

# _ws_timeout_ok -- WIN_SIGN_TIMEOUT is a whole number of seconds in
# 1..WIN_SIGN_TIMEOUT_MAX. Capped, not merely positive: a 20-digit value
# overflows sleep's arithmetic (GNU timeout, measured, reported 124 -- "timed
# out" -- for a command that exited normally), and no call here should get
# anywhere near an hour.
_ws_timeout_ok() {
  if [[ "$WIN_SIGN_TIMEOUT" =~ ^[1-9][0-9]{0,3}$ ]] && [ "$WIN_SIGN_TIMEOUT" -le "$WIN_SIGN_TIMEOUT_MAX" ]; then return 0; fi
  _ws_fail "OAM_WIN_SIGN_TIMEOUT must be a whole number of seconds from 1 to $WIN_SIGN_TIMEOUT_MAX, not '$WIN_SIGN_TIMEOUT'"
  return 1
}

# win_sign_tools -- every local prerequisite, none of them network.
win_sign_tools() {
  _ws_timeout_ok || return 1
  locate_signtool_x64 || return 1
  locate_artifact_signing_dlib || return 1
  probe_dotnet_x64 || return 1
  [ -f "$WIN_VERIFY_PS1" ] || { _ws_fail "$WIN_VERIFY_PS1 is missing"; return 1; }
  [ -n "${OAM_WIN_SIGN_PUBLISHER:-}" ] || { _ws_fail "OAM_WIN_SIGN_PUBLISHER is not set -- the CN/O every signature must carry"; return 1; }
  _ws_metadata >/dev/null || return 1
  return 0
}

# win_sign <file> -- Authenticode-sign <file> in place: SHA256 file digest, an
# RFC 3161 SHA256 timestamp from the Artifact Signing TSA. A staged copy only:
# signing rewrites the file, and a file under target/ is a build output that
# cargo, the parked-binary dance and live sessions all assume is unchanged.
# Success here means signtool SAID so; win_verify is what proves it.
win_sign() {
  local file="$1" meta out rc
  [ -f "$file" ] || { _ws_fail "win_sign: $file does not exist"; return 1; }
  case "$file" in
    */target/* | target/*) _ws_fail "win_sign: refusing to sign $file in place -- sign the staged copy, never a build output under target/"; return 1 ;;
  esac
  win_sign_tools || return 1
  meta="$(_ws_metadata)" || return 1
  # MSYS_NO_PATHCONV: Git Bash would otherwise rewrite /fd, /tr ... into paths.
  rc=0
  out="$(MSYS_NO_PATHCONV=1 _ws_run "$WIN_SIGNTOOL" sign /v /fd SHA256 /tr "$WIN_SIGN_TSA" /td SHA256 \
           /dlib "$(_ws_winpath "$WIN_SIGN_DLIB")" /dmdf "$(_ws_winpath "$meta")" \
           "$(_ws_winpath "$file")")" || rc=$?
  [ "$rc" = "0" ] && return 0
  # Redacted: /v echoes metadata.json, and metadata.json names the account.
  printf '%s\n' "$out" | _ws_show signtool "$meta"
  if _ws_timed_out "$rc"; then
    _ws_fail "signtool sign for $file did not finish in ${WIN_SIGN_TIMEOUT}s and was killed (output above) -- the Artifact Signing endpoint or the TSA ($WIN_SIGN_TSA) is unreachable or stalled; OAM_WIN_SIGN_TIMEOUT sets the limit"
  else
    _ws_fail "signtool sign failed for $file (output above). 401/403: run 'az login' and check the Certificate Profile Signer role; a SignerSign() error: metadata.json's Endpoint must be the account's region"
  fi
  return 1
}

# win_verify <file> -- fail-closed proof that <file> AS IT IS ON DISK carries
# the signature this release requires. Two independent readers:
#   1. signtool verify /pa: the default Authenticode policy, chain + timestamp
#   2. verify-authenticode.ps1: Get-AuthenticodeSignature, then the pins --
#      Valid, embedded (not catalog), timestamped, signer CN and O equal to
#      $OAM_WIN_SIGN_PUBLISHER, $WIN_SIGN_INTERMEDIATE in the chain.
# The path and the publisher reach PowerShell as -File ARGUMENTS, never spliced
# into a command string: a path is data, and so is a publisher with a comma.
win_verify() {
  local file="$1" out rc late="" pub="${OAM_WIN_SIGN_PUBLISHER:-}"
  [ -f "$file" ] || { _ws_fail "win_verify: $file does not exist"; return 1; }
  [ -n "$pub" ] || { _ws_fail "win_verify: OAM_WIN_SIGN_PUBLISHER is not set -- nothing to pin the signer to"; return 1; }
  locate_signtool_x64 || return 1
  _ws_timeout_ok || return 1
  rc=0
  out="$(MSYS_NO_PATHCONV=1 _ws_run "$WIN_SIGNTOOL" verify /pa /v "$(_ws_winpath "$file")")" || rc=$?
  if [ "$rc" != "0" ]; then
    if _ws_timed_out "$rc"; then late=" -- killed after ${WIN_SIGN_TIMEOUT}s"; fi
    printf '%s\n' "$out" | _ws_show signtool "$(_ws_meta_path)"
    _ws_fail "signtool verify /pa rejects $file (output above)$late"
    return 1
  fi
  rc=0
  out="$(MSYS_NO_PATHCONV=1 _ws_run "$WIN_POWERSHELL" -NoProfile -NonInteractive -ExecutionPolicy Bypass \
           -File "$(_ws_winpath "$WIN_VERIFY_PS1")" \
           -Path "$(_ws_winpath "$file")" -Publisher "$pub" -Intermediate "$WIN_SIGN_INTERMEDIATE")" || rc=$?
  if [ "$rc" != "0" ]; then
    if _ws_timed_out "$rc"; then late=" -- killed after ${WIN_SIGN_TIMEOUT}s"; fi
    printf '%s\n' "$out" | _ws_show verify "$(_ws_meta_path)"
    _ws_fail "Authenticode verification failed for $file (above)$late -- whatever signtool reported, the file on disk does not carry the required signature"
    return 1
  fi
  _ws_ok "Authenticode: $(basename "$file") signed by '$pub', timestamped, chained via $WIN_SIGN_INTERMEDIATE"
  return 0
}

# win_pe_signature_state <file> -- "signed" when the PE's certificate table
# (data directory 4, IMAGE_DIRECTORY_ENTRY_SECURITY) is non-empty, i.e. it
# carries an embedded Authenticode signature of SOME kind; "unsigned" when it
# is a PE without one; "unknown" for anything else. Structure only, no trust
# decision -- it answers "would replacing this file drop a signature?", which
# needs no Windows tooling and no network.
win_pe_signature_state() {
  local f="$1" lfanew magic dd ndirs size
  if [ -z "$(_ws_pe_machine "$f")" ]; then echo unknown; return 0; fi
  lfanew="$(od -An -tu4 -j60 -N4 "$f" 2>/dev/null | tr -d ' \n')"
  # The optional header starts 24 bytes past "PE\0\0"; its data directories
  # sit 112 (PE32+) or 96 (PE32) bytes in, 8 bytes each.
  magic="$(od -An -tx1 -j"$((lfanew + 24))" -N2 "$f" 2>/dev/null | awk '{ print $2 $1 }')"
  case "$magic" in
    020b) dd=$((lfanew + 24 + 112)) ;;
    010b) dd=$((lfanew + 24 + 96)) ;;
    *) echo unknown; return 0 ;;
  esac
  ndirs="$(od -An -tu4 -j"$((dd - 4))" -N4 "$f" 2>/dev/null | tr -d ' \n')"
  [[ "$ndirs" =~ ^[0-9]+$ ]] || { echo unknown; return 0; }
  # Too few directories to HAVE entry 4 is not "unsigned": a real linker
  # writes 16, so this is a file the check does not understand. Fail closed.
  if [ "$ndirs" -le 4 ]; then echo unknown; return 0; fi
  size="$(od -An -tu4 -j"$((dd + 4 * 8 + 4))" -N4 "$f" 2>/dev/null | tr -d ' \n')"
  [[ "$size" =~ ^[0-9]+$ ]] || { echo unknown; return 0; }
  if [ "$size" -gt 0 ]; then echo signed; else echo unsigned; fi
}

# win_make_unsigned_pe <out> -- write a minimal, valid, unsigned PE32+ x64
# image (1024 bytes: headers, then one .text section holding `ret`). It is the
# preflight's probe and the suite's unsigned fixture, generated rather than
# committed: a binary blob in a public repo is something a reviewer has to
# take on trust, and these bytes are all spelled out below.
win_make_unsigned_pe() {
  local out="$1" h=""
  _ws_le() { # <bytes> <value> -- little-endian \xHH escapes onto $h
    local n="$1" v="$2" i
    for ((i = 0; i < n; i++)); do h="$h\\x$(printf '%02x' $(( (v >> (8 * i)) & 255 )))"; done
  }
  _ws_zero() { local i; for ((i = 0; i < $1; i++)); do h="$h\\x00"; done; }
  # DOS header: "MZ", zeros, e_lfanew = 0x40.
  h='\x4d\x5a'; _ws_zero 58; _ws_le 4 0x40
  # "PE\0\0", then the COFF header: AMD64, 1 section, no symbols, a 240-byte
  # optional header, EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE.
  h="$h"'\x50\x45\x00\x00'
  _ws_le 2 0x8664; _ws_le 2 1; _ws_le 4 0; _ws_le 4 0; _ws_le 4 0; _ws_le 2 240; _ws_le 2 0x22
  # Optional header, PE32+.
  _ws_le 2 0x20b; _ws_le 1 14; _ws_le 1 0          # magic, linker 14.0
  _ws_le 4 0x200; _ws_le 4 0; _ws_le 4 0           # code / init / uninit sizes
  _ws_le 4 0x1000; _ws_le 4 0x1000                 # entry point, base of code
  _ws_le 8 0x140000000                             # image base
  _ws_le 4 0x1000; _ws_le 4 0x200                  # section / file alignment
  _ws_le 2 6; _ws_le 2 0; _ws_le 2 0; _ws_le 2 0   # OS version, image version
  _ws_le 2 6; _ws_le 2 0; _ws_le 4 0               # subsystem version, reserved
  _ws_le 4 0x2000; _ws_le 4 0x200; _ws_le 4 0      # image size, header size, checksum
  _ws_le 2 3; _ws_le 2 0x8100                      # console; NX_COMPAT | TS_AWARE
  _ws_le 8 0x100000; _ws_le 8 0x1000               # stack reserve / commit
  _ws_le 8 0x100000; _ws_le 8 0x1000               # heap reserve / commit
  _ws_le 4 0; _ws_le 4 16; _ws_zero 128            # loader flags, 16 empty data dirs
  # Section header: .text at RVA 0x1000, 0x200 raw bytes at file offset 0x200,
  # CODE | EXECUTE | READ.
  h="$h"'\x2e\x74\x65\x78\x74\x00\x00\x00'
  _ws_le 4 1; _ws_le 4 0x1000; _ws_le 4 0x200; _ws_le 4 0x200
  _ws_le 4 0; _ws_le 4 0; _ws_le 2 0; _ws_le 2 0; _ws_le 4 0x60000020
  # The headers end at 0x170; pad to 0x200, then the section: ret, zeros.
  _ws_zero $((0x200 - 0x170))
  h="$h"'\xc3'; _ws_zero 511
  unset -f _ws_le _ws_zero
  printf '%b' "$h" >"$out" || { _ws_fail "could not write $out"; return 1; }
  [ "$(wc -c <"$out" | tr -d ' ')" = "1024" ] || { _ws_fail "$out came out $(wc -c <"$out" | tr -d ' ') bytes, not 1024"; return 1; }
  return 0
}

# win_sign_preflight -- prove, BEFORE anything is bumped or tagged, that this
# box can produce a signature win_verify accepts: the tooling, a live az
# session, then a REAL signature on a throwaway PE in a temp dir outside the
# repo (the dirty-tree check runs right after). Only a real signature also
# covers the Signer role, metadata.json's endpoint/account/profile, the .NET
# runtime and the TSA -- each of which would otherwise fail on the first
# release asset, after the tag is public.
win_sign_preflight() {
  local d out rc
  win_sign_tools || return 1
  command -v "$WIN_AZ" >/dev/null 2>&1 \
    || { _ws_fail "the Azure CLI ($WIN_AZ) is not on PATH -- install it, then run 'az login'"; return 1; }
  # Bounded and tree-killed (az is a script around python.exe), and its
  # output redacted: a stale session names the signed-in account.
  rc=0
  out="$(_ws_run "$WIN_AZ" account get-access-token --resource "$WIN_SIGN_RESOURCE" -o none)" || rc=$?
  if [ "$rc" != "0" ]; then
    printf '%s\n' "$out" | _ws_show az "$(_ws_meta_path)"
    if _ws_timed_out "$rc"; then
      _ws_fail "az account get-access-token did not finish in ${WIN_SIGN_TIMEOUT}s and was killed (output above) -- Entra ID is unreachable or az is stuck; if it persists, run 'az login' again. OAM_WIN_SIGN_TIMEOUT sets the limit"
    else
      _ws_fail "no Azure token for $WIN_SIGN_RESOURCE -- run 'az login' as the identity holding the Artifact Signing Certificate Profile Signer role, then re-run"
    fi
    return 1
  fi
  d="$(mktemp -d "${TMPDIR:-/tmp}/oam-winsign-probe.XXXXXX")" || { _ws_fail "could not create a temp dir"; return 1; }
  if ! win_make_unsigned_pe "$d/oam-sign-probe.exe" \
     || ! win_sign "$d/oam-sign-probe.exe" \
     || ! win_verify "$d/oam-sign-probe.exe"; then
    rm -rf "$d"
    _ws_fail "Windows signing preflight: a throwaway signature did not sign and verify -- see above"
    return 1
  fi
  rm -rf "$d"
  _ws_ok "Windows signing preflight: signtool (SDK $(basename "$(dirname "$(dirname "$WIN_SIGNTOOL")")")), the dlib and x64 .NET sign, timestamp and verify as '$OAM_WIN_SIGN_PUBLISHER'"
  return 0
}
