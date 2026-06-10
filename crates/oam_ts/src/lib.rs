//! oam_ts: TypeScript type checking via tsgo (the TypeScript 7 Go-native
//! compiler) — the typed half of oam's wedge. Bun executes types-blind;
//! Node strips without checking; `oam check` turns tsgo's output into ODIF
//! so humans get pretty diagnostics and agents get stable codes + spans
//! from the same stream.
//!
//! M1 shape: one-shot `tsgo --pretty false --noEmit` invocation parsed from
//! the classic tsc machine format (`path(line,col): severity TSnnnn: msg`,
//! CWD-relative paths — both verified against tsgo 7.0.0-dev). The warm
//! per-project daemon over the @typescript/api stdio IPC (incremental
//! re-checks, streaming during `oam run`) is the next stage of this crate;
//! the CLI surface and ODIF mapping stay identical when it lands.

// Diagnostic-as-error is ~200 bytes on cold failure paths; boxing would tax
// every caller's API for nothing. Same deliberate stance as oam_loader.
#![allow(clippy::result_large_err)]

use oam_diagnostics::{Diagnostic, Origin, Position, Severity, Span};
use std::path::{Path, PathBuf};
use std::process::Command;

/// tsgo not found / not runnable. Stable code so tooling (and our own e2e
/// skip logic) can detect the condition.
const TSGO_MISSING: &str = "OAM-TS0000";

fn missing_tsgo(detail: &str) -> Diagnostic {
    Diagnostic::new(
        TSGO_MISSING,
        Severity::Error,
        Origin::Typecheck,
        format!(
            "tsgo (TypeScript native compiler) not found: {detail}. Install with \
             `npm i -g @typescript/native-preview` or set OAM_TSGO to the binary path"
        ),
    )
}

/// Locate tsgo: OAM_TSGO env var, then PATH (via the npm .cmd shim on
/// Windows, where CreateProcess only resolves .exe).
fn tsgo_command() -> Result<Command, Diagnostic> {
    if let Ok(explicit) = std::env::var("OAM_TSGO") {
        let path = PathBuf::from(&explicit);
        if !path.is_file() {
            return Err(missing_tsgo(&format!(
                "OAM_TSGO points to {explicit}, which does not exist"
            )));
        }
        return Ok(Command::new(path));
    }
    #[cfg(windows)]
    {
        // npm installs a tsgo.cmd shim; route through cmd /C to resolve it.
        let mut command = Command::new("cmd");
        command.arg("/C").arg("tsgo");
        Ok(command)
    }
    #[cfg(not(windows))]
    {
        Ok(Command::new("tsgo"))
    }
}

/// Walk up from `start` looking for tsconfig.json.
pub fn find_tsconfig(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        let candidate = dir.join("tsconfig.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Type-check `target` (a .ts file or a directory). Returns the diagnostics
/// tsgo produced (empty = clean). Err = could not run the check at all.
pub fn check(target: &Path) -> Result<Vec<Diagnostic>, Diagnostic> {
    let target = std::path::absolute(target).map_err(|e| {
        Diagnostic::new(
            "OAM-TS0002",
            Severity::Error,
            Origin::Typecheck,
            format!("bad check target {}: {e}", target.display()),
        )
    })?;

    let tsconfig = find_tsconfig(&target);
    let (mut command, base) = match (&tsconfig, target.is_file()) {
        (Some(tsconfig), _) => {
            let base = tsconfig
                .parent()
                .expect("tsconfig has a parent")
                .to_path_buf();
            let mut command = tsgo_command()?;
            command.arg("--pretty").arg("false").arg("--noEmit");
            command.arg("-p").arg(tsconfig);
            (command, base)
        }
        (None, true) => {
            let base = target.parent().expect("file has a parent").to_path_buf();
            let mut command = tsgo_command()?;
            command.arg("--pretty").arg("false").arg("--noEmit");
            command.arg(&target);
            (command, base)
        }
        (None, false) => {
            return Err(Diagnostic::new(
                "OAM-TS0003",
                Severity::Error,
                Origin::Typecheck,
                format!(
                    "no tsconfig.json found from {} upward; point oam check at a .ts file or add a tsconfig.json",
                    target.display()
                ),
            ));
        }
    };

    // Run from `base` so the CWD-relative paths in tsgo's output join back
    // to absolute ODIF spans unambiguously.
    let output = command
        .current_dir(&base)
        .output()
        .map_err(|e| missing_tsgo(&e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut diagnostics = parse_tsc_output(&stdout, &base);
    diagnostics.extend(parse_tsc_output(&stderr, &base));

    // Non-zero exit with zero parsed diagnostics = tsgo itself failed
    // (bad flags, crashed): surface raw output rather than claiming clean.
    if diagnostics.is_empty() && !output.status.success() {
        return Err(Diagnostic::new(
            "OAM-TS0004",
            Severity::Error,
            Origin::Typecheck,
            format!(
                "tsgo exited with {} but produced no diagnostics: {}",
                output.status,
                if stderr.trim().is_empty() {
                    stdout.trim()
                } else {
                    stderr.trim()
                }
            ),
        ));
    }
    Ok(diagnostics)
}

/// Parse the classic tsc machine format. Lines that don't match the shape
/// are continuation/elaboration lines and append to the previous diagnostic.
fn parse_tsc_output(output: &str, base: &Path) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_tsc_line(line, base) {
            Some(diagnostic) => diagnostics.push(diagnostic),
            None => {
                if let Some(last) = diagnostics.last_mut() {
                    last.message.push('\n');
                    last.message.push_str(line.trim_end());
                }
            }
        }
    }
    diagnostics
}

/// `path(line,col): severity TSnnnn: message`
fn parse_tsc_line(line: &str, base: &Path) -> Option<Diagnostic> {
    let (location, rest) = line.split_once("): ")?;
    let (file, position) = location.rsplit_once('(')?;
    let (line_str, col_str) = position.split_once(',')?;
    let line_no: u32 = line_str.trim().parse().ok()?;
    let col_no: u32 = col_str.trim().parse().ok()?;

    let (severity_word, rest) = rest.split_once(' ')?;
    let severity = match severity_word {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => Severity::Info,
    };
    let (ts_code, message) = rest.split_once(": ")?;
    let code_number = ts_code.strip_prefix("TS")?;
    code_number.parse::<u32>().ok()?;

    let file = base.join(file);
    let position = Position {
        line: line_no,
        col: col_no,
    };
    Some(
        Diagnostic::new(
            format!("OAM-TS{code_number}"),
            severity,
            Origin::Typecheck,
            message,
        )
        .with_span(Span {
            file: file.to_string_lossy().into_owned(),
            start: position.clone(),
            end: position,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_error_lines_with_spans() {
        let out = "src/a.ts(12,8): error TS2345: Argument of type 'number' is not assignable.\n";
        let diags = parse_tsc_output(out, Path::new("/proj"));
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.code, "OAM-TS2345");
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.origin, Origin::Typecheck);
        assert_eq!(d.spans[0].start, Position { line: 12, col: 8 });
        assert!(d.spans[0].file.ends_with("a.ts"));
    }

    #[test]
    fn continuation_lines_append_to_previous_message() {
        let out = "src/a.ts(1,1): error TS2322: Type 'A' is not assignable to type 'B'.\n  Property 'x' is missing in type 'A'.\n";
        let diags = parse_tsc_output(out, Path::new("/proj"));
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("Property 'x' is missing"));
    }

    #[test]
    fn parses_multiple_diagnostics_and_severities() {
        let out = "a.ts(1,1): error TS1000: one.\nb.ts(2,3): warning TS2000: two.\n";
        let diags = parse_tsc_output(out, Path::new("/p"));
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[1].code, "OAM-TS2000");
    }

    #[test]
    fn garbage_lines_without_prior_diagnostic_are_dropped() {
        let out = "Compiling project...\nnot a diagnostic\n";
        let diags = parse_tsc_output(out, Path::new("/p"));
        assert!(diags.is_empty());
    }
}
