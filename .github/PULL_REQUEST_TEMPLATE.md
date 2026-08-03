<!--
Large changes should start as a discussion before the PR (CONTRIBUTING.md).
Delete any section that genuinely does not apply — an empty heading is noise.
-->

## What this changes

<!-- One or two sentences. What behavior is different after this merges? -->

## Why

<!-- The problem, not the patch. Link the issue if there is one: Fixes #NNN -->

## How it was verified

<!--
CI here is script-driven, not GitHub Actions: nothing runs on push. State what
you ran and on what platform, so a reviewer knows what is actually covered.
-->

- [ ] `./scripts/ci-local.sh` passes locally (fmt, clippy `-D warnings`, build,
      tests, smoke, conformance, node-suite ratchet, THIRD_PARTY_LICENSES
      drift, unsafe audit)
- [ ] Platform(s) exercised: <!-- e.g. windows-arm64 (tier-1 dev target) -->
- [ ] New or changed behavior has a test that fails without this change

<!-- Paste the relevant tail of the gate output, or say which steps you skipped and why. -->

## Node compatibility

<!--
If this touches the Node-compatible surface, say what real node does and how
you confirmed the new behavior matches. Node is the specification.
-->

- [ ] Not applicable — this does not touch Node-visible behavior
- [ ] Behavior matches `node` (version: <!-- v22.x -->), verified by:

## Checklist

- [ ] Every commit is signed off (`git commit -s`) per the DCO — no CLA
- [ ] Any new `unsafe` block carries a `// SAFETY:` comment
- [ ] Conformance / node-suite scorecard stamps are regenerated if the numbers moved
- [ ] Public behavior changes are reflected in `CHANGELOG.md` under `## [Unreleased]`
- [ ] Docs updated if this changes a flag, an API, or an install path

## AI assistance

<!--
AI-assisted contributions are welcome and held to the same gates as everything
else. Read AI-POLICY.md. Two rules bite most often: keep the changeset inside
the limits, and never modify a test in the same PR as the implementation it
gates.
-->

- [ ] No AI assistance
- [ ] AI-assisted, and agent-authored commits are labeled per `AI-POLICY.md`
- [ ] Tests in this PR are not modified alongside the implementation they gate

## Attribution

<!--
Code ported or adapted from another project keeps its notices. This is a
mechanical step, not a judgment call (AI-POLICY.md, "Copyright provenance").
-->

- [ ] Nothing here is ported or adapted from another project
- [ ] Ported code is attributed in `NOTICE`, and its license is compatible with
      Apache-2.0. Source and upstream version:
