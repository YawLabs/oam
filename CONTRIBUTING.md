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
- **`unsafe`** requires a `// SAFETY:` comment. CI enforces coverage.
- **Windows is tier-1.** If your change can't pass windows-x64 + windows-arm64 CI, it doesn't
  merge.

## Dev setup

Rust stable (pinned by `rust-toolchain.toml`). `cargo build --workspace`,
`cargo test --workspace`, `cargo clippy --workspace`, `cargo fmt --check`.
