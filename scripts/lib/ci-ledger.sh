#!/bin/bash
# =============================================================================
# The ci-local.sh step ledger: which steps already passed on EXACTLY this tree
# =============================================================================
# ci-local.sh is fourteen steps and about seventy minutes, and it is
# release-local.sh's local gate. When one late step fails -- step 13's script
# suite flaked under load on 2026-10-04, an hour in -- the operator re-runs
# the whole file, and the twelve steps that passed run again to reach the one
# that did not. The ledger remembers each step that passed, keyed to the tree
# and toolchain it passed on, so a re-run on the same tree skips straight to
# the work that is still owed.
#
# The key is the whole argument. A skipped step is a claim -- "this step
# passed on what is on disk right now" -- and the claim holds only when
# everything a step can read is unchanged:
#   - HEAD, because the conformance receipts stamp the commit they measured;
#   - the WORKING TREE, untracked files included: an uncommitted edit, a new
#     file, a stray scratch manifest all change what a step sees. Measured as
#     a git tree id written from a scratch index (below), so an ignored path
#     (target/, node_modules/, the ledger itself) does not perturb it;
#   - rustc -V and cargo -V, because a toolchain update changes what fmt,
#     clippy and the build mean without touching one tracked byte;
#   - the ci-local.sh flags that change what a step means (--fast, --no-tests):
#     a pass under --no-tests says nothing about step 6.
# One token, sha256 over all of it. Anything else -- node's version, an env
# knob -- is NOT in the key; steps whose outcome such a thing decides are not
# marked when they take the self-skip path (ci-local.sh marks only a step
# that actually RAN and passed).
#
# NOTE ON THE SCRATCH INDEX. The tree id comes from `git add -A` into a
# private index (GIT_INDEX_FILE) followed by `git write-tree`, so the real
# index is never touched. That private index is seeded with a COPY of the real
# one, and the copy is correctness, not speed. Measured on the win-arm64 dev
# box: from an empty index, `git add -A` re-hashes every file from working-tree
# bytes and the resulting tree differs from HEAD's on a CLEAN checkout --
# core.filemode=false drops the scripts' exec bits, and the two tracked
# `.oam-eval-*.cjs` files under the vendored corpus match a .gitignore rule,
# so an empty index never picks them up. Seeded from the real index, an
# unchanged file keeps its committed blob and mode and only a file git itself
# would call modified is re-hashed: a clean checkout yields HEAD's tree id,
# and `git status`-clean means the same key run after run. (7.7s cold vs
# 0.46s warm on the same box, which is the pleasant side effect.)
#
# NOTE ON ATOMICITY. A mark is a temp file in the ledger dir renamed over the
# entry, and a pass check requires the entry to hold EXACTLY the key. A run
# killed mid-write (Ctrl-C, a lost console) therefore leaves either the old
# entry or the new one, never a half-written key that happens to count.
#
# NOTE ON PIPES. ci-local.sh runs under `set -e -o pipefail`. Nothing here
# pipes into `grep -q`; the comparison is a bash string test and the hash is
# `sha256sum` reading a here-string to EOF.
#
# Every function takes the ledger dir and the key as ARGUMENTS, so
# scripts/test-scripts.sh drives passed/mark/clear on a fixture directory
# with no git, and the tree id and the hash composition each on their own.
# No trap, no global state, nothing exported: this is sourced by ci-local.sh,
# which owns the one EXIT trap.
# =============================================================================

# ci_ledger_tree_id -- the git tree id of the working tree as it stands,
# untracked-but-not-ignored files included, printed on stdout. Returns 1 with
# a reason on stderr when git cannot answer (not a repo, an unborn HEAD is
# fine here -- write-tree needs no commit). Works from any directory inside
# the repo: the pathspec is the work tree root.
ci_ledger_tree_id() {
  local real_index scratch tree rc=0
  real_index="$(git rev-parse --git-path index 2>/dev/null)" \
    || { echo "ci_ledger_tree_id: not inside a git work tree" >&2; return 1; }
  scratch="$(mktemp)" || return 1
  # Seed from the real index (see the header). A repo with no index yet -- a
  # fresh `git init` -- has nothing to copy, and the empty scratch file is the
  # empty index git would create anyway.
  if [ -f "$real_index" ]; then cp "$real_index" "$scratch" || { rm -f "$scratch"; return 1; }; fi
  # Both commands under the scratch index; the status is kept and the scratch
  # removed before it is acted on, so a failure never strands a temp file (the
  # caller owns the one EXIT trap). The add's output is captured apart from
  # the tree id -- a warning it prints must not become part of the id.
  local add_out
  add_out="$(GIT_INDEX_FILE="$scratch" git add -A -- :/ 2>&1)" || rc=$?
  if [ "$rc" -eq 0 ]; then
    tree="$(GIT_INDEX_FILE="$scratch" git write-tree 2>&1)" || rc=$?
  else
    tree="$add_out"
  fi
  rm -f "$scratch"
  if [ "$rc" -ne 0 ]; then
    echo "ci_ledger_tree_id: git could not hash the working tree: $tree" >&2
    return 1
  fi
  printf '%s\n' "$tree"
}

# ci_ledger_key_from <head> <tree-id> <toolchain> [flag]... -- the key as a
# pure function of what the caller measured: one sha256 token over the parts,
# newline-separated so no part can run into the next. <toolchain> is the
# `rustc -V` and `cargo -V` lines; the flags are ci-local.sh's, in the order
# the caller passes them (ci-local.sh passes a fixed order). Empty <head> or
# <tree-id> is refused: a key made from a missing part would be a key that
# matches the wrong tree.
ci_ledger_key_from() {
  local head="$1" tree="$2" toolchain="$3" flag parts; shift 3
  if [ -z "$head" ] || [ -z "$tree" ]; then
    echo "ci_ledger_key_from: head and tree id are required" >&2
    return 1
  fi
  parts="head=$head"$'\n'"tree=$tree"$'\n'"toolchain=$toolchain"
  for flag in "$@"; do parts="$parts"$'\n'"flag=$flag"; done
  # `sha256sum` prints "<hex>  -"; keep the hex. A here-string, read to EOF.
  local sum
  sum="$(sha256sum <<<"$parts")" || return 1
  printf '%s\n' "${sum%% *}"
}

# ci_ledger_key [flag]... -- the key for the current tree and toolchain, from
# git, rustc and cargo as found on PATH. Fails (1, reason on stderr) rather
# than printing a partial key when any of them cannot answer. ci-local.sh
# calls this once at the start and AGAIN after any step that may rewrite a
# tracked file, because a key computed before such a step describes a tree
# that no longer exists.
ci_ledger_key() {
  local head tree rustc_v cargo_v
  head="$(git rev-parse HEAD 2>/dev/null)" \
    || { echo "ci_ledger_key: git rev-parse HEAD failed -- no commit to key the ledger to" >&2; return 1; }
  tree="$(ci_ledger_tree_id)" || return 1
  rustc_v="$(rustc -V 2>/dev/null)" || { echo "ci_ledger_key: rustc -V failed" >&2; return 1; }
  cargo_v="$(cargo -V 2>/dev/null)" || { echo "ci_ledger_key: cargo -V failed" >&2; return 1; }
  ci_ledger_key_from "$head" "$tree" "$rustc_v"$'\n'"$cargo_v" "$@"
}

# ci_ledger_passed <dir> <key> <step-id> -- 0 when <dir>/<step-id> exists and
# holds exactly <key>; 1 for a missing entry, a different key, or a torn one.
# The entry is read whole and compared as a string (a trailing newline is the
# one thing `$(cat)` drops, and the one thing mark writes after the key).
ci_ledger_passed() {
  local dir="$1" key="$2" step="$3" held
  [ -n "$key" ] || return 1
  [ -f "$dir/$step" ] || return 1
  held="$(cat "$dir/$step" 2>/dev/null)" || return 1
  [ "$held" = "$key" ]
}

# ci_ledger_mark <dir> <key> <step-id> -- record that <step-id> passed under
# <key>: written to a temp file IN <dir> (so the rename is within one
# filesystem and therefore atomic) and moved over the entry. Refuses an empty
# key or step: an entry that matched an empty key would match every run that
# could not compute one.
ci_ledger_mark() {
  local dir="$1" key="$2" step="$3" tmp
  if [ -z "$key" ] || [ -z "$step" ]; then
    echo "ci_ledger_mark: key and step id are required" >&2
    return 1
  fi
  mkdir -p "$dir" || return 1
  tmp="$dir/.$step.$$.tmp"
  printf '%s\n' "$key" >"$tmp" || { rm -f "$tmp"; return 1; }
  mv -f "$tmp" "$dir/$step" || { rm -f "$tmp"; return 1; }
}

# ci_ledger_clear <dir> -- forget every step. The dir is removed and recreated
# empty. An empty argument is refused with a return, not `${1:?}`: that form
# EXITS a non-interactive shell, which in the test suite would end the run
# instead of failing one case -- and either way an `rm -rf` of "" is the thing
# being refused.
ci_ledger_clear() {
  local dir="${1:-}"
  if [ -z "$dir" ]; then echo "ci_ledger_clear: ledger dir required" >&2; return 1; fi
  rm -rf -- "$dir" || return 1
  mkdir -p "$dir"
}
