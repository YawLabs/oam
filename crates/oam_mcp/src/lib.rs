//! oam_mcp: the runtime's introspection, served to coding agents over MCP.
//!
//! This is the agent half of the wedge: every tool result is ODIF — the
//! same stable codes and spans the CLI emits — so an agent can run
//! check -> fix -> run without scraping prose.
//!
//! v1 transport: stdio (newline-delimited JSON-RPC 2.0), the shape local
//! agents (Claude Code, Cursor) consume dev tools in. The stateless
//! Streamable HTTP transport (MCP 2026-07-28) lands with oam_http.
//! Dispatch is a pure function (message in, response out) so the protocol
//! logic unit-tests without processes or pipes.

use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Newest protocol revision we know; we echo the client's requested version
/// (stateless servers don't negotiate sessions).
const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";

/// Serve MCP over stdio until stdin closes. Responses are written as one
/// JSON line each; notifications produce no response. Lines are read as
/// bytes and lossy-decoded: one non-UTF-8 line from a buggy client gets the
/// graceful -32700, not server death (BufRead::lines would Err and exit).
pub fn serve_stdio() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if stdin.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(response) = handle_line(line) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
}

/// One message in, at most one response out (None for notifications).
pub fn handle_line(line: &str) -> Option<String> {
    let message: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return Some(
                error_response(Value::Null, -32700, "parse error: not valid JSON").to_string(),
            );
        }
    };
    handle_message(&message).map(|r| r.to_string())
}

fn handle_message(message: &Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(json!({}));

    // Notifications (no id) get processing but never a response.
    let is_notification = id.is_none();
    if is_notification {
        return None;
    }
    let id = id.unwrap_or(Value::Null);

    let result = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            json!({
                "protocolVersion": requested,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "oam", "version": env!("CARGO_PKG_VERSION") },
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_definitions() }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            match call_tool(name, &arguments) {
                Ok(result) => result,
                Err(message) => {
                    return Some(error_response(id, -32602, &message));
                }
            }
        }
        _ => {
            return Some(error_response(
                id,
                -32601,
                &format!("method not found: {method}"),
            ));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "oam_check",
            "description": "Type-check a TypeScript file or project with tsgo (TypeScript 7 native). Uses the per-project daemon for instant repeat checks; falls back to a one-shot check if the daemon is unavailable. Returns ODIF diagnostics: stable codes (OAM-TS*), file:line:col spans, severities. Empty diagnostics = clean.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "A .ts file or directory; nearest tsconfig.json upward wins. Defaults to cwd." }
                }
            }
        },
        {
            "name": "oam_run",
            "description": "Run a JavaScript/TypeScript file with the oam runtime. Returns exit code, stdout, and ODIF diagnostics (parse/resolve/runtime errors) as structured data. Kills the script and reports timedOut=true if it exceeds the time budget (long-lived servers/intervals will always hit it).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Path to the .js/.mjs/.ts/.mts entry file." },
                    "timeoutMs": { "type": "number", "description": "Time budget in milliseconds (default 30000, max 600000)." }
                },
                "required": ["file"]
            }
        },
        {
            "name": "oam_project_info",
            "description": "Runtime and project facts: oam version, cwd, nearest tsconfig.json, whether tsgo (type checker) is available.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "oam_explain",
            "description": "Explain an ODIF diagnostic code (e.g. OAM-MOD0002, OAM-TS2322): what it means and what usually fixes it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "The ODIF code to explain." }
                },
                "required": ["code"]
            }
        }
    ])
}

fn call_tool(name: &str, arguments: &Value) -> Result<Value, String> {
    match name {
        "oam_check" => {
            let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
            let check_result = match oam_ts::daemon::check_via_daemon(&PathBuf::from(path)) {
                Ok(diagnostics) => Ok(diagnostics),
                Err(_) => oam_ts::check(&PathBuf::from(path)),
            };
            match check_result {
                Ok(diagnostics) => {
                    let lines: Vec<Value> = diagnostics
                        .iter()
                        .map(|d| serde_json::to_value(d).expect("ODIF serializes"))
                        .collect();
                    Ok(tool_text(
                        &json!({ "diagnostics": lines, "errorCount": diagnostics.iter().filter(|d| matches!(d.severity, oam_diagnostics::Severity::Error)).count() })
                            .to_string(),
                        false,
                    ))
                }
                Err(failure) => Ok(tool_text(&failure.to_jsonl(), true)),
            }
        }
        "oam_run" => {
            let file = arguments
                .get("file")
                .and_then(Value::as_str)
                .ok_or("oam_run requires 'file'")?;
            let timeout_ms = arguments
                .get("timeoutMs")
                .and_then(Value::as_f64)
                .unwrap_or(30_000.0)
                .clamp(100.0, 600_000.0) as u64;
            run_subprocess(file, timeout_ms)
        }
        "oam_project_info" => {
            let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
            let tsconfig = oam_ts::find_tsconfig(&cwd);
            // available() is a --version probe (milliseconds); the old
            // check()-on-a-fake-file probe could run a FULL project
            // typecheck, and misreported missing tsgo as available.
            let tsgo_version = oam_ts::available().ok();
            let payload = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "cwd": cwd.to_string_lossy(),
                "tsconfig": tsconfig.map(|p| p.to_string_lossy().into_owned()),
                "tsgoAvailable": tsgo_version.is_some(),
                "tsgoVersion": tsgo_version,
            });
            Ok(tool_text(&payload.to_string(), false))
        }
        "oam_explain" => {
            let code = arguments
                .get("code")
                .and_then(Value::as_str)
                .ok_or("oam_explain requires 'code'")?;
            Ok(tool_text(&explain_code(code), false))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Run `oam run <file> --json` with a kill-on-deadline budget. The `--`
/// terminator keeps '-'-prefixed file strings out of clap's flag parsing
/// (file '--help' used to return a success-shaped payload). Pipes are
/// drained on threads so a chatty child can't deadlock the wait loop.
fn run_subprocess(file: &str, timeout_ms: u64) -> Result<Value, String> {
    use std::io::Read;
    use std::time::{Duration, Instant};

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // Windows: clear inherit on OUR std handles (the agent's pipes) so they
    // can't leak through the run child into its detached type-check daemon
    // and hold the agent's pipe chain open (see oam_ts::daemon docs). stdin
    // is explicitly null because inherit would no longer work after this.
    oam_ts::daemon::unshare_std_handles();
    // Absolutize so a dash-prefixed filename ('--help') can never read as a
    // flag — `--` is taken by script-args forwarding since the argv work.
    let file = std::path::absolute(file).unwrap_or_else(|_| std::path::PathBuf::from(file));
    let mut child = std::process::Command::new(exe)
        .arg("run")
        .arg("--json")
        .arg(file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn oam run: {e}"))?;

    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait: {e}"))? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(15)),
        }
    };

    let stdout = String::from_utf8_lossy(&stdout_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_reader.join().unwrap_or_default()).into_owned();
    let mut diagnostics: Vec<Value> = Vec::new();
    let mut stderr_raw: Vec<&str> = Vec::new();
    for line in stderr.lines() {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => diagnostics.push(v),
            // Non-ODIF stderr (e.g. a panic) must not vanish: agents need
            // SOMETHING to explain a non-zero exit with zero diagnostics.
            Err(_) if !line.trim().is_empty() => stderr_raw.push(line),
            Err(_) => {}
        }
    }
    let payload = json!({
        "exitCode": status.and_then(|s| s.code()),
        "timedOut": timed_out,
        "timeoutMs": timeout_ms,
        "stdout": stdout,
        "diagnostics": diagnostics,
        "stderrRaw": stderr_raw,
    });
    Ok(tool_text(&payload.to_string(), timed_out))
}

fn tool_text(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

/// The seed of oam.sh/e/<code>: offline explanations for ODIF codes.
fn explain_code(code: &str) -> String {
    let known: &[(&str, &str)] = &[
        (
            "OAM-PARSE0001",
            "TypeScript/JavaScript parse error from the oxc parser. The source is syntactically invalid at the span; fix the syntax before anything else will work.",
        ),
        (
            "OAM-PARSE0002",
            "TypeScript transform error: the source parsed but could not be lowered to JavaScript (e.g. unsupported syntax combination).",
        ),
        (
            "OAM-PARSE0003",
            ".tsx/.jsx is not supported yet; the JSX automatic runtime lands with npm resolution (M2). Rename to .ts or remove JSX.",
        ),
        (
            "OAM-MOD0001",
            "A relative import did not resolve to any file. The message lists every candidate path tried; create the file or fix the specifier.",
        ),
        (
            "OAM-MOD0002",
            "A package could not be found in any node_modules directory above the importing file (or its resolution pointed at a missing file). Is it installed? oam resolves against existing npm/pnpm installs until the oam installer lands (M3).",
        ),
        (
            "OAM-MOD0005",
            "Retired (no longer emitted): CommonJS entries execute through CJS interop since M2. If an old toolchain surfaced this code, upgrade oam and re-run.",
        ),
        (
            "OAM-MOD0006",
            "This Node builtin is not implemented yet. Shipped: assert, async_hooks (AsyncLocalStorage), buffer, crypto (hash/hmac/random/webcrypto-digest), events, fs (+streams), fs/promises, module, os, path, process, stream (+promises/web/consumers), string_decoder, tty, url, util; the rest (http, child_process, zlib, ...) land with later compat waves.",
        ),
        (
            "OAM-MOD0007",
            "The package's exports map does not export this subpath (Node's ERR_PACKAGE_PATH_NOT_EXPORTED). Check the package's documented entry points.",
        ),
        (
            "OAM-MOD0003",
            "This module type is not executable: a .json file used as the ENTRY (import it from a script — JSON imports work, with or without `with { type: 'json' }`), an unsupported import-attribute type, or an unknown extension. .cts runs through CJS interop with TS strip; .cjs runs through CJS interop; .tsx/.jsx is not supported yet (M2: JSX automatic runtime).",
        ),
        (
            "OAM-MOD0004",
            "Invalid module specifier shape: '.', '..', or backslashes. Use './name' or '../name' with forward slashes.",
        ),
        (
            "OAM-TEST0000",
            "oam test machine summary (counts per run). severity=info means all green; severity=error means at least one failure.",
        ),
        (
            "OAM-TEST0001",
            "A test failed. The message carries 'FAIL <full test name>: <assertion message>'; the span points at the test file. Run `oam test -t '<name>'` to re-run just that test.",
        ),
        (
            "OAM-TEST0003",
            "The test run itself crashed outside any test (a beforeAll threw at module scope, the runner deadlocked, or results failed to serialize). Fix the file-level error before reading individual results.",
        ),
        (
            "OAM-RT0001",
            "Uncaught runtime exception. The message carries the JS error; the span (when present) points at the throw site.",
        ),
        (
            "OAM-RT0002",
            "The runtime could not read a file or start a subsystem (I/O error). Check the path and permissions in the message.",
        ),
        (
            "OAM-RT0003",
            "Top-level await never settled and no timers or pending IO remain: the awaited promise can never resolve (deadlock). Find the promise that is never resolved/rejected.",
        ),
        (
            "OAM-RT0004",
            "A promise rejected with no handler attached (Node's ERR_UNHANDLED_REJECTION equivalent). Attach .catch() or await it in a try/catch.",
        ),
        (
            "OAM-TS0000",
            "tsgo (the TypeScript 7 native compiler) is not installed or not on PATH. Install: npm i -g @typescript/native-preview, or set OAM_TSGO.",
        ),
        (
            "OAM-TS0003",
            "oam check found no tsconfig.json walking up from the target. Point it at a .ts file directly or add a tsconfig.json.",
        ),
        (
            "OAM-TS0004",
            "tsgo exited abnormally without producing diagnostics — its raw output is in the message. Usually a tsgo crash or bad flags, not a problem in your code.",
        ),
    ];
    if let Some((_, explanation)) = known.iter().find(|(k, _)| *k == code) {
        return format!("{code}: {explanation}");
    }
    if let Some(ts) = code.strip_prefix("OAM-TS")
        && ts.parse::<u32>().is_ok()
    {
        return format!(
            "{code}: TypeScript diagnostic TS{ts}, passed through from tsgo with oam spans. \
             Search 'TS{ts}' in the TypeScript docs; the message text is the authoritative description."
        );
    }
    format!(
        "{code}: unknown code. Known families: OAM-PARSE* (parse/transform), OAM-MOD* (module resolution), OAM-RT* (runtime), OAM-TS* (type checking), OAM-TEST*/OAM-PKG* (reserved for the test runner and installer)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string()
    }

    #[test]
    fn initialize_echoes_protocol_version_and_advertises_tools() {
        let response = handle_line(&request(
            "initialize",
            json!({"protocolVersion": "2025-06-18"}),
        ))
        .expect("response");
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(v["result"]["serverInfo"]["name"], "oam");
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_has_four_tools_with_schemas() {
        let response = handle_line(&request("tools/list", json!({}))).expect("response");
        let v: Value = serde_json::from_str(&response).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        for tool in tools {
            assert!(tool["inputSchema"]["type"] == "object");
            assert!(tool["description"].as_str().unwrap().len() > 20);
        }
    }

    #[test]
    fn notifications_get_no_response() {
        let line = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        assert!(handle_line(&line).is_none());
    }

    #[test]
    fn unknown_method_is_32601() {
        let response = handle_line(&request("bogus/method", json!({}))).expect("response");
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn malformed_json_is_32700() {
        let response = handle_line("{not json").expect("response");
        let v: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(v["error"]["code"], -32700);
    }

    #[test]
    fn explain_knows_odif_codes_and_ts_passthrough() {
        assert!(explain_code("OAM-MOD0002").contains("node_modules"));
        assert!(explain_code("OAM-TS2322").contains("TS2322"));
        assert!(explain_code("OAM-NOPE").contains("unknown code"));
    }

    #[test]
    fn explain_tool_roundtrips_through_dispatch() {
        let response = handle_line(&request(
            "tools/call",
            json!({"name": "oam_explain", "arguments": {"code": "OAM-RT0004"}}),
        ))
        .expect("response");
        let v: Value = serde_json::from_str(&response).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("ERR_UNHANDLED_REJECTION"));
        assert_eq!(v["result"]["isError"], false);
    }
}
