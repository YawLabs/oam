#!/bin/bash
# =============================================================================
# What a published tap ACTUALLY serves: the classification, extracted to be TESTED
# =============================================================================
# bump-taps.sh's last step re-reads the Homebrew formula and the Scoop manifest
# from raw.githubusercontent and decides whether the push landed. That decision
# used to be one line -- does the served body contain "$VERSION" -- and it was
# wrong in two independent ways, both of which report GREEN:
#
#   - A VERSION STRING IS NOT THE CONTENT. brew and scoop install by fetching
#     the URL in the manifest and checking it against the HASH in the manifest,
#     so a manifest that names the right version with a wrong hash fails on
#     every user's machine while this step says "serves 0.14.0". The sibling
#     website check in release-local.sh already holds the higher standard and
#     says why: a WRONG published hash is worse than none, because it trains
#     people to ignore a mismatch. The tap step is the channel brew and scoop
#     users actually install from, so it has to meet at least that bar.
#
#   - A CACHED BODY IS NOT THE ORIGIN. raw.githubusercontent caches for ~5 min.
#     The documented repair path is "re-run with the same tag to fix a tap that
#     drifted", and a repair re-pushes a CORRECTED HASH UNDER THE SAME VERSION:
#     the cached body still carries that version, so a version-only check
#     matched the stale copy and reported success on the exact run whose job was
#     to prove the fix landed. Hence tap_cache_bust below -- and hence the
#     verdicts separate a stale hash from a stale version, which have different
#     causes and different fixes.
#
# NOTE ON PIPES, same as lib/miri-gate.sh. Every match here is a bash `case`
# builtin, never `printf ... | grep -q`. bump-taps.sh runs under `set -o
# pipefail` and `grep -q` exits at its FIRST match, which SIGPIPEs the writer
# and returns 141 for the whole pipeline -- i.e. a body that DID match reads as
# a miss, and reads as one more reliably the bigger the body gets. That was a
# real bug in this repo (#95). Do not reintroduce the shape.
#
# bash 3.2 throughout (no associative arrays, no `${var^^}`): macOS ships
# 3.2.57, the tailnet Mac is a release host, and bump-taps.sh advertises itself
# as the standalone repair path.
# =============================================================================

# tap_cache_bust <url> [stamp]
#
# Both mechanisms, because neither is guaranteed alone: the query string changes
# the CDN's cache KEY (what actually forces a Fastly miss), and the caller pairs
# this with a `Cache-Control: no-cache` request header for any layer that
# honours one. `stamp` is a parameter rather than an inlined `date +%s` so the
# tests can assert the shape without racing the clock.
tap_cache_bust() {
  local url="$1" stamp="${2:-}" sep='?'
  [ -n "$stamp" ] || stamp="$(date +%s)"
  case "$url" in *\?*) sep='&' ;; esac
  printf '%s%snocache=%s' "$url" "$sep" "$stamp"
}

# tap_verify_verdict <served-body> <version> <asset>:<hash>...
#
# Echoes exactly one verdict and decides nothing itself -- bump-taps.sh maps
# verdicts to ok/warn the way ci-local.sh maps miri_*_verdict:
#
#   unfetched                 nothing came back; the network, not the tap
#   version-stale             the body does not name <version> at all
#   no-hashes                 <version> matched but the CALLER passed no pairs,
#                             so this is the version-only check that cannot tell
#                             a correct manifest from a corrupt one. A verdict
#                             rather than a silent pass on purpose: a refactor
#                             that drops the hash arguments has to be LOUD, not
#                             green.
#   hash-mismatch <asset>...  <version> matched; these assets' hashes did not.
#                             Named, not counted -- "1 checksum differs" leaves
#                             the operator diffing three sha256 lines by hand.
#   ok                        <version> and every expected hash are in the body
#
# Substring matching rather than line parsing, so one predicate serves both the
# Ruby formula and the JSON manifest -- and it is the same predicate the release
# script already uses against oamjs.org/downloads.
tap_verify_verdict() {
  local live="$1" version="$2"
  shift 2
  [ -n "$live" ] || { printf 'unfetched'; return 0; }
  case "$live" in
    *"\"$version\""*) ;;
    *) printf 'version-stale'; return 0 ;;
  esac
  [ "$#" -gt 0 ] || { printf 'no-hashes'; return 0; }
  local missing="" pair asset hash
  for pair in "$@"; do
    asset="${pair%%:*}"
    hash="${pair#*:}"
    # An EMPTY hash is a substring of every body, so accepting one would turn
    # this into an unconditional pass -- the exact failure this file exists to
    # prevent. Same for a pair with no colon at all, where the strip leaves the
    # asset name in `hash`. bump-taps.sh refuses to publish a missing or
    # non-hex hash, so reaching here means the asset list and the hash table
    # drifted; count it as a miss and let the name say which.
    if [ -z "$hash" ] || [ "$hash" = "$pair" ]; then
      missing="$missing $asset"
      continue
    fi
    case "$live" in
      *"$hash"*) ;;
      *) missing="$missing $asset" ;;
    esac
  done
  if [ -n "$missing" ]; then printf 'hash-mismatch%s' "$missing"; else printf 'ok'; fi
}
