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
# WHY NOT `git grep`: the obvious one-liner is a false-clean trap. `git grep -I`
# skips files git considers BINARY, and a file containing a NUL is exactly what
# git calls binary -- so `-I` skips precisely the files this gate is hunting and
# reports success. Measured on a planted NUL: `-I` finds nothing, `-a` finds it.
# Using `-a` instead then matches every real image and archive, so the binary
# question has to be answered deliberately rather than delegated. That is what
# the node pass below does, and why it does not shell out per file.
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
    sed -n '2,39p' "$0" | sed 's/^# \{0,1\}//'
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

command -v node >/dev/null 2>&1 || {
  # A gate that cannot run must say so rather than report clean.
  echo "check-control-bytes: node not on PATH; cannot scan" >&2
  exit 2
}

MODE="$MODE" node --input-type=module -e '
import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { extname } from "node:path";

const staged = process.env.MODE === "staged";

// Bytes that legitimately appear in text.
const OK = new Set([0x09, 0x0a, 0x0d]);

// A DENYLIST of binary extensions rather than an allowlist of text: a text
// extension nobody listed should still be scanned. The cost of scanning
// something unexpected is one loud finding a human can waive with the marker;
// the cost of skipping is silence, which is the failure this gate exists for.
const BINARY = new Set([
  ".png", ".jpg", ".jpeg", ".gif", ".ico", ".webp", ".avif", ".bmp",
  ".pdf", ".zip", ".gz", ".tgz", ".xz", ".bz2", ".7z", ".tar",
  ".woff", ".woff2", ".ttf", ".otf", ".eot",
  ".wasm", ".node", ".exe", ".dll", ".dylib", ".so", ".a", ".rlib", ".pdb",
  ".mp4", ".webm", ".mov", ".mp3", ".wav", ".ogg", ".snap",
]);

const listArgs = staged
  ? ["diff", "--cached", "--name-only", "--diff-filter=ACMR", "-z"]
  : ["ls-files", "-z"];
const files = execFileSync("git", listArgs, { maxBuffer: 64 * 1024 * 1024 })
  .toString("utf8")
  .split("\0")
  .filter(Boolean);

const hits = [];
for (const f of files) {
  if (BINARY.has(extname(f).toLowerCase())) continue;

  let buf;
  try {
    // Read the INDEX in staged mode: the working tree can differ from what is
    // about to be committed, and the commit is what the gate is about.
    buf = staged
      ? execFileSync("git", ["show", `:${f}`], { maxBuffer: 64 * 1024 * 1024 })
      : (existsSync(f) ? readFileSync(f) : null);
  } catch {
    continue; // unreadable, a submodule, a broken symlink -- not this gate.
  }
  if (!buf) continue;

  let bad = -1;
  for (let i = 0; i < buf.length; i++) {
    const b = buf[i];
    if (b < 0x20 && !OK.has(b)) { bad = i; break; }
  }
  if (bad < 0) continue;
  if (buf.includes("control-byte-ok")) continue;

  // Line number, so the finding is actionable. The matched line itself is
  // deliberately NOT printed -- it carries the control byte, and echoing it
  // re-injects the escape into the readers terminal.
  let line = 1;
  for (let i = 0; i < bad; i++) if (buf[i] === 0x0a) line++;
  hits.push(`  ${f}:${line}  byte 0x${buf[bad].toString(16).padStart(2, "0")} at offset ${bad}`);
}

if (hits.length === 0) process.exit(0);
console.error("Raw control bytes in tracked text:");
for (const h of hits) console.error(h);
console.error(`
These are invisible in a terminal, make \`git diff\` report the file as binary
(so the change cannot be reviewed), and are valid inside a string literal --
so fmt, clippy and the tests all pass with them present.

Build such a byte from a numeric code point rather than typing an escape, or
add the marker control-byte-ok to the file if it is deliberate.`);
process.exit(1);
'
