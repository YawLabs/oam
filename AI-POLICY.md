# AI Development Policy — "AI-built, human-accountable, machine-verified"

oam is developed heavily with AI agents. That is a strength — and the industry has a fresh,
named cautionary tale about doing it badly: a million-line, practically unreviewable
AI-generated rewrite merged into a runtime millions depend on. This policy makes that failure
mode structurally impossible here. Same tools, opposite discipline, receipts published.

## The gates (identical for human and AI changes)

1. **No fast path.** AI-authored changes pass exactly the review, test, and CI gates human
   changes do. There is no "the agent ran the tests" exemption.
2. **Changeset limits, CI-enforced.** No PR exceeds ~400 lines of logic change (~1,500 for
   mechanical/generated churn) without a documented decomposition reviewed first. Mega-merges
   are rejected by tooling, not by willpower.
3. **Test-first ports.** Behavior specs — usually differential tests against real Node
   behavior — are reviewed and merged BEFORE the implementation. Implementation PRs may not
   modify the tests that gate them.
4. **Named human accountability.** Every merge carries a responsible human reviewer. Initially
   that is the founder for everything; two-human review for `unsafe` and public-API changes
   once maintainers exist.
5. **`unsafe` budget.** Every `unsafe` block and `unsafe impl` carries a `// SAFETY:`
   justification and every public `unsafe fn` a `/// # Safety` section, enforced per-site on
   every crate by clippy (`undocumented_unsafe_blocks`, `unnecessary_safety_comment`,
   `missing_safety_doc`, all `deny`) under CI's `-D warnings` -- not by a counted metric.
   Honest residue: clippy structurally does not cover *private* `unsafe fn` definitions or
   `unsafe extern` blocks, so those rest on review. The published metric is unsafe-per-crate
   (`conformance/unsafe-budget.json`, a ratcheting CEILING that may only go down, covering
   every workspace member including `xtask`) with `oam_engine` quarantined as the FFI
   boundary, benchmarked against deno_core (the honest comparable for a V8 embedding).
   Unjustified regressions fail the build.
6. **Provenance labels.** Commits are labeled human/agent-authored. The public stat we aim to
   keep true: "X% AI-authored, 100% human-reviewed, 0 unreviewed merges."
7. **Copyright provenance.** Agent-generated code is screened for verbatim reproduction of
   licensed training data. Ported shim logic (e.g. from Deno or workerd — both
   MIT/Apache-licensed) carries NOTICE-preserving attribution as a mechanical PR-template step.
