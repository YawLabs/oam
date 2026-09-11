#!/bin/bash
# =============================================================================
# THIRD_PARTY_LICENSES.md drift: the decision logic, extracted so it can be
# TESTED and SHARED
# =============================================================================
# Every released oam binary statically links ~380 crates, so their notices ship
# with it as THIRD_PARTY_LICENSES.md -- a GENERATED file (`cargo about generate
# about.hbs`) that Cargo.lock changes silently invalidate. Two scripts care:
#
#   - ci-local.sh step 10 diffs a fresh generate against the committed file and
#     fails on drift. But `--fast` skipped that step unconditionally, so a PR
#     that changed Cargo.lock and gated with --fast landed stale attribution on
#     main, and the RELEASE run was the first thing to notice (v0.15.0, #114's
#     reqwest -> tower-http -> async-compression addition). The fast path now
#     asks attribution_fast_decision below: skip only when nothing that feeds
#     the file changed relative to origin/main.
#   - release-local.sh reconciles the file in preflight, the way it already
#     reconciles the version: regenerate, and if the committed copy drifted,
#     commit the fresh one and land it on main BEFORE the tag is created. A
#     release run should make the tree releasable, not discover 20 minutes in
#     that it is not.
#
# What is here is the part both need and the part worth testing: which paths
# count as inputs, the CR-insensitive comparison, the human-readable delta for
# a commit body, and the --fast skip decision. The cargo-about invocation
# itself is one line and stays with the callers.
#
# NOTE ON CR BYTES. Upstream license texts carry a handful of literal CR bytes
# (41 at the time of writing), and .gitattributes pins the file `-text` so the
# committed bytes match a fresh generate exactly. A clone made before that rule
# landed, or a stray core.autocrlf, still has to pass -- so every comparison
# here strips CR first. The bytes carry no meaning; the line endings are not
# what the gate is for.
#
# NOTE ON PIPES. ci-local.sh runs under `set -o pipefail` and `grep -q` exits at
# its first match, so a `<big file> | grep -q` can SIGPIPE the writer and turn a
# match into a non-zero pipeline. Nothing here pipes into grep -q; comparisons
# go through `diff -q` on process substitutions (which reads both sides fully)
# or bash builtins.
# =============================================================================

# attribution_inputs -- the tracked paths whose change can move the generated
# file. Printed one per line, only those that exist in the index.
#
# Cargo.lock is the obvious one (a crate enters or leaves the graph). The
# manifests are included too because `publish = false` is what about.toml keys
# its private-crate filter on, and a feature or target-specific dependency
# change starts in a Cargo.toml -- Cargo.lock usually moves with it, but
# "usually" is the word this list exists to remove. about.toml / about.hbs are
# the generator's config and template. The output file itself is an input in
# the sense that matters: a hand edit to it is exactly a change that should be
# re-verified against a fresh generate.
#
# `git ls-files` rather than a shell glob: the list must be the SAME set on
# every box, and a glob picks up an untracked scratch manifest.
attribution_inputs() {
  git ls-files -- Cargo.lock Cargo.toml 'crates/*/Cargo.toml' xtask/Cargo.toml \
    about.toml about.hbs THIRD_PARTY_LICENSES.md
}

# attribution_changed_paths <base> -- the inputs that differ between <base>
# (a commit) and the WORKING TREE, one per line. Working tree rather than HEAD
# on purpose: this runs from a pre-push hook and from a dev loop, and an
# uncommitted Cargo.lock change is the case most worth catching.
attribution_changed_paths() {
  local base="$1" inputs
  inputs="$(attribution_inputs)"
  [ -n "$inputs" ] || return 0
  # shellcheck disable=SC2086 -- word-splitting the newline list is the intent
  git diff --name-only "$base" -- $inputs
}

# attribution_fast_decision <base> <changed-paths> -- the --fast skip rule,
# as a pure function of what the caller measured. Prints ONE line:
#   run:<reason>    the attribution step must run even under --fast
#   skip:<reason>   it may be skipped
# Kept free of git so scripts/test-scripts.sh can drive it with fixtures.
#
# The rule fails toward RUNNING. No base to compare against (no origin/main
# fetched, a detached checkout) is "cannot prove nothing changed", which is not
# the same as "nothing changed". An origin/main that is merely stale-behind
# makes the diff a superset and the step run more often -- the safe direction.
attribution_fast_decision() {
  local base="$1" changed="$2"
  if [ -z "$base" ]; then
    printf 'run:no origin/main to compare against -- cannot prove the attribution inputs are unchanged\n'
    return 0
  fi
  if [ -n "$changed" ]; then
    printf 'run:attribution inputs changed since origin/main: %s\n' "$(printf '%s' "$changed" | tr '\n' ' ')"
    return 0
  fi
  printf 'skip:Cargo.lock, the manifests, about.toml/about.hbs and THIRD_PARTY_LICENSES.md are unchanged vs origin/main\n'
}

# attribution_matches <fresh> <committed> -- 0 when the two files are the same
# modulo CR bytes, 1 otherwise. `diff -q` rather than `cmp`: both sides go
# through `tr -d '\r'` first (see the CR note above), and diff reads both
# process substitutions to the end.
attribution_matches() {
  local fresh="$1" committed="$2"
  diff -q <(tr -d '\r' <"$fresh") <(tr -d '\r' <"$committed") >/dev/null 2>&1
}

# attribution_delta <committed> <fresh> -- the crate-level story of a drift,
# for a commit body or a log line: one `+ name version` per crate that entered
# the graph, one `- name version` per crate that left, in file order. The
# generated file lists every crate under a "Used by:" heading as `- name ver`,
# so the added/removed `- ` lines of a plain diff ARE the crate delta; the
# license-count summary line ("Apache License 2.0 -- 257 crate(s)") and any
# license-text change are deliberately not reported here -- they are
# consequences, and the diff itself is in the PR for anyone who wants them.
#
# No interval expressions in the awk (mawk matches nothing for those), and no
# `grep` so a delta of zero lines cannot look like a failure.
#
# diff's exit status is captured, not piped: 1 means "the files differ", which
# is the whole reason this is being called, and under the callers' `set -e -o
# pipefail` a bare `diff | awk` would abort the script on exactly that. Only
# 2 ("trouble" -- a side could not be read) propagates.
attribution_delta() {
  local committed="$1" fresh="$2" raw rc=0
  # Checked up front: a missing side would make its process substitution
  # empty, diff would exit 1, and the "delta" would be every crate in the
  # other file -- a clean-looking answer to a broken question.
  [ -r "$committed" ] && [ -r "$fresh" ] || return 2
  raw="$(diff <(tr -d '\r' <"$committed") <(tr -d '\r' <"$fresh") 2>/dev/null)" || rc=$?
  [ "$rc" -le 1 ] || return "$rc"
  # The per-license summary lines ("- Apache License 2.0 -- 257 crate(s)")
  # share the `- ` prefix; they move whenever a crate does and are not crates.
  printf '%s\n' "$raw" | awk '
    / crate\(s\)$/ { next }
    /^> - / { sub(/^> - /, ""); print "+ " $0; next }
    /^< - / { sub(/^< - /, ""); print "- " $0; next }
  '
}
