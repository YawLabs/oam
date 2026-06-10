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
    /// Type-check a file or project with tsgo (TypeScript 7 native).
    Check {
        /// A .ts file or a directory; the nearest tsconfig.json upward wins.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Skip the type-check daemon; run a one-shot check.
        #[arg(long)]
        no_daemon: bool,
    },
    /// Inspect or stop the per-project type-check daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Serve oam's introspection to coding agents over MCP (stdio transport).
    /// Register with e.g.: claude mcp add oam -- oam mcp
    Mcp,
    /// Internal: type-check daemon server process. Not for direct use.
    #[command(name = "__oamd-ts", hide = true)]
    DaemonServe { tsconfig: PathBuf },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Report whether the project's daemon is running (JSON).
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Stop the project's daemon.
    Stop {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Command::Run { file } => match run_file(file) {
            Ok(()) => ExitCode::SUCCESS,
            Err(diagnostics) => {
                for d in &diagnostics {
                    render(d, cli.json);
                }
                ExitCode::FAILURE
            }
        },
        Command::Check { path, no_daemon } => check_path(path, cli.json, *no_daemon),
        Command::Daemon { action } => match action {
            DaemonAction::Status { path } => {
                let status = oam_ts::daemon::status(path);
                println!(
                    "{}",
                    serde_json::to_string(&status).expect("status serializes")
                );
                ExitCode::SUCCESS
            }
            DaemonAction::Stop { path } => {
                let stopped = oam_ts::daemon::stop(path);
                println!("{{\"stopped\":{stopped}}}");
                ExitCode::SUCCESS
            }
        },
        Command::Mcp => match oam_mcp::serve_stdio() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("oam mcp: stdio transport failed: {e}");
                ExitCode::FAILURE
            }
        },
        Command::DaemonServe { tsconfig } => match oam_ts::daemon::serve(tsconfig) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("oam type-check daemon failed: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

fn check_path(path: &Path, json: bool, no_daemon: bool) -> ExitCode {
    // Daemon first (instant repeat checks), one-shot on ANY daemon trouble:
    // the daemon may never make `oam check` less reliable than no daemon.
    let result = if no_daemon {
        oam_ts::check(path)
    } else {
        match oam_ts::daemon::check_via_daemon(path) {
            Ok(diagnostics) => Ok(diagnostics),
            Err(_) => oam_ts::check(path),
        }
    };
    match result {
        Err(failure) => {
            render(&failure, json);
            ExitCode::FAILURE
        }
        Ok(diagnostics) => {
            for d in &diagnostics {
                render(d, json);
            }
            let errors = diagnostics
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .count();
            if !json {
                // Human summary only; --json stays pure JSONL.
                if errors == 0 {
                    eprintln!(
                        "oam check: clean ({} non-error diagnostic(s))",
                        diagnostics.len()
                    );
                } else {
                    eprintln!("oam check: {errors} error(s)");
                }
            }
            if errors == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
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

/// The CLI's module host: filesystem loading + oxc transpilation, with the
/// resolution rules from oam_loader. Everything `oam run` executes goes
/// through this — entry file included — as an ES module (ESM-first per plan).
struct CliHost;

impl oam_engine::ModuleHost for CliHost {
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, Vec<Diagnostic>> {
        oam_loader::resolve_import(specifier, referrer).map_err(|d| vec![d])
    }

    fn load(&self, path: &Path) -> Result<String, Vec<Diagnostic>> {
        let kind = oam_loader::classify(path);
        if kind == SourceKind::Jsx {
            return Err(vec![Diagnostic::new(
                "OAM-PARSE0003",
                Severity::Error,
                Origin::Parse,
                format!(
                    ".tsx/.jsx support lands with the JSX automatic runtime (needs npm resolution, M2): {}",
                    path.display()
                ),
            )]);
        }

        // Anything we'd silently mis-execute gets a clear diagnostic instead:
        // .cjs/.cts would run as ESM and die on a bare 'require is not
        // defined'; .json would parse as JS and die on 'Unexpected token'.
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "js" | "mjs" | "ts" | "mts" => {}
            "cjs" | "cts" => {
                return Err(vec![Diagnostic::new(
                    "OAM-MOD0003",
                    Severity::Error,
                    Origin::Resolve,
                    format!(
                        "CommonJS modules (.cjs/.cts) land with npm/CJS interop (M2): {}",
                        path.display()
                    ),
                )]);
            }
            "json" => {
                return Err(vec![Diagnostic::new(
                    "OAM-MOD0003",
                    Severity::Error,
                    Origin::Resolve,
                    format!(
                        "JSON modules land with import-attributes support (M2): {}",
                        path.display()
                    ),
                )]);
            }
            other => {
                return Err(vec![Diagnostic::new(
                    "OAM-MOD0003",
                    Severity::Error,
                    Origin::Resolve,
                    format!(
                        "unsupported module type '.{other}' (expected .js/.mjs/.ts/.mts): {}",
                        path.display()
                    ),
                )]);
            }
        }

        let source = std::fs::read_to_string(path).map_err(|e| {
            vec![Diagnostic::new(
                "OAM-RT0002",
                Severity::Error,
                Origin::Runtime,
                format!("could not read {}: {e}", path.display()),
            )]
        })?;

        match kind {
            SourceKind::TypeScript => {
                oam_loader::transpile_typescript(path, &source).map_err(|e| e.diagnostics)
            }
            _ => Ok(source),
        }
    }
}

fn run_file(file: &Path) -> Result<(), Vec<Diagnostic>> {
    let mut rt = oam_engine::JsRuntime::new();
    rt.execute_module(file, &CliHost)
}
