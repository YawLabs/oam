#!/usr/bin/env bash
# Promote CHANGELOG.md's [Unreleased] entries to a dated heading for a release.
#
# Why this exists: release-local.sh checked only that [Unreleased] was
# non-empty, and nothing ever moved those entries under a version heading. So
# every release from 0.15.1 on left its notes in [Unreleased], and by 0.16.4 six
# shipped versions had no heading at all and one 1100-line block held them all.
# Backfilled once before (d2632ae, 0.12.0 through 0.15.0), which is how we know
# it recurs. The preflight now refuses a release whose entries are still under
# [Unreleased]; this is the one command that fixes that.
#
# Idempotent: a version already promoted is left alone, so a re-run after a
# failed release is a no-op.
#
#   scripts/changelog-release.sh 0.16.5            # dated today
#   scripts/changelog-release.sh 0.16.5 2026-09-21 # dated explicitly
set -euo pipefail

version="${1:?usage: changelog-release.sh <version> [YYYY-MM-DD]   (e.g. 0.16.5)}"
version="${version#v}"
date="${2:-$(date +%F)}"
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
file="$repo_root/CHANGELOG.md"

[ -f "$file" ] || { echo "no CHANGELOG.md at $file" >&2; exit 1; }
case "$version" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "'$version' is not a x.y.z version" >&2; exit 1 ;;
esac
case "$date" in
  [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]) ;;
  *) echo "'$date' is not a YYYY-MM-DD date" >&2; exit 1 ;;
esac

if grep -q "^##[[:space:]]*\[${version}\]" "$file"; then
  echo "CHANGELOG.md already has a [${version}] heading -- nothing to do"
  exit 0
fi

body="$(awk '
  /^#+[[:space:]]*\[?[Uu]nreleased\]?/ { inside = 1; next }
  inside && /^#+[[:space:]]/ && !/^###[[:space:]]/ { exit }
  inside && /^###[[:space:]]/ { next }
  inside { print }
' "$file" | tr -d '[:space:]')"
[ -n "$body" ] || { echo "[Unreleased] has no entries -- write them before releasing $version" >&2; exit 1; }

# The version this one follows, for the compare link: the first version heading
# below [Unreleased], read before we insert ours.
prev="$(awk '/^##[[:space:]]*\[[0-9]/ { gsub(/[^0-9.]/, "", $2); print $2; exit }' "$file")"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# Insert the version heading directly under [Unreleased], which keeps every
# entry where it is and leaves [Unreleased] empty for the next cycle.
awk -v v="$version" -v d="$date" '
  { print }
  !done && /^##[[:space:]]*\[?[Uu]nreleased\]?/ { print ""; print "## [" v "] - " d; done = 1 }
' "$file" > "$tmp"

# Link reference, above the newest existing one.
if [ -n "$prev" ] && grep -q "^\[${prev}\]:" "$tmp"; then
  awk -v v="$version" -v p="$prev" '
    !done && $0 ~ "^\\[" p "\\]:" { print "[" v "]: https://github.com/YawLabs/oam/compare/v" p "...v" v; done = 1 }
    { print }
  ' "$tmp" > "$tmp.2" && mv "$tmp.2" "$tmp"
fi

cp "$tmp" "$file"
echo "CHANGELOG.md: [Unreleased] -> ## [${version}] - ${date}${prev:+ (compare link from v$prev)}"
echo "next: git add CHANGELOG.md && git commit -m \"docs(changelog): release ${version}\""
