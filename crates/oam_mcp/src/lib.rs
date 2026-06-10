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
/// JSON line each; notifications produce no response.
pub fn serve_stdio() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
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
            "description": "Type-check a TypeScript file or project with tsgo (TypeScript 7 native). Returns ODIF diagnostics: stable codes (OAM-TS*), file:line:col spans, severities. Empty diagnostics = clean.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "A .ts file or directory; nearest tsconfig.json upward wins. Defaults to cwd." }
                }
            }
        },
        {
            "name": "oam_run",
            "description": "Run a JavaScript/TypeScript file with the oam runtime. Returns exit code, stdout, and ODIF diagnostics (parse/resolve/runtime errors) as structured data.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Path to the .js/.mjs/.ts/.mts entry file." }
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
            match oam_ts::check(&PathBuf::from(path)) {
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
            let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
            // Subprocess isolation: a crashing or hanging-then-killed script
            // never takes the MCP server down with it.
            let output = std::process::Command::new(exe)
                .arg("run")
                .arg(file)
                .arg("--json")
                .output()
                .map_err(|e| format!("spawn oam run: {e}"))?;
            let diagnostics: Vec<Value> = String::from_utf8_lossy(&output.stderr)
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            let payload = json!({
                "exitCode": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "diagnostics": diagnostics,
            });
            Ok(tool_text(&payload.to_string(), false))
        }
        "oam_project_info" => {
            let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
            let tsconfig = oam_ts::find_tsconfig(&cwd);
            let tsgo_available = oam_ts::check(&cwd.join("__oam_probe_nonexistent__.ts"))
                .err()
                .is_none_or(|d| d.code != "OAM-TS0000");
            let payload = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "cwd": cwd.to_string_lossy(),
                "tsconfig": tsconfig.map(|p| p.to_string_lossy().into_owned()),
                "tsgoAvailable": tsgo_available,
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
            "Bare specifiers (npm packages) and node: builtins are not resolvable yet — npm resolution lands in M2. Use a relative path for local code.",
        ),
        (
            "OAM-MOD0003",
            "This module type (.cjs/.cts/.json or unknown extension) is not executable yet. CommonJS interop and JSON modules land in M2.",
        ),
        (
            "OAM-MOD0004",
            "Invalid module specifier shape: '.', '..', or backslashes. Use './name' or '../name' with forward slashes.",
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
        assert!(explain_code("OAM-MOD0002").contains("npm resolution"));
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
