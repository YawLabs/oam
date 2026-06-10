//! End-to-end tests against the real `oam` binary.

use std::path::PathBuf;
use std::process::Output;

fn write_temp(name: &str, content: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oam-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

fn oam(args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args(args)
        .output()
        .expect("oam binary runs")
}

#[test]
fn runs_javascript() {
    let file = write_temp("hello.js", "console.log('hello', 6 * 7);");
    let out = oam(&["run", file.to_str().unwrap()]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello 42");
}

#[test]
fn runs_typescript_with_types_and_enums() {
    let file = write_temp(
        "hello.ts",
        "enum Mode { Fast = 'fast' }\nconst n: number = 6 * 7;\nconsole.log(Mode.Fast, n);",
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "fast 42");
}

#[test]
fn typescript_parse_errors_emit_odif_jsonl() {
    let file = write_temp("bad.ts", "const x: = 1;");
    let out = oam(&["run", file.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first_line = stderr.lines().next().unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(first_line).expect("stderr is JSONL");
    assert_eq!(parsed["odif"], "1");
    assert_eq!(parsed["code"], "OAM-PARSE0001");
    assert_eq!(parsed["origin"], "parse");
}

#[test]
fn runtime_exceptions_emit_odif_jsonl() {
    let file = write_temp("boom.js", "throw new Error('kaboom');");
    let out = oam(&["run", file.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let parsed: serde_json::Value =
        serde_json::from_str(stderr.lines().next().unwrap()).expect("stderr is JSONL");
    assert_eq!(parsed["code"], "OAM-RT0001");
    assert!(parsed["message"].as_str().unwrap().contains("kaboom"));
}

#[test]
fn jsx_is_a_clear_diagnostic_not_a_crash() {
    let file = write_temp("app.tsx", "const a = <div/>;");
    let out = oam(&["run", file.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-PARSE0003"), "stderr: {stderr}");
}
