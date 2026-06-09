//! The `oam` CLI. M0 surface: `oam run <file.js>` and `oam --version`.
//! TypeScript, modules, and the event loop arrive in M1 — this slice exists
//! to prove the engine boundary end-to-end on every tier-1 target.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use oam_diagnostics::{Diagnostic, Origin, Severity};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "oam",
    version,
    about = "oam — the reliable TypeScript runtime for the AI era"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Emit machine-readable ODIF JSONL on stderr instead of pretty errors.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run a JavaScript file.
    Run { file: PathBuf },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let diag = Diagnostic::new(
                "OAM-RT0001",
                Severity::Error,
                Origin::Runtime,
                err.to_string(),
            );
            if cli.json {
                eprintln!("{}", diag.to_jsonl());
            } else {
                // Pretty renderer over the same diagnostic — never a separate path.
                eprintln!("error[{}]: {}", diag.code, diag.message);
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Run { file } => {
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "ts" | "tsx" | "mts" | "cts") {
                bail!(
                    "TypeScript execution lands in M1 (oxc strip + tsgo sidecar); M0 runs JavaScript only: {}",
                    file.display()
                );
            }
            let source = std::fs::read_to_string(file)
                .with_context(|| format!("could not read {}", file.display()))?;
            let name = file.to_string_lossy();
            let mut rt = oam_engine::JsRuntime::new();
            rt.execute_script(&name, &source)?;
            Ok(())
        }
    }
}
