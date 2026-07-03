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
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "oam",
    version,
    about = "oam — the reliable TypeScript runtime for the AI era"
)]
struct Cli {
    /// No subcommand drops into the typed REPL.
    #[command(subcommand)]
    command: Option<Command>,
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
        /// Attach the V8 Inspector (Chrome DevTools Protocol). Optional
        /// value is `[host:]port` (default 127.0.0.1:9229).
        #[arg(long, num_args = 0..=1, default_missing_value = "127.0.0.1:9229", value_name = "[host:]port")]
        inspect: Option<String>,
        /// Like --inspect, but wait for a debugger to attach and break on the
        /// first line. Optional value is `[host:]port`.
        #[arg(long, num_args = 0..=1, default_missing_value = "127.0.0.1:9229", value_name = "[host:]port")]
        inspect_brk: Option<String>,
        /// Record all non-deterministic I/O to FILE (JSON Lines). Mutually
        /// exclusive with --replay.
        #[arg(long, value_name = "FILE", conflicts_with = "replay")]
        record: Option<PathBuf>,
        /// Replay non-deterministic I/O from a FILE previously captured with
        /// --record. Timers fire instantly; Math.random/Date.now return
        /// recorded values. Mutually exclusive with --record.
        #[arg(long, value_name = "FILE", conflicts_with = "record")]
        replay: Option<PathBuf>,
        /// Arguments passed to the script (process.argv) after `--`.
        #[arg(last = true)]
        script_args: Vec<String>,
    },
    /// Run tests (*.test.* / *.spec.* / *_test.* files; import 'oam:test').
    /// Each file runs in a fresh isolate. Type checking stays with `oam
    /// check` — the runner never blocks on types.
    Test {
        /// Files or directories to search (default: current directory).
        paths: Vec<PathBuf>,
        /// Run only tests whose full name matches (regex, else substring).
        #[arg(short = 't', long)]
        test_name_pattern: Option<String>,
    },
    /// Interactive typed REPL (also the default with no subcommand).
    Repl,
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
    /// Start a server from a JS/TS entry file. Equivalent to `oam run`
    /// with `PORT` and `HOST` env vars set from `--port` / `--host`.
    Serve {
        file: PathBuf,
        /// Port to listen on (sets the PORT env var; default 3000).
        #[arg(short, long, default_value = "3000")]
        port: u16,
        /// Bind address (sets the HOST env var; default 0.0.0.0).
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        /// Number of worker isolates for request dispatch. 0 = single-process
        /// (handler file runs directly). >0 = pool mode (handler file must
        /// export a default `(req, res) => ...` function).
        #[arg(short, long, default_value = "0")]
        workers: u16,
        /// Attach the V8 Inspector (Chrome DevTools Protocol). Optional
        /// value is `[host:]port` (default 127.0.0.1:9229).
        #[arg(long, num_args = 0..=1, default_missing_value = "127.0.0.1:9229", value_name = "[host:]port")]
        inspect: Option<String>,
        /// Like --inspect, but wait for a debugger to attach and break on the
        /// first line. Optional value is `[host:]port`.
        #[arg(long, num_args = 0..=1, default_missing_value = "127.0.0.1:9229", value_name = "[host:]port")]
        inspect_brk: Option<String>,
    },
    /// Install packages from the lockfile (npm ci equivalent).
    Install {
        /// Refuse to modify the lockfile (default and only mode for MVP).
        #[arg(long, default_value = "true")]
        frozen_lockfile: bool,
        /// Pre-compile TypeScript files in installed packages to .js at install
        /// time. Cached under node_modules/.oam/precompile/ for faster first run.
        #[arg(long, default_value = "false")]
        precompile: bool,
    },
    /// Manage the trust list for lifecycle scripts.
    ///
    /// oam install skips all lifecycle scripts (postinstall, preinstall, install)
    /// by default. Trust a package to see its skipped scripts; script execution
    /// is not yet supported, but trusted packages suppress the OAM-PKG0007 warning.
    Trust {
        #[command(subcommand)]
        action: TrustAction,
    },
    /// Compile a pre-bundled JS file into a standalone executable.
    /// The user bundles externally (esbuild/rollup); this embeds the result.
    Compile {
        /// Pre-bundled JS/CJS entry file.
        entry: PathBuf,
        /// Output binary path.
        #[arg(short, long)]
        output: PathBuf,
        /// Reserved for future use (minify the embedded source).
        #[arg(long)]
        minify: bool,
    },
    /// Update oam in place to the latest release by running the canonical
    /// oam.sh installer (verifies via the published SHA256SUMS). Updates the
    /// currently-running binary's location.
    SelfUpdate {
        /// Install a specific tag (e.g. v0.7.0) instead of the latest.
        #[arg(long, value_name = "TAG")]
        version: Option<String>,
        /// Print the installer command that would run, without executing it.
        #[arg(long)]
        dry_run: bool,
    },
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

#[derive(Subcommand)]
enum TrustAction {
    /// Allow a package to suppress the OAM-PKG0007 lifecycle-script warning.
    Add {
        /// npm package name (e.g. "esbuild" or "@scope/pkg").
        package: String,
        /// Write to the global trust list (~/.config/oam/trust.json) instead of
        /// the project-local .oam/trust.json.
        #[arg(long)]
        global: bool,
    },
    /// Remove a package from the trust list.
    Remove {
        /// npm package name.
        package: String,
        /// Remove from the global list instead of the project-local list.
        #[arg(long)]
        global: bool,
    },
    /// Show trusted packages.
    List {
        /// Show only the global list.
        #[arg(long, conflicts_with = "local")]
        global: bool,
        /// Show only the project-local list (default).
        #[arg(long)]
        local: bool,
    },
}

fn main() -> ExitCode {
    // Crash reporter FIRST: a structured stderr banner + a crash file under the
    // oam cache dir on any Rust panic or V8 OOM (internal diagnostics, not
    // public telemetry). Installed before anything can panic.
    oam_engine::install_panic_hook();
    // Internal self-test hook (undocumented, env-gated): exercises the crash
    // path deterministically in CI without needing a JS-reachable panic.
    if std::env::var("OAM_CRASH_SELFTEST").as_deref() == Ok("panic") {
        panic!("oam crash-reporter self-test");
    }
    if let Some((source, bytecode)) = extract_embedded() {
        return run_embedded(&source, bytecode, std::env::args().collect());
    }

    let cli = Cli::parse();
    let Some(command) = &cli.command else {
        return repl_command();
    };
    match command {
        Command::Repl => repl_command(),
        Command::Run {
            file,
            check,
            no_check,
            inspect,
            inspect_brk,
            record,
            replay,
            script_args,
        } => {
            let inspect = match resolve_inspect(inspect.as_deref(), inspect_brk.as_deref()) {
                Ok(value) => value,
                Err(message) => {
                    eprintln!("oam run: {message}");
                    return ExitCode::FAILURE;
                }
            };
            let replay_mode = if let Some(path) = record {
                oam_engine::ReplayMode::Record(path.clone())
            } else if let Some(path) = replay {
                oam_engine::ReplayMode::Replay(path.clone())
            } else {
                oam_engine::ReplayMode::Off
            };
            run_command(
                file,
                *check,
                *no_check,
                cli.json,
                script_args,
                inspect,
                replay_mode,
            )
        }
        Command::Test {
            paths,
            test_name_pattern,
        } => test_command(paths, test_name_pattern.as_deref(), cli.json),
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
        Command::Serve {
            file,
            port,
            host,
            workers,
            inspect,
            inspect_brk,
        } => {
            // Safety: single-threaded at this point (before JsRuntime::new).
            unsafe {
                std::env::set_var("PORT", port.to_string());
                std::env::set_var("HOST", host);
            }
            let inspect = match resolve_inspect(inspect.as_deref(), inspect_brk.as_deref()) {
                Ok(value) => value,
                Err(message) => {
                    eprintln!("oam serve: {message}");
                    return ExitCode::FAILURE;
                }
            };
            if *workers > 0 {
                serve_with_workers(file, *workers, cli.json, inspect)
            } else {
                run_command(
                    file,
                    CheckMode::Warn,
                    false,
                    cli.json,
                    &[],
                    inspect,
                    oam_engine::ReplayMode::Off,
                )
            }
        }
        Command::Install {
            frozen_lockfile,
            precompile,
        } => install_command(*frozen_lockfile, *precompile, cli.json),
        Command::Trust { action } => trust_command(action),
        Command::Compile {
            entry,
            output,
            minify: _,
        } => compile_command(entry, output),
        Command::SelfUpdate { version, dry_run } => {
            self_update_command(version.as_deref(), *dry_run)
        }
        Command::DaemonServe { tsconfig } => match oam_ts::daemon::serve(tsconfig) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("oam type-check daemon failed: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Build the platform installer invocation for `oam self-update`. Pure (no I/O)
/// so the per-platform command shape is unit-testable. `url` is the installer
/// script URL; the installer itself does the download + checksum-verify +
/// cross-platform self-replace (see install/), so no network code lives here.
fn build_self_update_cmd(is_windows: bool, url: &str) -> (&'static str, Vec<String>) {
    if is_windows {
        (
            "powershell",
            vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-Command".into(),
                format!("irm {url} | iex"),
            ],
        )
    } else {
        ("sh", vec!["-c".into(), format!("curl -fsSL {url} | sh")])
    }
}

/// `oam self-update`: re-run the canonical oam.sh installer to replace the
/// running binary in place. Delegating keeps ONE source of download +
/// checksum-verify + running-exe-replace logic. We point the installer at the
/// CURRENT binary's directory (via OAM_INSTALL_DIR) so it updates oam where it
/// actually lives, and pass through a pinned --version as OAM_VERSION.
fn self_update_command(version: Option<&str>, dry_run: bool) -> ExitCode {
    // Update the binary where it currently lives, unless the user pinned a dir.
    let install_dir = match std::env::var_os("OAM_INSTALL_DIR") {
        Some(d) => PathBuf::from(d),
        None => match std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        {
            Some(dir) => dir,
            None => {
                eprintln!("oam self-update: cannot resolve the current binary location");
                return ExitCode::FAILURE;
            }
        },
    };

    let default_url = if cfg!(target_os = "windows") {
        "https://oam.sh/install.ps1"
    } else {
        "https://oam.sh/install.sh"
    };
    let url = std::env::var("OAM_SELF_UPDATE_URL").unwrap_or_else(|_| default_url.to_string());
    let (program, args) = build_self_update_cmd(cfg!(target_os = "windows"), &url);

    println!(
        "oam self-update: current version {}",
        env!("CARGO_PKG_VERSION")
    );
    println!("oam self-update: updating oam in {}", install_dir.display());
    match version {
        Some(v) => println!("oam self-update: pinning to {v}"),
        None => println!("oam self-update: targeting the latest release"),
    }

    if dry_run {
        println!(
            "oam self-update: (dry-run) would run: {program} {}",
            args.join(" ")
        );
        return ExitCode::SUCCESS;
    }

    let mut cmd = std::process::Command::new(program);
    cmd.args(&args).env("OAM_INSTALL_DIR", &install_dir);
    if let Some(v) = version {
        cmd.env("OAM_VERSION", v);
    }
    match cmd.status() {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => {
            eprintln!("oam self-update: installer exited with {s}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("oam self-update: failed to launch installer ({program}): {e}");
            ExitCode::FAILURE
        }
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
/// Resolve the `--inspect` / `--inspect-brk` flags into an optional
/// (address, break-on-start) pair. `--inspect-brk` wins if both are given.
/// The value is `PORT` or `HOST:PORT`; a bare port binds 127.0.0.1.
fn resolve_inspect(
    inspect: Option<&str>,
    inspect_brk: Option<&str>,
) -> Result<Option<(std::net::SocketAddr, bool)>, String> {
    let (value, brk) = match (inspect_brk, inspect) {
        (Some(v), _) => (v, true),
        (None, Some(v)) => (v, false),
        (None, None) => return Ok(None),
    };
    let with_host = if value.parse::<u16>().is_ok() {
        format!("127.0.0.1:{value}")
    } else {
        value.to_string()
    };
    let addr = with_host
        .parse::<std::net::SocketAddr>()
        .map_err(|_| format!("invalid --inspect address '{with_host}' (expected [host:]port)"))?;
    Ok(Some((addr, brk)))
}

fn run_command(
    file: &Path,
    check: CheckMode,
    no_check: bool,
    json: bool,
    script_args: &[String],
    inspect: Option<(std::net::SocketAddr, bool)>,
    replay_mode: oam_engine::ReplayMode,
) -> ExitCode {
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

    let exit = match run_file(file, script_args, inspect, replay_mode) {
        Ok(code) => ExitCode::from(code),
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

/// Brace/paren/bracket balance ignoring string/template contents — the
/// REPL's multi-line continuation heuristic.
fn input_balanced(source: &str) -> bool {
    let mut depth: i64 = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in source.chars() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            _ => {}
        }
    }
    depth <= 0 && quote.is_none()
}

/// The typed REPL: every line runs through the oxc TypeScript strip, so
/// annotations just work. A reader thread feeds lines over a channel
/// while the main thread ticks the event loop between inputs — timers
/// and async ops stay LIVE at the prompt.
fn repl_command() -> ExitCode {
    use std::io::{BufRead, Write};

    println!(
        "oam v{} — typed REPL (TypeScript welcome; .exit or Ctrl+C to quit)",
        env!("CARGO_PKG_VERSION")
    );
    let mut rt = oam_engine::JsRuntime::new();
    // The REPL doesn't call execute_module/execute_cjs (which run
    // reset_run_slots and create a CoreRuntime). Init it here so
    // tick() and repl_eval() have a Tokio runtime to drive ops on.
    rt.ensure_core_runtime();
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "oam".to_string());
    rt.set_process_argv(vec![exe]);

    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(Some(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(None); // EOF
    });

    let prompt = |continuation: bool| {
        print!("{}", if continuation { "... " } else { "> " });
        let _ = std::io::stdout().flush();
    };

    let mut buffer = String::new();
    prompt(false);
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(40)) {
            Ok(None) => break,
            Ok(Some(line)) => {
                if buffer.is_empty() {
                    let trimmed = line.trim();
                    match trimmed {
                        ".exit" => break,
                        ".help" => {
                            println!(
                                ".exit  quit | .help  this | `_` holds the last value | top-level await works"
                            );
                            prompt(false);
                            continue;
                        }
                        "" => {
                            prompt(false);
                            continue;
                        }
                        _ => {}
                    }
                }
                buffer.push_str(&line);
                buffer.push('\n');
                if !input_balanced(&buffer) {
                    prompt(true);
                    continue;
                }
                let source = std::mem::take(&mut buffer);
                // Typed input: strip annotations first; oxc handles plain
                // JS identically, and its parse errors are the user's.
                let prepared = match oam_loader::transpile_typescript(Path::new("repl.ts"), &source)
                {
                    Ok(stripped) => stripped,
                    Err(e) => {
                        for d in &e.diagnostics {
                            eprintln!("{}", d.message);
                        }
                        prompt(false);
                        continue;
                    }
                };
                match rt.repl_eval(&prepared) {
                    Ok(rendered) => println!("{rendered}"),
                    Err(message) => eprintln!("{message}"),
                }
                prompt(false);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Idle: keep timers/ops alive at the prompt.
                let _ = rt.tick(std::time::Duration::from_millis(25));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    ExitCode::SUCCESS
}

/// Is this file named like a test? (*.test.* / *.spec.* / *_test.*)
fn is_test_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    if !matches!(ext, "ts" | "mts" | "cts" | "js" | "mjs" | "cjs") {
        return false;
    }
    let stem = &name[..name.len() - ext.len() - 1];
    stem.ends_with(".test") || stem.ends_with(".spec") || stem.ends_with("_test")
}

/// Discover test files under `paths` (explicit files pass regardless of
/// naming; directories recurse, skipping dependency/build trees).
fn discover_test_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    const SKIP_DIRS: [&str; 6] = ["node_modules", ".git", "target", "dist", "coverage", ".oam"];
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if SKIP_DIRS.iter().any(|s| name == *s) || name.starts_with(".oam") {
                    continue;
                }
                walk(&path, out);
            } else if is_test_file(&path) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.to_vec()
    };
    for root in roots {
        if root.is_file() {
            out.push(root);
        } else {
            walk(&root, &mut out);
        }
    }
    out
}

/// `oam test`: every file in a FRESH isolate (registered state cannot leak
/// across files), results rendered pretty or as ODIF JSONL from the same
/// data. Exit 0 only when every discovered test passed.
fn test_command(paths: &[PathBuf], filter: Option<&str>, json: bool) -> ExitCode {
    let files = discover_test_files(paths);
    if files.is_empty() {
        eprintln!(
            "oam test: no test files found (looked for *.test.* / *.spec.* / *_test.* with js/mjs/cjs/ts/mts/cts extensions)"
        );
        return ExitCode::FAILURE;
    }

    let started = std::time::Instant::now();
    let (mut pass, mut fail, mut skip, mut todo) = (0u64, 0u64, 0u64, 0u64);
    let mut file_failures = 0u64;

    for file in &files {
        if !json {
            eprintln!("{}", file.display());
        }
        let mut rt = oam_engine::JsRuntime::new();
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "oam".to_string());
        let script = std::path::absolute(file)
            .unwrap_or_else(|_| file.clone())
            .to_string_lossy()
            .into_owned();
        rt.set_process_argv(vec![exe, script]);

        let evaluated = if oam_loader::module_kind(file) == oam_loader::ModuleKind::Cjs {
            rt.execute_cjs(file)
        } else {
            rt.execute_module(file, &CliHost)
        };
        if let Err(diagnostics) = evaluated {
            file_failures += 1;
            for d in &diagnostics {
                render(d, json);
            }
            continue;
        }

        let results = match rt.run_registered_tests(filter) {
            Ok(json_text) => json_text,
            Err(diagnostics) => {
                file_failures += 1;
                for d in &diagnostics {
                    render(d, json);
                }
                continue;
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&results) {
            Ok(value) => value,
            Err(e) => {
                file_failures += 1;
                eprintln!("oam test: unreadable results from {}: {e}", file.display());
                continue;
            }
        };

        let tests = parsed["tests"].as_array().cloned().unwrap_or_default();
        if tests.is_empty() && parsed["noTestModule"].as_bool() == Some(true) && !json {
            eprintln!("  (no tests registered — does the file import 'oam:test'?)");
        }
        for t in &tests {
            let name = t["name"].as_str().unwrap_or("(unnamed)");
            let status = t["status"].as_str().unwrap_or("fail");
            let duration = t["durationMs"].as_u64().unwrap_or(0);
            match status {
                "pass" => {
                    pass += 1;
                    if !json {
                        eprintln!("  ok   {name} ({duration}ms)");
                    }
                }
                "skip" => {
                    skip += 1;
                    if !json {
                        eprintln!("  skip {name}");
                    }
                }
                "todo" => {
                    todo += 1;
                    if !json {
                        eprintln!("  todo {name}");
                    }
                }
                _ => {
                    fail += 1;
                    let message = t["error"]["message"].as_str().unwrap_or("failed");
                    if json {
                        let d = Diagnostic::new(
                            "OAM-TEST0001",
                            Severity::Error,
                            Origin::Test,
                            format!("FAIL {name}: {message}"),
                        )
                        .with_span(oam_diagnostics::Span {
                            file: file.to_string_lossy().into_owned(),
                            start: oam_diagnostics::Position { line: 1, col: 1 },
                            end: oam_diagnostics::Position { line: 1, col: 1 },
                        });
                        render(&d, true);
                    } else {
                        eprintln!("  FAIL {name} ({duration}ms)");
                        eprintln!("       {message}");
                        if let Some(stack) = t["error"]["stack"].as_str() {
                            for line in stack.lines().take(4) {
                                eprintln!("       {}", line.trim());
                            }
                        }
                    }
                }
            }
        }
    }

    let elapsed = started.elapsed().as_millis();
    let failed_total = fail + file_failures;
    if json {
        // Machine summary as an ODIF info/error diagnostic, same stream.
        let d = Diagnostic::new(
            "OAM-TEST0000",
            if failed_total == 0 {
                Severity::Info
            } else {
                Severity::Error
            },
            Origin::Test,
            format!(
                "{} file(s): {pass} passed, {fail} failed, {skip} skipped, {todo} todo, {file_failures} file error(s) in {elapsed}ms",
                files.len()
            ),
        );
        render(&d, true);
    } else {
        eprintln!();
        eprintln!(
            "{} file(s): {pass} passed, {fail} failed, {skip} skipped, {todo} todo{} ({elapsed}ms)",
            files.len(),
            if file_failures > 0 {
                format!(", {file_failures} file error(s)")
            } else {
                String::new()
            }
        );
    }
    if failed_total == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
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

/// `oam install`: frozen-lockfile package install from package-lock.json v3.
fn install_command(frozen_lockfile: bool, precompile: bool, json: bool) -> ExitCode {
    // Walk upward from cwd to find the directory containing package-lock.json.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_dir = find_project_dir(&cwd, "package-lock.json").unwrap_or(cwd);

    match oam_loader::install::install(&project_dir, frozen_lockfile, precompile) {
        Ok(outcome) => {
            let (installed, elapsed, errors) = match outcome {
                oam_loader::install::InstallOutcome::Complete(summary) => {
                    (summary.packages_installed, summary.elapsed, Vec::new())
                }
                oam_loader::install::InstallOutcome::Partial {
                    installed,
                    elapsed,
                    errors,
                } => (installed, elapsed, errors),
            };
            // Render any diagnostics FIRST so the user sees per-package
            // failures / bin-shim warnings before the success summary line --
            // otherwise the summary at the bottom is the last thing on
            // screen and a reader skimming the tail can miss errors that
            // scrolled off above the "installed N package(s)" line.
            for d in &errors {
                render(d, json);
            }
            if json {
                let d = Diagnostic::new(
                    "OAM-PKG0000",
                    Severity::Info,
                    Origin::Install,
                    format!(
                        "installed {} package(s) in {:.1}s",
                        installed,
                        elapsed.as_secs_f64()
                    ),
                );
                render(&d, true);
            } else {
                eprintln!(
                    "oam install: {} package(s) in {:.1}s",
                    installed,
                    elapsed.as_secs_f64()
                );
            }
            // Only Severity::Error diagnostics fail the install -- a
            // bin-shim PKG0005 (Severity::Warning) is informational and
            // matches npm behavior of warning + exit 0. install.rs emits
            // PKG0005 with Severity::Warning explicitly.
            let has_fatal = errors.iter().any(|d| d.severity == Severity::Error);
            if has_fatal {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(diagnostics) => {
            for d in &diagnostics {
                render(d, json);
            }
            ExitCode::FAILURE
        }
    }
}

/// `oam trust`: manage the per-project and global lifecycle-script trust list.
fn trust_command(action: &TrustAction) -> ExitCode {
    use oam_loader::trust::TrustConfig;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_dir = find_project_dir(&cwd, "package.json")
        .or_else(|| find_project_dir(&cwd, "package-lock.json"))
        .unwrap_or(cwd);

    match action {
        TrustAction::Add { package, global } => {
            let mut config = if *global {
                TrustConfig::load_global()
            } else {
                TrustConfig::load_local(&project_dir)
            };
            if config.add(package) {
                let result = if *global {
                    config.save_global()
                } else {
                    config.save_local(&project_dir)
                };
                match result {
                    Ok(()) => {
                        if *global {
                            eprintln!("oam trust: added '{package}' to global trust list");
                        } else {
                            eprintln!("oam trust: added '{package}' to .oam/trust.json");
                        }
                    }
                    Err(e) => {
                        eprintln!("oam trust: failed to save: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                eprintln!("oam trust: '{package}' is already trusted");
            }
            ExitCode::SUCCESS
        }
        TrustAction::Remove { package, global } => {
            let mut config = if *global {
                TrustConfig::load_global()
            } else {
                TrustConfig::load_local(&project_dir)
            };
            if config.remove(package) {
                let result = if *global {
                    config.save_global()
                } else {
                    config.save_local(&project_dir)
                };
                match result {
                    Ok(()) => eprintln!("oam trust: removed '{package}'"),
                    Err(e) => {
                        eprintln!("oam trust: failed to save: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                eprintln!("oam trust: '{package}' was not in the trust list");
            }
            ExitCode::SUCCESS
        }
        TrustAction::List { global, local: _ } => {
            if *global {
                let config = TrustConfig::load_global();
                let path = TrustConfig::global_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.config/oam/trust.json".to_string());
                eprintln!("Global trust list ({path}):");
                for pkg in config.entries() {
                    println!("  {pkg}");
                }
                if config.entries().is_empty() {
                    eprintln!("  (empty)");
                }
            } else {
                // Default: show local (also used when --local is explicit).
                let config = TrustConfig::load_local(&project_dir);
                eprintln!(
                    "Project trust list ({}):",
                    project_dir.join(".oam").join("trust.json").display()
                );
                for pkg in config.entries() {
                    println!("  {pkg}");
                }
                if config.entries().is_empty() {
                    eprintln!("  (empty)");
                }
            }
            ExitCode::SUCCESS
        }
    }
}

/// Walk upward from `start` looking for a directory that contains `target`.
fn find_project_dir(start: &Path, target: &str) -> Option<PathBuf> {
    let mut dir = Some(start.to_path_buf());
    while let Some(current) = dir {
        if current.join(target).is_file() {
            return Some(current);
        }
        dir = current.parent().map(Path::to_path_buf);
    }
    None
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
        // .json would parse as JS and die on 'Unexpected token'. CJS files
        // never reach this host — the engine routes them through interop
        // before load — so a .cjs/.cts here is an engine routing bug.
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "js" | "mjs" | "ts" | "mts" => {}
            "cjs" | "cts" => {
                return Err(vec![Diagnostic::new(
                    "OAM-MOD0003",
                    Severity::Error,
                    Origin::Resolve,
                    format!(
                        "internal: CommonJS file reached the ESM loader (oam bug — please report): {}",
                        path.display()
                    ),
                )]);
            }
            "json" => {
                // Unreachable for imports (the engine routes .json through
                // the JSON-module branch before host.load); only a .json
                // ENTRY file lands here, which is not a program.
                return Err(vec![Diagnostic::new(
                    "OAM-MOD0003",
                    Severity::Error,
                    Origin::Resolve,
                    format!(
                        "a .json file is not a program — import it from a script instead: {}",
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
                // Install-time pre-compilation cache (oam install --precompile),
                // keyed by source hash; a hit skips the oxc transpile. Misses
                // (project sources, stale entries) fall through to transpile.
                if let Some(cached) = oam_loader::precompile::try_precompile_cache(path, &source) {
                    Ok(cached)
                } else {
                    oam_loader::transpile_typescript(path, &source).map_err(|e| e.diagnostics)
                }
            }
            _ => Ok(source),
        }
    }
}

/// Worker-pool mode: write a dispatcher + worker shim to temp, run the
/// dispatcher. The dispatcher creates the http server in the main isolate,
/// spawns N worker isolates, and round-robins requests via postMessage.
/// Bodies cross the channel as base64 (M3; approach C fd-transfer is the
/// perf endgame).
fn serve_with_workers(
    handler: &Path,
    workers: u16,
    json: bool,
    inspect: Option<(std::net::SocketAddr, bool)>,
) -> ExitCode {
    let handler_abs = std::path::absolute(handler)
        .unwrap_or_else(|_| handler.to_path_buf())
        .to_string_lossy()
        .into_owned()
        .replace('\\', "/");

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("oam-serve-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let worker_path = dir.join("_pool_worker.cjs");
    std::fs::write(&worker_path, POOL_WORKER_CJS).expect("write worker shim");

    let worker_path_str = worker_path
        .to_string_lossy()
        .into_owned()
        .replace('\\', "/");

    let dispatcher_src = POOL_DISPATCHER_CJS
        .replace("__OAM_WORKERS__", &workers.to_string())
        .replace("__OAM_HANDLER__", &handler_abs)
        .replace("__OAM_WORKER_SCRIPT__", &worker_path_str);

    let dispatcher_path = dir.join("_pool_dispatcher.cjs");
    std::fs::write(&dispatcher_path, &dispatcher_src).expect("write dispatcher");

    let result = run_file(&dispatcher_path, &[], inspect, oam_engine::ReplayMode::Off);
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(code) => ExitCode::from(code),
        Err(diagnostics) => {
            for d in &diagnostics {
                render(d, json);
            }
            ExitCode::FAILURE
        }
    }
}

const POOL_DISPATCHER_CJS: &str = r#"
const http = require("http");
const { Worker } = require("worker_threads");

const WORKER_COUNT = __OAM_WORKERS__;
const HANDLER = "__OAM_HANDLER__";
const WORKER_SCRIPT = "__OAM_WORKER_SCRIPT__";

const workers = [];
for (let i = 0; i < WORKER_COUNT; i++) {
  const w = new Worker(WORKER_SCRIPT, {
    workerData: { handler: HANDLER, workerId: i },
  });
  w.on("error", (err) => {
    console.error("worker " + i + " error:", err.message);
  });
  workers.push(w);
}

const pending = new Map();
let nextId = 1;
let nextWorker = 0;

for (const w of workers) {
  w.on("message", (msg) => {
    const res = pending.get(msg.id);
    if (!res) return;
    pending.delete(msg.id);
    res.writeHead(msg.status, msg.headers || {});
    res.end(msg.body ? Buffer.from(msg.body, "base64") : undefined);
  });
}

const server = http.createServer((req, res) => {
  const id = nextId++;
  pending.set(id, res);
  const chunks = [];
  req.on("data", (chunk) => chunks.push(chunk));
  req.on("end", () => {
    const body = Buffer.concat(chunks);
    const worker = workers[nextWorker % WORKER_COUNT];
    nextWorker++;
    worker.postMessage({
      id: id,
      method: req.method,
      url: req.url,
      headers: req.headers,
      body: body.length > 0 ? body.toString("base64") : null,
    });
  });
});

const port = parseInt(process.env.PORT) || 3000;
const host = process.env.HOST || "0.0.0.0";
server.listen(port, host, () => {
  const addr = server.address();
  console.log(
    "oam serve: " + addr.address + ":" + addr.port + " (" + WORKER_COUNT + " workers)"
  );
});
"#;

const POOL_WORKER_CJS: &str = r#"
const { parentPort, workerData } = require("worker_threads");
const { Readable } = require("stream");

const mod = require(workerData.handler);
const handler = typeof mod === "function" ? mod : mod.default;
if (typeof handler !== "function") {
  throw new Error(
    "oam serve --workers: handler file must export a (req, res) => ... function, " +
    "got " + typeof handler
  );
}

parentPort.on("message", (msg) => {
  const req = new Readable({ read() {} });
  req.method = msg.method;
  req.url = msg.url;
  req.headers = msg.headers;
  req.httpVersion = "1.1";
  req.socket = { remoteAddress: "127.0.0.1", encrypted: false };
  if (msg.body) req.push(Buffer.from(msg.body, "base64"));
  req.push(null);

  let statusCode = 200;
  const resHeaders = {};
  const bodyChunks = [];
  let ended = false;

  const res = {
    statusCode: 200,
    headersSent: false,
    setHeader(name, value) {
      resHeaders[String(name).toLowerCase()] = String(value);
    },
    getHeader(name) {
      return resHeaders[String(name).toLowerCase()];
    },
    removeHeader(name) {
      delete resHeaders[String(name).toLowerCase()];
    },
    writeHead(status, headers) {
      statusCode = status;
      res.statusCode = status;
      res.headersSent = true;
      if (headers) {
        if (typeof headers === "object") {
          for (const k of Object.keys(headers)) {
            resHeaders[k.toLowerCase()] = String(headers[k]);
          }
        }
      }
      return res;
    },
    write(chunk, encoding) {
      if (typeof chunk === "string") chunk = Buffer.from(chunk, encoding);
      else if (!(chunk instanceof Uint8Array)) chunk = Buffer.from(String(chunk));
      bodyChunks.push(chunk);
      return true;
    },
    end(chunk, encoding) {
      if (ended) return res;
      ended = true;
      if (chunk !== undefined && chunk !== null) {
        if (typeof chunk === "string") chunk = Buffer.from(chunk, encoding);
        else if (!(chunk instanceof Uint8Array)) chunk = Buffer.from(String(chunk));
        bodyChunks.push(chunk);
      }
      const body = Buffer.concat(bodyChunks);
      parentPort.postMessage({
        id: msg.id,
        status: statusCode,
        headers: resHeaders,
        body: body.length > 0 ? body.toString("base64") : null,
      });
      return res;
    },
  };

  try {
    const result = handler(req, res);
    if (result && typeof result.catch === "function") {
      result.catch((err) => {
        if (!ended) {
          ended = true;
          parentPort.postMessage({
            id: msg.id,
            status: 500,
            headers: { "content-type": "text/plain" },
            body: Buffer.from(err.message || "Internal Server Error").toString("base64"),
          });
        }
      });
    }
  } catch (err) {
    if (!ended) {
      ended = true;
      parentPort.postMessage({
        id: msg.id,
        status: 500,
        headers: { "content-type": "text/plain" },
        body: Buffer.from(err.message || "Internal Server Error").toString("base64"),
      });
    }
  }
});
"#;

/// Run the entry; Ok carries the process exit code (0, or a natural-exit
/// process.exitCode the script declared — Node honors it, so does oam).
fn run_file(
    file: &Path,
    script_args: &[String],
    inspect: Option<(std::net::SocketAddr, bool)>,
    replay_mode: oam_engine::ReplayMode,
) -> Result<u8, Vec<Diagnostic>> {
    if let Some("json") = file.extension().and_then(|e| e.to_str()) {
        return Err(vec![Diagnostic::new(
            "OAM-MOD0003",
            Severity::Error,
            Origin::Resolve,
            format!(
                "a .json file is not a program — import it from a script instead: {}",
                file.display()
            ),
        )]);
    }
    let mut rt = oam_engine::JsRuntime::new();
    // process.argv: [exe, absolute script path, ...script args] — Node's
    // shape. Script args arrive after `--` (cargo-run convention).
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "oam".to_string());
    let script = std::path::absolute(file)
        .unwrap_or_else(|_| file.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut argv = vec![exe, script];
    argv.extend(script_args.iter().cloned());
    rt.set_process_argv(argv);

    if !matches!(replay_mode, oam_engine::ReplayMode::Off) {
        rt.set_replay_mode(replay_mode);
        rt.apply_replay_patches();
    }

    if let Some((addr, brk)) = inspect {
        match rt.attach_inspector(addr, brk) {
            Ok(url) => {
                // Node prints these two lines to stderr; tooling scrapes the
                // first for the WebSocket URL.
                eprintln!("Debugger listening on {url}");
                eprintln!("For help, see: https://oam.sh/docs/inspector");
            }
            Err(e) => {
                return Err(vec![Diagnostic::new(
                    "OAM-RT0004",
                    Severity::Error,
                    Origin::Runtime,
                    format!("could not start inspector on {addr}: {e}"),
                )]);
            }
        }
    }

    // Entry routing follows module kind: .cjs (or "type": "commonjs"
    // project .js) runs as a CJS program through interop; everything else
    // is the ESM graph. See oam_loader::module_kind for the typeless
    // default divergence.
    let result = if oam_loader::module_kind(file) == oam_loader::ModuleKind::Cjs {
        rt.execute_cjs(file)
    } else {
        rt.execute_module(file, &CliHost)
    };
    // On the fatal/uncaught path Node's process 'exit' handlers observe exit 1
    // (an uncaught exception / unhandled rejection forces a non-zero exit). Set
    // process.exitCode = 1 if the script left it unset, so the 'exit' listeners
    // emit_process_exit runs next see 1 rather than 0. The process already
    // returns ExitCode::FAILURE on this path, so this only fixes the code the
    // 'exit' handlers observe (execute_script runs in a fresh scope, so a
    // pending exception from the failed run does not disturb it).
    if result.is_err() {
        let _ = rt.execute_script(
            "<fatal-exit-code>",
            "(function () { var p = globalThis.process; \
               if (p && typeof p.exitCode !== 'number') { p.exitCode = 1; } })();",
        );
    }
    // Node fires 'exit' on BOTH natural completion AND a fatal/uncaught error,
    // so emit before returning either way (process.on('exit') handlers must run
    // even when the program crashed). emit_process_exit is idempotent and runs
    // in a fresh scope, so a pending exception from the failed run is fine.
    rt.emit_process_exit();
    result.map(|()| rt.process_exit_code().unwrap_or(0).clamp(0, 255) as u8)
}

// -- oam compile: embed a pre-bundled JS file into a standalone binary --

/// Magic trailers marking an embedded payload, checked at startup.
/// - v1 (JS only):     `[JS][u64 LE js_len][b"OAMEXEC\0"]` (16-byte trailer)
/// - v2 (JS+bytecode): `[JS][bytecode][u64 LE js_len][u64 LE bc_len][b"OAMEXC2\0"]`
///   (24-byte trailer). v2 embeds V8 bytecode so the compiled binary's first
///   run skips parse+compile even with no writable cache dir.
///
/// Both are read by `extract_embedded`. `oam compile` writes v2 when bytecode
/// production succeeds, else falls back to v1.
const COMPILE_MAGIC: &[u8; 8] = b"OAMEXEC\0";
const COMPILE_MAGIC_V2: &[u8; 8] = b"OAMEXC2\0";

/// Inspect the tail of the current executable for an embedded payload.
/// Returns `Some((source, bytecode))` where `bytecode` is `Some` only for a
/// v2 binary that carries embedded V8 bytecode; `None` for a normal CLI
/// binary. Both the v1 and v2 trailer formats are recognized.
fn extract_embedded() -> Option<(String, Option<Vec<u8>>)> {
    let exe = std::env::current_exe().ok()?;
    let mut f = std::fs::File::open(&exe).ok()?;
    let file_len = f.metadata().ok()?.len();
    if file_len < 8 {
        return None;
    }
    // The magic is always the final 8 bytes.
    let mut magic = [0u8; 8];
    f.seek(SeekFrom::End(-8)).ok()?;
    f.read_exact(&mut magic).ok()?;

    if &magic == COMPILE_MAGIC_V2 {
        // v2 trailer (24 bytes): [js_len u64][bc_len u64][magic].
        if file_len < 24 {
            return None;
        }
        let mut lens = [0u8; 16];
        f.seek(SeekFrom::End(-24)).ok()?;
        f.read_exact(&mut lens).ok()?;
        let js_len = u64::from_le_bytes(lens[0..8].try_into().unwrap());
        let bc_len = u64::from_le_bytes(lens[8..16].try_into().unwrap());
        // The two payloads + 24-byte trailer must fit in the file.
        let payload = js_len.checked_add(bc_len)?;
        if payload > file_len - 24 {
            return None;
        }
        let js_off = file_len - 24 - payload;
        f.seek(SeekFrom::Start(js_off)).ok()?;
        let mut js_buf = vec![0u8; js_len as usize];
        f.read_exact(&mut js_buf).ok()?;
        // The cursor is now at the start of the bytecode (js_off + js_len).
        let mut bc_buf = vec![0u8; bc_len as usize];
        f.read_exact(&mut bc_buf).ok()?;
        let source = String::from_utf8(js_buf).ok()?;
        let bytecode = (!bc_buf.is_empty()).then_some(bc_buf);
        return Some((source, bytecode));
    }

    if &magic == COMPILE_MAGIC {
        // v1 trailer (16 bytes): [js_len u64][magic]. JS only.
        if file_len < 16 {
            return None;
        }
        let mut len_bytes = [0u8; 8];
        f.seek(SeekFrom::End(-16)).ok()?;
        f.read_exact(&mut len_bytes).ok()?;
        let js_len = u64::from_le_bytes(len_bytes);
        if js_len > file_len - 16 {
            return None;
        }
        let offset = file_len - 16 - js_len;
        f.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0u8; js_len as usize];
        f.read_exact(&mut buf).ok()?;
        return Some((String::from_utf8(buf).ok()?, None));
    }

    None
}

/// Execute embedded JS source as a CJS script (the typical output of
/// esbuild/rollup --format=cjs). Supports `--inspect` / `--inspect-brk`
/// flags for debugging the compiled binary.
fn run_embedded(source: &str, bytecode: Option<Vec<u8>>, args: Vec<String>) -> ExitCode {
    // Parse --inspect / --inspect-brk from raw args (we bypass clap for
    // embedded binaries so the user's positional args pass through).
    let mut inspect: Option<(std::net::SocketAddr, bool)> = None;
    let mut script_args: Vec<String> = Vec::new();
    let mut iter = args.iter().skip(1); // skip argv[0]
    #[allow(clippy::while_let_on_iterator)] // need iter.cloned() inside the loop body
    while let Some(arg) = iter.next() {
        if arg == "--inspect-brk" || arg.starts_with("--inspect-brk=") {
            let value = if let Some(v) = arg.strip_prefix("--inspect-brk=") {
                v.to_string()
            } else {
                "127.0.0.1:9229".to_string()
            };
            match resolve_inspect(None, Some(&value)) {
                Ok(v) => inspect = v,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            }
        } else if arg == "--inspect" || arg.starts_with("--inspect=") {
            let value = if let Some(v) = arg.strip_prefix("--inspect=") {
                v.to_string()
            } else {
                "127.0.0.1:9229".to_string()
            };
            match resolve_inspect(Some(&value), None) {
                Ok(v) => inspect = v,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            }
        } else if arg == "--" {
            script_args.extend(iter.cloned());
            break;
        } else {
            script_args.push(arg.clone());
        }
    }

    let mut rt = oam_engine::JsRuntime::new();
    // Seed embedded bytecode (v2 binaries) now that V8 is initialized, so the
    // CJS loader consumes it without needing a writable cache dir. Keyed by the
    // embedded source -- the same string the loader keys on after writing it to
    // the temp file below.
    if let Some(blob) = bytecode {
        oam_engine::seed_cjs_bytecode(source, blob);
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "oam-compiled".to_string());

    if let Some((addr, brk)) = inspect {
        match rt.attach_inspector(addr, brk) {
            Ok(url) => {
                eprintln!("Debugger listening on {url}");
                eprintln!("For help, see: https://oam.sh/docs/inspector");
            }
            Err(e) => {
                eprintln!("could not start inspector: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Write the embedded source to a temp file so the CJS loader has a
    // real path for __filename / __dirname / require() resolution.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp_dir = std::env::temp_dir().join(format!("oam-embed-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir for embedded source");
    let tmp_file = tmp_dir.join("__oam_embedded.js");
    std::fs::write(&tmp_file, source).expect("write embedded source to temp");

    // Node convention: argv[0]=binary, argv[1]=script path. Set argv[1] to
    // the temp file so isEntryPoint checks (import.meta.url vs
    // pathToFileURL(argv[1])) pass -- __filename is this same temp path.
    let mut argv = vec![exe, tmp_file.to_string_lossy().into_owned()];
    argv.extend(script_args);
    rt.set_process_argv(argv);

    let result = rt.execute_cjs(&tmp_file);
    let _ = std::fs::remove_dir_all(&tmp_dir);
    // Fatal path parity: Node's 'exit' handlers observe exit 1 on an
    // uncaught/unhandled failure. Set process.exitCode = 1 if the script left it
    // unset before emitting 'exit' (the process already exits FAILURE here).
    if result.is_err() {
        let _ = rt.execute_script(
            "<fatal-exit-code>",
            "(function () { var p = globalThis.process; \
               if (p && typeof p.exitCode !== 'number') { p.exitCode = 1; } })();",
        );
    }
    // Fire 'exit' on both natural completion and a fatal error (Node parity).
    rt.emit_process_exit();
    match result {
        Ok(()) => ExitCode::from(rt.process_exit_code().unwrap_or(0).clamp(0, 255) as u8),
        Err(diagnostics) => {
            for d in &diagnostics {
                render(d, false);
            }
            ExitCode::FAILURE
        }
    }
}

/// `oam compile <entry> --output <path>`: read the JS source, copy the
/// current oam binary, and append the JS payload with a magic trailer.
fn compile_command(entry: &Path, output: &Path) -> ExitCode {
    // 1. Read the entry JS file.
    let source = match std::fs::read(entry) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("oam compile: could not read {}: {e}", entry.display());
            return ExitCode::FAILURE;
        }
    };

    // Validate it's UTF-8 (JS source must be).
    if std::str::from_utf8(&source).is_err() {
        eprintln!(
            "oam compile: {} is not valid UTF-8 (expected a JS source file)",
            entry.display()
        );
        return ExitCode::FAILURE;
    }

    // 2. Copy the current oam binary to the output path.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("oam compile: could not locate own executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "oam compile: could not create output directory {}: {e}",
            parent.display()
        );
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::copy(&exe, output) {
        eprintln!(
            "oam compile: could not copy binary to {}: {e}",
            output.display()
        );
        return ExitCode::FAILURE;
    }

    // 3. Append: [JS source bytes][u64 LE length][magic "OAMEXEC\0"]
    let mut out_file = match std::fs::OpenOptions::new().append(true).open(output) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "oam compile: could not open {} for append: {e}",
                output.display()
            );
            return ExitCode::FAILURE;
        }
    };
    use std::io::Write;

    // Produce V8 bytecode for the entry so the compiled binary's first run
    // skips parse+compile (v2). If production fails for any reason, fall back
    // to a JS-only v1 binary -- the lazy runtime cache still kicks in there.
    let source_str = std::str::from_utf8(&source).expect("UTF-8 validated above");
    let bytecode = oam_engine::JsRuntime::precompile_cjs_source(source_str);

    let js_len = source.len() as u64;
    let write_result = match &bytecode {
        Some(bc) => out_file
            .write_all(&source)
            .and_then(|()| out_file.write_all(bc))
            .and_then(|()| out_file.write_all(&js_len.to_le_bytes()))
            .and_then(|()| out_file.write_all(&(bc.len() as u64).to_le_bytes()))
            .and_then(|()| out_file.write_all(COMPILE_MAGIC_V2)),
        None => out_file
            .write_all(&source)
            .and_then(|()| out_file.write_all(&js_len.to_le_bytes()))
            .and_then(|()| out_file.write_all(COMPILE_MAGIC)),
    };
    if let Err(e) = write_result {
        eprintln!("oam compile: write failed: {e}");
        let _ = std::fs::remove_file(output);
        return ExitCode::FAILURE;
    }

    let out_abs = std::path::absolute(output).unwrap_or_else(|_| output.to_path_buf());
    match &bytecode {
        Some(bc) => eprintln!(
            "oam compile: {} ({} bytes JS + {} bytes bytecode) -> {}",
            entry.display(),
            source.len(),
            bc.len(),
            out_abs.display()
        ),
        None => eprintln!(
            "oam compile: {} ({} bytes JS, no bytecode embedded) -> {}",
            entry.display(),
            source.len(),
            out_abs.display()
        ),
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{build_self_update_cmd, resolve_inspect};

    #[test]
    fn self_update_cmd_unix_pipes_installer_to_sh() {
        let (prog, args) = build_self_update_cmd(false, "https://oam.sh/install.sh");
        assert_eq!(prog, "sh");
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "curl -fsSL https://oam.sh/install.sh | sh");
    }

    #[test]
    fn self_update_cmd_windows_pipes_installer_to_iex() {
        let (prog, args) = build_self_update_cmd(true, "https://oam.sh/install.ps1");
        assert_eq!(prog, "powershell");
        assert_eq!(args.last().unwrap(), "irm https://oam.sh/install.ps1 | iex");
        assert!(args.contains(&"-NoProfile".to_string()));
    }

    #[test]
    fn inspect_bare_port_binds_127_0_0_1() {
        let (addr, brk) = resolve_inspect(Some("9229"), None).unwrap().unwrap();
        assert_eq!(addr.to_string(), "127.0.0.1:9229");
        assert!(!brk);
    }

    #[test]
    fn inspect_host_port_pair_is_honored() {
        let (addr, brk) = resolve_inspect(Some("0.0.0.0:7000"), None)
            .unwrap()
            .unwrap();
        assert_eq!(addr.to_string(), "0.0.0.0:7000");
        assert!(!brk);
    }

    #[test]
    fn inspect_brk_wins_over_inspect() {
        let (_, brk) = resolve_inspect(Some("9229"), Some("9230"))
            .unwrap()
            .unwrap();
        assert!(brk);
    }

    #[test]
    fn inspect_ipv6_bracketed_host_port_parses() {
        // [::1]:9229 must NOT trip the bare-port heuristic. The whole string
        // is a valid SocketAddr literal; pass-through must succeed.
        let (addr, _) = resolve_inspect(Some("[::1]:9229"), None).unwrap().unwrap();
        assert_eq!(addr.to_string(), "[::1]:9229");
    }

    #[test]
    fn inspect_bare_ipv6_host_fails_with_with_host_in_message() {
        // A bare IPv6 address (no port) is a user error. The error message
        // must surface what was actually parsed -- previously the message
        // echoed the raw input, hiding that ':' triggered the host branch.
        let err = resolve_inspect(Some("::1"), None).unwrap_err();
        assert!(err.contains("::1"), "err: {err}");
        assert!(err.contains("[host:]port"), "err: {err}");
    }

    #[test]
    fn inspect_neither_flag_returns_none() {
        assert!(resolve_inspect(None, None).unwrap().is_none());
    }

    #[test]
    fn compile_magic_is_8_bytes() {
        assert_eq!(super::COMPILE_MAGIC.len(), 8);
        assert_eq!(super::COMPILE_MAGIC, b"OAMEXEC\0");
        assert_eq!(super::COMPILE_MAGIC_V2.len(), 8);
        assert_eq!(super::COMPILE_MAGIC_V2, b"OAMEXC2\0");
    }
}
