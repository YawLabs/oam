//! The `oam` CLI.
//!
//! M1 slice 1 surface: `oam run <file.{js,mjs,cjs,ts,mts,cts}>`. TypeScript
//! is transpiled (types stripped, enums lowered) before execution — strictly
//! more than Node's strip-only support, and typed where Bun is types-blind.
//! All failures render from ODIF diagnostics: `--json` emits the JSONL
//! source of truth, the pretty printer is a renderer over the same data.

use clap::{Parser, Subcommand};
use oam_diagnostics::{Diagnostic, Origin, Severity};
use oam_loader::SourceKind;
use std::path::{Path, PathBuf};
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
    /// Run a JavaScript or TypeScript file.
    Run { file: PathBuf },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(diagnostics) => {
            for d in &diagnostics {
                render(d, cli.json);
            }
            ExitCode::FAILURE
        }
    }
}

/// Render one diagnostic. JSON mode is the machine-facing source of truth;
/// the pretty form is a projection of the same Diagnostic, never a fork.
fn render(d: &Diagnostic, json: bool) {
    if json {
        eprintln!("{}", d.to_jsonl());
    } else {
        let loc = d
            .spans
            .first()
            .map(|s| format!(" ({}:{}:{})", s.file, s.start.line, s.start.col))
            .unwrap_or_default();
        eprintln!("error[{}]: {}{}", d.code, d.message, loc);
    }
}

fn run(cli: &Cli) -> Result<(), Vec<Diagnostic>> {
    match &cli.command {
        Command::Run { file } => run_file(file),
    }
}

fn run_file(file: &Path) -> Result<(), Vec<Diagnostic>> {
    let kind = oam_loader::classify(file);
    if kind == SourceKind::Jsx {
        return Err(vec![Diagnostic::new(
            "OAM-PARSE0003",
            Severity::Error,
            Origin::Parse,
            format!(
                ".tsx/.jsx support lands with the module loader (JSX automatic runtime needs ESM): {}",
                file.display()
            ),
        )]);
    }

    let source = std::fs::read_to_string(file).map_err(|e| {
        vec![Diagnostic::new(
            "OAM-RT0002",
            Severity::Error,
            Origin::Runtime,
            format!("could not read {}: {e}", file.display()),
        )]
    })?;

    let code = match kind {
        SourceKind::TypeScript => {
            oam_loader::transpile_typescript(file, &source).map_err(|e| e.diagnostics)?
        }
        _ => source,
    };

    let mut rt = oam_engine::JsRuntime::new();
    rt.execute_script(&file.to_string_lossy(), &code)
        .map_err(|e| {
            vec![Diagnostic::new(
                "OAM-RT0001",
                Severity::Error,
                Origin::Runtime,
                e.to_string(),
            )]
        })?;
    Ok(())
}
