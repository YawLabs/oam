# Contributing

Thanks for considering it. Until the first public release the codebase moves fast; issues and
small PRs are welcome, large changes should start as a discussion.

## Ground rules

- **DCO:** every commit is signed off (`git commit -s`), certifying the
  [Developer Certificate of Origin](https://developercertificate.org/). No CLA, ever
  (see GOVERNANCE.md).
- **AI-assisted contributions are welcome** and held to the same gates as everything else —
  read AI-POLICY.md first. Practical implications: keep PRs under the changeset limits, never
  modify a test in the same PR as the implementation it gates, and label agent-authored
  commits.
- **`unsafe`** must be justified AT ITS SITE, and the right form depends on the site: a
  `// SAFETY:` comment on an unsafe block or `unsafe impl`, a `/// # Safety` doc section on a
  public unsafe fn, and neither on an `unsafe extern` foreign module -- clippy rejects a
  `// SAFETY:` comment on those last two. This is enforced per-site on every crate
  (`undocumented_unsafe_blocks`, `unnecessary_safety_comment`, `missing_safety_doc`, all
  `deny`), so a misplaced comment is a hard CI error. The gate (`scripts/ci-local.sh`)
  separately ratchets the per-crate unsafe COUNT, which may only go down.
- **Windows is tier-1.** If your change can't pass the CI gate on windows-arm64 (the daily dev
  target), it doesn't merge; win-x64 is exercised by the emulated release leg
  (`scripts/release-local.sh`).

## Dev setup

Rust stable (pinned by `rust-toolchain.toml`). `cargo build --workspace`,
`cargo test --workspace`, `cargo clippy --workspace`, `cargo fmt --check`.

Node exactly as pinned by `.node-version` for the conformance steps: the node-differential
compares oam with the `node` on PATH, and `cargo run -p xtask -- conformance` (and the
`ci-local.sh` preflight) refuse any other version, because every recorded expectation was
measured against that one. `OAM_ALLOW_NODE_MISMATCH=1` downgrades the refusal to a warning for
an ad-hoc run. The remote build legs provision the pinned Node themselves.

CI is script-driven, not GitHub Actions: `./scripts/ci-local.sh` runs the full gate
(fmt, clippy `-D warnings`, build, tests, smoke, conformance, node-suite ratchet,
THIRD_PARTY_LICENSES drift, unsafe audit) and installs as a pre-push hook. Cross-platform legs run on remote
build hosts via `scripts/release-local.sh` / `scripts/node-compat-measure.sh` /
`scripts/bench-platforms.sh`.

### Tests touching process-global state

Cargo runs unit tests in one process on parallel threads by default, so tests sharing a
process-global race with each other: registries keyed by heap address, static caches, lazily
initialized once-cell state. Address-keyed tables are the worst case -- a key freed by one test
is instantly re-issued by the allocator to a sibling, so a lookup finds a foreign entry and an
assertion fails only under parallelism (a release-leg flake, not a local one).

The pattern: a `#[cfg(test)] static TEST_LOCK: std::sync::Mutex<()>` taken as the first
statement of every test that touches the state, plus a guard helper that locks with
`.unwrap_or_else(std::sync::PoisonError::into_inner)` so one test's panic doesn't cascade into
misleading lock-poison failures in the others. Serializing those tests is the deliberate trade
against cargo's parallelism.

Working exemplar: `crates/oam_napi_test_addon/src/lib.rs` (`mod tests`, the `TEST_LOCK` /
`test_guard` pair). Don't bother when the state is private to a single test, or when the tests
have been proven independent.
