//! Repo automation. Invoked as `cargo run -p xtask -- <command>`.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

mod bench;
mod conformance;
mod node_suite;
mod unsafe_budget;

#[derive(Parser)]
#[command(name = "xtask", about = "oam repo automation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run micro-benchmarks against oam (and optionally node/bun/deno).
    ///
    /// Pass --release to benchmark the release binary instead of debug.
    /// Pass --compare to also run the same scripts under other runtimes.
    /// Pass --case <name> (repeatable) to re-measure only those cases and
    /// merge them into the committed BENCHMARKS.md + bench/results.json.
    Bench {
        /// Build and benchmark the release binary (cargo build --release).
        #[arg(long)]
        release: bool,
        /// Also run benchmarks under node, bun, deno (if found on PATH).
        #[arg(long)]
        compare: bool,
        /// Run only this case (repeatable); the result merges into the
        /// committed files instead of replacing them.
        #[arg(long = "case", value_name = "NAME")]
        cases: Vec<String>,
    },
    /// Run the conformance suites (WPT URL, Node differential, builtin
    /// surface) and regenerate CONFORMANCE.md + conformance/scorecard.json.
    ///
    /// Pass --release to test the release binary instead of debug.
    /// Alternatively set CONFORMANCE_RELEASE=1 in the environment.
    Conformance {
        /// Build and test the release binary (cargo build --release).
        #[arg(long)]
        release: bool,
    },
    /// Run a vendored subset of Node's own test suite under oam (oracle:
    /// exit 0 == pass, since Node core tests self-assert). Slice 1 of the
    /// Node-suite conformance harness; covers conformance/vendor/node/test/parallel.
    ///
    /// Pass --release to test the release binary instead of debug.
    NodeSuite {
        /// Build and test the release binary (cargo build --release).
        #[arg(long)]
        release: bool,
    },
    /// Bidirectional-ratchet gate for the `unsafe` surface (AI-POLICY.md gate
    /// 5). Counts real unsafe constructs per crate (ceiling, may only drop) and
    /// `// SAFETY:` comments (floor, may only rise) and diffs both against
    /// conformance/unsafe-budget.json.
    ///
    /// Pass --regen (alias --bless) to rewrite the baseline from the tree.
    UnsafeBudget {
        /// Regenerate conformance/unsafe-budget.json from the current tree.
        #[arg(long, alias = "bless")]
        regen: bool,
    },
    /// Open a PR bumping the pinned rusty_v8/V8 version (4-week cadence; never >2 majors behind).
    V8Bump,
    /// Rebuild the startup snapshot blobs.
    Snapshot,
    /// Package release artifacts (signing + SLSA provenance later).
    Package,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Bench {
            release,
            compare,
            cases,
        } => bench::run(release, compare, &cases),
        Command::Conformance { release } => conformance::run(release),
        Command::NodeSuite { release } => node_suite::run(release),
        Command::UnsafeBudget { regen } => unsafe_budget::run(regen),
        Command::V8Bump => bail!("not implemented: lands with CI (task: M0/CI)"),
        Command::Snapshot => bail!("not implemented: lands with M1 snapshot pipeline"),
        Command::Package => bail!("not implemented: lands with first public release"),
    }
}
