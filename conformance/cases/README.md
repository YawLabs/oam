# Node-differential conformance cases

Each `.mjs` here runs under BOTH oam and Node; the harness compares
stdout and exit codes byte-for-byte. A case passes when the two runtimes
are indistinguishable.

Rules for cases:
- Deterministic output only: no randomness, no timing values, no
  absolute paths, no locale-dependent formatting.
- One concern per file, named `NN-topic.mjs`. Ordering is cosmetic, but the
  number is the case's name everywhere else ("case 378" in docs, comments and
  the changelog), so no two files may share one -- an xtask unit test
  (`no_two_cases_share_a_number`) refuses a duplicate.
- Keep `util.inspect`-style output to SMALL structures (large-object
  line-wrapping is presentation, not semantics).

The corpus seeds from the parity batteries that hardened each compat
slice; every confirmed-and-fixed divergence should eventually have a
line here so it can never silently return.
