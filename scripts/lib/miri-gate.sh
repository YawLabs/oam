#!/bin/bash
# =============================================================================
# Miri aliasing-model gate: the decision logic, extracted so it can be TESTED
# =============================================================================
# ci-local.sh step 12 runs `cargo +nightly miri test -p oam_aliasing_model` in
# two halves and decides pass/fail from the exit status plus the output text.
# That decision was the last piece of the gate with no coverage of its own --
# it was verified once by hand, against a simulation, and then trusted. Every
# way it can go wrong makes the gate quieter, not louder:
#
#   - libtest exits 0 when a filter matches NOTHING (verified: `cargo test
#     -p oam_aliasing_model -- --ignored no_such_name` prints "running 0 tests"
#     and returns 0). So a deleted, renamed, or un-`#[ignore]`d model reads as
#     "the run succeeded", which is why neither half may key on the status
#     alone.
#   - A held_* case that fails for ANY other reason -- a plain assert, an ICE,
#     an OOM, a compile error -- would read as "still rejected" unless the
#     failure is required to carry a UB diagnosis.
#
# So both halves classify (status, output) into a named verdict, and ci-local.sh
# maps verdicts to ok/ko. The verdicts are the testable surface:
# scripts/test-scripts.sh drives these functions with CAPTURED miri output on
# every box, including the many that have no nightly toolchain at all. A gate
# whose logic only runs where miri is installed is a gate that is unchecked
# almost everywhere.
#
# NOTE ON PIPES. Everything here matches with bash builtins -- `[[ == * ]]`,
# `=~`, a read loop over a here-string -- and never `printf ... | grep -q`.
# ci-local.sh runs under `set -o pipefail` and `grep -q` exits at its FIRST
# match, so a large miri report SIGPIPEs the writer and the pipeline returns
# 141: a UB report would be misread as "no UB reported" precisely because it
# was big. That was a real bug (#95). Do not reintroduce the shape here.
# =============================================================================

# The known-UB models the gate must still see REJECTED. Declared here rather
# than inline in ci-local.sh so the loop and the tests that check these names
# still exist read the SAME list -- a renamed case then fails the suite on
# every box, instead of silently shrinking the loop's coverage where miri runs.
# shellcheck disable=SC2034  # consumed by ci-local.sh and test-scripts.sh
OAM_MIRI_HELD_CASES=(
  held_two_exclusive_scopes_is_ub
  held_deref_deleted_handle_is_use_after_free
  held_derive_then_move_box_is_ub
)

# How many CURRENT-design models the unfiltered run must actually execute. A
# bidirectional ratchet, same contract as the unsafe budget: a run that executes
# FEWER has lost coverage (the failure this catches), and one that executes MORE
# means models were added and the floor needs re-blessing here. Kept honest by a
# test that counts the non-`#[ignore]`d `#[test]` functions in the crate.
OAM_MIRI_CURRENT_MIN=6

# miri_executed_count <output>
# Sum of "N passed" across every libtest summary line in <output> (a cargo test
# run emits one per test binary, plus one for doctests). Prints the total and
# returns 0; returns 1 with no output when the run emitted no summary at all,
# which means it never got as far as running tests.
miri_executed_count() {
  local out="$1" total=0 found=0 line
  while IFS= read -r line; do
    if [[ "$line" =~ ^[[:space:]]*test\ result:.*[^0-9]([0-9]+)\ passed ]]; then
      total=$(( total + BASH_REMATCH[1] ))
      found=1
    fi
  done <<< "$out"
  [ "$found" -eq 1 ] || return 1
  printf '%s\n' "$total"
}

# miri_current_verdict <status> <output>
# Classifies the FIRST half -- the unfiltered run of the current-design models.
# Prints exactly one of:
#   pass           every model ran and miri accepted it
#   rejected       miri rejected a current-design model (a real regression in a
#                  pointer discipline napi.rs relies on)
#   not-exercised  exit 0, but fewer than OAM_MIRI_CURRENT_MIN models ran -- the
#                  vacuous pass, which the exit status cannot distinguish from
#                  a real one
#   unparsable     exit 0 with no libtest summary at all
miri_current_verdict() {
  local status="$1" out="$2" count
  if [ "$status" -ne 0 ]; then printf 'rejected\n'; return 0; fi
  if ! count="$(miri_executed_count "$out")"; then printf 'unparsable\n'; return 0; fi
  if [ "$count" -lt "$OAM_MIRI_CURRENT_MIN" ]; then printf 'not-exercised\n'; return 0; fi
  printf 'pass\n'
}

# miri_held_verdict <status> <output>
# Classifies ONE held_* case -- a model of a shape that was a real bug, run
# `--ignored` and required to still be rejected ON A UB DIAGNOSIS. Prints
# exactly one of:
#   rejected-ub        the only passing verdict: non-zero AND miri named UB
#   accepted           exit 0 with the case actually executed -- miri no longer
#                      considers the modelled shape UB, so the harness has lost
#                      its teeth
#   missing            exit 0 with ZERO tests executed -- the case was renamed,
#                      deleted, or is no longer `#[ignore]`d, so `--ignored
#                      <name>` matched nothing. Reported apart from `accepted`
#                      because the fix is entirely different
#   failed-without-ub  non-zero without a UB diagnosis: the case still fails,
#                      but no longer for the reason it names
miri_held_verdict() {
  local status="$1" out="$2" count
  if [ "$status" -eq 0 ]; then
    if count="$(miri_executed_count "$out")" && [ "$count" -eq 0 ]; then
      printf 'missing\n'
    else
      printf 'accepted\n'
    fi
    return 0
  fi
  if [[ "$out" == *"Undefined Behavior"* ]]; then printf 'rejected-ub\n'; return 0; fi
  printf 'failed-without-ub\n'
}
