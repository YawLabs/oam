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
fn imports_typescript_across_files_extensionless() {
    write_temp(
        "greet_lib.ts",
        "export function greet(name: string): string { return `hi ${name}`; }",
    );
    let main = write_temp(
        "greet_main.ts",
        "import { greet } from './greet_lib';\nconsole.log(greet('oam'));",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi oam");
}

#[test]
fn js_extension_specifier_finds_ts_source() {
    write_temp("answer.ts", "export const answer: number = 42;");
    let main = write_temp(
        "answer_main.ts",
        "import { answer } from './answer.js';\nconsole.log(answer);",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
}

#[test]
fn missing_import_is_odif_mod0001() {
    let main = write_temp("missing_main.ts", "import './nope';");
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-MOD0001"), "stderr: {stderr}");
    assert!(
        stderr.contains("\"origin\":\"resolve\""),
        "stderr: {stderr}"
    );
}

#[test]
fn bare_specifier_is_odif_mod0002_until_m2() {
    let main = write_temp("bare_main.ts", "import 'lodash';");
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-MOD0002"), "stderr: {stderr}");
}

#[test]
fn top_level_await_settles_on_microtasks() {
    let main = write_temp(
        "tla_main.ts",
        "const v: string = await Promise.resolve('settled');\nconsole.log(v);",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "settled");
}

#[test]
fn import_cycles_follow_esm_tdz_semantics() {
    // cycle_a evaluates first (entry-last post-order), so its top level must
    // not touch cycle_b's bindings — but lazy access via a function is fine.
    write_temp(
        "cycle_a.ts",
        "import { b } from './cycle_b';\nexport const a: string = 'a';\nexport function aSeesB(): string { return b; }",
    );
    let main = write_temp(
        "cycle_b.ts",
        "import { a, aSeesB } from './cycle_a';\nexport const b: string = 'b';\nconsole.log('b sees', a);\nconsole.log(aSeesB());",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "b sees a\nb");

    // And the TDZ violation direction stays a proper ReferenceError, like Node.
    write_temp("tdz_a.ts", "import { b } from './tdz_b';\nconsole.log(b);");
    let tdz = write_temp(
        "tdz_b.ts",
        "import './tdz_a';\nexport const b: string = 'b';",
    );
    let out = oam(&["run", tdz.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("before initialization"),
        "expected TDZ ReferenceError"
    );
}

#[test]
fn jsx_is_a_clear_diagnostic_not_a_crash() {
    let file = write_temp("app.tsx", "const a = <div/>;");
    let out = oam(&["run", file.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-PARSE0003"), "stderr: {stderr}");
}
