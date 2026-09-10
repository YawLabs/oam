#!/usr/bin/env bash
#
# check-control-bytes.sh -- refuse a tracked text file carrying a raw C0
# control byte (anything under 0x20 that is not tab, LF or CR).
#
# Why this exists: writing source through a shell heredoc silently consumes one
# level of backslash escaping, so an intended escape sequence -- backslash, `u`,
# four hex digits -- arrives as a single REAL control byte and is written into
# the file. It bites hardest when the content is ABOUT control characters: a
# sanitizer, a terminal-escape constant, a test fixture.
#
# The reason it needs its own gate is that no ordinary gate can see it. A NUL
# inside a string literal is semantically valid, so the formatter, the
# type-checker and the whole test suite pass with it present, before and after.
# Five literal NULs reached `main` in a sibling repo (YawLabs/mcp, commit
# b365955) with every gate green, and were found only by a byte-level scan.
#
# Review does not catch it either, and for a worse reason: `git diff` reports a
# file containing one as "Binary file ... matches" and prints no hunks at all,
# so the change is not merely easy to overlook -- it is unviewable in the tool
# the reviewer is using.
#
# Usage:
#   scripts/check-control-bytes.sh              # scan tracked files
#   scripts/check-control-bytes.sh --staged     # scan staged content only
#
# Exit 0 clean, 1 on a finding, 2 on a usage error.
#
# Escape hatch: `control-byte-ok` anywhere in the file skips it, for the rare
# case where the byte is the point. Deliberately in-band rather than a path
# list, so the justification lives beside the bytes it excuses.

set -uo pipefail

MODE="tracked"
case "${1:-}" in
  "") ;;
  --staged) MODE="staged" ;;
  -h | --help)
    sed -n '2,31p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *)
    echo "usage: $0 [--staged]" >&2
    exit 2
    ;;
esac

git rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
  echo "not a git repo" >&2
  exit 2
}

cd "$(git rev-parse --show-toplevel)"

# The whole scan is one `git grep`, which is why this costs ~0.5s on an
# 859-file tree rather than the ~10 minutes a per-file `od` loop took.
#
# Three flags carry the weight:
#   -I  skips files git considers binary, so images and archives cost nothing
#       and need no extension denylist to maintain.
#   -P  gives PCRE byte ranges. The BRE/ERE fallback cannot express \x00.
#   -n  gives line numbers, so a finding is actionable rather than "somewhere
#       in this 4000-line file".
#
# The range is every C0 byte EXCEPT tab (09), LF (0a) and CR (0d) -- the three
# that legitimately appear in text.
PATTERN='[\x00-\x08\x0b\x0c\x0e-\x1f]'

if ! git grep -qP '' -- . >/dev/null 2>&1; then
  # -P needs a git built with PCRE. Degrade honestly: a gate that cannot run
  # must say so, not report clean.
  echo "  n/a -- this git has no PCRE support (-P); cannot scan for control bytes" >&2
  exit 0
fi

if [ "$MODE" = "staged" ]; then
  # --cached scans the INDEX, which is what a pre-commit gate must check: the
  # working tree can differ from what is about to be committed.
  hits=$(git grep -I -n -P --cached "$PATTERN" -- . 2>/dev/null || true)
else
  hits=$(git grep -I -n -P "$PATTERN" -- . 2>/dev/null || true)
fi

[ -z "$hits" ] && exit 0

# Drop any file carrying the opt-out marker. Done here rather than in the grep
# so the marker can sit anywhere in the file.
found=0
while IFS= read -r line; do
  [ -n "$line" ] || continue
  file="${line%%:*}"
  if [ "$MODE" = "staged" ]; then
    marker=$(git show ":$file" 2>/dev/null | grep -cF 'control-byte-ok' || true)
  else
    marker=$(grep -cF 'control-byte-ok' "$file" 2>/dev/null || true)
  fi
  [ "${marker:-0}" -gt 0 ] && continue
  if [ "$found" -eq 0 ]; then
    echo "Raw control bytes in tracked text:"
    found=1
  fi
  # Print file:line only. The matched LINE is deliberately not echoed -- it
  # contains the control byte, and printing it re-injects the escape into the
  # reader's terminal, which is the mess this gate exists to stop.
  echo "  ${line%%:*}:$(printf '%s' "$line" | cut -d: -f2)"
done <<< "$hits"

[ "$found" -eq 0 ] && exit 0

cat >&2 <<'EOF'

These are invisible in a terminal, make `git diff` report the file as binary
(so the change cannot be reviewed), and are valid inside a string literal --
so fmt, clippy and the tests all pass with them present.

Build such a byte from a numeric code point rather than typing an escape, or
add the marker control-byte-ok to the file if it is deliberate.
EOF
exit 1
