#!/usr/bin/env python3
"""Add Install + Compile match arms and install_command/find_project_dir functions."""
import pathlib, sys

p = pathlib.Path("crates/oam_cli/src/main.rs")
src = p.read_text(encoding="utf-8")
orig = src

# 1. Add Install + Compile match arms before DaemonServe
old_daemon = '        Command::DaemonServe { tsconfig } => match oam_ts::daemon::serve(tsconfig) {'
new_arms = '''        Command::Install { frozen_lockfile } => {
            install_command(*frozen_lockfile, cli.json)
        }
        Command::Compile {
            entry,
            output,
            minify: _,
        } => compile_command(entry, output),
        Command::DaemonServe { tsconfig } => match oam_ts::daemon::serve(tsconfig) {'''

assert old_daemon in src, "could not find DaemonServe match arm"
src = src.replace(old_daemon, new_arms, 1)

# 2. Add install_command and find_project_dir functions before the CliHost struct
old_clihost = '/// The CLI\'s module host: filesystem loading + oxc transpilation, with the\n/// resolution rules from oam_loader.'
install_fns = r'''/// `oam install`: frozen-lockfile package install from package-lock.json v3.
fn install_command(frozen_lockfile: bool, json: bool) -> ExitCode {
    // Walk upward from cwd to find the directory containing package-lock.json.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_dir = find_project_dir(&cwd, "package-lock.json").unwrap_or(cwd);

    match oam_loader::install::install(&project_dir, frozen_lockfile) {
        Ok(summary) => {
            if json {
                let d = Diagnostic::new(
                    "OAM-PKG0000",
                    Severity::Info,
                    Origin::Install,
                    format!(
                        "installed {} package(s) in {:.1}s",
                        summary.packages_installed,
                        summary.elapsed.as_secs_f64()
                    ),
                );
                render(&d, true);
            } else {
                eprintln!(
                    "oam install: {} package(s) in {:.1}s",
                    summary.packages_installed,
                    summary.elapsed.as_secs_f64()
                );
            }
            ExitCode::SUCCESS
        }
        Err(diagnostics) => {
            for d in &diagnostics {
                render(d, json);
            }
            ExitCode::FAILURE
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

''' + old_clihost

assert old_clihost in src, "could not find CliHost struct marker"
src = src.replace(old_clihost, install_fns, 1)

if src == orig:
    print("ERROR: no changes applied", file=sys.stderr)
    sys.exit(1)

p.write_text(src, encoding="utf-8")
print("OK -- added Install+Compile match arms + install_command + find_project_dir")
