# Governance — the oam Covenant

oam exists to be the reliable, vendor-neutral TypeScript runtime. Its governance is designed so
that promise is structural, not aspirational.

## The Covenant

1. **Apache-2.0, forever.** oam will never relicense, never go source-available, never adopt a
   "business source" license. The patent grant travels with every copy.
2. **DCO, never CLA.** Contributions are accepted under the Developer Certificate of Origin
   (`Signed-off-by`). oam will never require a contributor license agreement — a CLA is
   relicensing optionality, and we are removing that option on purpose.
3. **Model-agnostic by charter.** oam favors no AI vendor. Built-in AI primitives (`oam:ai`)
   speak open protocols and every provider equally. The project's own AI-assisted development
   (see AI-POLICY.md) may use any vendor's tools; the runtime privileges none.

## Decision making

Pre-1.0: BDFL model — the founder (Jeff) is the final reviewer and decision maker.
Maintainers are added by demonstrated contribution; each owns review authority over named
crates. Two-human review is required for `unsafe` code and public-API changes once two or
more maintainers exist.

## Foundation triggers (published now, executed when met)

Trademark and governance transfer to a neutral foundation (OpenJS or independent) when EITHER:

- three or more unaffiliated companies run oam in production and ask for shared governance, OR
- the project takes institutional funding exceeding the founder's control comfort.

## Succession (bus-factor plan)

If the founder is unable to continue: maintainers (or, before any exist, the most recent
release's co-signers) gain admin on the GitHub org; DNS and signing keys are escrowed with
instructions; the Covenant binds successors. This file is the authoritative statement.

## Releases

Release cadence, semver policy, and the behavior-change log are specified in RELIABILITY.md.
Every release is signed; provenance (SLSA) attaches from the first public binary.
