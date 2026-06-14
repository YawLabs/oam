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
        /// Attach the V8 Inspector (Chrome DevTools Protocol). Optional
        /// value is `[host:]port` (default 127.0.0.1:9229).
        #[arg(long, num_args = 0..=1, default_missing_value = "127.0.0.1:9229", value_name = "[host:]port")]
        inspect: Option<String>,
        /// Like --inspect, but wait for a debugger to attach and break on the
        /// first line. Optional value is `[host:]port`.
        #[arg(long, num_args = 0..=1, default_missing_value = "127.0.0.1:9229", value_name = "[host:]port")]
        inspect_brk: Option<String>,
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

fn main() -> ExitCode {
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
            script_args,
        } => {
            let inspect = match resolve_inspect(inspect.as_deref(), inspect_brk.as_deref()) {
                Ok(value) => value,
                Err(message) => {
                    eprintln!("oam run: {message}");
                    return ExitCode::FAILURE;
                }
            };
            run_command(file, *check, *no_check, cli.json, script_args, inspect)
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
            run_command(file, CheckMode::Warn, false, cli.json, &[], inspect)
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

    let exit = match run_file(file, script_args, inspect) {
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
    if !matches!(ext, "ts" | "mts" | "js" | "mjs" | "cjs") {
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
            "oam test: no test files found (looked for *.test.* / *.spec.* / *_test.* with js/ts extensions)"
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
                oam_loader::transpile_typescript(path, &source).map_err(|e| e.diagnostics)
            }
            _ => Ok(source),
        }
    }
}

/// Run the entry; Ok carries the process exit code (0, or a natural-exit
/// process.exitCode the script declared — Node honors it, so does oam).
fn run_file(
    file: &Path,
    script_args: &[String],
    inspect: Option<(std::net::SocketAddr, bool)>,
) -> Result<u8, Vec<Diagnostic>> {
    match file.extension().and_then(|e| e.to_str()) {
        Some("cts") => {
            return Err(vec![Diagnostic::new(
                "OAM-MOD0003",
                Severity::Error,
                Origin::Resolve,
                format!(
                    "TypeScript CommonJS (.cts) is not supported — write ESM TypeScript (.ts): {}",
                    file.display()
                ),
            )]);
        }
        Some("json") => {
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
        _ => {}
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
    result.map(|()| rt.process_exit_code().unwrap_or(0).clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::resolve_inspect;

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
}
