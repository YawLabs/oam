//! Repo automation. Invoked as `cargo run -p xtask -- <command>`.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

mod bench;
mod conformance;

#[derive(Parser)]
#[command(name = "xtask", about = "oam repo automation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run micro-benchmarks (cold-start, url-parse, http-throughput, fs-read).
    ///
    /// Pass --release to benchmark the release binary instead of debug.
    Bench {
        /// Build and benchmark the release binary (cargo build --release).
        #[arg(long)]
        release: bool,
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
        Command::Bench { release } => bench::run(release),
        Command::Conformance { release } => conformance::run(release),
        Command::V8Bump => bail!("not implemented: lands with CI (task: M0/CI)"),
        Command::Snapshot => bail!("not implemented: lands with M1 snapshot pipeline"),
        Command::Package => bail!("not implemented: lands with first public release"),
    }
}
