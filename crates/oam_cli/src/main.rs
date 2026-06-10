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

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum CheckMode {
    /// Check concurrently with execution; report, never block (default).
    Warn,
    /// Check BEFORE execution; type errors prevent the run (CI gate).
    Block,
    /// No type checking.
    Off,
}

#[derive(Subcommand)]
enum Command {
    /// Run a JavaScript or TypeScript file (TypeScript is type-checked
    /// concurrently by default — execution never waits for the checker).
    Run {
        file: PathBuf,
        /// Type-check policy for TypeScript entries.
        #[arg(long, value_enum, default_value = "warn")]
        check: CheckMode,
        /// Alias for --check=off.
        #[arg(long)]
        no_check: bool,
    },
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
        Command::Run {
            file,
            check,
            no_check,
        } => run_command(file, *check, *no_check, cli.json),
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

/// Daemon first (instant repeat checks), one-shot on ANY daemon trouble:
/// the daemon may never make checking less reliable than no daemon.
// result_large_err: cold path, deliberate — same stance as oam_loader/oam_ts.
#[allow(clippy::result_large_err)]
fn run_check(path: &Path) -> Result<Vec<Diagnostic>, Diagnostic> {
    match oam_ts::daemon::check_via_daemon(path) {
        Ok(diagnostics) => Ok(diagnostics),
        Err(_) => oam_ts::check(path),
    }
}

fn error_count(diagnostics: &[Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count()
}

/// `oam run` with the typed loop: strip-and-run starts immediately; the
/// checker runs concurrently (warn), gates (block), or stays off.
fn run_command(file: &Path, check: CheckMode, no_check: bool, json: bool) -> ExitCode {
    let mode = if no_check { CheckMode::Off } else { check };
    // Only TypeScript entries are checkable; .js and .tsx flow through
    // their normal paths untouched.
    let checkable = oam_loader::classify(file) == SourceKind::TypeScript;

    if mode == CheckMode::Block && checkable {
        match run_check(file) {
            Err(failure) => {
                render(&failure, json);
                return ExitCode::FAILURE;
            }
            Ok(diagnostics) => {
                let errors = error_count(&diagnostics);
                for d in &diagnostics {
                    render(d, json);
                }
                if errors > 0 {
                    if !json {
                        eprintln!(
                            "oam run: {errors} type error(s) — not executing (--check=block)"
                        );
                    }
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    // Warn mode: the check races execution on a thread; we collect it after
    // the program finishes. Execution NEVER waits for the checker to start.
    let pending = (mode == CheckMode::Warn && checkable).then(|| {
        let path = file.to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_check(&path));
        });
        rx
    });

    let exit = match run_file(file) {
        Ok(()) => ExitCode::SUCCESS,
        Err(diagnostics) => {
            for d in &diagnostics {
                render(d, json);
            }
            ExitCode::FAILURE
        }
    };

    if let Some(rx) = pending {
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(diagnostics)) => {
                let errors = error_count(&diagnostics);
                for d in &diagnostics {
                    render(d, json);
                }
                if errors > 0 && !json {
                    eprintln!(
                        "oam run: {errors} type error(s) (execution was not blocked; use --check=block to gate)"
                    );
                }
            }
            Ok(Err(failure)) => {
                // Checker unavailable (e.g. no tsgo): report once, quietly,
                // human-mode only — never fail a successful run over it.
                if !json {
                    eprintln!("oam run: type check skipped: {}", failure.message);
                }
            }
            Err(_) => {
                if !json {
                    eprintln!(
                        "oam run: type check still running (daemon warming); results are instant on the next run"
                    );
                }
            }
        }
    }
    exit
}

fn check_path(path: &Path, json: bool, no_daemon: bool) -> ExitCode {
    let result = if no_daemon {
        oam_ts::check(path)
    } else {
        run_check(path)
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
