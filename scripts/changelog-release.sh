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

# Headings are matched LITERALLY: a version spliced into a regex would let each
# of its dots match any character.
has_heading() {
  awk -v head="## [$1]" 'index($0, head) == 1 { found = 1; exit } END { exit !found }' "$2"
}

if has_heading "$version" "$file"; then
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

# The version this one follows, for the compare link: whatever sits between the
# brackets of the first version heading below [Unreleased], read before ours is
# inserted. Taken whole, so a pre-release such as 0.17.0-rc.1 survives intact.
prev="$(awk '/^##[[:space:]]*\[[0-9]/ {
  s = $0; sub(/^##[[:space:]]*\[/, "", s); sub(/\].*$/, "", s); print s; exit
}' "$file")"

tmp="$(mktemp)"
trap 'rm -f "$tmp" "$tmp.2"' EXIT

# Insert the version heading directly under [Unreleased], which keeps every
# entry where it is and leaves [Unreleased] empty for the next cycle. The
# heading is found at any depth, as the release gate and the body scan above
# find it.
awk -v v="$version" -v d="$date" '
  { print }
  !done && /^#+[[:space:]]*\[?[Uu]nreleased\]?/ { print ""; print "## [" v "] - " d; done = 1 }
' "$file" > "$tmp"
has_heading "$version" "$tmp" \
  || { echo "found no [Unreleased] heading to put [${version}] under -- CHANGELOG.md is unchanged" >&2; exit 1; }

# Link references, matched literally as the headings are: the new version's
# above the newest existing one, and [Unreleased] moved on to compare from it.
if [ -n "$prev" ]; then
  awk -v v="$version" -v p="$prev" '
    !done && index($0, "[" p "]:") == 1 {
      print "[" v "]: https://github.com/YawLabs/oam/compare/v" p "...v" v
      done = 1
    }
    { print }
  ' "$tmp" > "$tmp.2" && mv "$tmp.2" "$tmp" || { echo "could not rewrite the link references -- CHANGELOG.md is unchanged" >&2; exit 1; }
fi
awk -v v="$version" '
  index(tolower($0), "[unreleased]:") == 1 {
    print "[Unreleased]: https://github.com/YawLabs/oam/compare/v" v "...HEAD"
    next
  }
  { print }
' "$tmp" > "$tmp.2" && mv "$tmp.2" "$tmp" || { echo "could not rewrite the link references -- CHANGELOG.md is unchanged" >&2; exit 1; }

cp "$tmp" "$file"
echo "CHANGELOG.md: [Unreleased] -> ## [${version}] - ${date}${prev:+ (compare link from v$prev)}"
echo "next: git add CHANGELOG.md && git commit -m \"docs(changelog): release ${version}\""
