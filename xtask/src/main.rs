//! Repo automation. Invoked as `cargo run -p xtask -- <command>`.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

mod conformance;

#[derive(Parser)]
#[command(name = "xtask", about = "oam repo automation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the conformance suites (WPT URL, Node differential, builtin
    /// surface) and regenerate CONFORMANCE.md + conformance/scorecard.json.
    Conformance,
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
        Command::Conformance => conformance::run(),
        Command::V8Bump => bail!("not implemented: lands with CI (task: M0/CI)"),
        Command::Snapshot => bail!("not implemented: lands with M1 snapshot pipeline"),
        Command::Package => bail!("not implemented: lands with first public release"),
    }
}
