//! End-to-end tests against the real `oam` binary.

use std::path::PathBuf;
use std::process::Output;

fn write_temp(name: &str, content: &str) -> PathBuf {
    use std::sync::OnceLock;
    // PID alone can be recycled across runs and leave stale files behind;
    // a startup-time nanos component makes the dir unique per suite run.
    static RUN_DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = RUN_DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oam-e2e-{}-{nanos}", std::process::id()))
    });
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
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

#[test]
fn parent_dir_specifiers_dedup_to_one_module_instance() {
    // The same module reached as './b1_x' and '../b1_x' (through a subdir)
    // must instantiate once: side effect printed exactly once.
    write_temp(
        "b1_x.ts",
        "console.log('x-effect');\nexport const x: number = 1;",
    );
    write_temp(
        "b1_sub/y.ts",
        "import { x } from '../b1_x';\nexport const y: number = x + 1;",
    );
    let main = write_temp(
        "b1_main.ts",
        "import { x } from './b1_x';\nimport { y } from './b1_sub/y';\nconsole.log(x, y);",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "x-effect\n1 2");
    assert_eq!(stdout.matches("x-effect").count(), 1, "module ran twice");
}

#[test]
fn unhandled_rejection_fails_like_node() {
    let main = write_temp(
        "reject_main.ts",
        "Promise.reject(new Error('lost rejection'));\nconsole.log('body done');",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(
        !out.status.success(),
        "a detached rejection must not exit 0"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-RT0004"), "stderr: {stderr}");
    assert!(stderr.contains("lost rejection"), "stderr: {stderr}");
}

#[test]
fn late_handled_rejection_is_not_reported() {
    // queueMicrotask lands with the ECMA-429 surface; plain Promise scheduling
    // exercises the same late-handler path.
    let main = write_temp(
        "handled_main.ts",
        "const p = Promise.reject(new Error('caught later'));\nPromise.resolve().then(() => { p.catch(() => console.log('handled')); });",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn json_import_is_a_clear_diagnostic() {
    write_temp("data.json", "{\"a\": 1}");
    let main = write_temp("json_main.ts", "import './data.json';");
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-MOD0003"), "stderr: {stderr}");
    assert!(stderr.contains("import-attributes"), "stderr: {stderr}");
}

#[test]
fn cjs_is_a_clear_diagnostic() {
    let file = write_temp("legacy.cjs", "module.exports = { a: 1 };");
    let out = oam(&["run", file.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-MOD0003"), "stderr: {stderr}");
    assert!(stderr.contains("CommonJS"), "stderr: {stderr}");
}

#[test]
fn dot_and_backslash_specifiers_are_invalid_not_bare() {
    let main = write_temp("dot_main.ts", "import '.';");
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("OAM-MOD0004"),
        "'.' should be invalid-specifier, not bare"
    );
}

#[test]
fn timers_fire_in_deadline_order() {
    let main = write_temp(
        "timer_order.ts",
        "setTimeout(() => console.log('late'), 30);\nsetTimeout(() => console.log('early'), 0);\nconsole.log('sync');",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "sync\nearly\nlate"
    );
}

#[test]
fn clear_timeout_cancels() {
    let main = write_temp(
        "timer_clear.ts",
        "const id = setTimeout(() => console.log('never'), 10);\nclearTimeout(id);\nsetTimeout(() => console.log('done'), 20);",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "done");
}

#[test]
fn interval_repeats_until_cleared() {
    let main = write_temp(
        "timer_interval.ts",
        "let n: number = 0;\nconst id = setInterval(() => {\n  n++;\n  console.log('tick', n);\n  if (n === 3) { clearInterval(id); console.log('done'); }\n}, 5);",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "tick 1\ntick 2\ntick 3\ndone"
    );
}

#[test]
fn top_level_await_on_timer_settles() {
    // Before the event loop this was OAM-RT0003; now it must just work.
    let main = write_temp(
        "tla_timer.ts",
        "const v: string = await new Promise<string>(r => setTimeout(() => r('woke'), 15));\nconsole.log(v);",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "woke");
}

#[test]
fn deadlocked_top_level_await_is_rt0003() {
    let main = write_temp("tla_stuck.ts", "await new Promise(() => {});");
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("OAM-RT0003"),
        "expected deadlock diagnostic"
    );
}

#[test]
fn exception_in_timer_callback_fails_run() {
    let main = write_temp(
        "timer_throw.ts",
        "setTimeout(() => { throw new Error('timer kaboom'); }, 0);",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-RT0001"), "stderr: {stderr}");
    assert!(stderr.contains("timer kaboom"), "stderr: {stderr}");
}

#[test]
fn queue_microtask_runs_before_timers() {
    let main = write_temp(
        "micro_order.ts",
        "setTimeout(() => console.log('macro'), 0);\nqueueMicrotask(() => console.log('micro'));",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "micro\nmacro");
}

#[test]
fn set_timeout_passes_extra_args() {
    let main = write_temp(
        "timer_args.ts",
        "setTimeout((a: string, b: string) => console.log(a + b), 0, 'oa', 'm');",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "oam");
}

#[test]
fn oam_sleep_settles_through_tokio() {
    let main = write_temp("op_sleep.ts", "await oam.sleep(20);\nconsole.log('slept');");
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "slept");
}

#[test]
fn oam_read_text_file_roundtrips() {
    let data = write_temp("op_data.txt", "hello from disk");
    let main = write_temp(
        "op_read.ts",
        &format!(
            "const text: string = await oam.readTextFile({:?});\nconsole.log(text);",
            data.to_str().unwrap()
        ),
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "hello from disk"
    );
}

#[test]
fn oam_read_text_file_rejects_catchably() {
    let main = write_temp(
        "op_read_missing.ts",
        "try {\n  await oam.readTextFile('/definitely/not/here.txt');\n} catch (e) {\n  console.log('caught:', (e as Error).message.includes('could not read'));\n}",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "caught: true");
}

#[test]
fn timers_and_ops_interleave() {
    // A 0ms timer must fire while a longer op is in flight.
    let main = write_temp(
        "op_interleave.ts",
        "setTimeout(() => console.log('timer'), 0);\nawait oam.sleep(40);\nconsole.log('slept');",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "timer\nslept");
}

#[test]
fn pending_op_keeps_process_alive() {
    // No await: the entry fulfills immediately, but the op must still
    // complete before exit (Node keep-alive semantics).
    let main = write_temp(
        "op_keepalive.ts",
        "oam.sleep(20).then(() => console.log('kept alive'));",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "kept alive");
}

#[test]
fn dotted_basename_resolves_appended_extension() {
    write_temp("my.module.ts", "export const m: string = 'dotted';");
    let main = write_temp(
        "dotted_main.ts",
        "import { m } from './my.module';\nconsole.log(m);",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "dotted");
}
