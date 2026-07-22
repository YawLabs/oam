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
- **`unsafe`** requires a `// SAFETY:` comment. The CI gate (`scripts/ci-local.sh`) audits the
  per-crate count.
- **Windows is tier-1.** If your change can't pass the CI gate on windows-arm64 (the daily dev
  target), it doesn't merge; win-x64 is exercised by the emulated release leg
  (`scripts/release-local.sh`).

## Dev setup

Rust stable (pinned by `rust-toolchain.toml`). `cargo build --workspace`,
`cargo test --workspace`, `cargo clippy --workspace`, `cargo fmt --check`.

CI is script-driven, not GitHub Actions: `./scripts/ci-local.sh` runs the full gate
(fmt, clippy `-D warnings`, build, tests, smoke, conformance, node-suite ratchet,
THIRD_PARTY_LICENSES drift, unsafe audit) and installs as a pre-push hook. Cross-platform legs run on remote
build hosts via `scripts/release-local.sh` / `scripts/node-compat-measure.sh` /
`scripts/bench-platforms.sh`.
