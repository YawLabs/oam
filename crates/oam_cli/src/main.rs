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
        /// Carrier binary to embed into, instead of the running oam. Point it
        /// at an oam release binary for another OS/arch to cross-compile --
        /// the payload is platform-independent, only the carrier is not.
        /// Without this, compile always produces a binary for the platform it
        /// runs on, so shipping N targets needed N machines.
        #[arg(long, value_name = "PATH")]
        carrier: Option<PathBuf>,
        /// Reserved for future use (minify the embedded source).
        #[arg(long)]
        minify: bool,
    },
    /// Update oam in place to the latest release by running the canonical
    /// oamjs.org installer (verifies via the published SHA256SUMS). Updates the
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
    register_worker_host();
    // Internal self-test hook (undocumented, env-gated): exercises the crash
    // path deterministically in CI without needing a JS-reachable panic.
    if std::env::var("OAM_CRASH_SELFTEST").as_deref() == Ok("panic") {
        panic!("oam crash-reporter self-test");
    }
    if let Some((source, bytecode)) = extract_embedded() {
        return run_embedded(&source, bytecode, std::env::args().collect());
    }

    // Node-style top-level flags, intercepted before clap (they are not
    // subcommands): `oam [-flags] -e <code>` / `--eval`, `-p` / `--print`,
    // and `oam --pending-deprecation <script>` (bare-script, node argv
    // shape). Suite children spawn process.execPath with these shapes --
    // the compat surface that unblocks spawnPromisified-shaped tests.
    {
        let argv: Vec<String> = std::env::args().collect();
        let raw: Vec<String> = argv.clone();
        let mut i = 1;
        let mut flags = NodeFlags::default();
        // NODE_OPTIONS, folded in BEFORE argv so an explicit command-line
        // flag still wins. Node gates it behind an allowlist and so does
        // this: it is a strict allowlist of process-level toggles, it never
        // touches `raw`/`i` (so no environment token can reach clap and
        // brick a subcommand), and it deliberately excludes everything that
        // executes code or names a file -- `-e`/`--eval`/`-p`/`--input-type`/
        // `--env-file`/`--permission`. An unrecognized token is ignored
        // rather than fatal, because NODE_OPTIONS is usually set globally in
        // a shell profile and may legitimately carry flags oam has no
        // opinion on.
        let from_env = apply_node_options_env(&mut flags);
        // Node accepts eval flags in any order and in several spellings:
        // `-e code`, `--eval code`, `--eval=code`, `-p code`, `-pe code`
        // (bundled: print + eval), and `-p -e code`. print and eval compose;
        // a later source wins. Leading node-level flags may precede them.
        let mut print = false;
        let mut eval_source: Option<String> = None;
        let mut saw_eval_flag = false;
        let mut bad_flag: Option<&str> = None;
        while i < raw.len() {
            let arg = raw[i].as_str();
            if arg == "--pending-deprecation" {
                flags.pending_deprecation = true;
                i += 1;
            } else if let Some(v) = arg.strip_prefix("--input-type=") {
                flags.input_type = Some(v.to_string());
                i += 1;
            } else if arg == "--input-type" {
                match raw.get(i + 1) {
                    Some(v) => {
                        flags.input_type = Some(v.clone());
                        i += 2;
                    }
                    None => {
                        eprintln!("oam: --input-type requires an argument");
                        return ExitCode::from(9);
                    }
                }
            } else if let Some(v) = arg.strip_prefix("--env-file=") {
                flags.env_files.push((v.to_string(), true));
                i += 1;
            } else if let Some(v) = arg.strip_prefix("--env-file-if-exists=") {
                flags.env_files.push((v.to_string(), false));
                i += 1;
            } else if matches!(arg, "--env-file" | "--env-file-if-exists") {
                let required = arg == "--env-file";
                match raw.get(i + 1) {
                    Some(v) => {
                        flags.env_files.push((v.clone(), required));
                        i += 2;
                    }
                    None => {
                        eprintln!("oam: {arg} requires an argument");
                        return ExitCode::from(9);
                    }
                }
            } else if arg == "--" {
                // End of node options: everything after is the script and
                // its arguments. Consume it (it is a separator, never part
                // of execArgv) and stop option parsing.
                i += 1;
                break;
            } else if arg == "--permission" {
                flags.permission = true;
                i += 1;
            } else if let Some(list) = arg.strip_prefix("--allow-fs-read=") {
                flags.allow_fs_read = Some(list.to_string());
                i += 1;
            } else if let Some(list) = arg.strip_prefix("--allow-fs-write=") {
                flags.allow_fs_write = Some(list.to_string());
                i += 1;
            } else if let Some(list) = arg.strip_prefix("--allow-net=") {
                flags.allow_net = Some(list.to_string());
                i += 1;
            } else if arg == "--allow-net" {
                // Bare form grants everything, stored as the same "*" the list
                // form uses so `grant()` below needs no extra case.
                flags.allow_net = Some("*".to_string());
                i += 1;
            } else if let Some(list) = arg.strip_prefix("--allow-env=") {
                flags.allow_env = Some(list.to_string());
                i += 1;
            } else if arg == "--allow-env" {
                flags.allow_env = Some("*".to_string());
                i += 1;
            } else if arg == "--allow-child-process" {
                flags.allow_child_process = true;
                i += 1;
            } else if arg == "--allow-worker" {
                flags.allow_worker = true;
                i += 1;
            } else if arg == "--allow-addons" {
                flags.allow_addons = true;
                i += 1;
            } else if arg == "--expose-gc" {
                flags.expose_gc = true;
                i += 1;
            } else if arg == "--expose-internals" {
                flags.expose_internals = true;
                i += 1;
            } else if arg == "--experimental-vm-modules" {
                flags.experimental_vm_modules = true;
                i += 1;
            } else if arg == "--allow-natives-syntax" {
                flags.allow_natives_syntax = true;
                i += 1;
            } else if arg == "--js-float16array" {
                flags.js_float16array = true;
                i += 1;
            } else if arg == "--zero-fill-buffers" {
                flags.zero_fill_buffers = true;
                i += 1;
            } else if let Some(v) = arg.strip_prefix("--title=") {
                flags.title = Some(v.to_string());
                i += 1;
            } else if arg == "--no-warnings" {
                flags.no_warnings = true;
                i += 1;
            } else if arg == "--no-deprecation" {
                flags.no_deprecation = true;
                i += 1;
            } else if let Some(v) = arg.strip_prefix("--disable-warning=") {
                flags.disabled_warnings.push(v.to_string());
                i += 1;
            } else if arg == "--disable-warning" {
                match raw.get(i + 1) {
                    Some(v) => {
                        flags.disabled_warnings.push(v.clone());
                        i += 2;
                    }
                    None => {
                        eprintln!("oam: --disable-warning requires an argument");
                        return ExitCode::from(9);
                    }
                }
            } else if let Some(v) = arg.strip_prefix("--redirect-warnings=") {
                flags.redirect_warnings = Some(v.to_string());
                i += 1;
            } else if arg == "--redirect-warnings" {
                match raw.get(i + 1) {
                    Some(v) => {
                        flags.redirect_warnings = Some(v.clone());
                        i += 2;
                    }
                    None => {
                        eprintln!("oam: --redirect-warnings requires an argument");
                        return ExitCode::from(9);
                    }
                }
            } else if let Some(v) = arg.strip_prefix("--eval=") {
                saw_eval_flag = true;
                eval_source = Some(v.to_string());
                i += 1;
            } else if let Some(v) = arg.strip_prefix("--print=") {
                saw_eval_flag = true;
                print = true;
                eval_source = Some(v.to_string());
                i += 1;
            } else if matches!(arg, "-e" | "--eval" | "-p" | "--print" | "-pe" | "-ep") {
                saw_eval_flag = true;
                if arg != "-e" && arg != "--eval" {
                    print = true;
                }
                // -p alone may still be followed by -e; a source consumes.
                if matches!(arg, "-e" | "--eval" | "-pe" | "-ep") || raw.len() > i + 1 {
                    match raw.get(i + 1) {
                        Some(next)
                            if !matches!(
                                next.as_str(),
                                "-e" | "--eval" | "-p" | "--print" | "-pe" | "-ep"
                            ) =>
                        {
                            eval_source = Some(next.clone());
                            i += 2;
                        }
                        Some(_) => i += 1, // another eval flag follows
                        None => {
                            bad_flag = Some(if print { "-p" } else { "-e" });
                            i += 1;
                        }
                    }
                } else {
                    i += 1;
                }
            } else {
                break;
            }
        }
        // V8 flags must be set before V8::initialize, i.e. before any
        // JsRuntime is constructed.
        // The loader decides whether `internal/...` resolves, and it reads
        // this rather than taking a plumbed argument: resolution happens in
        // several places (require, dynamic import, the ESM path) and a
        // process-wide toggle keeps them from disagreeing.
        if flags.expose_internals {
            // SAFETY: single-threaded startup, before any runtime exists.
            unsafe { std::env::set_var("OAM_EXPOSE_INTERNALS", "1") };
        }
        // Same reason: the `vm` module is built inside the snapshot, long
        // before argv is parsed, so the gate has to be readable at call time.
        if flags.experimental_vm_modules {
            // SAFETY: single-threaded startup, before any runtime exists.
            unsafe { std::env::set_var("OAM_EXPERIMENTAL_VM_MODULES", "1") };
        }
        // Collect every node flag that is really a V8 flag: they all have to
        // go in ONE call, because V8::initialize runs on the first one and
        // later calls are ignored.
        {
            let mut v8_flags: Vec<&str> = Vec::new();
            if flags.expose_gc {
                v8_flags.push("--expose-gc");
            }
            if flags.allow_natives_syntax {
                v8_flags.push("--allow-natives-syntax");
            }
            if flags.js_float16array {
                v8_flags.push("--js-float16array");
            }
            if !v8_flags.is_empty() {
                oam_engine::init_platform_with_flags(&v8_flags);
            }
        }
        if let Some(source) = eval_source {
            return run_eval(&source, print, &raw[i..], &flags);
        }
        if saw_eval_flag {
            // Node: `-e` with no code exits 9; bare `-p` reads stdin, which
            // oam does not implement -- report rather than silently hang.
            let f = bad_flag.unwrap_or("-e");
            eprintln!(
                "oam: {f} requires an argument (reading the program from stdin is not supported)"
            );
            return ExitCode::from(9);
        }
        let saw_node_flag = i > 1;
        if i < raw.len() {
            let flag = raw[i].as_str();
            // Bare-script node shape, only when a node flag preceded it AND
            // the argument is an existing file. Node's `node script.js` shape
            // is accepted too: every tool that re-invokes the runtime through
            // process.execPath uses it (test runners, build scripts, and the
            // vendored suite's `"${process.execPath}" "${__filename}" child`).
            // A real SUBCOMMAND always wins, so `oam test` still runs the
            // test runner even if a file named `test` sits in the cwd.
            const SUBCOMMANDS: &[&str] = &[
                "run",
                "test",
                "repl",
                "check",
                "daemon",
                "mcp",
                "serve",
                "install",
                "trust",
                "compile",
                "self-update",
                "help",
            ];
            if !flag.starts_with('-') && !SUBCOMMANDS.contains(&flag) && Path::new(flag).is_file() {
                let file = PathBuf::from(flag);
                return match run_file_with_flags(
                    &file,
                    &raw[i + 1..],
                    None,
                    oam_engine::ReplayMode::Off,
                    &flags,
                ) {
                    Ok(code) => ExitCode::from(code),
                    Err((diagnostics, code)) => {
                        for d in &diagnostics {
                            render(d, false);
                        }
                        ExitCode::from(code)
                    }
                };
            }
        }
        // Node flags followed by an oam SUBCOMMAND (`oam --no-warnings run
        // x.js`) -- the shape child_process.fork emits as
        // [...execArgv, "run", file]. Hand clap the argv with the node
        // flags stripped, and stash them for the run to install; without
        // this, ANY fork({execArgv}) dies with a clap "unexpected argument".
        // Stash whenever anything set flags -- including NODE_OPTIONS alone,
        // where argv carries no flag at all and the run paths would
        // otherwise start with defaults.
        if saw_node_flag || from_env {
            NODE_FLAGS.set(flags.clone()).ok();
        }
        if saw_node_flag {
            let filtered: Vec<String> = std::iter::once(argv[0].clone())
                .chain(raw[i..].iter().cloned())
                .collect();
            let cli = Cli::parse_from(filtered);
            return dispatch(cli);
        }
    }

    dispatch(Cli::parse())
}

/// Node process-level flags parsed before the subcommand, stashed for the
/// run paths to install (set at most once, before dispatch).
static NODE_FLAGS: std::sync::OnceLock<NodeFlags> = std::sync::OnceLock::new();

fn dispatch(cli: Cli) -> ExitCode {
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
            carrier,
            minify: _,
        } => compile_command(entry, output, carrier.as_deref()),
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

/// `oam self-update`: re-run the canonical oamjs.org installer to replace the
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

    // oamjs.org is the canonical home; it serves both installers as
    // text/plain, which is what makes `| sh` and `| iex` work.
    let default_url = if cfg!(target_os = "windows") {
        "https://oamjs.org/install.ps1"
    } else {
        "https://oamjs.org/install.sh"
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
        Err((diagnostics, code)) => {
            for d in &diagnostics {
                render(d, json);
            }
            // The fatal path's exit code honors 'exit'-handler mutations of
            // process.exitCode (Node parity); never 0 -- a fatal stays fatal
            // unless a handler explicitly reset it, which run_file reports.
            ExitCode::from(code)
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
                // Checker unavailable (e.g. no tsgo): report once — never
                // fail a successful run over it. JSON mode emits the ODIF
                // (stable OAM-TS0000, the documented skip signal for tooling
                // and the e2e suite); human mode gets one quiet line. Fully
                // silent json output here masked a missing tsgo on the GCP
                // builder as "typecheck ran clean".
                if json {
                    render(&failure, json);
                } else {
                    eprintln!("oam run: type check skipped: {}", failure.message);
                }
            }
            Err(_) => {
                // Checker still running when the wait deadline hit (cold
                // tsgo / daemon warming under load). Same contract as the
                // unavailable case above: the json stream must say the check
                // did not happen -- a silently empty stream reads as
                // "typecheck ran clean" (this exact silence flaked the e2e
                // suite under full-parallel load).
                if json {
                    render(
                        &Diagnostic::new(
                            "OAM-TS0005",
                            Severity::Warning,
                            Origin::Typecheck,
                            "type check did not finish before the program exited (checker \
                             warming); results are instant on the next run"
                                .to_string(),
                        ),
                        json,
                    );
                } else {
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
        // The test runner emits no 'beforeExit' at the module-eval drain --
        // Node --test emits only after the tests have run.
        rt.suppress_before_exit();
        // Node flags parsed before the subcommand apply to test runtimes too
        // (`oam --no-warnings test x.test.js`); each file gets a fresh
        // isolate, so install per runtime.
        if let Some(flags) = NODE_FLAGS.get() {
            flags.install(&mut rt);
            if flags.apply_env_files(&mut rt).is_err() {
                return ExitCode::from(9);
            }
        }

        let evaluated = if oam_loader::module_kind(file) == oam_loader::ModuleKind::Cjs {
            rt.execute_cjs(file, &CliHost)
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
    } else if d.code == "OAM-RT0005" {
        // An uncaught user exception is reported in Node's format (source
        // frame + stack + own properties). Wrapping it in `error[CODE]:`
        // would bury the stack and break the `^Error:` line tools grep for.
        eprint!("{}", d.message);
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
/// Split NODE_OPTIONS the way Node does: on whitespace, honoring single and
/// double quotes (no escape processing inside quotes).
fn split_node_options(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for ch in raw.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                has = true;
            }
            None if ch.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None => cur.push(ch),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Fold the allowlisted NODE_OPTIONS tokens into `flags`. See the call site
/// for why the allowlist is strict.
fn apply_node_options_env(flags: &mut NodeFlags) -> bool {
    let Ok(raw) = std::env::var("NODE_OPTIONS") else {
        return false;
    };
    let before = flags.clone();
    let tokens = split_node_options(&raw);
    let mut it = tokens.iter().peekable();
    while let Some(tok) = it.next() {
        let tok = tok.as_str();
        if tok == "--pending-deprecation" {
            flags.pending_deprecation = true;
        } else if tok == "--no-warnings" {
            flags.no_warnings = true;
        } else if tok == "--no-deprecation" {
            flags.no_deprecation = true;
        } else if tok == "--expose-gc" {
            flags.expose_gc = true;
        } else if let Some(v) = tok.strip_prefix("--disable-warning=") {
            flags.disabled_warnings.push(v.to_string());
        } else if tok == "--disable-warning"
            && let Some(v) = it.next()
        {
            flags.disabled_warnings.push(v.clone());
        } else if let Some(v) = tok.strip_prefix("--redirect-warnings=") {
            flags.redirect_warnings = Some(v.to_string());
        } else if tok == "--redirect-warnings"
            && let Some(v) = it.next()
        {
            flags.redirect_warnings = Some(v.clone());
        }
        // Anything else: ignored on purpose (see the call site).
    }
    before != *flags
}

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

/// Workers are spawned inside the engine, which cannot name this type -- so
/// hand it a constructor (see oam_engine::set_worker_host_factory).
fn register_worker_host() {
    oam_engine::set_worker_host_factory(|| Box::new(CliHost));
}

impl oam_engine::ModuleHost for CliHost {
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, Vec<Diagnostic>> {
        oam_loader::resolve_import(specifier, referrer).map_err(|d| vec![d])
    }

    fn load(&self, path: &Path) -> Result<String, Vec<Diagnostic>> {
        let kind = oam_loader::classify(path);

        // Anything we'd silently mis-execute gets a clear diagnostic instead:
        // .json would parse as JS and die on 'Unexpected token'. CJS files
        // never reach this host — the engine routes them through interop
        // before load — so a .cjs/.cts here is an engine routing bug.
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "js" | "mjs" | "ts" | "mts" | "tsx" | "jsx" => {}
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
            // .tsx/.jsx take the same oxc pipeline as .ts -- the JSX
            // transform emits the automatic runtime
            // (`import { jsx as _jsx } from "react/jsx-runtime"`), which
            // resolves through normal npm resolution + CJS interop.
            SourceKind::TypeScript | SourceKind::Jsx => {
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
        Err((diagnostics, code)) => {
            for d in &diagnostics {
                render(d, json);
            }
            ExitCode::from(code)
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
) -> Result<u8, (Vec<Diagnostic>, u8)> {
    // Flags parsed before the subcommand (fork's [...execArgv, "run", file]
    // shape) were stashed by main; a direct `oam run` has none.
    let flags = NODE_FLAGS.get().cloned().unwrap_or_default();
    run_file_with_flags(file, script_args, inspect, replay_mode, &flags)
}

fn run_file_with_flags(
    file: &Path,
    script_args: &[String],
    inspect: Option<(std::net::SocketAddr, bool)>,
    replay_mode: oam_engine::ReplayMode,
    flags: &NodeFlags,
) -> Result<u8, (Vec<Diagnostic>, u8)> {
    if let Some("json") = file.extension().and_then(|e| e.to_str()) {
        return Err(err_exit(vec![Diagnostic::new(
            "OAM-MOD0003",
            Severity::Error,
            Origin::Resolve,
            format!(
                "a .json file is not a program — import it from a script instead: {}",
                file.display()
            ),
        )]));
    }
    let mut rt = oam_engine::JsRuntime::new_with_permissions(flags.permissions());
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
                eprintln!("For help, see: https://oamjs.org/docs/inspector");
            }
            Err(e) => {
                return Err(err_exit(vec![Diagnostic::new(
                    "OAM-RT0004",
                    Severity::Error,
                    Origin::Runtime,
                    format!("could not start inspector on {addr}: {e}"),
                )]));
            }
        }
    }

    flags.install(&mut rt);
    if let Err(code) = flags.apply_env_files(&mut rt) {
        return Err((Vec::new(), code));
    }
    let exec_argv = flags.exec_argv();
    if !exec_argv.is_empty() {
        let json = serde_json::to_string(&exec_argv).unwrap_or_else(|_| "[]".into());
        let _ = rt.execute_script("<flags>", &format!("globalThis.process.execArgv = {json};"));
    }

    // Entry routing follows module kind: .cjs (or "type": "commonjs"
    // project .js) runs as a CJS program through interop; everything else
    // is the ESM graph. See oam_loader::module_kind for the typeless
    // default divergence.
    let result = if oam_loader::module_kind(file) == oam_loader::ModuleKind::Cjs {
        rt.execute_cjs(file, &CliHost)
    } else {
        rt.execute_module(file, &CliHost)
    };
    match result {
        Ok(()) => {
            rt.emit_process_exit();
            // Node's EmitExit RE-READS process.exitCode after the 'exit'
            // handlers ran: a handler mutation decides the final code.
            Ok(rt.process_exit_code().unwrap_or(0).clamp(0, 255) as u8)
        }
        Err(diagnostics) => {
            // Fatal sub-codes (6 = _fatalException monkeypatched away,
            // 7 = throwing uncaughtException handler): Node exits WITHOUT
            // emitting 'exit', and the code is not handler-overridable.
            let sub_code = rt
                .execute_script(
                    "<fatal-sub-code>",
                    "String((globalThis.process && globalThis.process.__oamFatalCode) || '')",
                )
                .ok()
                .and_then(|s| s.parse::<u8>().ok());
            if let Some(code) = sub_code {
                return Err((diagnostics, code));
            }
            // Regular fatal: Node forces process.exitCode = 1 at
            // TriggerUncaughtException (UNCONDITIONALLY -- an earlier script
            // assignment loses), emits 'exit', then re-reads: a handler
            // mutating exitCode (98, or back to 0) decides the final code.
            let _ = rt.execute_script(
                "<fatal-exit-code>",
                "(function () { var p = globalThis.process; if (p) { p.exitCode = 1; } })();",
            );
            rt.emit_process_exit();
            let final_code = rt.process_exit_code().unwrap_or(1).clamp(0, 255) as u8;
            Err((diagnostics, final_code))
        }
    }
}

/// Pre-engine failure: diagnostics with Node's generic fatal exit code 1.
fn err_exit(diagnostics: Vec<Diagnostic>) -> (Vec<Diagnostic>, u8) {
    (diagnostics, 1)
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

/// True when `bytes` already ends with either compile trailer magic, i.e. it
/// is itself a compile output rather than a plain oam binary. Used to reject
/// such a file as a `--carrier`: appending onto it would nest payloads, and
/// since the loader reads the LAST trailer it would silently keep running the
/// older embedded program.
fn ends_with_compile_magic(bytes: &[u8]) -> bool {
    let Some(tail) = bytes.get(bytes.len().wrapping_sub(8)..) else {
        return false;
    };
    tail == COMPILE_MAGIC.as_slice() || tail == COMPILE_MAGIC_V2.as_slice()
}

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
/// `oam -e <code>` / `oam -p <code>`: evaluate a string as a CJS program,
/// Node-shaped. The source runs from a file named `[eval]` inside the CWD
/// so require() resolves relative paths and node_modules from the CWD the
/// way node's eval does; -p prints the completion value via console.log.
/// process.argv = [exe, ...extra] (node omits a script-path slot for eval).
/// Falls back to a temp dir when the CWD is not writable (read-only mount,
/// sandbox) -- require() resolution then roots at that temp dir.
/// Node process-level flags accepted before any oam subcommand (also read
/// from NODE_OPTIONS). They reach JS as non-enumerable globals -- node's own
/// test/common leaked-globals check walks enumerable globals.
#[derive(Default, Clone, PartialEq)]
struct NodeFlags {
    pending_deprecation: bool,
    expose_gc: bool,
    permission: bool,
    /// Node's granular grants. Only meaningful under `--permission`, which
    /// denies everything first.
    allow_fs_read: Option<String>,
    allow_fs_write: Option<String>,
    /// `--allow-net[=list]` and `--allow-env[=list]`. Bare form grants
    /// everything (stored as `Some("*")`); `=a,b` grants a list.
    ///
    /// Without these, `permissions()` hardcoded net and env closed, so
    /// `--permission` denied all network access with no way to open it. That
    /// made the whole permission model unusable for any server: a process
    /// that cannot open a socket has nothing left to do.
    ///
    /// Both are enforced end to end. `Permissions::check_env` already existed
    /// (oam_engine/src/permissions.rs:142) but nothing called it -- `op_env`
    /// copied the whole environment unconditionally, so env reads succeeded
    /// under `--permission` regardless of any grant. `op_env` now filters
    /// through it, so a denied variable is absent from `process.env` rather
    /// than throwing: it is a snapshot object with no per-property hook, and
    /// an exception would leak the names of variables the script may not see.
    allow_net: Option<String>,
    allow_env: Option<String>,
    allow_child_process: bool,
    allow_worker: bool,
    allow_addons: bool,
    /// `--env-file=P` (required) and `--env-file-if-exists=P` (optional), in
    /// command-line order -- Node applies them left to right, later files
    /// winning.
    env_files: Vec<(String, bool)>,
    /// `--input-type=commonjs|module`: how `--eval` source is interpreted.
    /// oam runs eval from a real file, so this picks the extension.
    input_type: Option<String>,
    no_warnings: bool,
    no_deprecation: bool,
    disabled_warnings: Vec<String>,
    redirect_warnings: Option<String>,
    /// Node flags that are really V8 flags: forwarded verbatim before
    /// V8::initialize rather than reimplemented.
    allow_natives_syntax: bool,
    js_float16array: bool,
    /// `--zero-fill-buffers`: Buffer.allocUnsafe behaves like Buffer.alloc.
    zero_fill_buffers: bool,
    /// `--title=x`: the initial process.title.
    title: Option<String>,
    /// `--expose-internals`: make `require('internal/...')` resolve. A
    /// test/debug surface in Node too -- never listed in builtinModules.
    expose_internals: bool,
    experimental_vm_modules: bool,
}

impl NodeFlags {
    fn install(&self, rt: &mut oam_engine::JsRuntime) {
        let pending = self.pending_deprecation
            || std::env::var("NODE_PENDING_DEPRECATION").as_deref() == Ok("1");
        let def = |name: &str, value: String| {
            format!(
                "Object.defineProperty(globalThis, '{name}', \
                   {{ value: {value}, writable: false, enumerable: false, configurable: false }});"
            )
        };
        let mut js = String::new();
        if pending {
            js.push_str(&def("__oamPendingDeprecation", "true".into()));
        }
        if self.no_warnings {
            js.push_str(&def("__oamNoWarnings", "true".into()));
        }
        if self.no_deprecation {
            js.push_str(&def("__oamNoDeprecation", "true".into()));
            js.push_str("globalThis.process.noDeprecation = true;");
        }
        if !self.disabled_warnings.is_empty() {
            let list =
                serde_json::to_string(&self.disabled_warnings).unwrap_or_else(|_| "[]".into());
            js.push_str(&def("__oamDisabledWarnings", list));
        }
        if let Some(path) = &self.redirect_warnings {
            let p = serde_json::to_string(path).unwrap_or_else(|_| "\"\"".into());
            js.push_str(&def("__oamRedirectWarnings", p));
        }
        if self.zero_fill_buffers {
            js.push_str(&def("__oamZeroFillBuffers", "true".into()));
        }
        if let Some(title) = &self.title {
            let t = serde_json::to_string(title).unwrap_or_else(|_| "\"\"".into());
            js.push_str(&format!("globalThis.process.title = {t};"));
        }
        if !js.is_empty() {
            let _ = rt.execute_script("<flags>", &js);
            // Workers and forks get their own isolate; publish the same
            // snippet so they inherit the flags instead of starting clean.
            oam_engine::set_inherited_flags_js(js);
        }
    }

    /// Apply `--env-file` before the program runs. Node parses these in C++
    /// (node_dotenv.cc); oam reuses the same parser userland gets via
    /// `process.loadEnvFile` so the two can never drift. Err = exit code 9,
    /// matching Node's abort on a missing required file.
    fn apply_env_files(&self, rt: &mut oam_engine::JsRuntime) -> Result<(), u8> {
        for (path, required) in &self.env_files {
            let literal = serde_json::to_string(path).unwrap_or_else(|_| "\"\"".into());
            let script = format!("globalThis.process.loadEnvFile({literal});");
            if let Err(e) = rt.execute_script("<env-file>", &script) {
                if *required {
                    let exe = std::env::current_exe()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "oam".to_string());
                    let _ = e;
                    eprintln!("{exe}: {path}: not found");
                    return Err(9);
                }
                eprintln!("{path} not found. Continuing without it.");
            }
        }
        Ok(())
    }

    /// Node's `--permission` denies everything not explicitly allowed; oam's
    /// permission checks are already enforced in the native fs/net/env ops,
    /// so the flag just constructs the restrictive option set.
    fn permissions(&self) -> Option<oam_engine::PermissionsOptions> {
        if !self.permission {
            return None;
        }
        // `--allow-fs-read=*` is "everything"; anything else is a
        // comma-separated allow-list, which is how node spells it.
        fn grant(list: &Option<String>) -> oam_engine::BoolOrList {
            match list {
                None => oam_engine::BoolOrList::Bool(false),
                Some(spec) if spec == "*" => oam_engine::BoolOrList::Bool(true),
                Some(spec) => oam_engine::BoolOrList::List(
                    spec.split(',')
                        .map(str::trim)
                        .filter(|entry| !entry.is_empty())
                        .map(str::to_string)
                        .collect(),
                ),
            }
        }
        Some(oam_engine::PermissionsOptions {
            read: grant(&self.allow_fs_read),
            write: grant(&self.allow_fs_write),
            net: grant(&self.allow_net),
            env: grant(&self.allow_env),
            // --allow-addons is node's name for loading native addons, which
            // is what oam's `ffi` permission gates.
            ffi: oam_engine::BoolOrList::Bool(self.allow_addons),
            // A worker can spawn, so --allow-worker implies the same grant.
            child: oam_engine::BoolOrList::Bool(self.allow_child_process || self.allow_worker),
        })
    }

    /// The subset node reflects in process.execArgv (NODE_OPTIONS tokens are
    /// excluded there, so callers pass only argv-sourced flags).
    fn exec_argv(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.pending_deprecation {
            out.push("--pending-deprecation".into());
        }
        if self.expose_gc {
            out.push("--expose-gc".into());
        }
        if self.experimental_vm_modules {
            out.push("--experimental-vm-modules".into());
        }
        if self.permission {
            out.push("--permission".into());
        }
        if let Some(list) = &self.allow_fs_read {
            out.push(format!("--allow-fs-read={list}"));
        }
        if let Some(list) = &self.allow_fs_write {
            out.push(format!("--allow-fs-write={list}"));
        }
        // Round-trip the bare form as bare: a script reading process.execArgv
        // to re-spawn itself should get back the flags it was given, not a
        // `=*` spelling node never produces.
        if let Some(list) = &self.allow_net {
            out.push(if list == "*" {
                "--allow-net".into()
            } else {
                format!("--allow-net={list}")
            });
        }
        if let Some(list) = &self.allow_env {
            out.push(if list == "*" {
                "--allow-env".into()
            } else {
                format!("--allow-env={list}")
            });
        }
        if self.allow_child_process {
            out.push("--allow-child-process".into());
        }
        if self.allow_worker {
            out.push("--allow-worker".into());
        }
        if self.allow_addons {
            out.push("--allow-addons".into());
        }
        if self.no_warnings {
            out.push("--no-warnings".into());
        }
        if self.no_deprecation {
            out.push("--no-deprecation".into());
        }
        for w in &self.disabled_warnings {
            out.push(format!("--disable-warning={w}"));
        }
        if let Some(p) = &self.redirect_warnings {
            out.push(format!("--redirect-warnings={p}"));
        }
        out
    }
}

/// Remove the eval source file (and its temp dir, when the CWD was not
/// writable). Best-effort: a `process.exit()` inside the eval terminates the
/// process before this runs, so the CWD file name carries the pid and a
/// stale-file sweep is not attempted.
fn cleanup_eval_artifacts(tmp_dir: Option<&Path>, tmp_file: &Path) {
    match tmp_dir {
        Some(dir) => {
            let _ = std::fs::remove_dir_all(dir);
        }
        None => {
            let _ = std::fs::remove_file(tmp_file);
        }
    }
}

fn run_eval(source: &str, print: bool, extra_args: &[String], flags: &NodeFlags) -> ExitCode {
    // Node's eval module resolves from CWD; oam executes a real file, so
    // place it in the CWD (unique name, cleaned up below).
    // Node reads --input-type to decide how to parse --eval source; oam runs
    // it from a real file, so the extension carries the same decision.
    let ext = match flags.input_type.as_deref() {
        None | Some("commonjs") => "cjs",
        Some("module") => "mjs",
        Some(other) => {
            eprintln!("oam: --input-type must be \"commonjs\" or \"module\", got \"{other}\"");
            return ExitCode::from(9);
        }
    };
    let cwd_candidate = std::env::current_dir()
        .ok()
        .map(|d| d.join(format!(".oam-eval-{}.{ext}", std::process::id())));
    let (tmp_dir, tmp_file) = match cwd_candidate {
        Some(p) if std::fs::write(&p, "").is_ok() => (None, p),
        _ => {
            let dir = std::env::temp_dir().join(format!("oam-eval-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create eval temp dir");
            let file = dir.join(format!("__oam_eval.{ext}"));
            (Some(dir), file)
        }
    };
    // -p evaluates through a DIRECT eval, which yields the completion value
    // of a statement list the way node's script evaluation does
    // (`-p "a(); b"` prints b) AND inherits the enclosing module scope. It
    // used to be an indirect `(0, eval)`, whose scope is the GLOBAL one --
    // so `oam -pe "require('assert')"` died with "require is not defined"
    // while the identical `-e` worked, because require is a parameter of
    // the CJS module wrapper, not a global.
    //
    // The user source stays the FIRST thing in the file so a leading
    // 'use strict' keeps its directive-prologue position and line numbers
    // are not shifted; the builtin-globals prelude runs separately, before
    // this file (see EVAL_PRELUDE at the call site).
    let body = if print {
        format!(
            "console.log(eval({}));\n",
            serde_json::to_string(source).expect("encode eval source")
        )
    } else {
        source.to_string()
    };
    std::fs::write(&tmp_file, body).expect("write eval source to temp");

    let mut rt = oam_engine::JsRuntime::new_with_permissions(flags.permissions());
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "oam".to_string());
    let mut argv = vec![exe];
    argv.extend(extra_args.iter().cloned());
    rt.set_process_argv(argv);
    // Node's eval context exposes builtin modules as lazy globals
    // (addBuiltinLibsToObject): `oam -e "url.parse(...)"` works bare, and
    // assignment overrides the getter. Sub-path builtins (timers/promises),
    // '_'-prefixed legacy aliases, and existing globals are skipped. Runs
    // as its own script so the user source keeps prologue position.
    // Sourced from the builtin registry, NOT require(): this runs as a bare
    // script (no CJS scope), and it must run before the user source so the
    // globals exist without displacing the source's directive prologue.
    const EVAL_PRELUDE: &str = "(function () {\n  var reg = globalThis.__oamNode;\n  if (!reg || !reg.factories || typeof reg.get !== 'function') return;\n  var names = Object.keys(reg.factories);\n  for (var i = 0; i < names.length; i++) (function (n) {\n    if (n.indexOf('/') !== -1 || n.charAt(0) === '_' || n === 'module' || n in globalThis) return;\n    Object.defineProperty(globalThis, n, {\n      configurable: true, enumerable: false,\n      get: function () {\n        var v = reg.get(n);\n        Object.defineProperty(globalThis, n, { configurable: true, enumerable: false, writable: true, value: v });\n        return v;\n      },\n      set: function (v) {\n        Object.defineProperty(globalThis, n, { configurable: true, enumerable: false, writable: true, value: v });\n      },\n    });\n  })(names[i]);\n})();\n";
    let _ = rt.execute_script("<eval-prelude>", EVAL_PRELUDE);
    // execArgv carries the node flags (suite children re-spawn with
    // [...process.execArgv, ...]); the eval source itself is not included.
    flags.install(&mut rt);
    if flags.apply_env_files(&mut rt).is_err() {
        return ExitCode::from(9);
    }
    let exec_argv = flags.exec_argv();
    if !exec_argv.is_empty() {
        let json = serde_json::to_string(&exec_argv).unwrap_or_else(|_| "[]".into());
        let _ = rt.execute_script("<flags>", &format!("globalThis.process.execArgv = {json};"));
    }

    // --input-type=module makes the eval source an ES module.
    let result = if ext == "mjs" {
        rt.execute_module(&tmp_file, &CliHost)
    } else {
        rt.execute_cjs(&tmp_file, &CliHost)
    };
    cleanup_eval_artifacts(tmp_dir.as_deref(), &tmp_file);
    // Same fatal-path shape as run_file: sub-codes skip 'exit', a regular
    // fatal forces 1 + honors exit-handler mutations.
    match result {
        Ok(()) => {
            rt.emit_process_exit();
            ExitCode::from(rt.process_exit_code().unwrap_or(0).clamp(0, 255) as u8)
        }
        Err(diagnostics) => {
            for d in &diagnostics {
                render(d, false);
            }
            let sub_code = rt
                .execute_script(
                    "<fatal-sub-code>",
                    "String((globalThis.process && globalThis.process.__oamFatalCode) || '')",
                )
                .ok()
                .and_then(|s| s.parse::<u8>().ok());
            if let Some(code) = sub_code {
                return ExitCode::from(code);
            }
            let _ = rt.execute_script(
                "<fatal-exit-code>",
                "(function () { var p = globalThis.process; if (p) { p.exitCode = 1; } })();",
            );
            rt.emit_process_exit();
            ExitCode::from(rt.process_exit_code().unwrap_or(1).clamp(0, 255) as u8)
        }
    }
}

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
                eprintln!("For help, see: https://oamjs.org/docs/inspector");
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

    let result = rt.execute_cjs(&tmp_file, &CliHost);
    let _ = std::fs::remove_dir_all(&tmp_dir);
    // Same fatal-path shape as run_file: sub-codes 6/7 skip the 'exit'
    // event entirely and are not handler-overridable; a regular fatal
    // forces exitCode = 1, emits 'exit', then honors handler mutations.
    match result {
        Ok(()) => {
            rt.emit_process_exit();
            ExitCode::from(rt.process_exit_code().unwrap_or(0).clamp(0, 255) as u8)
        }
        Err(diagnostics) => {
            for d in &diagnostics {
                render(d, false);
            }
            let sub_code = rt
                .execute_script(
                    "<fatal-sub-code>",
                    "String((globalThis.process && globalThis.process.__oamFatalCode) || '')",
                )
                .ok()
                .and_then(|s| s.parse::<u8>().ok());
            if let Some(code) = sub_code {
                return ExitCode::from(code);
            }
            let _ = rt.execute_script(
                "<fatal-exit-code>",
                "(function () { var p = globalThis.process; if (p) { p.exitCode = 1; } })();",
            );
            rt.emit_process_exit();
            ExitCode::from(rt.process_exit_code().unwrap_or(1).clamp(0, 255) as u8)
        }
    }
}

/// `oam compile <entry> --output <path>`: read the JS source, copy the
/// current oam binary, and append the JS payload with a magic trailer.
fn compile_command(entry: &Path, output: &Path, carrier: Option<&Path>) -> ExitCode {
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

    // 2. Copy the carrier binary to the output path. Defaults to the running
    //    oam; `--carrier` points at a different one so a release binary for
    //    another OS/arch can be used, which is what makes cross-compiling
    //    possible. The payload appended below is platform-independent -- only
    //    the carrier decides the output's platform.
    let exe = match carrier {
        Some(p) => {
            if !p.is_file() {
                eprintln!("oam compile: carrier {} is not a file", p.display());
                return ExitCode::FAILURE;
            }
            // Refuse a carrier that already carries a payload. Compiling onto
            // a previous compile output would nest payloads, and the loader
            // reads the LAST magic footer -- so it would silently run the
            // older embedded program instead of this one.
            match std::fs::read(p) {
                Ok(bytes) if ends_with_compile_magic(&bytes) => {
                    eprintln!(
                        "oam compile: carrier {} is already a compiled output (it ends with the \
                         payload magic). Pass a plain oam binary.",
                        p.display()
                    );
                    return ExitCode::FAILURE;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("oam compile: could not read carrier {}: {e}", p.display());
                    return ExitCode::FAILURE;
                }
            }
            p.to_path_buf()
        }
        None => match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("oam compile: could not locate own executable: {e}");
                return ExitCode::FAILURE;
            }
        },
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
    // The output embeds the whole oam runtime -- V8 (BSD-3), ICU (Unicode),
    // the Node streams port (MIT) and ~380 Rust crates. Whoever ships that
    // binary inherits their notice obligations, and would have no way to know
    // it from a success line that only reports byte counts.
    eprintln!(
        "oam compile: the output embeds oam's runtime (V8, ICU and others);          if you redistribute it, ship the notices from LICENSE, NOTICE and          THIRD_PARTY_LICENSES.md with it"
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{build_self_update_cmd, resolve_inspect};

    #[test]
    fn self_update_cmd_unix_pipes_installer_to_sh() {
        let (prog, args) = build_self_update_cmd(false, "https://oamjs.org/install.sh");
        assert_eq!(prog, "sh");
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "curl -fsSL https://oamjs.org/install.sh | sh");
    }

    #[test]
    fn self_update_cmd_windows_pipes_installer_to_iex() {
        let (prog, args) = build_self_update_cmd(true, "https://oamjs.org/install.ps1");
        assert_eq!(prog, "powershell");
        assert_eq!(
            args.last().unwrap(),
            "irm https://oamjs.org/install.ps1 | iex"
        );
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

    // --- `--carrier` guard ------------------------------------------------

    #[test]
    fn ends_with_compile_magic_detects_both_trailer_versions() {
        let mut v1 = b"a plain binary".to_vec();
        v1.extend_from_slice(super::COMPILE_MAGIC);
        assert!(super::ends_with_compile_magic(&v1));

        let mut v2 = b"a plain binary".to_vec();
        v2.extend_from_slice(super::COMPILE_MAGIC_V2);
        assert!(super::ends_with_compile_magic(&v2));
    }

    #[test]
    fn ends_with_compile_magic_rejects_plain_and_short_input() {
        assert!(!super::ends_with_compile_magic(b"not a compile output"));
        // The magic must be at the END, not merely present.
        let mut leading = super::COMPILE_MAGIC.to_vec();
        leading.extend_from_slice(b"trailing bytes");
        assert!(!super::ends_with_compile_magic(&leading));
        // Shorter than the magic: must not panic on the subtraction.
        assert!(!super::ends_with_compile_magic(b""));
        assert!(!super::ends_with_compile_magic(b"abc"));
    }

    // --- `--allow-net` / `--allow-env` ------------------------------------

    fn perms_for(flags: &super::NodeFlags) -> oam_engine::PermissionsOptions {
        flags.permissions().expect("--permission was set")
    }

    fn is_denied(g: &oam_engine::BoolOrList) -> bool {
        matches!(g, oam_engine::BoolOrList::Bool(false))
    }

    #[test]
    fn permission_without_net_or_env_grants_denies_both() {
        // The pre-existing posture, now reachable only when the grants are
        // actually absent rather than hardcoded.
        let flags = super::NodeFlags {
            permission: true,
            ..Default::default()
        };
        let p = perms_for(&flags);
        assert!(is_denied(&p.net));
        assert!(is_denied(&p.env));
    }

    #[test]
    fn bare_allow_net_and_allow_env_grant_everything() {
        let flags = super::NodeFlags {
            permission: true,
            allow_net: Some("*".into()),
            allow_env: Some("*".into()),
            ..Default::default()
        };
        let p = perms_for(&flags);
        assert!(matches!(p.net, oam_engine::BoolOrList::Bool(true)));
        assert!(matches!(p.env, oam_engine::BoolOrList::Bool(true)));
    }

    #[test]
    fn list_form_grants_only_the_listed_entries() {
        let flags = super::NodeFlags {
            permission: true,
            allow_net: Some("127.0.0.1:5432, example.com".into()),
            allow_env: Some("DATABASE_URL".into()),
            ..Default::default()
        };
        let p = perms_for(&flags);
        match p.net {
            // Whitespace around list entries is trimmed, so a human-spaced
            // list behaves the same as a tight one.
            oam_engine::BoolOrList::List(ref v) => {
                assert_eq!(
                    v,
                    &["127.0.0.1:5432".to_string(), "example.com".to_string()]
                );
            }
            _ => panic!("expected a list grant, got {:?}", p.net),
        }
        match p.env {
            oam_engine::BoolOrList::List(ref v) => assert_eq!(v, &["DATABASE_URL".to_string()]),
            _ => panic!("expected a list grant, got {:?}", p.env),
        }
    }

    #[test]
    fn grants_are_inert_without_permission() {
        // Node's grants only mean anything under --permission; asking for one
        // without it must not silently enable the restrictive mode.
        let flags = super::NodeFlags {
            permission: false,
            allow_net: Some("*".into()),
            ..Default::default()
        };
        assert!(flags.permissions().is_none());
    }

    #[test]
    fn exec_argv_round_trips_bare_and_list_forms() {
        // A script re-spawning itself from process.execArgv must get back what
        // it was given -- bare stays bare, a list stays a list.
        let bare = super::NodeFlags {
            permission: true,
            allow_net: Some("*".into()),
            allow_env: Some("*".into()),
            ..Default::default()
        };
        let argv = bare.exec_argv();
        assert!(argv.contains(&"--allow-net".to_string()));
        assert!(argv.contains(&"--allow-env".to_string()));
        assert!(!argv.iter().any(|a| a.contains("=*")));

        let listed = super::NodeFlags {
            permission: true,
            allow_net: Some("a,b".into()),
            ..Default::default()
        };
        assert!(listed.exec_argv().contains(&"--allow-net=a,b".to_string()));
    }
}
