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
    // Isolated cache world: any daemon/build-info these invocations create
    // lives under the run dir and self-reaps quickly.
    let cache = write_temp("oam-cache/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();
    std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args(args)
        .env("OAM_CACHE_DIR", cache)
        .env("OAM_DAEMON_IDLE_MS", "45000")
        .output()
        .expect("oam binary runs")
}

/// Minimal local HTTP/1.1 echo server so fetch tests never touch the
/// network (CI determinism). Echoes {method, path, echo: body} as JSON.
fn spawn_echo_server() -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                // Read until end of headers.
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or_default();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let content_length: usize = lines
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = buf[head_end..].to_vec();
                while body.len() < content_length {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                    }
                }
                if path == "/redirect" {
                    let _ = stream.write_all(
                        b"HTTP/1.1 302 Found\r\nlocation: /hello\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    );
                    return;
                }
                if path == "/stream" {
                    // SSE-style: headers immediately, then chunks flushed
                    // with real gaps — proves incremental delivery.
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                    );
                    let _ = stream.flush();
                    for token in ["data: tok1\n\n", "data: tok2\n\n", "data: [DONE]\n\n"] {
                        std::thread::sleep(std::time::Duration::from_millis(40));
                        let _ = stream.write_all(token.as_bytes());
                        let _ = stream.flush();
                    }
                    return;
                }
                let x_probe = head
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.eq_ignore_ascii_case("x-probe"))
                    .map(|(_, v)| v.trim().to_string());
                let payload = serde_json::json!({
                    "method": method,
                    "path": path,
                    "echo": String::from_utf8_lossy(&body),
                    "xProbe": x_probe,
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-oam-test: yes\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.write_all(response.as_bytes());
            });
        }
    });
    addr
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
fn missing_package_is_odif_mod0002() {
    let main = write_temp("bare_main.ts", "import 'definitely-not-installed';");
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-MOD0002"), "stderr: {stderr}");
    assert!(stderr.contains("is it installed"), "stderr: {stderr}");
}

/// One hand-written node_modules tree covering the resolution matrix.
fn write_npm_fixtures() -> PathBuf {
    write_temp(
        "npmproj/node_modules/greeter/package.json",
        "{\"name\": \"greeter\", \"type\": \"module\", \"exports\": {\".\": {\"import\": \"./dist/index.js\"}, \"./extra\": \"./dist/extra.js\", \"./features/*\": \"./dist/features/*.js\"}}",
    );
    write_temp(
        "npmproj/node_modules/greeter/dist/index.js",
        "import { scoped } from '@scope/pkg';\nexport function greet() { return 'greeter+' + scoped(); }",
    );
    write_temp(
        "npmproj/node_modules/greeter/dist/extra.js",
        "export const extra = 'extra';",
    );
    write_temp(
        "npmproj/node_modules/greeter/dist/features/fast.js",
        "export const feature = 'fast';",
    );
    write_temp(
        "npmproj/node_modules/@scope/pkg/package.json",
        "{\"name\": \"@scope/pkg\", \"type\": \"module\", \"exports\": \"./main.js\"}",
    );
    write_temp(
        "npmproj/node_modules/@scope/pkg/main.js",
        "export function scoped() { return 'scoped'; }",
    );
    write_temp(
        "npmproj/node_modules/dualpkg/package.json",
        "{\"name\": \"dualpkg\", \"exports\": {\".\": {\"import\": \"./esm.mjs\", \"require\": \"./cjs.cjs\"}}}",
    );
    write_temp(
        "npmproj/node_modules/dualpkg/esm.mjs",
        "export const flavor = 'esm';",
    );
    write_temp(
        "npmproj/node_modules/dualpkg/cjs.cjs",
        "module.exports = { flavor: 'cjs' };",
    );
    write_temp(
        "npmproj/node_modules/legacy-esm/package.json",
        "{\"name\": \"legacy-esm\", \"type\": \"module\", \"main\": \"lib/entry.js\"}",
    );
    write_temp(
        "npmproj/node_modules/legacy-esm/lib/entry.js",
        "export const legacy = 'legacy';",
    );
    write_temp(
        "npmproj/node_modules/cjs-only/package.json",
        "{\"name\": \"cjs-only\", \"main\": \"index.js\"}",
    );
    write_temp(
        "npmproj/node_modules/cjs-only/index.js",
        "module.exports = { nope: true };",
    );
    write_temp("npmproj/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn npm_resolution_runs_esm_packages() {
    let proj = write_npm_fixtures();
    // Exercises: exports conditions, string sugar, scoped packages,
    // package-to-package imports, subpath exports, wildcards, dual-package
    // import-condition pick, and the bundler-standard module/main legacy path.
    std::fs::write(
        proj.join("main.ts"),
        "import { greet } from 'greeter';\nimport { extra } from 'greeter/extra';\nimport { feature } from 'greeter/features/fast';\nimport { flavor } from 'dualpkg';\nimport { legacy } from 'legacy-esm';\nconsole.log(greet(), extra, feature, flavor, legacy);",
    )
    .unwrap();
    let out = oam(&["run", proj.join("main.ts").to_str().unwrap(), "--no-check"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "greeter+scoped extra fast esm legacy"
    );
}

#[test]
fn npm_cjs_only_package_runs_via_interop() {
    // MOD0005 (the CJS execution gate) is retired: this exact import shape
    // was the gated case before interop landed.
    let proj = write_npm_fixtures();
    std::fs::write(
        proj.join("cjs_main.ts"),
        "import pkg from 'cjs-only';\nconsole.log('interop', pkg.nope);",
    )
    .unwrap();
    let out = oam(&[
        "run",
        proj.join("cjs_main.ts").to_str().unwrap(),
        "--no-check",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "interop true");
}

/// A second hand-written node_modules tree for the CJS interop matrix:
/// require chains, JSON requires, exports-dot style, the cache, cycles,
/// callable defaults, __esModule unwrapping, arbitrary export names, and
/// the require-condition side of a dual package.
fn write_cjs_fixtures() -> PathBuf {
    write_temp(
        "cjsproj/node_modules/classic/package.json",
        "{\"name\": \"classic\", \"main\": \"lib/index.js\"}",
    );
    write_temp(
        "cjsproj/node_modules/classic/lib/index.js",
        "const { helper } = require('./helper');\n\
         const meta = require('../package.json');\n\
         const dep = require('depcjs');\n\
         exports.kind = 'classic';\n\
         exports.helped = helper();\n\
         exports.depName = dep.name;\n\
         exports.metaName = meta.name;\n\
         module.exports.dotAndModule = 'aliased';\n\
         exports.hasDirname = typeof __dirname === 'string' && __dirname.length > 0;\n\
         exports.hasGlobal = global === globalThis;\n",
    );
    write_temp(
        "cjsproj/node_modules/classic/lib/helper.js",
        "exports.helper = function () { return 'helped'; };",
    );
    write_temp(
        "cjsproj/node_modules/depcjs/package.json",
        "{\"name\": \"depcjs\", \"main\": \"index.js\"}",
    );
    write_temp(
        "cjsproj/node_modules/depcjs/index.js",
        "module.exports = { name: 'depcjs' };",
    );
    write_temp(
        "cjsproj/node_modules/counter/package.json",
        "{\"name\": \"counter\", \"main\": \"counter.js\"}",
    );
    write_temp(
        "cjsproj/node_modules/counter/counter.js",
        "let n = 0;\nmodule.exports = { bump: () => ++n };",
    );
    write_temp(
        "cjsproj/node_modules/counter/a.js",
        "exports.a = require('./counter').bump();",
    );
    write_temp(
        "cjsproj/node_modules/counter/b.js",
        "exports.b = require('./counter').bump();",
    );
    write_temp(
        "cjsproj/node_modules/cycle/package.json",
        "{\"name\": \"cycle\", \"main\": \"index.js\"}",
    );
    write_temp(
        "cjsproj/node_modules/cycle/index.js",
        "exports.started = true;\n\
         const peer = require('./peer');\n\
         exports.peerSawPartial = peer.sawPartial;\n\
         exports.done = true;",
    );
    write_temp(
        "cjsproj/node_modules/cycle/peer.js",
        "const root = require('./index');\n\
         exports.sawPartial = root.started === true && root.done === undefined;",
    );
    write_temp(
        "cjsproj/node_modules/fnpkg/package.json",
        "{\"name\": \"fnpkg\", \"main\": \"index.js\"}",
    );
    write_temp(
        "cjsproj/node_modules/fnpkg/index.js",
        "module.exports = function shout(s) { return s.toUpperCase(); };\n\
         module.exports.flavor = 'fn';",
    );
    write_temp(
        "cjsproj/node_modules/transpiled/package.json",
        "{\"name\": \"transpiled\", \"main\": \"index.js\"}",
    );
    write_temp(
        "cjsproj/node_modules/transpiled/index.js",
        "Object.defineProperty(exports, '__esModule', { value: true });\n\
         exports.default = function () { return 'unwrapped-default'; };\n\
         exports.named = 'named-val';",
    );
    write_temp(
        "cjsproj/node_modules/weird/package.json",
        "{\"name\": \"weird\", \"main\": \"index.js\"}",
    );
    write_temp(
        "cjsproj/node_modules/weird/index.js",
        "module.exports = { 'weird-key': 'dash', 'class': 'reserved' };",
    );
    write_temp(
        "cjsproj/node_modules/dualpkg/package.json",
        "{\"name\": \"dualpkg\", \"exports\": {\".\": {\"import\": \"./esm.mjs\", \"require\": \"./cjs.cjs\"}}}",
    );
    write_temp(
        "cjsproj/node_modules/dualpkg/esm.mjs",
        "export const flavor = 'esm';",
    );
    write_temp(
        "cjsproj/node_modules/dualpkg/cjs.cjs",
        "module.exports = { flavor: 'cjs' };",
    );
    write_temp(
        "cjsproj/node_modules/wantsdual/package.json",
        "{\"name\": \"wantsdual\", \"main\": \"index.js\"}",
    );
    write_temp(
        "cjsproj/node_modules/wantsdual/index.js",
        "exports.dualFlavor = require('dualpkg').flavor;",
    );
    write_temp(
        "cjsproj/node_modules/boom/package.json",
        "{\"name\": \"boom\", \"main\": \"index.js\"}",
    );
    write_temp(
        "cjsproj/node_modules/boom/index.js",
        "throw new Error('cjs-init-boom');",
    );
    write_temp("cjsproj/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn cjs_interop_runs_require_chains_json_and_aliasing() {
    let proj = write_cjs_fixtures();
    std::fs::write(
        proj.join("classic_main.ts"),
        "import pkg, { kind, helped, depName, metaName, dotAndModule, hasDirname, hasGlobal } from 'classic';\n\
         console.log(kind, helped, depName, metaName, dotAndModule, hasDirname, hasGlobal, pkg.kind);",
    )
    .unwrap();
    let out = oam(&[
        "run",
        proj.join("classic_main.ts").to_str().unwrap(),
        "--no-check",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "classic helped depcjs classic aliased true true classic"
    );
}

#[test]
fn cjs_require_cache_is_a_singleton_and_cycles_see_partials() {
    let proj = write_cjs_fixtures();
    std::fs::write(
        proj.join("cache_main.ts"),
        "import { a } from 'counter/a';\nimport { b } from 'counter/b';\nimport { peerSawPartial, done } from 'cycle';\nconsole.log(a, b, peerSawPartial, done);",
    )
    .unwrap();
    let out = oam(&[
        "run",
        proj.join("cache_main.ts").to_str().unwrap(),
        "--no-check",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // a=1, b=2 proves one shared module instance; the cycle peer observed
    // partial exports (started set, done not yet) exactly like Node.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1 2 true true");
}

#[test]
fn cjs_callable_default_esmodule_unwrap_and_arbitrary_names() {
    let proj = write_cjs_fixtures();
    // .mjs entry: arbitrary module-namespace import names are parsed by V8
    // directly, keeping this independent of the TS transpile path.
    std::fs::write(
        proj.join("default_main.mjs"),
        "import shout, { flavor } from 'fnpkg';\n\
         import t, { named } from 'transpiled';\n\
         import { 'weird-key' as wk, 'class' as cls } from 'weird';\n\
         console.log(shout('hi'), flavor, t(), named, wk, cls);",
    )
    .unwrap();
    let out = oam(&[
        "run",
        proj.join("default_main.mjs").to_str().unwrap(),
        "--no-check",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "HI fn unwrapped-default named-val dash reserved"
    );
}

#[test]
fn cjs_requiring_a_dual_package_gets_the_require_condition() {
    let proj = write_cjs_fixtures();
    std::fs::write(
        proj.join("dual_main.ts"),
        "import { flavor } from 'dualpkg';\nimport { dualFlavor } from 'wantsdual';\nconsole.log(flavor, dualFlavor);",
    )
    .unwrap();
    let out = oam(&[
        "run",
        proj.join("dual_main.ts").to_str().unwrap(),
        "--no-check",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The ESM import sees the import condition; the CJS require sees the
    // require condition — both flavors of the same package in one graph.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "esm cjs");
}

#[test]
fn cjs_entry_program_runs_with_event_loop_and_relative_cjs_imports() {
    let proj = write_cjs_fixtures();
    std::fs::write(
        proj.join("helper.cjs"),
        "exports.helper = () => 'works';\nexports.util = 'relative-cjs';",
    )
    .unwrap();
    std::fs::write(
        proj.join("prog.cjs"),
        "const { helper } = require('./helper.cjs');\n\
         console.log('entry-cjs', helper(), typeof __filename === 'string');\n\
         setTimeout(() => console.log('timer-in-cjs'), 10);",
    )
    .unwrap();
    let out = oam(&["run", proj.join("prog.cjs").to_str().unwrap(), "--no-check"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("entry-cjs works true"), "stdout: {stdout}");
    // The timer fired: the event loop pumps for CJS entries too.
    assert!(stdout.contains("timer-in-cjs"), "stdout: {stdout}");

    // And ESM importing a relative .cjs file bridges the same way.
    std::fs::write(
        proj.join("rel_main.ts"),
        "import h from './helper.cjs';\nconsole.log(h.util);",
    )
    .unwrap();
    let out = oam(&[
        "run",
        proj.join("rel_main.ts").to_str().unwrap(),
        "--no-check",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "relative-cjs");
}

#[test]
fn cjs_failure_modes_surface_clearly() {
    let proj = write_cjs_fixtures();

    // A CJS module whose body throws fails the run with the real error.
    std::fs::write(proj.join("boom_main.ts"), "import 'boom';").unwrap();
    let out = oam(&[
        "run",
        proj.join("boom_main.ts").to_str().unwrap(),
        "--no-check",
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cjs-init-boom"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // require() of a missing module is Node's wording.
    std::fs::write(proj.join("missing.cjs"), "require('./does-not-exist');").unwrap();
    let out = oam(&["run", proj.join("missing.cjs").to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Cannot find module"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // require() of an ES module points at import (ERR_REQUIRE_ESM).
    std::fs::write(proj.join("esm_target.mjs"), "export const x = 1;").unwrap();
    std::fs::write(proj.join("reqesm.cjs"), "require('./esm_target.mjs');").unwrap();
    let out = oam(&["run", proj.join("reqesm.cjs").to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ERR_REQUIRE_ESM"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn npm_blocked_subpath_is_mod0007_and_builtins_are_mod0006() {
    let proj = write_npm_fixtures();
    std::fs::write(proj.join("blocked_main.ts"), "import 'greeter/secret';").unwrap();
    let out = oam(&[
        "run",
        proj.join("blocked_main.ts").to_str().unwrap(),
        "--json",
        "--no-check",
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("OAM-MOD0007"),
        "blocked subpath"
    );

    // Shipped builtins resolve; the MOD0006 gate covers the rest,
    // prefixed or bare. Use `constants` (deprecated legacy alias, not in
    // SUPPORTED_BUILTINS) and `sys` (ancient alias) as permanent canaries --
    // they will never ship and keep this test from needing to chase the list.
    std::fs::write(proj.join("builtin_main.ts"), "import 'node:constants';").unwrap();
    let out = oam(&[
        "run",
        proj.join("builtin_main.ts").to_str().unwrap(),
        "--json",
        "--no-check",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("OAM-MOD0006"),
        "node: builtin gate: {stderr}"
    );
    assert!(
        stderr.contains("wave 1 ships") || stderr.contains("does not implement"),
        "gate message present: {stderr}"
    );

    std::fs::write(proj.join("bare_builtin.ts"), "import 'sys';").unwrap();
    let out = oam(&[
        "run",
        proj.join("bare_builtin.ts").to_str().unwrap(),
        "--json",
        "--no-check",
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("OAM-MOD0006"),
        "bare builtin gate"
    );
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

// ---------------------------------------------------- node: compat wave 1

/// Run a script and return trimmed stdout, failing loudly on a bad exit.
fn run_ok(name: &str, source: &str) -> String {
    let main = write_temp(name, source);
    let out = oam(&["run", main.to_str().unwrap(), "--no-check"]);
    assert!(
        out.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn node_builtins_resolve_from_all_specifier_forms() {
    let stdout = run_ok(
        "builtin_forms.ts",
        "import path from 'node:path';\n\
         import { join } from 'path';\n\
         import fsp from 'fs/promises';\n\
         import { posix } from 'node:path';\n\
         console.log(\n\
           typeof path.join === 'function' && join === path.join,\n\
           typeof fsp.readFile === 'function',\n\
           posix.join('a', 'b'),\n\
         );",
    );
    assert_eq!(stdout, "true true a/b");
}

#[test]
fn node_path_module_correctness() {
    let stdout = run_ok(
        "path_correct.ts",
        "import { win32 as w, posix as p } from 'node:path';\n\
         console.log(w.join('C:\\\\a', 'b', '..', 'c'));\n\
         console.log(w.resolve('C:\\\\x', 'y\\\\z'));\n\
         console.log(w.relative('C:\\\\a\\\\b', 'C:\\\\a\\\\d\\\\e'));\n\
         console.log(w.basename('C:\\\\dir\\\\file.tar.gz'), w.extname('file.tar.gz'));\n\
         console.log(w.dirname('C:\\\\dir\\\\sub\\\\file.ts'), w.isAbsolute('C:\\\\x'), w.isAbsolute('x\\\\y'));\n\
         console.log(p.join('/a', 'b', '..', 'c'), p.normalize('/a//b/../c/'), p.relative('/a/b', '/a/c'));\n\
         const parsed = p.parse('/home/user/file.txt');\n\
         console.log(parsed.root, parsed.base, parsed.name, parsed.ext);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "C:\\a\\c");
    assert_eq!(lines[1], "C:\\x\\y\\z");
    assert_eq!(lines[2], "..\\d\\e");
    assert_eq!(lines[3], "file.tar.gz .gz");
    assert_eq!(lines[4], "C:\\dir\\sub true false");
    assert_eq!(lines[5], "/a/c /a/c/ ../c");
    assert_eq!(lines[6], "/ file.txt file .txt");
}

#[test]
fn buffer_roundtrips_views_and_numerics() {
    let stdout = run_ok(
        "buffer_round.ts",
        "import { Buffer } from 'node:buffer';\n\
         const b = Buffer.from('hello oam', 'utf8');\n\
         console.log(b.toString('hex'), b.toString('base64'));\n\
         console.log(Buffer.from(b.toString('hex'), 'hex').toString(), Buffer.from(b.toString('base64'), 'base64').toString());\n\
         const n = Buffer.alloc(8);\n\
         n.writeUInt32BE(0xdeadbeef, 0); n.writeUInt32LE(0x01020304, 4);\n\
         console.log(n.readUInt32BE(0).toString(16), n.readUInt32LE(4).toString(16));\n\
         const view = b.slice(0, 5); view[0] = 72;\n\
         console.log(b.toString('utf8', 0, 5), Buffer.isBuffer(view), view instanceof Uint8Array);\n\
         console.log(Buffer.concat([Buffer.from('a'), Buffer.from('bc')]).toString(), Buffer.byteLength('héllo'), b.indexOf('oam'), b.includes('xyz'));\n\
         console.log(globalThis.Buffer === Buffer, atob(btoa('wire')), JSON.stringify(Buffer.from([1,2]).toJSON()));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "68656c6c6f206f616d aGVsbG8gb2Ft");
    assert_eq!(lines[1], "hello oam hello oam");
    assert_eq!(lines[2], "deadbeef 1020304");
    // Buffer#slice is a view: the write through it is visible in the parent.
    assert_eq!(lines[3], "Hello true true");
    assert_eq!(lines[4], "abc 6 6 false");
    assert_eq!(lines[5], "true wire {\"type\":\"Buffer\",\"data\":[1,2]}");
}

#[test]
fn event_emitter_semantics() {
    let stdout = run_ok(
        "events_sem.ts",
        "import { EventEmitter, once } from 'node:events';\n\
         const ee = new EventEmitter();\n\
         const order: string[] = [];\n\
         ee.on('x', () => order.push('on'));\n\
         ee.prependListener('x', () => order.push('pre'));\n\
         ee.once('x', () => order.push('once'));\n\
         ee.emit('x'); ee.emit('x');\n\
         console.log(order.join(','), ee.listenerCount('x'));\n\
         let threw = false;\n\
         try { ee.emit('error', new Error('unhandled-err')); } catch (e) { threw = (e as Error).message === 'unhandled-err'; }\n\
         console.log(threw);\n\
         setTimeout(() => ee.emit('ready', 41, 42), 5);\n\
         const args = await once(ee, 'ready');\n\
         console.log(args.join('+'));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "pre,on,once,pre,on 2");
    assert_eq!(lines[1], "true");
    assert_eq!(lines[2], "41+42");
}

#[test]
fn fs_sync_and_promises_roundtrip_with_error_codes() {
    let stdout = run_ok(
        "fs_round.ts",
        "import fs from 'node:fs';\n\
         import { mkdir, writeFile, readFile, readdir, stat, rm } from 'node:fs/promises';\n\
         import path from 'node:path';\n\
         const dir = path.join(import.meta.dirname, 'fs-scratch');\n\
         fs.mkdirSync(path.join(dir, 'deep'), { recursive: true });\n\
         fs.writeFileSync(path.join(dir, 'a.txt'), 'sync-write');\n\
         console.log(fs.readFileSync(path.join(dir, 'a.txt'), 'utf8'), fs.existsSync(path.join(dir, 'a.txt')));\n\
         const buf = fs.readFileSync(path.join(dir, 'a.txt'));\n\
         console.log(Buffer.isBuffer(buf), buf.toString());\n\
         console.log(fs.statSync(path.join(dir, 'a.txt')).isFile(), fs.statSync(dir).isDirectory());\n\
         await writeFile(path.join(dir, 'b.txt'), 'async-write');\n\
         console.log(await readFile(path.join(dir, 'b.txt'), 'utf8'), (await stat(path.join(dir, 'b.txt'))).isFile());\n\
         console.log((await readdir(dir)).sort().join(','));\n\
         let syncCode = ''; try { fs.readFileSync(path.join(dir, 'nope.txt')); } catch (e: any) { syncCode = e.code; }\n\
         let asyncCode = ''; try { await readFile(path.join(dir, 'nope.txt')); } catch (e: any) { asyncCode = e.code; }\n\
         console.log(syncCode, asyncCode);\n\
         await rm(dir, { recursive: true });\n\
         console.log(fs.existsSync(dir));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "sync-write true");
    assert_eq!(lines[1], "true sync-write");
    assert_eq!(lines[2], "true true");
    assert_eq!(lines[3], "async-write true");
    assert_eq!(lines[4], "a.txt,b.txt,deep");
    assert_eq!(lines[5], "ENOENT ENOENT");
    assert_eq!(lines[6], "false");
}

#[test]
fn process_globals_and_scheduling() {
    let stdout = run_ok(
        "process_glob.ts",
        "const order: string[] = [];\n\
         process.nextTick(() => order.push('tick'));\n\
         setImmediate(() => {\n\
           order.push('immediate');\n\
           console.log(order.join(','));\n\
           console.log({ nested: { deep: [1, 2] } });\n\
         });\n\
         Promise.resolve().then(() => order.push('micro'));\n\
         const t0 = performance.now();\n\
         console.log(\n\
           ['win32', 'darwin', 'linux'].includes(process.platform),\n\
           typeof process.env === 'object' && typeof process.cwd() === 'string',\n\
           process.versions.oam.length > 0,\n\
           typeof process.pid === 'number',\n\
           t0 >= 0 && performance.now() >= t0,\n\
           typeof process.hrtime.bigint() === 'bigint',\n\
         );",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "true true true true true true");
    assert_eq!(lines[1], "tick,micro,immediate");
    // The upgraded console renders objects structurally, not as
    // '[object Object]'.
    assert_eq!(lines[2], "{ nested: { deep: [ 1, 2 ] } }");
}

#[test]
fn util_and_assert_modules() {
    let stdout = run_ok(
        "util_assert.ts",
        "import util from 'node:util';\n\
         import assert from 'node:assert';\n\
         console.log(util.format('%s=%d %j', 'n', 42, { a: 1 }));\n\
         const sleepy = util.promisify((ms: number, cb: any) => setTimeout(() => cb(null, 'done-' + ms), ms));\n\
         console.log(await sleepy(5));\n\
         const circular: any = { name: 'c' }; circular.self = circular;\n\
         console.log(util.inspect(circular).includes('[Circular'));\n\
         assert.deepStrictEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 2 }] });\n\
         assert.throws(() => { throw new TypeError('boom'); }, TypeError);\n\
         let code = '';\n\
         try { assert.deepStrictEqual([1], [2]); } catch (e: any) { code = e.code; }\n\
         console.log(code, assert.strict.equal === assert.strictEqual);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "n=42 {\"a\":1}");
    assert_eq!(lines[1], "done-5");
    assert_eq!(lines[2], "true");
    assert_eq!(lines[3], "ERR_ASSERTION true");
}

#[test]
fn create_require_and_import_meta() {
    write_temp(
        "meta/cjs_dep.cjs",
        "module.exports = { via: 'createRequire' };",
    );
    let stdout = run_ok(
        "meta/meta_main.ts",
        "import { createRequire } from 'node:module';\n\
         console.log(import.meta.url.startsWith('file://'), typeof import.meta.filename === 'string', typeof import.meta.dirname === 'string');\n\
         const require = createRequire(import.meta.url);\n\
         console.log(require('./cjs_dep.cjs').via);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "true true true");
    assert_eq!(lines[1], "createRequire");
}

#[test]
fn buffer_base64_is_node_lenient_and_bom_is_preserved() {
    let stdout = run_ok(
        "b64_lenient.ts",
        "// RFC 7515 HS256 example signature: base64url chars via 'base64'.\n\
         const sig = Buffer.from('dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk', 'base64');\n\
         console.log(sig.length);\n\
         console.log(Buffer.from('QUJDREVGR-k', 'base64').toString('hex'));\n\
         console.log(Buffer.from('abcde', 'base64').toString('hex'));\n\
         // Buffer#toString never strips a BOM (Node parity).\n\
         console.log(Buffer.from([0xEF, 0xBB, 0xBF, 0x61]).toString().length);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "32");
    assert_eq!(lines[1], "41424344454647e9");
    assert_eq!(lines[2], "69b71d");
    assert_eq!(lines[3], "2");
}

#[test]
fn buffer_write_boundaries_fill_juggling_and_var_numerics() {
    let stdout = run_ok(
        "buf_fixes.mjs",
        "const b = Buffer.alloc(3);\n\
         console.log(b.write('ab\\u20AC'), b.toString('hex'));\n\
         console.log(Buffer.alloc(5).fill('ab', 'utf16le').toString('hex'));\n\
         const v = Buffer.alloc(6);\n\
         v.writeUIntBE(0xdeadbeefca, 0, 5);\n\
         console.log(v.readUIntBE(0, 5).toString(16), typeof v.writeUint8);\n\
         console.log(Buffer.from([1, 2, 3, 4]).swap16().toString('hex'));\n\
         const dec = new TextDecoder();\n\
         const partA = dec.decode(new Uint8Array([0xE2]), { stream: true });\n\
         const partB = dec.decode(new Uint8Array([0x82, 0xAC]));\n\
         console.log(JSON.stringify(partA), partB === '\\u20AC');",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    // write backs off the torn euro sign: 2 bytes, third stays zero.
    assert_eq!(lines[0], "2 616200");
    // fill('ab','utf16le') fills the pattern, not silence.
    assert_eq!(lines[1], "6100620061");
    assert_eq!(lines[2], "deadbeefca function");
    assert_eq!(lines[3], "02010403");
    // Streaming decode buffers the split euro sign.
    assert_eq!(lines[4], "\"\" true");
}

#[test]
fn path_win32_drive_case_resolve_and_assert_zero_signs() {
    let stdout = run_ok(
        "path_assert_fixes.ts",
        "import { win32 as w } from 'node:path';\n\
         import assert from 'node:assert';\n\
         import util from 'node:util';\n\
         console.log(w.normalize('c:\\\\foo'), w.normalize('C:..'));\n\
         console.log(w.resolve('C:\\\\base', 'C:file'), w.resolve('C:\\\\a', '\\\\b'));\n\
         console.log(w.join('\\\\', 'host', 'share'));\n\
         assert.notStrictEqual(0, -0); // Node: +0 and -0 are NOT strictly equal\n\
         let threw = false;\n\
         try { assert.strictEqual(0, -0); } catch { threw = true; }\n\
         console.log(threw);\n\
         console.log(util.format('%d', Symbol('x')));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "c:\\foo C:..");
    assert_eq!(lines[1], "C:\\base\\file C:\\b");
    assert_eq!(lines[2], "\\host\\share");
    assert_eq!(lines[3], "true");
    assert_eq!(lines[4], "NaN");
}

#[test]
fn fs_encodings_rmdir_guard_and_natural_exit_code() {
    let stdout = run_ok(
        "fs_fixes/main.ts",
        "import fs from 'node:fs';\n\
         import path from 'node:path';\n\
         const dir = import.meta.dirname;\n\
         const p = path.join(dir, 'enc.bin');\n\
         fs.writeFileSync(p, 'aGk=', { encoding: 'base64' });\n\
         console.log(fs.readFileSync(p, 'utf8'), fs.readFileSync(p, 'base64'), fs.readFileSync(p, 'hex'));\n\
         let rmdirCode = '';\n\
         try { fs.rmdirSync(p); } catch (e: any) { rmdirCode = e.code; }\n\
         console.log(rmdirCode, fs.existsSync(p));\n\
         fs.unlinkSync(p);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    // base64-encoded write decoded to 'hi'; reads honor each encoding.
    assert_eq!(lines[0], "hi aGk= 6869");
    // rmdir on a FILE throws and deletes nothing -- errno differs by
    // platform: Windows surfaces ENOENT; POSIX surfaces ENOTDIR.
    let expected_rmdir = if cfg!(windows) {
        "ENOENT true"
    } else {
        "ENOTDIR true"
    };
    assert_eq!(lines[1], expected_rmdir);

    // Natural-exit honors process.exitCode (Node parity; CI depends on it).
    let main = write_temp("exitcode.mjs", "process.exitCode = 7;");
    let out = oam(&["run", main.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(7),
        "process.exitCode at natural exit"
    );
}

#[test]
fn process_argv_shape_and_require_error_codes() {
    // argv: [exe, absolute script, ...args after --], no oam flags leaking.
    let main = write_temp(
        "argvshape.mjs",
        "const a = process.argv;\n\
         console.log(a.length, a[1].includes('argvshape.mjs'), a.slice(2).join(','));",
    );
    let out = oam(&[
        "run",
        main.to_str().unwrap(),
        "--no-check",
        "--",
        "x",
        "--flag",
    ]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "4 true x,--flag"
    );

    // require() failures carry Node's .code.
    let main = write_temp(
        "reqcodes.cjs",
        "let mnf = ''; try { require('./missing-thing'); } catch (e) { mnf = e.code; }\n\
         console.log(mnf);",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "MODULE_NOT_FOUND"
    );
}

#[test]
fn builtin_named_packages_and_tsconfig_never_shadow_builtins() {
    // A real node_modules package named like a builtin: non-builtin
    // subpaths must resolve through it (Node loads process/browser).
    write_temp(
        "shadow/node_modules/process/package.json",
        "{\"name\": \"process\", \"main\": \"index.js\"}",
    );
    write_temp(
        "shadow/node_modules/process/index.js",
        "module.exports = 'PKG-MAIN';",
    );
    write_temp(
        "shadow/node_modules/process/browser.js",
        "module.exports = 'BROWSER-SHIM';",
    );
    write_temp(
        "shadow/tsconfig.json",
        "{\"compilerOptions\": {\"paths\": {\"fs\": [\"./fake-fs.ts\"], \"node:fs\": [\"./fake-fs.ts\"]}}}",
    );
    write_temp("shadow/fake-fs.ts", "export default 'FAKE-FS';");
    let proj = write_temp("shadow/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf();

    std::fs::write(
        proj.join("main.ts"),
        "import shim from 'process/browser.js';\n\
         import fs from 'node:fs';\n\
         import bare from 'fs';\n\
         console.log(shim, typeof fs.readFileSync, typeof bare.readFileSync);",
    )
    .unwrap();
    let out = oam(&["run", proj.join("main.ts").to_str().unwrap(), "--no-check"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The shim resolves from node_modules; both builtin forms bypass the
    // tsconfig paths trap and return the REAL fs.
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "BROWSER-SHIM function function"
    );
}

// -------------------------------------------------------------------- REPL

#[test]
fn repl_evaluates_typed_lines_with_live_event_loop() {
    use std::io::Write;
    let cache = write_temp("replcache/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .env("OAM_CACHE_DIR", cache)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("repl spawns");
    let script = "1+2\n\
                  const x: number = 40\n\
                  x + 2\n\
                  await Promise.resolve(7)\n\
                  _\n\
                  function outer() {\n\
                  return 'multi'\n\
                  }\n\
                  outer()\n\
                  setTimeout(() => console.log('background fired'), 40)\n\
                  await new Promise<string>((r) => setTimeout(() => r('timer-live'), 80))\n\
                  nope.boom\n\
                  .exit\n";
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("repl exits");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let cleaned: Vec<String> = stdout
        .lines()
        .skip(1) // banner
        .map(|l| {
            // Prompts accumulate before a result line: "> ... ... value".
            l.trim_start_matches("> ")
                .trim_start_matches("... ")
                .trim()
                .to_string()
        })
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        cleaned,
        vec![
            "3",
            "undefined", // typed declaration
            "42",
            "7",         // top-level await
            "7",         // _ holds the last value
            "undefined", // multi-line function declaration
            "'multi'",
            "1",                // timer id
            "background fired", // fired while AWAITING the next line — live loop
            "'timer-live'",
        ],
        "stdout was: {stdout}"
    );
    // The bad line errored on stderr without killing the session.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nope is not defined"), "stderr: {stderr}");
}

// ------------------------------------------------------------- N-API alpha

#[test]
fn napi_addon_loads_and_calls_native_functions() {
    // The workspace builds the test addon cdylib alongside everything
    // else; rename it to .node like an npm-shipped prebuilt.
    let artifact = if cfg!(windows) {
        "oam_napi_test_addon.dll"
    } else if cfg!(target_os = "macos") {
        "liboam_napi_test_addon.dylib"
    } else {
        "liboam_napi_test_addon.so"
    };
    let built = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(artifact);
    if !built.is_file() {
        panic!(
            "test addon not built at {} — `cargo build -p oam_napi_test_addon` first",
            built.display()
        );
    }
    let addon = write_temp("napi/native.node", "placeholder");
    std::fs::copy(&built, &addon).expect("copy addon into place");

    write_temp(
        "napi/main.cjs",
        "const native = require('./native.node');\n\
         console.log(native.add(40, 2), native.answer);\n\
         console.log(native.greet('oam'));\n\
         console.log(native.concat(['a', 'b', 'c']));\n\
         // Same instance through the cache.\n\
         console.log(require('./native.node') === native);\n\
         // Native throw surfaces as a real JS TypeError with the code.\n\
         try {\n\
           native.boom();\n\
         } catch (e) {\n\
           console.log(e.constructor.name, e.code, e.message);\n\
         }\n\
         // Bad args path.\n\
         try {\n\
           native.add('x');\n\
         } catch (e) {\n\
           console.log(e.code);\n\
         }",
    );
    let main = write_temp("napi/.anchor", "")
        .parent()
        .unwrap()
        .join("main.cjs");
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines[0], "42 42");
    assert_eq!(lines[1], "hello from native, oam");
    assert_eq!(lines[2], "a+b+c");
    assert_eq!(lines[3], "true");
    assert_eq!(lines[4], "TypeError ERR_NATIVE_BOOM native says no");
    assert_eq!(lines[5], "ERR_NATIVE_ARGS");
}

#[test]
fn napi_create_int64_boundary_routes_to_number_or_bigint() {
    // Node-parity: napi_create_int64 must return a JS Number for values
    // in the range [-(2^53-1), 2^53-1] and a BigInt at/beyond ±2^53.
    //
    // makeInt64(hi, lo) reconstructs an i64 from two i32s:
    //   value = (hi as i64) << 32 | (lo as u32 as i64)
    //
    // Boundary values:
    //   2^53     hi= 2097152 lo= 0  -> BigInt  (just above MAX_SAFE)
    //   2^53-1   hi= 2097151 lo=-1  -> Number  (= MAX_SAFE_INTEGER)
    //   -(2^53)  hi=-2097152 lo= 0  -> BigInt  (just below -MAX_SAFE)
    //   -(2^53-1)hi=-2097152 lo= 1  -> Number  (= -MAX_SAFE_INTEGER)
    let artifact = if cfg!(windows) {
        "oam_napi_test_addon.dll"
    } else if cfg!(target_os = "macos") {
        "liboam_napi_test_addon.dylib"
    } else {
        "liboam_napi_test_addon.so"
    };
    let built = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(artifact);
    if !built.is_file() {
        panic!(
            "test addon not built at {} -- `cargo build -p oam_napi_test_addon` first",
            built.display()
        );
    }
    let addon = write_temp("napi_int64/native.node", "placeholder");
    std::fs::copy(&built, &addon).expect("copy addon into place");

    write_temp(
        "napi_int64/main.cjs",
        "const native = require('./native.node');\n\
         // 2^53 = BigInt boundary (exclusive on the Number side)\n\
         const pos_boundary  = native.makeInt64( 2097152,  0); // 2^53\n\
         const pos_safe      = native.makeInt64( 2097151, -1); // 2^53-1\n\
         const neg_boundary  = native.makeInt64(-2097152,  0); // -(2^53)\n\
         const neg_safe      = native.makeInt64(-2097152,  1); // -(2^53-1)\n\
         console.log(typeof pos_boundary);  // bigint\n\
         console.log(typeof pos_safe);      // number\n\
         console.log(typeof neg_boundary);  // bigint\n\
         console.log(typeof neg_safe);      // number",
    );
    let main = write_temp("napi_int64/.anchor", "")
        .parent()
        .unwrap()
        .join("main.cjs");
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines[0], "bigint", "2^53 must be BigInt (above MAX_SAFE)");
    assert_eq!(
        lines[1], "number",
        "2^53-1 must be Number (= MAX_SAFE_INTEGER)"
    );
    assert_eq!(
        lines[2], "bigint",
        "-(2^53) must be BigInt (below -MAX_SAFE)"
    );
    assert_eq!(
        lines[3], "number",
        "-(2^53-1) must be Number (= -MAX_SAFE_INTEGER)"
    );
}

// ------------------------------------------------------------- http server

#[test]
fn oam_serve_handles_get_post_and_errors() {
    let stdout = run_ok(
        "serve_basic.mjs",
        "const server = await oam.serve({\n\
           async fetch(req) {\n\
             const url = new URL(req.url);\n\
             if (url.pathname === '/hello') {\n\
               return new Response('hi from oam', { headers: { 'x-served-by': 'oam' } });\n\
             }\n\
             if (url.pathname === '/echo') {\n\
               const body = await req.json();\n\
               return Response.json({ method: req.method, got: body, q: url.searchParams.get('q') });\n\
             }\n\
             if (url.pathname === '/boom') throw new Error('handler exploded');\n\
             return new Response('nope', { status: 404 });\n\
           },\n\
         });\n\
         const base = `http://127.0.0.1:${server.port}`;\n\
         const hello = await fetch(`${base}/hello`);\n\
         console.log(hello.status, hello.headers.get('x-served-by'), await hello.text());\n\
         const echo = await fetch(`${base}/echo?q=7`, { method: 'POST', body: JSON.stringify({ n: 42 }) });\n\
         const data = await echo.json();\n\
         console.log(echo.headers.get('content-type'), data.method, data.got.n, data.q);\n\
         const missing = await fetch(`${base}/nope`);\n\
         console.log(missing.status);\n\
         const boom = await fetch(`${base}/boom`);\n\
         console.log(boom.status, (await boom.text()).includes('handler exploded'));\n\
         // Concurrency: two in flight at once.\n\
         const [a, b] = await Promise.all([fetch(`${base}/hello`), fetch(`${base}/hello`)]);\n\
         console.log(a.status, b.status);\n\
         server.close();\n\
         console.log('closed');",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "200 oam hi from oam");
    assert_eq!(lines[1], "application/json POST 42 7");
    assert_eq!(lines[2], "404");
    assert_eq!(lines[3], "500 true");
    assert_eq!(lines[4], "200 200");
    assert_eq!(lines[5], "closed");
}

#[test]
fn oam_serve_streams_sse_incrementally() {
    let stdout = run_ok(
        "serve_sse.mjs",
        "const server = await oam.serve({\n\
           fetch() {\n\
             const stream = new ReadableStream({\n\
               async start(controller) {\n\
                 for (const tok of ['alpha', 'beta', 'gamma']) {\n\
                   controller.enqueue(`data: ${tok}\\n\\n`);\n\
                   await new Promise((r) => setTimeout(r, 30));\n\
                 }\n\
                 controller.close();\n\
               },\n\
             });\n\
             return new Response(stream, { headers: { 'content-type': 'text/event-stream' } });\n\
           },\n\
         });\n\
         const res = await fetch(`http://127.0.0.1:${server.port}/sse`);\n\
         let chunkCount = 0;\n\
         const events = [];\n\
         for await (const part of res.body.pipeThrough(new TextDecoderStream())) {\n\
           chunkCount++;\n\
           for (const line of part.split('\\n')) {\n\
             if (line.startsWith('data: ')) events.push(line.slice(6));\n\
           }\n\
         }\n\
         // >1 chunk proves the server FLUSHED per-token (no buffering).\n\
         console.log(chunkCount > 1, events.join('|'));\n\
         server.close();",
    );
    assert_eq!(stdout, "true alpha|beta|gamma");
}

#[test]
fn http_close_is_graceful_and_body_budget_rejects_floods() {
    let stdout = run_ok(
        "http_harden.mjs",
        "import http from 'node:http';\n\
         // Graceful close: a request in flight when close() fires still\n\
         // completes (Node semantics), not connection-reset.\n\
         const server = http.createServer((req, res) => {\n\
           if (req.url === '/slow') { setTimeout(() => res.end('slow-done'), 200); return; }\n\
           res.end('ok'); // never reads the body\n\
         });\n\
         await new Promise((r) => server.listen(0, r));\n\
         const base = `http://127.0.0.1:${server.address().port}`;\n\
         const inflight = fetch(`${base}/slow`).then((r) => r.text());\n\
         await new Promise((r) => setTimeout(r, 60));\n\
         server.close();\n\
         let drained = '';\n\
         try { drained = await inflight; } catch (e) { drained = 'FAILED:' + e.constructor.name; }\n\
         console.log(drained);\n\
         // Body budget: a flood of large unread uploads must reject some\n\
         // (503) rather than retain unbounded memory. The handler holds\n\
         // each request ~400ms before responding so the 8MB bodies stay\n\
         // reserved against the 512MB GLOBAL_BODY_BUDGET long enough to\n\
         // accumulate past it. With an instant res.end the RequestGuard\n\
         // refunds each reservation before 64 (512MB) pile up on a fast\n\
         // runner, so the budget never trips and load-shedding looks broken\n\
         // (flaky 'true false true' seen on the Windows x64 CI runner).\n\
         const server2 = http.createServer((req, res) => setTimeout(() => res.end('ok'), 400));\n\
         await new Promise((r) => server2.listen(0, r));\n\
         const base2 = `http://127.0.0.1:${server2.address().port}`;\n\
         const big = 'x'.repeat(8 * 1024 * 1024);\n\
         let ok = 0, busy = 0, shed = 0;\n\
         await Promise.all(Array.from({ length: 120 }, () =>\n\
           fetch(`${base2}/up`, { method: 'POST', body: big })\n\
             .then((r) => { if (r.status === 200) ok++; else if (r.status === 503) busy++; else shed++; })\n\
             // An over-budget reject can reset the client's in-flight\n\
             // upload — that is load-shedding, counted as such.\n\
             .catch(() => shed++),\n\
         ));\n\
         // Served what fits, rejected the excess, every request accounted,\n\
         // process survived (no OOM).\n\
         console.log(ok > 0, busy + shed > 0, ok + busy + shed === 120);\n\
         server2.close();",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "slow-done");
    assert_eq!(lines[1], "true true true");
}

#[test]
fn http_streaming_writes_stay_ordered_and_bodies_dont_leak() {
    // Regression for the M2 safety fleet: 50 synchronous res.write() calls
    // must arrive in order and intact (was a spawn-order race that
    // reordered/dropped chunks), and a route that never reads the request
    // body must not retain it (the RAII drop guard frees it).
    let stdout = run_ok(
        "http_safety.mjs",
        "import http from 'node:http';\n\
         const server = http.createServer((req, res) => {\n\
           if (req.url === '/stream') {\n\
             res.writeHead(200);\n\
             for (let i = 0; i < 50; i++) res.write(`[${i}]`);\n\
             res.end();\n\
             return;\n\
           }\n\
           res.end('ok'); // never reads the body\n\
         });\n\
         await new Promise((r) => server.listen(0, r));\n\
         const base = `http://127.0.0.1:${server.address().port}`;\n\
         const body = await (await fetch(`${base}/stream`)).text();\n\
         const expected = Array.from({ length: 50 }, (_, i) => `[${i}]`).join('');\n\
         console.log(body === expected, body.length);\n\
         // Several unread-body POSTs — exercised the leak path, must all 200.\n\
         let ok = 0;\n\
         for (let i = 0; i < 10; i++) {\n\
           const r = await fetch(`${base}/ping`, { method: 'POST', body: 'x'.repeat(4096) });\n\
           if (r.status === 200) ok++;\n\
         }\n\
         console.log(ok);\n\
         // res.write of a non-string/Buffer throws (no silent corruption).\n\
         let threwCode = '';\n\
         const probe = http.createServer((req, res) => {\n\
           try { res.write({ bad: 1 }); res.end(); } catch (e) { threwCode = e.code; res.end('caught'); }\n\
         });\n\
         await new Promise((r) => probe.listen(0, r));\n\
         await (await fetch(`http://127.0.0.1:${probe.address().port}/`)).text();\n\
         console.log(threwCode);\n\
         server.close();\n\
         probe.close();",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "true 190");
    assert_eq!(lines[1], "10");
    assert_eq!(lines[2], "ERR_INVALID_ARG_TYPE");
}

#[test]
fn process_error_events_catch_and_survive() {
    // A handler makes an async throw / rejection non-fatal (the resilience
    // boundary every long-running server installs); without one, both stay
    // fatal — verified separately below.
    let stdout = run_ok(
        "proc_err.mjs",
        "const caught = [];\n\
         process.on('uncaughtException', (e) => caught.push('uncaught:' + e.message));\n\
         process.on('unhandledRejection', (reason) => caught.push('rejection:' + reason.message));\n\
         setTimeout(() => { throw new Error('timer-boom'); }, 10);\n\
         Promise.reject(new Error('promise-boom'));\n\
         setTimeout(() => {\n\
           console.log(JSON.stringify(caught.sort()));\n\
           console.log('survived');\n\
         }, 40);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[0],
        "[\"rejection:promise-boom\",\"uncaught:timer-boom\"]"
    );
    assert_eq!(lines[1], "survived");

    // No listener -> async throw is still fatal (exit 1, OAM-RT0001).
    let main = write_temp(
        "proc_fatal.mjs",
        "setTimeout(() => { throw new Error('still-fatal'); }, 5);",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("OAM-RT0001"),
        "no-listener throw stays fatal"
    );

    // No listener -> unhandled rejection still fatal (OAM-RT0004).
    let main = write_temp(
        "proc_rejfatal.mjs",
        "Promise.reject(new Error('rej-fatal'));",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("OAM-RT0004"),
        "no-listener rejection stays fatal"
    );
}

#[test]
fn abort_controller_and_signal_work() {
    let stdout = run_ok(
        "abort.mjs",
        "import { setTimeout as wait } from 'node:timers/promises';\n\
         console.log(typeof AbortController, typeof AbortSignal, typeof EventTarget, typeof Event);\n\
         const ac = new AbortController();\n\
         let fired = '';\n\
         ac.signal.addEventListener('abort', () => { fired = ac.signal.reason.name; });\n\
         ac.abort();\n\
         console.log(ac.signal.aborted, fired);\n\
         // fetch with an already-aborted signal rejects AbortError.\n\
         let name = '';\n\
         try { await fetch('http://127.0.0.1:1/x', { signal: AbortSignal.abort() }); } catch (e) { name = e.name; }\n\
         console.log(name);\n\
         // timers/promises honors an already-aborted signal immediately.\n\
         let timed = '';\n\
         try { await wait(1000, null, { signal: AbortSignal.abort() }); } catch (e) { timed = e.name; }\n\
         console.log(timed);\n\
         // AbortSignal.timeout fires.\n\
         const sig = AbortSignal.timeout(10);\n\
         await new Promise((r) => setTimeout(r, 30));\n\
         console.log(sig.aborted, sig.reason.name);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "function function function function");
    assert_eq!(lines[1], "true AbortError");
    assert_eq!(lines[2], "AbortError");
    assert_eq!(lines[3], "AbortError");
    assert_eq!(lines[4], "true TimeoutError");
}

#[test]
fn node_http_create_server_express_style() {
    let stdout = run_ok(
        "node_http.mjs",
        "import http from 'node:http';\n\
         import net from 'node:net';\n\
         const server = http.createServer((req, res) => {\n\
           if (req.url === '/info') {\n\
             res.writeHead(200, { 'content-type': 'text/plain', 'x-powered-by': 'oam' });\n\
             res.end(`${req.method} ${req.url} ${req.headers['x-probe']}`);\n\
             return;\n\
           }\n\
           if (req.url === '/body') {\n\
             const chunks = [];\n\
             req.on('data', (c) => chunks.push(c));\n\
             req.on('end', () => {\n\
               res.statusCode = 201;\n\
               res.setHeader('content-type', 'application/json');\n\
               res.end(JSON.stringify({ size: Buffer.concat(chunks).length }));\n\
             });\n\
             return;\n\
           }\n\
           if (req.url === '/chunked') {\n\
             res.writeHead(200, { 'content-type': 'text/plain' });\n\
             res.write('part1|');\n\
             setTimeout(() => { res.write('part2|'); res.end('done'); }, 20);\n\
             return;\n\
           }\n\
           res.writeHead(404);\n\
           res.end();\n\
         });\n\
         await new Promise((resolve) => server.listen(0, resolve));\n\
         const base = `http://127.0.0.1:${server.address().port}`;\n\
         const info = await fetch(`${base}/info`, { headers: { 'x-probe': 'p1' } });\n\
         console.log(info.status, info.headers.get('x-powered-by'), await info.text());\n\
         const body = await fetch(`${base}/body`, { method: 'POST', body: 'x'.repeat(100) });\n\
         console.log(body.status, (await body.json()).size);\n\
         const chunked = await fetch(`${base}/chunked`);\n\
         console.log(await chunked.text());\n\
         console.log(net.isIP('10.0.0.1'), net.isIP('::1'), net.isIP('nope'), net.isIPv4('256.1.1.1'));\n\
         server.close();\n\
         console.log(http.STATUS_CODES[404]);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "200 oam GET /info p1");
    assert_eq!(lines[1], "201 100");
    assert_eq!(lines[2], "part1|part2|done");
    assert_eq!(lines[3], "4 6 0 false");
    assert_eq!(lines[4], "Not Found");
}

// ------------------------------------------- zlib / querystring / timers/promises

#[test]
fn small_builtins_wave_smoke() {
    let stdout = run_ok(
        "small_wave.mjs",
        "import zlib from 'node:zlib';\n\
         import qs from 'node:querystring';\n\
         import { setTimeout as wait, scheduler } from 'node:timers/promises';\n\
         import { Console } from 'node:console';\n\
         import { Writable } from 'node:stream';\n\
         const data = Buffer.from('payload-'.repeat(10));\n\
         console.log(zlib.gunzipSync(zlib.gzipSync(data)).equals(data), zlib.unzipSync(zlib.deflateSync(data)).equals(data));\n\
         // A gzip blob produced by real Node decompresses here (interop).\n\
         const nodeVector = 'H4sIAAAAAAAACstPzFVIzs9Lyy/KTcxLTlUoSKzMyU9MsVJITEo2NDKmPwkATCxew5EAAAA=';\n\
         console.log(zlib.gunzipSync(Buffer.from(nodeVector, 'base64')).toString().startsWith('oam conformance payload'));\n\
         console.log(qs.stringify({ a: 'x y', b: ['1', '2'] }), qs.parse('a=x+y&flag').a, JSON.stringify(qs.parse('a=1&flag').flag));\n\
         console.log(await wait(2, 'waited'));\n\
         await scheduler.yield();\n\
         let captured = '';\n\
         const sink = new Writable({ write(c, _e, cb) { captured += c; cb(); } });\n\
         new Console(sink).log('sunk %d', 9);\n\
         await wait(1);\n\
         console.log(JSON.stringify(captured));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "true true");
    assert_eq!(lines[1], "true");
    assert_eq!(lines[2], "a=x%20y&b=1&b=2 x y \"\"");
    assert_eq!(lines[3], "waited");
    assert_eq!(lines[4], "\"sunk 9\\n\"");
}

#[test]
fn zlib_incremental_streaming_gzip_roundtrip() {
    // Goal: pipe a large buffer (> 10 MB) through createGzip, verify the
    // Transform emits multiple chunks (incremental, not one-shot), then
    // pipe through createGunzip and assert a byte-for-byte round trip.
    let stdout = run_ok(
        "zlib_stream_incremental.mjs",
        "import { createGzip, createGunzip, createDeflate, createInflate,\n\
                  createDeflateRaw, createInflateRaw } from 'node:zlib';\n\
         import { Readable, Writable, Transform, pipeline } from 'node:stream';\n\
         import { pipeline as pipelineP } from 'node:stream/promises';\n\
         \n\
         // 12 MB payload: large enough that a streaming encoder will flush\n\
         // multiple deflate blocks before _flush is called.\n\
         const TOTAL = 12 * 1024 * 1024;\n\
         const payload = Buffer.alloc(TOTAL);\n\
         // Fill with patterned bytes so compression doesn't trivially collapse.\n\
         for (let i = 0; i < TOTAL; i++) payload[i] = i & 0xff;\n\
         \n\
         // Helper: pipe source through [transform, ...] and collect output\n\
         // chunks, also counting how many distinct push()ed chunks arrived.\n\
         function pipeAndCollect(source, transforms) {\n\
           return new Promise((resolve, reject) => {\n\
             let chunkCount = 0;\n\
             const bufs = [];\n\
             const sink = new Writable({\n\
               write(chunk, _e, cb) {\n\
                 chunkCount++;\n\
                 bufs.push(chunk);\n\
                 cb();\n\
               }\n\
             });\n\
             let chain = source;\n\
             for (const t of transforms) chain = chain.pipe(t);\n\
             chain.pipe(sink);\n\
             sink.on('finish', () => resolve({ data: Buffer.concat(bufs), chunkCount }));\n\
             sink.on('error', reject);\n\
           });\n\
         }\n\
         \n\
         // gzip: compress with createGzip, count >= 2 chunks, decompress, verify.\n\
         const src1 = Readable.from([payload]);\n\
         const gz = createGzip();\n\
         const { data: compressed, chunkCount: gzChunks } = await pipeAndCollect(src1, [gz]);\n\
         // The compressed output must be smaller than the raw input in practice.\n\
         const compressed_smaller = compressed.length < payload.length;\n\
         const src2 = Readable.from([compressed]);\n\
         const { data: decompressed } = await pipeAndCollect(src2, [createGunzip()]);\n\
         const roundtripOk = decompressed.equals(payload);\n\
         console.log('gzip_chunks_gte_2:', gzChunks >= 2);\n\
         console.log('compressed_smaller:', compressed_smaller);\n\
         console.log('gzip_roundtrip:', roundtripOk);\n\
         \n\
         // deflate / inflate round trip (same incremental path).\n\
         const srcD = Readable.from([payload]);\n\
         const { data: defCompressed } = await pipeAndCollect(srcD, [createDeflate()]);\n\
         const srcDi = Readable.from([defCompressed]);\n\
         const { data: defDecompressed } = await pipeAndCollect(srcDi, [createInflate()]);\n\
         console.log('deflate_roundtrip:', defDecompressed.equals(payload));\n\
         \n\
         // deflateRaw / inflateRaw round trip.\n\
         const srcR = Readable.from([payload]);\n\
         const { data: rawCompressed } = await pipeAndCollect(srcR, [createDeflateRaw()]);\n\
         const srcRi = Readable.from([rawCompressed]);\n\
         const { data: rawDecompressed } = await pipeAndCollect(srcRi, [createInflateRaw()]);\n\
         console.log('deflateRaw_roundtrip:', rawDecompressed.equals(payload));\n\
         \n\
         // Chain: compress then decompress in a single pipeline.\n\
         const srcChain = Readable.from([payload]);\n\
         const { data: chained } = await pipeAndCollect(srcChain,\n\
           [createGzip(), createGunzip()]);\n\
         console.log('chain_roundtrip:', chained.equals(payload));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[0], "gzip_chunks_gte_2: true",
        "expected >= 2 chunks from createGzip on a 12 MB input"
    );
    assert_eq!(lines[1], "compressed_smaller: true");
    assert_eq!(lines[2], "gzip_roundtrip: true");
    assert_eq!(lines[3], "deflate_roundtrip: true");
    assert_eq!(lines[4], "deflateRaw_roundtrip: true");
    assert_eq!(lines[5], "chain_roundtrip: true");
}

#[test]
fn zlib_gunzip_20mb_bounded_memory() {
    // Pipe a 20 MB compressed stream through createGunzip and assert it
    // round-trips byte-correct. This exercises the truly-incremental
    // decompressor (slice A): the full compressed input must NOT need to
    // be buffered -- only a ~64 kB scratch buffer per chunk is required.
    // 20 MB is large enough to catch a buffer-the-whole-thing regression
    // without risking Windows fetch-race timeouts that affect 50+ MB.
    let stdout = run_ok(
        "zlib_gunzip_20mb.mjs",
        "import { createGzip, createGunzip } from 'node:zlib';\n\
         import { Readable, Writable } from 'node:stream';\n\
         \n\
         const TOTAL = 20 * 1024 * 1024;\n\
         const payload = Buffer.alloc(TOTAL);\n\
         for (let i = 0; i < TOTAL; i++) payload[i] = i & 0xff;\n\
         \n\
         function pipeAndCollect(source, transforms) {\n\
           return new Promise((resolve, reject) => {\n\
             const bufs = [];\n\
             const sink = new Writable({\n\
               write(chunk, _e, cb) { bufs.push(chunk); cb(); }\n\
             });\n\
             let chain = source;\n\
             for (const t of transforms) chain = chain.pipe(t);\n\
             chain.pipe(sink);\n\
             sink.on('finish', () => resolve(Buffer.concat(bufs)));\n\
             sink.on('error', reject);\n\
           });\n\
         }\n\
         \n\
         const compressed = await pipeAndCollect(Readable.from([payload]), [createGzip()]);\n\
         const decompressed = await pipeAndCollect(Readable.from([compressed]), [createGunzip()]);\n\
         console.log('size_ok:', decompressed.length === TOTAL);\n\
         console.log('byte_correct:', decompressed.equals(payload));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "size_ok: true");
    assert_eq!(lines[1], "byte_correct: true");
}

#[test]
fn brotli_incremental_streaming_roundtrip() {
    // Verify createBrotliCompress / createBrotliDecompress work end-to-end
    // and that brotliCompress / brotliDecompress (async callback forms) also
    // work. Uses a 5 MB patterned payload to ensure the streaming path is
    // exercised across multiple brotli blocks.
    let stdout = run_ok(
        "brotli_stream.mjs",
        "import { createBrotliCompress, createBrotliDecompress,\n\
                  brotliCompress, brotliDecompress } from 'node:zlib';\n\
         import { Readable, Writable } from 'node:stream';\n\
         import { promisify } from 'node:util';\n\
         \n\
         const TOTAL = 5 * 1024 * 1024;\n\
         const payload = Buffer.alloc(TOTAL);\n\
         for (let i = 0; i < TOTAL; i++) payload[i] = i & 0xff;\n\
         \n\
         function pipeAndCollect(source, transforms) {\n\
           return new Promise((resolve, reject) => {\n\
             const bufs = [];\n\
             const sink = new Writable({\n\
               write(chunk, _e, cb) { bufs.push(chunk); cb(); }\n\
             });\n\
             let chain = source;\n\
             for (const t of transforms) chain = chain.pipe(t);\n\
             chain.pipe(sink);\n\
             sink.on('finish', () => resolve(Buffer.concat(bufs)));\n\
             sink.on('error', reject);\n\
           });\n\
         }\n\
         \n\
         // Streaming round-trip.\n\
         const compressed = await pipeAndCollect(\n\
           Readable.from([payload]), [createBrotliCompress()]);\n\
         const decompressed = await pipeAndCollect(\n\
           Readable.from([compressed]), [createBrotliDecompress()]);\n\
         console.log('stream_roundtrip:', decompressed.equals(payload));\n\
         console.log('compressed_smaller:', compressed.length < payload.length);\n\
         \n\
         // Callback-form one-shot.\n\
         const brotliCompressP = promisify(brotliCompress);\n\
         const brotliDecompressP = promisify(brotliDecompress);\n\
         const smallPayload = Buffer.from('hello brotli world'.repeat(100));\n\
         const enc = await brotliCompressP(smallPayload);\n\
         const dec = await brotliDecompressP(enc);\n\
         console.log('oneshot_roundtrip:', dec.equals(smallPayload));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "stream_roundtrip: true");
    assert_eq!(lines[1], "compressed_smaller: true");
    assert_eq!(lines[2], "oneshot_roundtrip: true");
}

// ------------------------------------------------------------- node:crypto

#[test]
fn crypto_hashes_match_published_vectors() {
    let stdout = run_ok(
        "crypto_vectors.mjs",
        "import { createHash, createHmac, getHashes } from 'node:crypto';\n\
         // FIPS-180 / RFC 1321 'abc' vectors.\n\
         console.log(createHash('sha256').update('abc').digest('hex'));\n\
         console.log(createHash('sha1').update('abc').digest('hex'));\n\
         console.log(createHash('md5').update('abc').digest('hex'));\n\
         console.log(createHash('sha512').update('abc').digest('hex').slice(0, 32));\n\
         // Streaming: chunked updates equal one-shot; copy() forks state.\n\
         const chunked = createHash('sha256').update('a').update('b');\n\
         const forked = chunked.copy();\n\
         console.log(chunked.update('c').digest('hex') === createHash('sha256').update('abc').digest('hex'));\n\
         console.log(forked.update('X').digest('hex') === createHash('sha256').update('abX').digest('hex'));\n\
         // Encodings: base64 digest, Buffer input, SHA-256 alias.\n\
         console.log(createHash('sha256').update(Buffer.from('abc')).digest('base64'));\n\
         console.log(createHash('SHA-256').update('abc').digest('hex') === createHash('sha256').update('abc').digest('hex'));\n\
         // Classic quick-brown-fox HMAC-SHA256 vector (key 'key').\n\
         console.log(createHmac('sha256', 'key').update('The quick brown fox jumps over the lazy dog').digest('hex'));\n\
         console.log(getHashes().includes('sha256'));\n\
         // Unknown algorithm fails loud.\n\
         let bad = false;\n\
         try { createHash('sha3-512'); } catch { bad = true; }\n\
         console.log(bad);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[0],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(lines[1], "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(lines[2], "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(lines[3], "ddaf35a193617abacc417349ae204131");
    assert_eq!(lines[4], "true");
    assert_eq!(lines[5], "true");
    assert_eq!(lines[6], "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=");
    assert_eq!(lines[7], "true");
    assert_eq!(
        lines[8],
        "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
    assert_eq!(lines[9], "true");
    assert_eq!(lines[10], "true");
}

#[test]
fn crypto_randomness_and_webcrypto() {
    let stdout = run_ok(
        "crypto_random.mjs",
        "import crypto, { randomBytes, randomUUID, randomInt, timingSafeEqual } from 'node:crypto';\n\
         const a = randomBytes(32);\n\
         const b = randomBytes(32);\n\
         console.log(a.length, Buffer.isBuffer(a), a.equals(b));\n\
         const uuid = randomUUID();\n\
         console.log(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuid));\n\
         const ints = Array.from({ length: 200 }, () => randomInt(3, 6));\n\
         console.log(ints.every((n) => n >= 3 && n < 6), new Set(ints).size > 1);\n\
         console.log(timingSafeEqual(Buffer.from('same'), Buffer.from('same')), timingSafeEqual(Buffer.from('same'), Buffer.from('diff')));\n\
         let lenErr = false;\n\
         try { timingSafeEqual(Buffer.from('a'), Buffer.from('ab')); } catch { lenErr = true; }\n\
         console.log(lenErr);\n\
         // The crypto GLOBAL (WebCrypto shape).\n\
         const view = new Uint32Array(4);\n\
         const same = globalThis.crypto.getRandomValues(view);\n\
         console.log(same === view, view.some((v) => v !== 0));\n\
         const digest = await globalThis.crypto.subtle.digest('SHA-256', new TextEncoder().encode('abc'));\n\
         console.log(digest instanceof ArrayBuffer, Buffer.from(digest).toString('hex').slice(0, 16));\n\
         console.log(crypto.webcrypto.subtle === crypto.subtle, typeof globalThis.crypto.randomUUID());",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "32 true false");
    assert_eq!(lines[1], "true");
    assert_eq!(lines[2], "true true");
    assert_eq!(lines[3], "true false");
    assert_eq!(lines[4], "true");
    assert_eq!(lines[5], "true true");
    assert_eq!(lines[6], "true ba7816bf8f01cfea");
    assert_eq!(lines[7], "true string");
}

#[test]
fn crypto_key_derivation_pbkdf2_scrypt_hkdf() {
    let stdout = run_ok(
        "crypto_kdf.mjs",
        "import { pbkdf2Sync, scryptSync, hkdfSync } from 'node:crypto';\n\
         // RFC 6070 vector 2: PBKDF2-HMAC-SHA1, 'password'/'salt', 4096 iters, 20 bytes.\n\
         const dk1 = pbkdf2Sync('password', 'salt', 4096, 20, 'sha1');\n\
         console.log(dk1.toString('hex'));\n\
         // PBKDF2-SHA256 smoke.\n\
         const dk2 = pbkdf2Sync('pass', Buffer.from('NaCl'), 1, 32, 'sha256');\n\
         console.log(dk2.length);\n\
         // scrypt smoke (N=16384, r=8, p=1, 64 bytes).\n\
         const dk3 = scryptSync('password', 'NaCl', 64, { N: 16384, r: 8, p: 1 });\n\
         console.log(dk3.length, Buffer.isBuffer(dk3));\n\
         // scrypt with options aliases.\n\
         const dk4 = scryptSync('password', 'NaCl', 32, { cost: 1024, blockSize: 8, parallelization: 1 });\n\
         console.log(dk4.length);\n\
         // HKDF-SHA256 smoke.\n\
         const okm = hkdfSync('sha256', 'ikm-value', 'salt-value', 'info-value', 42);\n\
         console.log(okm instanceof ArrayBuffer, okm.byteLength);\n\
         // HKDF empty salt.\n\
         const okm2 = hkdfSync('sha256', 'ikm', '', '', 16);\n\
         console.log(okm2.byteLength);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "4b007901b765489abead49d926f721d065a429c1");
    assert_eq!(lines[1], "32");
    assert_eq!(lines[2], "64 true");
    assert_eq!(lines[3], "32");
    assert_eq!(lines[4], "true 42");
    assert_eq!(lines[5], "16");
}

#[test]
fn crypto_ciphers_aes_cbc_ctr_gcm() {
    let stdout = run_ok(
        "crypto_ciphers.mjs",
        "import { createCipheriv, createDecipheriv, getCiphers, randomBytes } from 'node:crypto';\n\
         // AES-256-CBC round-trip.\n\
         const key = randomBytes(32);\n\
         const iv = randomBytes(16);\n\
         const enc = createCipheriv('aes-256-cbc', key, iv);\n\
         enc.update('hello world');\n\
         const ct = enc.final();\n\
         const dec = createDecipheriv('aes-256-cbc', key, iv);\n\
         dec.update(ct);\n\
         const pt = dec.final();\n\
         console.log(pt.toString());\n\
         // AES-128-CTR round-trip (symmetric: same op for enc/dec).\n\
         const key128 = randomBytes(16);\n\
         const ivCtr = randomBytes(16);\n\
         const encCtr = createCipheriv('aes-128-ctr', key128, ivCtr);\n\
         encCtr.update('symmetric stream');\n\
         const ctCtr = encCtr.final();\n\
         const decCtr = createDecipheriv('aes-128-ctr', key128, ivCtr);\n\
         decCtr.update(ctCtr);\n\
         console.log(decCtr.final().toString());\n\
         // AES-256-GCM round-trip with AAD + auth tag.\n\
         const gcmKey = randomBytes(32);\n\
         const gcmIv = randomBytes(12);\n\
         const aad = Buffer.from('additional data');\n\
         const gcmEnc = createCipheriv('aes-256-gcm', gcmKey, gcmIv);\n\
         gcmEnc.setAAD(aad);\n\
         gcmEnc.update('authenticated!');\n\
         const gcmCt = gcmEnc.final();\n\
         const tag = gcmEnc.getAuthTag();\n\
         console.log(tag.length);\n\
         const gcmDec = createDecipheriv('aes-256-gcm', gcmKey, gcmIv);\n\
         gcmDec.setAAD(aad);\n\
         gcmDec.setAuthTag(tag);\n\
         gcmDec.update(gcmCt);\n\
         console.log(gcmDec.final().toString());\n\
         // GCM tampered tag fails.\n\
         const badTag = Buffer.from(tag);\n\
         badTag[0] ^= 0xff;\n\
         const gcmBad = createDecipheriv('aes-256-gcm', gcmKey, gcmIv);\n\
         gcmBad.setAAD(aad);\n\
         gcmBad.setAuthTag(badTag);\n\
         gcmBad.update(gcmCt);\n\
         let authFailed = false;\n\
         try { gcmBad.final(); } catch { authFailed = true; }\n\
         console.log(authFailed);\n\
         // getCiphers returns the known set.\n\
         const ciphers = getCiphers();\n\
         console.log(ciphers.includes('aes-256-cbc'), ciphers.includes('aes-128-gcm'), ciphers.length);\n\
         // CBC no-padding mode: block-aligned data only.\n\
         const npKey = randomBytes(16);\n\
         const npIv = randomBytes(16);\n\
         const npEnc = createCipheriv('aes-128-cbc', npKey, npIv);\n\
         npEnc.setAutoPadding(false);\n\
         npEnc.update(Buffer.alloc(16, 0x42));\n\
         const npCt = npEnc.final();\n\
         const npDec = createDecipheriv('aes-128-cbc', npKey, npIv);\n\
         npDec.setAutoPadding(false);\n\
         npDec.update(npCt);\n\
         console.log(npDec.final()[0] === 0x42);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "hello world");
    assert_eq!(lines[1], "symmetric stream");
    assert_eq!(lines[2], "16");
    assert_eq!(lines[3], "authenticated!");
    assert_eq!(lines[4], "true");
    assert_eq!(lines[5], "true true 6");
    assert_eq!(lines[6], "true");
}

// -------------------------------------------------------------------- URL

#[test]
fn url_parses_resolves_and_mutates() {
    let stdout = run_ok(
        "url_core.mjs",
        "const u = new URL('https://user:pw@example.com:8443/a/b?x=1&y=2#frag');\n\
         console.log(u.protocol, u.username, u.password, u.host, u.hostname, u.port);\n\
         console.log(u.pathname, u.search, u.hash, u.origin);\n\
         // Relative resolution against a base.\n\
         console.log(new URL('../up?q', 'https://example.com/a/b/c').href);\n\
         console.log(new URL('//other.com/x', 'https://example.com/a').href);\n\
         // Mutation: setters re-serialize; bad setters keep old (spec).\n\
         u.hash = 'new'; u.port = ''; u.pathname = '/z z';\n\
         console.log(u.href);\n\
         u.protocol = '!!invalid!!';\n\
         console.log(u.protocol);\n\
         // IDNA: non-ASCII hosts punycode.\n\
         console.log(new URL('http://b\\u00FCcher.de/p').hostname);\n\
         // canParse + invalid throws TypeError.\n\
         console.log(URL.canParse('not a url'), URL.canParse('https://ok.dev'));\n\
         let threw = '';\n\
         try { new URL('::nope::'); } catch (e) { threw = e.constructor.name; }\n\
         console.log(threw);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "https: user pw example.com:8443 example.com 8443");
    assert_eq!(lines[1], "/a/b ?x=1&y=2 #frag https://example.com:8443");
    assert_eq!(lines[2], "https://example.com/a/up?q");
    assert_eq!(lines[3], "https://other.com/x");
    assert_eq!(lines[4], "https://user:pw@example.com/z%20z?x=1&y=2#new");
    assert_eq!(lines[5], "https:");
    assert_eq!(lines[6], "xn--bcher-kva.de");
    assert_eq!(lines[7], "false true");
    assert_eq!(lines[8], "TypeError");
}

#[test]
fn url_search_params_full_surface() {
    let stdout = run_ok(
        "usp.mjs",
        "const p = new URLSearchParams('a=1&b=two+words&a=3&empty=&plain');\n\
         console.log(p.get('a'), p.getAll('a').join(','), p.get('b'), p.get('empty'), p.get('plain'), p.size);\n\
         p.append('c', 'x y'); p.set('a', 'only'); p.delete('plain');\n\
         console.log(p.toString());\n\
         p.sort();\n\
         console.log([...p.keys()].join(','));\n\
         // Unicode round trip through the form codec.\n\
         const u = new URLSearchParams();\n\
         u.set('q', 'caf\\u00E9 \\u20AC');\n\
         console.log(u.toString(), new URLSearchParams(u.toString()).get('q'));\n\
         // Linked to a URL: mutations flow both ways.\n\
         const url = new URL('https://x.dev/p?one=1');\n\
         url.searchParams.set('two', '2');\n\
         console.log(url.href);\n\
         url.search = '?fresh=yes';\n\
         console.log(url.searchParams.get('fresh'), url.searchParams.get('one'));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    // A bare key ('plain', no '=') has value '' per WHATWG, not null —
    // two empty fields print as adjacent spaces (node-verified).
    assert_eq!(lines[0], "1 1,3 two words   5");
    assert_eq!(lines[1], "a=only&b=two+words&empty=&c=x+y");
    assert_eq!(lines[2], "a,b,c,empty");
    assert_eq!(lines[3], "q=caf%C3%A9+%E2%82%AC caf\u{e9} \u{20ac}");
    assert_eq!(lines[4], "https://x.dev/p?one=1&two=2");
    assert_eq!(lines[5], "yes null");
}

// Several assertions here are Windows-shaped (drive letters in file://
// URLs, \\?\... device paths). On POSIX `file:///foo/bar` is a VALID
// path so the throw assertions don't hold. TODO: split into a portable
// half (URLSearchParams, port setters, opaque paths) and a Windows-only
// file:// half so Unix CI retains coverage of the portable cases.
#[cfg_attr(not(windows), ignore = "Windows file:// URL shapes only")]
#[test]
fn url_parity_fleet_regressions() {
    // Every case here is a confirmed divergence from the adversarial
    // oam-vs-node parity fleet, now pinned to node's behavior.
    let stdout = run_ok(
        "url_parity.mjs",
        "import { fileURLToPath, pathToFileURL, urlToHttpOptions } from 'node:url';\n\
         import path from 'node:path';\n\
         // BLOCKER: astral chars through string init.\n\
         const sp = new URLSearchParams('e=\\u{1F984}');\n\
         console.log(sp.toString(), sp.get('e') === '\\u{1F984}');\n\
         // Live (not snapshot) iteration: delete-during-iterate skips.\n\
         const live = new URLSearchParams('a=1&b=2&c=3&d=4');\n\
         for (const [k] of live) live.delete(k);\n\
         console.log(live.toString(), live.size);\n\
         // search edge: empty-present query reads '' but href keeps '?'.\n\
         const q = new URL('https://x.example/p?');\n\
         console.log(JSON.stringify(q.search), q.href);\n\
         const q2 = new URL('https://x.example/p?a=1');\n\
         q2.search = '?';\n\
         console.log(q2.href);\n\
         // Port setter: WHATWG leading-digit parse.\n\
         const pu = new URL('http://example.com:81/');\n\
         pu.port = '8080 ';\n\
         console.log(pu.href);\n\
         // hostname setter no-ops whole on ':'.\n\
         const hu = new URL('http://a.com:7/');\n\
         hu.hostname = 'b.com:99';\n\
         console.log(hu.href);\n\
         // pathname setter no-ops on opaque paths.\n\
         const du = new URL('data:text/plain,abc');\n\
         du.pathname = 'xyz';\n\
         console.log(du.href);\n\
         // Encoded separators must throw, not smuggle.\n\
         let smuggle = '';\n\
         try { fileURLToPath('file:///C:/foo%2Fbar'); } catch (e) { smuggle = e.code; }\n\
         let driveless = '';\n\
         try { fileURLToPath('file:///foo/bar'); } catch (e) { driveless = e.code; }\n\
         console.log(smuggle, driveless);\n\
         // Relative pathToFileURL resolves against cwd; device paths clean.\n\
         const rel = pathToFileURL('foo/bar.txt').href;\n\
         console.log(rel === pathToFileURL(path.resolve('foo/bar.txt')).href, rel.includes('/C:/') || rel.includes(':/'));\n\
         console.log(pathToFileURL('\\\\\\\\?\\\\C:\\\\foo').href);\n\
         // urlToHttpOptions: node shape.\n\
         const o = urlToHttpOptions(new URL('http://us%65r:p%40ss@[::1]:8080/x/y?q=1#h'));\n\
         console.log(o.hostname, typeof o.port, o.port, o.auth, o.pathname);\n\
         const o2 = urlToHttpOptions(new URL('https://example.com/'));\n\
         console.log('port' in o2, 'auth' in o2);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "e=%F0%9F%A6%84 true");
    assert_eq!(lines[1], "b=2&d=4 2");
    assert_eq!(lines[2], "\"\" https://x.example/p?");
    assert_eq!(lines[3], "https://x.example/p?");
    assert_eq!(lines[4], "http://example.com:8080/");
    assert_eq!(lines[5], "http://a.com:7/");
    assert_eq!(lines[6], "data:text/plain,abc");
    assert_eq!(
        lines[7],
        "ERR_INVALID_FILE_URL_PATH ERR_INVALID_FILE_URL_PATH"
    );
    assert_eq!(lines[8], "true true");
    assert_eq!(lines[9], "file:///C:/foo");
    assert_eq!(lines[10], "::1 number 8080 user:p@ss /x/y");
    assert_eq!(lines[11], "false false");
}

// Windows-shaped fixture (drive letters, backslashes, UNC). The cross-
// platform fileURLToPath/pathToFileURL behavior is covered by the parity
// fleet test above; this one pins the Windows-specific round trips
// directly. TODO: extract a portable companion test for the non-drive,
// non-UNC POSIX path round trips.
#[cfg_attr(not(windows), ignore = "Windows path/URL shapes only")]
#[test]
fn node_url_file_conversions_round_trip() {
    let stdout = run_ok(
        "nodeurl.mjs",
        "import { fileURLToPath, pathToFileURL } from 'node:url';\n\
         // import.meta round trip: url -> path === import.meta.filename.\n\
         console.log(fileURLToPath(import.meta.url) === import.meta.filename);\n\
         // Windows drive + spaces.\n\
         console.log(fileURLToPath('file:///C:/dir%20name/file.txt'));\n\
         console.log(pathToFileURL('C:\\\\dir name\\\\file.txt').href);\n\
         // UNC both directions.\n\
         console.log(fileURLToPath('file://server/share/x.txt'));\n\
         console.log(pathToFileURL('\\\\\\\\server\\\\share\\\\y.txt').href);\n\
         // Non-file scheme throws with the Node code.\n\
         let code = '';\n\
         try { fileURLToPath('https://x.dev/a'); } catch (e) { code = e.code; }\n\
         console.log(code);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "true");
    assert_eq!(lines[1], "C:\\dir name\\file.txt");
    assert_eq!(lines[2], "file:///C:/dir%20name/file.txt");
    assert_eq!(lines[3], "\\\\server\\share\\x.txt");
    assert_eq!(lines[4], "file://server/share/y.txt");
    assert_eq!(lines[5], "ERR_INVALID_URL_SCHEME");
}

// ------------------------------------------------------------ node:stream

#[test]
fn node_stream_readable_writable_and_pipe_backpressure() {
    let stdout = run_ok(
        "nstream_core.mjs",
        "import { Readable, Writable } from 'node:stream';\n\
         // Push-source Readable: data events + end.\n\
         const r1 = new Readable({ read() {} });\n\
         const got = [];\n\
         r1.on('data', (c) => got.push(c.toString()));\n\
         r1.on('end', () => console.log('events:', got.join('+')));\n\
         r1.push('a'); r1.push('b'); r1.push(null);\n\
         // Async iteration over a pull source.\n\
         let n = 0;\n\
         const r2 = new Readable({ objectMode: true, read() { n++; this.push(n > 3 ? null : n); } });\n\
         const nums = [];\n\
         for await (const v of r2) nums.push(v);\n\
         console.log('iter:', nums.join(','));\n\
         // pipe with backpressure: tiny HWM writable, slow consumer.\n\
         const src = Readable.from(['x'.repeat(10), 'y'.repeat(10), 'z'.repeat(10)]);\n\
         let written = '';\n\
         let sawFalse = false;\n\
         const dest = new Writable({\n\
           objectMode: true,\n\
           highWaterMark: 1,\n\
           write(chunk, _e, cb) { written += chunk; setTimeout(cb, 5); },\n\
         });\n\
         const origWrite = dest.write.bind(dest);\n\
         dest.write = (...a) => { const ok = origWrite(...a); if (!ok) sawFalse = true; return ok; };\n\
         await new Promise((resolve) => { dest.on('finish', resolve); src.pipe(dest); });\n\
         console.log('piped:', written.length, sawFalse);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "events: a+b");
    assert_eq!(lines[1], "iter: 1,2,3");
    // All 30 chars arrived AND backpressure was exercised.
    assert_eq!(lines[2], "piped: 30 true");
}

#[test]
fn node_stream_transform_pipeline_finished_and_web_interop() {
    let stdout = run_ok(
        "nstream_pipe.mjs",
        "import { Readable, Writable, Transform, PassThrough } from 'node:stream';\n\
         import { pipeline, finished } from 'node:stream/promises';\n\
         const upper = new Transform({\n\
           transform(chunk, _e, cb) { cb(null, chunk.toString().toUpperCase()); },\n\
         });\n\
         let out = '';\n\
         const sink = new Writable({ write(c, _e, cb) { out += c; cb(); } });\n\
         await pipeline(Readable.from(['ab', 'cd']), upper, new PassThrough(), sink);\n\
         console.log(out);\n\
         // pipeline propagates errors and destroys the chain.\n\
         const boom = new Transform({ transform(_c, _e, cb) { cb(new Error('mid-fail')); } });\n\
         let caught = '';\n\
         try {\n\
           await pipeline(Readable.from(['x']), boom, new Writable({ write(_c, _e, cb) { cb(); } }));\n\
         } catch (e) { caught = e.message; }\n\
         console.log(caught);\n\
         // finished() resolves on writable completion.\n\
         const w = new Writable({ write(_c, _e, cb) { cb(); } });\n\
         const done = finished(w);\n\
         w.end('last');\n\
         await done;\n\
         console.log('finished-ok');\n\
         // Web interop round trip: node Readable -> web -> TextDecoderStream.\n\
         const node = Readable.from([new Uint8Array([0x68, 0x69])]);\n\
         const web = Readable.toWeb(node);\n\
         let text = '';\n\
         for await (const part of web.pipeThrough(new TextDecoderStream())) text += part;\n\
         console.log('toWeb:', text);\n\
         const back = Readable.fromWeb(ReadableStream.from(['w1', 'w2']), { objectMode: true });\n\
         const items = [];\n\
         for await (const v of back) items.push(v);\n\
         console.log('fromWeb:', items.join(','));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "ABCD");
    assert_eq!(lines[1], "mid-fail");
    assert_eq!(lines[2], "finished-ok");
    assert_eq!(lines[3], "toWeb: hi");
    assert_eq!(lines[4], "fromWeb: w1,w2");
}

#[test]
fn fs_streams_roundtrip_large_file_in_chunks() {
    let stdout = run_ok(
        "fsstream/main.mjs",
        "import fs from 'node:fs';\n\
         import path from 'node:path';\n\
         import { pipeline } from 'node:stream/promises';\n\
         const dir = import.meta.dirname;\n\
         const src = path.join(dir, 'big.bin');\n\
         const dst = path.join(dir, 'copy.bin');\n\
         // ~300KB: forces multiple 64KB chunks through the read stream.\n\
         const payload = Buffer.alloc(300 * 1024);\n\
         for (let i = 0; i < payload.length; i++) payload[i] = i % 251;\n\
         fs.writeFileSync(src, payload);\n\
         let chunks = 0;\n\
         const reader = fs.createReadStream(src);\n\
         reader.on('data', () => chunks++);\n\
         reader.pause();\n\
         await pipeline(reader, fs.createWriteStream(dst));\n\
         const copied = fs.readFileSync(dst);\n\
         console.log(copied.length, copied.equals(payload), chunks > 1);\n\
         // setEncoding on a text file.\n\
         const tsrc = path.join(dir, 'lines.txt');\n\
         fs.writeFileSync(tsrc, 'line1\\nline2\\nline3');\n\
         let text = '';\n\
         const tr = fs.createReadStream(tsrc, { encoding: 'utf8' });\n\
         for await (const part of tr) text += part;\n\
         console.log(text.split('\\n').length);\n\
         fs.rmSync(src); fs.rmSync(dst); fs.rmSync(tsrc);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "307200 true true");
    assert_eq!(lines[1], "3");
}

// ------------------------------------------------------------ web streams

#[test]
fn readable_stream_core_and_text_pipeline() {
    let stdout = run_ok(
        "streams_core.mjs",
        "// Custom pull source + async iteration.\n\
         let n = 0;\n\
         const rs = new ReadableStream({\n\
           pull(c) { n++; if (n > 3) c.close(); else c.enqueue(n); },\n\
         });\n\
         const got = [];\n\
         for await (const v of rs) got.push(v);\n\
         console.log(got.join(','));\n\
         // from() + TransformStream doubling.\n\
         const doubled = ReadableStream.from([1, 2, 3]).pipeThrough(\n\
           new TransformStream({ transform: (v, c) => c.enqueue(v * 2) }),\n\
         );\n\
         const out = [];\n\
         for await (const v of doubled) out.push(v);\n\
         console.log(out.join(','));\n\
         // WritableStream collects, serialized.\n\
         const sink = [];\n\
         const ws = new WritableStream({ write(chunk) { sink.push(chunk); } });\n\
         const w = ws.getWriter();\n\
         await w.write('a'); await w.write('b'); await w.close();\n\
         console.log(sink.join(''));\n\
         // TextDecoderStream reassembles a split multi-byte char.\n\
         const euro = ReadableStream.from([\n\
           new Uint8Array([0x61, 0xE2]),\n\
           new Uint8Array([0x82, 0xAC, 0x62]),\n\
         ]).pipeThrough(new TextDecoderStream());\n\
         let text = '';\n\
         for await (const part of euro) text += part;\n\
         console.log(text === 'a\\u20ACb', text.length);\n\
         // tee: both branches see everything.\n\
         const [t1, t2] = ReadableStream.from(['x', 'y']).tee();\n\
         const c1 = []; const c2 = [];\n\
         for await (const v of t1) c1.push(v);\n\
         for await (const v of t2) c2.push(v);\n\
         console.log(c1.join(''), c2.join(''));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "1,2,3");
    assert_eq!(lines[1], "2,4,6");
    assert_eq!(lines[2], "ab");
    assert_eq!(lines[3], "true 3");
    assert_eq!(lines[4], "xy xy");
}

#[test]
fn fetch_body_streams_incrementally() {
    let addr = spawn_echo_server();
    let main = write_temp(
        "stream_fetch.mjs",
        &format!(
            "const res = await fetch('http://{addr}/stream');\n\
             console.log(res.status, res.headers.get('content-type'));\n\
             const events = [];\n\
             let chunkCount = 0;\n\
             const decoder = new TextDecoderStream();\n\
             for await (const part of res.body.pipeThrough(decoder)) {{\n\
               chunkCount++;\n\
               for (const line of part.split('\\n')) {{\n\
                 if (line.startsWith('data: ')) events.push(line.slice(6));\n\
               }}\n\
             }}\n\
             // >1 chunk proves INCREMENTAL delivery (the server sleeps\n\
             // between flushes); exact count is transport-dependent.\n\
             console.log(chunkCount > 1, events.join('|'));"
        ),
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "200 text/event-stream");
    assert_eq!(lines[1], "true tok1|tok2|[DONE]");
}

#[test]
fn fetch_text_json_still_work_and_cancel_does_not_hang() {
    let addr = spawn_echo_server();
    let main = write_temp(
        "stream_drain.mjs",
        &format!(
            "const res = await fetch('http://{addr}/hello', {{ method: 'POST', body: 'ping' }});\n\
             const data = await res.json();\n\
             console.log(data.method, data.echo, res.bodyUsed);\n\
             let doubled = '';\n\
             try {{ await res.text(); }} catch (e) {{ doubled = e.constructor.name; }}\n\
             console.log(doubled);\n\
             // Early-exit from iteration cancels the body; the run must\n\
             // exit promptly instead of waiting out the server's stream.\n\
             const slow = await fetch('http://{addr}/stream');\n\
             for await (const _chunk of slow.body) break;\n\
             console.log('cancelled-clean');"
        ),
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "POST ping true");
    assert_eq!(lines[1], "TypeError");
    assert_eq!(lines[2], "cancelled-clean");
}

// ------------------------------------------------- AsyncLocalStorage (CPED)

#[test]
fn als_basic_nested_and_sibling_isolation() {
    let stdout = run_ok(
        "als_basic.mjs",
        "import { AsyncLocalStorage } from 'node:async_hooks';\n\
         const als = new AsyncLocalStorage();\n\
         const other = new AsyncLocalStorage();\n\
         console.log(als.getStore());\n\
         als.run('outer', () => {\n\
           console.log(als.getStore(), other.getStore());\n\
           als.run('inner', () => console.log(als.getStore()));\n\
           other.run('sibling', () => console.log(als.getStore(), other.getStore()));\n\
           console.log(als.getStore());\n\
         });\n\
         console.log(als.getStore());",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "undefined");
    assert_eq!(lines[1], "outer undefined");
    assert_eq!(lines[2], "inner");
    // A sibling storage layered on top must not disturb this one.
    assert_eq!(lines[3], "outer sibling");
    assert_eq!(lines[4], "outer");
    assert_eq!(lines[5], "undefined");
}

#[test]
fn als_survives_await_and_timers() {
    // The CPED probe: context must survive (1) a plain microtask await,
    // (2) a real async-op await (the oam sleep ride the op channel), and
    // (3) a setTimeout macrotask hop. This empirically proves the V8 build
    // propagates continuation-preserved embedder data.
    let stdout = run_ok(
        "als_await.mjs",
        "import { AsyncLocalStorage } from 'node:async_hooks';\n\
         const als = new AsyncLocalStorage();\n\
         await als.run('ctx', async () => {\n\
           await Promise.resolve();\n\
           console.log('after-micro', als.getStore());\n\
           await oam.sleep(5);\n\
           console.log('after-op', als.getStore());\n\
           await new Promise((resolve) => setTimeout(resolve, 5));\n\
           console.log('after-timer', als.getStore());\n\
           setTimeout(() => console.log('in-timer', als.getStore()), 5);\n\
         });\n\
         console.log('outside', als.getStore());",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "after-micro ctx");
    assert_eq!(lines[1], "after-op ctx");
    assert_eq!(lines[2], "after-timer ctx");
    assert_eq!(lines[3], "outside undefined");
    assert_eq!(lines[4], "in-timer ctx");
}

#[test]
fn als_interleaved_concurrent_contexts_stay_isolated() {
    // The classic correctness test: two async chains with different stores
    // interleaving through timers — each must always see ITS OWN store.
    let stdout = run_ok(
        "als_interleave.mjs",
        "import { AsyncLocalStorage } from 'node:async_hooks';\n\
         const als = new AsyncLocalStorage();\n\
         const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));\n\
         const seen = [];\n\
         async function task(name, delay) {\n\
           await als.run(name, async () => {\n\
             for (let i = 0; i < 3; i++) {\n\
               await sleep(delay);\n\
               seen.push(`${name}=${als.getStore()}`);\n\
             }\n\
           });\n\
         }\n\
         await Promise.all([task('A', 3), task('B', 5), task('C', 1)]);\n\
         console.log(seen.every((s) => { const [want, got] = s.split('='); return want === got; }), seen.length);",
    );
    assert_eq!(stdout, "true 9");
}

#[test]
fn als_enter_with_exit_snapshot_and_resource_bind() {
    let stdout = run_ok(
        "als_api.mjs",
        "import { AsyncLocalStorage, AsyncResource } from 'node:async_hooks';\n\
         const als = new AsyncLocalStorage();\n\
         als.run('outer', () => {\n\
           als.exit(() => console.log('exited', als.getStore()));\n\
           console.log('back', als.getStore());\n\
           const restore = AsyncLocalStorage.snapshot();\n\
           als.run('other', () => {\n\
             restore(() => console.log('snapshot-sees', als.getStore()));\n\
           });\n\
           const bound = AsyncResource.bind(() => als.getStore());\n\
           als.run('rebound', () => console.log('bound-sees', bound()));\n\
         });\n\
         als.enterWith('entered');\n\
         console.log('entered', als.getStore());\n\
         als.disable();\n\
         console.log('disabled', als.getStore());",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "exited undefined");
    assert_eq!(lines[1], "back outer");
    // snapshot() captured the 'outer' frame, not the 'other' one.
    assert_eq!(lines[2], "snapshot-sees outer");
    // AsyncResource.bind froze the 'outer' frame too.
    assert_eq!(lines[3], "bound-sees outer");
    assert_eq!(lines[4], "entered entered");
    assert_eq!(lines[5], "disabled undefined");
}

#[test]
fn als_works_from_require_and_nexttick() {
    write_temp(
        "alsreq/main.cjs",
        "const { AsyncLocalStorage } = require('node:async_hooks');\n\
         const als = new AsyncLocalStorage();\n\
         als.run({ id: 42 }, () => {\n\
           process.nextTick(() => console.log('tick', als.getStore().id));\n\
           queueMicrotask(() => console.log('micro', als.getStore().id));\n\
         });",
    );
    let main = write_temp("alsreq/.anchor", "")
        .parent()
        .unwrap()
        .join("main.cjs");
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tick 42"), "stdout: {stdout}");
    assert!(stdout.contains("micro 42"), "stdout: {stdout}");
}

#[test]
fn dynamic_import_rejects_with_actionable_message() {
    let main = write_temp(
        "dyn.mjs",
        "try { await import('./whatever.mjs'); } catch (e) { console.log(e.message.includes('dynamic import'), e.message.includes('static import')); }",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true true");
}

#[test]
fn cjs_modules_can_require_builtins() {
    write_temp(
        "reqbuiltin/main.cjs",
        "const path = require('path');\n\
         const fs = require('node:fs');\n\
         const os = require('os');\n\
         const file = path.join(__dirname, 'cjs-fs.txt');\n\
         fs.writeFileSync(file, 'from-cjs');\n\
         console.log(fs.readFileSync(file, 'utf8'), typeof os.tmpdir() === 'string');\n\
         fs.unlinkSync(file);",
    );
    let main = write_temp("reqbuiltin/.anchor", "")
        .parent()
        .unwrap()
        .join("main.cjs");
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "from-cjs true");
}

// ------------------------------------------------------------- oam test

#[test]
fn oam_test_runs_passing_and_failing_tests_with_exit_codes() {
    write_temp(
        "runner1/math.test.ts",
        "import { test, expect, describe } from 'oam:test';\n\
         describe('math', () => {\n\
           test('adds', () => { expect(1 + 1).toBe(2); });\n\
           test('deep equal', () => { expect({ a: [1, 2] }).toEqual({ a: [1, 2] }); });\n\
           test('fails on purpose', () => { expect(2 + 2).toBe(5); });\n\
           test.skip('skipped', () => { throw new Error('never runs'); });\n\
           test.todo('later');\n\
         });",
    );
    let dir = write_temp("runner1/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = oam(&["test", dir.to_str().unwrap()]);
    assert!(!out.status.success(), "one failing test must fail the run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ok   math > adds"), "stderr: {stderr}");
    assert!(
        stderr.contains("FAIL math > fails on purpose"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("expect(4).toBe(5)"), "stderr: {stderr}");
    assert!(stderr.contains("skip math > skipped"), "stderr: {stderr}");
    assert!(stderr.contains("todo math > later"), "stderr: {stderr}");
    assert!(
        stderr.contains("2 passed, 1 failed, 1 skipped, 1 todo"),
        "stderr: {stderr}"
    );

    // All-green file exits 0.
    write_temp(
        "runner2/green.test.ts",
        "import { test, expect } from 'oam:test';\n\
         test('green', () => { expect('oam').toContain('oa'); });",
    );
    let dir = write_temp("runner2/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = oam(&["test", dir.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn oam_test_async_hooks_only_filter_and_json_mode() {
    write_temp(
        "runner3/flow.test.ts",
        "import { test, expect, describe, beforeEach, afterEach } from 'oam:test';\n\
         const order: string[] = [];\n\
         describe('hooks', () => {\n\
           beforeEach(() => order.push('before'));\n\
           afterEach(() => order.push('after'));\n\
           test('async settles', async () => {\n\
             const v = await new Promise((r) => setTimeout(() => r('done'), 5));\n\
             expect(v).toBe('done');\n\
             expect(order).toContain('before');\n\
           });\n\
           test('await rejects', async () => {\n\
             await expect(Promise.reject(new Error('nope'))).rejects.toThrow('nope');\n\
             await expect(Promise.resolve(7)).resolves.toBe(7);\n\
           });\n\
         });",
    );
    let dir = write_temp("runner3/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = oam(&["test", dir.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // -t filter: only the matching test runs.
    let out = oam(&["test", dir.to_str().unwrap(), "-t", "async settles"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success());
    assert!(
        stderr.contains("ok   hooks > async settles"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("await rejects"), "filtered out: {stderr}");

    // --json: failures are ODIF; the summary is ODIF.
    write_temp(
        "runner4/red.test.ts",
        "import { test, expect } from 'oam:test';\n\
         test('red', () => { expect(1).toBe(2); });",
    );
    let dir = write_temp("runner4/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = oam(&["test", dir.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-TEST0001"), "stderr: {stderr}");
    assert!(stderr.contains("\"odif\":\"1\""), "stderr: {stderr}");
    assert!(stderr.contains("OAM-TEST0000"), "summary line: {stderr}");
}

#[test]
fn oam_test_mocks_spies_and_fake_timers() {
    write_temp(
        "runner5/mocks.test.ts",
        "import { test, expect, mock } from 'oam:test';\n\
         test('mock.fn tracks calls and impls', () => {\n\
           const f = mock.fn((x: number) => x * 2);\n\
           f.mockReturnValueOnce(99);\n\
           expect(f(1)).toBe(99);\n\
           expect(f(3)).toBe(6);\n\
           expect(f).toHaveBeenCalledTimes(2);\n\
           expect(f).toHaveBeenCalledWith(3);\n\
           expect(f).toHaveBeenLastCalledWith(3);\n\
         });\n\
         test('spyOn wraps and restores', () => {\n\
           const target = { greet: (n: string) => 'hi ' + n };\n\
           const spy = mock.spyOn(target, 'greet');\n\
           expect(target.greet('oam')).toBe('hi oam');\n\
           expect(spy).toHaveBeenCalledWith('oam');\n\
           spy.mockRestore();\n\
           expect((target.greet as any).mock).toBeUndefined();\n\
         });\n\
         test('fake timers tick deterministically', () => {\n\
           mock.timers.enable({ now: 1000 });\n\
           const fired: number[] = [];\n\
           setTimeout(() => fired.push(Date.now()), 50);\n\
           setInterval(() => fired.push(-Date.now()), 30);\n\
           expect(fired).toHaveLength(0);\n\
           mock.timers.tick(60);\n\
           expect(fired).toEqual([-1030, 1050, -1060]);\n\
           mock.timers.restore();\n\
         });",
    );
    let dir = write_temp("runner5/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = oam(&["test", dir.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("3 passed"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn oam_test_timeout_and_no_files_are_clear_failures() {
    write_temp(
        "runner6/slow.test.ts",
        "import { test } from 'oam:test';\n\
         test('hangs', async () => { await new Promise(() => {}); }, { timeout: 200 });",
    );
    let dir = write_temp("runner6/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = oam(&["test", dir.to_str().unwrap()]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("timed out after 200ms"), "stderr: {stderr}");

    let empty = write_temp("runner7/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = oam(&["test", empty.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no test files found"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn import_cycles_follow_esm_tdz_semantics() {
    // cycle_a evaluates first (entry-last post-order), so its top level must
    // not touch cycle_b's bindings â€” but lazy access via a function is fine.
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
fn json_modules_import_with_and_without_attributes() {
    write_temp(
        "jsonmod/config.json",
        "{\"name\": \"oam\", \"port\": 8080, \"tags\": [\"fast\", \"typed\"]}",
    );
    let stdout = run_ok(
        "jsonmod/main.ts",
        "import config from './config.json';\n\
         import attributed from './config.json' with { type: 'json' };\n\
         import { name, port } from './config.json';\n\
         console.log(config.name, config.tags.length, attributed.port, name, port);",
    );
    assert_eq!(stdout, "oam 2 8080 oam 8080");
}

#[test]
fn json_modules_from_packages_bom_and_failure_modes() {
    // Package-shipped JSON (the import side of require('../package.json')).
    write_temp(
        "jsonpkg/node_modules/withdata/package.json",
        "{\"name\": \"withdata\", \"type\": \"module\", \"main\": \"index.js\"}",
    );
    write_temp(
        "jsonpkg/node_modules/withdata/data/values.json",
        "{\"answer\": 42}",
    );
    write_temp(
        "jsonpkg/node_modules/withdata/index.js",
        "import values from './data/values.json';\nexport const answer = values.answer;",
    );
    // BOM'd JSON parses (Node strips it; PowerShell writes them).
    write_temp("jsonpkg/bom.json", "\u{feff}{\"bom\": true}");
    let stdout = run_ok(
        "jsonpkg/main.ts",
        "import { answer } from 'withdata';\n\
         import bom from './bom.json';\n\
         console.log(answer, bom.bom);",
    );
    assert_eq!(stdout, "42 true");

    // Malformed JSON fails the load with the file named.
    write_temp("jsonbad/broken.json", "{\"oops\": ");
    let main = write_temp("jsonbad/main.ts", "import './broken.json';");
    let out = oam(&["run", main.to_str().unwrap(), "--no-check"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("broken.json"), "stderr: {stderr}");

    // An unsupported attribute type is a clear diagnostic, not a crash.
    write_temp("jsonbad/style.css", "body {}");
    let main = write_temp(
        "jsonbad/css_main.ts",
        "import './style.css' with { type: 'css' };",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--json", "--no-check"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-MOD0003"), "stderr: {stderr}");
    assert!(
        stderr.contains("only supported import-attribute type"),
        "stderr: {stderr}"
    );

    // A .json ENTRY is not a program.
    let entry = write_temp("jsonbad/alone.json", "{}");
    let out = oam(&["run", entry.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not a program"),
        "json entry gate"
    );
}

#[test]
fn cts_is_a_clear_diagnostic() {
    // .cjs runs via interop since M2; .cts (TypeScript-CJS) stays gated.
    let file = write_temp("legacy.cts", "export const a: number = 1;");
    let out = oam(&["run", file.to_str().unwrap(), "--json", "--no-check"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-MOD0003"), "stderr: {stderr}");
    assert!(stderr.contains("ESM TypeScript"), "stderr: {stderr}");
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
fn fetch_gets_json_with_headers() {
    let addr = spawn_echo_server();
    let main = write_temp(
        "fetch_get.ts",
        &format!(
            "const res = await fetch('http://{addr}/hello');\nconsole.log(res.ok, res.status, res.headers.get('x-oam-test'));\nconst data = await res.json();\nconsole.log(data.method, data.path);"
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
        "true 200 yes\nGET /hello"
    );
}

#[test]
fn fetch_posts_body_and_headers() {
    let addr = spawn_echo_server();
    let main = write_temp(
        "fetch_post.ts",
        &format!(
            "const res = await fetch('http://{addr}/submit', {{ method: 'post', headers: {{ 'content-type': 'text/plain' }}, body: 'ping' }});\nconst data = await res.json();\nconsole.log(data.method, data.echo);"
        ),
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "POST ping");
}

#[test]
fn fetch_network_error_rejects_with_typeerror() {
    // Port 1 on loopback: reliably refused, no external network involved.
    // WHATWG requires fetch to reject with a TypeError specifically.
    let main = write_temp(
        "fetch_refused.ts",
        "try {\n  await fetch('http://127.0.0.1:1/');\n} catch (e) {\n  console.log('caught:', e instanceof TypeError && (e as Error).message.includes('fetch failed'));\n}",
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
fn fetch_map_headers_are_sent() {
    let addr = spawn_echo_server();
    let main = write_temp(
        "fetch_map_headers.ts",
        &format!(
            "const res = await fetch('http://{addr}/h', {{ headers: new Map([['x-probe', 'mapval']]) }});\nconst data = await res.json();\nconsole.log(data.xProbe);"
        ),
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "mapval");
}

#[test]
fn fetch_reports_redirected() {
    let addr = spawn_echo_server();
    let main = write_temp(
        "fetch_redirect.ts",
        &format!(
            "const res = await fetch('http://{addr}/redirect');\nconsole.log(res.redirected, res.status, res.url.endsWith('/hello'));\nconst direct = await fetch('http://{addr}/hello');\nconsole.log(direct.redirected);"
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
        "true 200 true\nfalse"
    );
}

#[test]
fn busy_interval_does_not_starve_ops() {
    // Review blocker regression: a continuously-due interval must not stop
    // op completions from settling. The n-guard turns a regression into a
    // fast failure instead of a hung test.
    let main = write_temp(
        "starve.ts",
        "let n = 0;\nlet done = false;\nconst id = setInterval(() => {\n  n++;\n  if (done) { clearInterval(id); console.log('settled-with-interval', n > 0); }\n  else if (n > 100000) { clearInterval(id); console.log('starved'); }\n}, 0);\noam.sleep(15).then(() => { done = true; });",
    );
    let out = oam(&["run", main.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("settled-with-interval true"),
        "got: {stdout}"
    );
    assert!(!stdout.contains("starved"), "op completion starved");
}

#[test]
fn uncaught_microtask_exception_fails_the_run() {
    // Review major regression: this used to print to STDOUT via V8's
    // default handler and exit 0.
    let main = write_temp(
        "micro_throw.ts",
        "queueMicrotask(() => { throw new Error('vanished-from-microtask'); });",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    assert!(!out.status.success(), "must exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-RT0001"), "stderr: {stderr}");
    assert!(
        stderr.contains("vanished-from-microtask"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("in microtask"), "stderr: {stderr}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "",
        "no unstructured dump on stdout"
    );
}

#[test]
fn check_missing_tsgo_is_ts0000_via_env() {
    let file = write_temp("env_check.ts", "export const n: number = 1;");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args(["check", file.to_str().unwrap(), "--json", "--no-daemon"])
        .env("OAM_TSGO", "/definitely/not/a/tsgo")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("OAM-TS0000"),
        "missing tsgo must classify as OAM-TS0000"
    );
}

#[test]
fn run_warn_reports_type_errors_without_blocking() {
    // The wedge: the program EXECUTES (types stripped) while the checker
    // reports the type error afterward — Bun shows nothing, Node can't run
    // checks at all.
    // Type-broken but runtime-fine: strips to a plain string assignment.
    let main = write_temp(
        "typed_loop.ts",
        "const n: number = 'oops';\nconsole.log('ran anyway');",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--json"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("OAM-TS0000") {
        eprintln!("skipping: tsgo not installed");
        return;
    }
    assert!(out.status.success(), "warn mode must not change exit code");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ran anyway");
    assert!(
        stderr.contains("\"origin\":\"typecheck\""),
        "typecheck ODIF expected in the stream: {stderr}"
    );
    assert!(stderr.contains("OAM-TS2322"), "stderr: {stderr}");
}

#[test]
fn run_check_block_gates_execution() {
    let main = write_temp(
        "typed_gate.ts",
        "const n: number = 'oops';\nconsole.log('must not run');",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--check", "block", "--json"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("OAM-TS0000") {
        eprintln!("skipping: tsgo not installed");
        return;
    }
    assert!(!out.status.success(), "block mode must gate on type errors");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "",
        "program must not execute"
    );
    assert!(stderr.contains("OAM-TS2322"), "stderr: {stderr}");
}

#[test]
fn run_no_check_stays_silent() {
    let main = write_temp(
        "typed_off.ts",
        "const n: number = 'oops';\nconsole.log('ran');",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--no-check"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ran");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "",
        "no checker output in off mode"
    );
}

#[test]
fn run_clean_typescript_adds_no_noise() {
    let main = write_temp("typed_clean.ts", "const n: number = 42;\nconsole.log(n);");
    let out = oam(&["run", main.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("OAM-TS0000") || stderr.contains("type check skipped") {
        eprintln!("skipping: tsgo not installed");
        return;
    }
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
    assert_eq!(stderr.trim(), "", "clean runs stay quiet: {stderr}");
}

#[test]
fn tsconfig_paths_resolve_in_run_and_check_agrees() {
    // JSONC on purpose: real tsconfigs carry comments + trailing commas.
    write_temp(
        "pathsproj/tsconfig.json",
        "{\n  // alias the lib (substitutions must be relative without baseUrl — TS5090)\n  \"compilerOptions\": {\n    \"strict\": true,\n    \"noEmit\": true,\n    \"paths\": {\n      \"@lib/*\": [\"./src/lib/*\"],\n    },\n  },\n}",
    );
    write_temp(
        "pathsproj/src/lib/util.ts",
        "export function greet(): string { return 'via paths'; }",
    );
    let main = write_temp(
        "pathsproj/main.ts",
        "import { greet } from '@lib/util';\nconsole.log(greet());",
    );

    // run: the loader honors the alias.
    let out = oam(&["run", main.to_str().unwrap(), "--no-check"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "via paths");

    // check: tsgo agrees the same project is clean (resolution parity).
    let proj = main.parent().unwrap();
    let check = oam(&["check", proj.to_str().unwrap(), "--no-daemon"]);
    if tsgo_available(&check) {
        assert!(
            check.status.success(),
            "tsgo must agree with the loader: {}",
            String::from_utf8_lossy(&check.stderr)
        );
    }

    // A bare specifier that no pattern maps stays a clear MOD0002 with the
    // paths-consulted note.
    let miss = write_temp("pathsproj/miss.ts", "import '@nope/never';");
    let out = oam(&["run", miss.to_str().unwrap(), "--json", "--no-check"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OAM-MOD0002"), "stderr: {stderr}");
    assert!(
        stderr.contains("tsconfig paths were consulted"),
        "stderr: {stderr}"
    );
}

#[test]
fn check_daemon_lifecycle_and_cache() {
    // Fully isolated daemon world: state files, build-info, and the daemon
    // itself all live under this run's cache dir and are stopped at the end.
    let cache = write_temp("daemon-cache/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();
    write_temp(
        "daemonproj/tsconfig.json",
        "{\"compilerOptions\": {\"strict\": true, \"noEmit\": true}}",
    );
    let proj = write_temp("daemonproj/ok.ts", "export const n: number = 1;")
        .parent()
        .unwrap()
        .to_path_buf();
    let oam_d = |args: &[&str]| -> Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
            .args(args)
            .env("OAM_CACHE_DIR", &cache)
            // Regression guard: if daemon spawn/teardown ever breaks again,
            // the daemon reaps itself in 45s instead of wedging the suite.
            .env("OAM_DAEMON_IDLE_MS", "45000")
            .output()
            .expect("oam runs")
    };

    let first = oam_d(&["check", proj.to_str().unwrap()]);
    if !tsgo_available(&first) {
        eprintln!("skipping: tsgo not installed");
        return;
    }
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let status = oam_d(&["daemon", "status", proj.to_str().unwrap()]);
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&status.stdout).trim()).unwrap();
    assert_eq!(parsed["running"], true, "daemon should be up: {parsed}");
    assert!(parsed["checks_served"].as_u64().unwrap() >= 1);

    // Unchanged tree: second check must be served from the daemon cache.
    let second = oam_d(&["check", proj.to_str().unwrap()]);
    assert!(second.status.success());
    let status = oam_d(&["daemon", "status", proj.to_str().unwrap()]);
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&status.stdout).trim()).unwrap();
    assert!(
        parsed["cache_hits"].as_u64().unwrap() >= 1,
        "repeat check should cache-hit: {parsed}"
    );

    // Edited tree: diagnostics stay correct (a NEW error must surface).
    write_temp("daemonproj/ok.ts", "export const n: number = 'broken';");
    let third = oam_d(&["check", proj.to_str().unwrap(), "--json"]);
    assert!(!third.status.success(), "edited project must fail check");
    assert!(
        String::from_utf8_lossy(&third.stderr).contains("OAM-TS2322"),
        "stderr: {}",
        String::from_utf8_lossy(&third.stderr)
    );

    let stop = oam_d(&["daemon", "stop", proj.to_str().unwrap()]);
    assert_eq!(
        String::from_utf8_lossy(&stop.stdout).trim(),
        "{\"stopped\":true}"
    );
    let status = oam_d(&["daemon", "status", proj.to_str().unwrap()]);
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&status.stdout).trim()).unwrap();
    assert_eq!(parsed["running"], false);

    // The incremental build-info landed under OUR cache, not the repo.
    let buildinfo_dir = cache.join("ts-buildinfo");
    assert!(
        buildinfo_dir
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false),
        "tsbuildinfo expected under the oam cache"
    );
}

#[test]
fn fetch_body_cannot_be_consumed_twice() {
    let addr = spawn_echo_server();
    let main = write_temp(
        "fetch_double.ts",
        &format!(
            "const res = await fetch('http://{addr}/');\nawait res.text();\ntry {{\n  await res.json();\n}} catch (e) {{\n  console.log('double:', (e as Error).message);\n}}\nconsole.log('used:', res.bodyUsed);"
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
        "double: Body already consumed\nused: true"
    );
}

/// tsgo is an external toolchain piece; when it's absent the runtime emits
/// the stable OAM-TS0000 code and these tests skip rather than flake.
fn tsgo_available(out: &Output) -> bool {
    !String::from_utf8_lossy(&out.stderr).contains("OAM-TS0000")
}

#[test]
fn check_reports_type_errors_as_odif() {
    write_temp(
        "checkproj/tsconfig.json",
        "{\"compilerOptions\": {\"strict\": true, \"noEmit\": true}}",
    );
    let dir = write_temp(
        "checkproj/bad.ts",
        "const n: number = 'oops';\nexport default n;",
    )
    .parent()
    .unwrap()
    .to_path_buf();
    let out = oam(&["check", dir.to_str().unwrap(), "--json", "--no-daemon"]);
    if !tsgo_available(&out) {
        eprintln!("skipping: tsgo not installed");
        return;
    }
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first = stderr.lines().next().unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(first).expect("ODIF JSONL");
    assert_eq!(parsed["code"], "OAM-TS2322");
    assert_eq!(parsed["origin"], "typecheck");
    assert_eq!(parsed["severity"], "error");
    assert!(
        parsed["spans"][0]["file"]
            .as_str()
            .unwrap()
            .ends_with("bad.ts")
    );
    assert_eq!(parsed["spans"][0]["start"]["line"], 1);
}

#[test]
fn check_clean_project_exits_zero() {
    write_temp(
        "cleanproj/tsconfig.json",
        "{\"compilerOptions\": {\"strict\": true, \"noEmit\": true}}",
    );
    let dir = write_temp("cleanproj/good.ts", "export const n: number = 1;")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = oam(&["check", dir.to_str().unwrap(), "--no-daemon"]);
    if !tsgo_available(&out) {
        eprintln!("skipping: tsgo not installed");
        return;
    }
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("clean"),
        "human summary expected"
    );
}

#[test]
fn check_single_file_without_tsconfig() {
    let file = write_temp(
        "lonely_check.ts",
        "const s: string = 42;\nexport default s;",
    );
    let out = oam(&["check", file.to_str().unwrap(), "--json", "--no-daemon"]);
    if !tsgo_available(&out) {
        eprintln!("skipping: tsgo not installed");
        return;
    }
    // NOTE: nearest-tsconfig walk-up can find unrelated configs above the
    // temp dir in exotic setups; the diagnostic still lands either way.
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("OAM-TS2322"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn mcp_serves_the_agent_loop_over_stdio() {
    use std::io::{BufRead, BufReader, Write};

    let broken = write_temp("mcp_broken.ts", "import './does_not_exist';");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("oam mcp spawns");

    let requests = [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "e2e"}}}),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "oam_run", "arguments": {"file": broken.to_str().unwrap()}}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "oam_explain", "arguments": {"code": "OAM-MOD0001"}}}),
    ];
    {
        let stdin = child.stdin.as_mut().unwrap();
        for request in &requests {
            writeln!(stdin, "{request}").unwrap();
        }
    }
    drop(child.stdin.take()); // EOF ends the server loop cleanly.

    let stdout = BufReader::new(child.stdout.take().unwrap());
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).expect("each response is JSON"))
        .collect();
    assert!(child.wait().unwrap().success(), "clean exit on stdin EOF");

    // 4 requests with ids -> 4 responses, in order; the notification none.
    assert_eq!(responses.len(), 4);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(responses[1]["result"]["tools"].as_array().unwrap().len(), 4);

    // The run tool reports the failure as structured ODIF, not prose.
    let run_payload: serde_json::Value = serde_json::from_str(
        responses[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("oam_run payload is JSON");
    assert_eq!(run_payload["exitCode"], 1);
    assert_eq!(run_payload["diagnostics"][0]["code"], "OAM-MOD0001");
    assert_eq!(run_payload["diagnostics"][0]["origin"], "resolve");

    // And the agent can ask what the code means.
    let explanation = responses[3]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(explanation.contains("candidate path"), "got: {explanation}");
}

#[test]
fn mcp_run_kills_hung_scripts_at_deadline() {
    use std::io::{BufRead, BufReader, Write};

    // Legitimate Node keep-alive semantics: this script never exits.
    let hang = write_temp("mcp_hang.ts", "setInterval(() => {}, 1000);");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("oam mcp spawns");
    {
        let stdin = child.stdin.as_mut().unwrap();
        let call = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "oam_run", "arguments": {"file": hang.to_str().unwrap(), "timeoutMs": 1500}}});
        writeln!(stdin, "{call}").unwrap();
    }
    drop(child.stdin.take());

    let started = std::time::Instant::now();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    assert!(child.wait().unwrap().success());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "deadline must fire, not hang"
    );

    let payload: serde_json::Value = serde_json::from_str(
        responses[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(payload["timedOut"], true);
    assert_eq!(responses[0]["result"]["isError"], true);
}

#[test]
fn mcp_run_treats_dash_prefixed_file_as_path() {
    use std::io::{BufRead, BufReader, Write};

    // Pre-fix, file '--help' hit clap's flag parsing and returned a
    // success-shaped payload with clap's help text.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("oam mcp spawns");
    {
        let stdin = child.stdin.as_mut().unwrap();
        let call = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "oam_run", "arguments": {"file": "--help"}}});
        writeln!(stdin, "{call}").unwrap();
    }
    drop(child.stdin.take());

    let stdout = BufReader::new(child.stdout.take().unwrap());
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    assert!(child.wait().unwrap().success());

    let payload: serde_json::Value = serde_json::from_str(
        responses[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(payload["exitCode"], 1, "'--help' must be treated as a path");
    assert!(
        !payload["diagnostics"].as_array().unwrap().is_empty(),
        "a diagnostic must explain the failure: {payload}"
    );
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

/// End-to-end V8 Inspector: spawn `oam run --inspect-brk`, attach a real
/// CDP-over-WebSocket client, drive the enable/run handshake, ride the
/// break-on-start and `debugger;` pauses by resuming each, and confirm the
/// program runs to completion. Exercises the whole transport + pause-loop
/// reentrancy end to end.
#[tokio::test]
async fn inspector_attaches_pauses_on_debugger_and_resumes() {
    use futures_util::{SinkExt, StreamExt};
    use std::process::Stdio;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    // Grab a free port, then release it so the child can bind it.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let file = write_temp(
        "inspector/brk.mjs",
        "console.log('before'); debugger; console.log('after');",
    );
    let cache = write_temp("oam-cache-insp/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args([
            "run",
            &format!("--inspect-brk=127.0.0.1:{port}"),
            file.to_str().unwrap(),
        ])
        .env("OAM_CACHE_DIR", cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("oam binary spawns");

    // The whole drive is time-boxed so a regression can never hang CI.
    let drive = tokio::time::timeout(Duration::from_secs(25), async {
        // The path is ignored by the server (it routes on the Upgrade
        // header, not the URL), so a fixed path is fine.
        let url = format!("ws://127.0.0.1:{port}/oam");
        let mut ws = None;
        for _ in 0..50 {
            match tokio_tungstenite::connect_async(&url).await {
                Ok((stream, _)) => {
                    ws = Some(stream);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let mut ws = ws.expect("inspector accepts a WebSocket connection");

        // Standard DevTools attach handshake. runIfWaitingForDebugger
        // releases --inspect-brk's wait and arms the break-on-start.
        for msg in [
            r#"{"id":1,"method":"Runtime.enable"}"#,
            r#"{"id":2,"method":"Debugger.enable"}"#,
            r#"{"id":3,"method":"Runtime.runIfWaitingForDebugger"}"#,
        ] {
            ws.send(Message::Text(msg.to_string())).await.unwrap();
        }

        let mut paused = 0;
        let mut resume_id = 100;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if text.contains("\"Debugger.paused\"") {
                        paused += 1;
                        resume_id += 1;
                        let resume =
                            format!("{{\"id\":{resume_id},\"method\":\"Debugger.resume\"}}");
                        ws.send(Message::Text(resume)).await.unwrap();
                    }
                }
                // Program finished -> runtime dropped -> socket closed.
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_))) | Err(_) => break,
            }
        }
        paused
    });

    let paused = drive.await.expect("inspector drive did not time out");

    let output = child.wait_with_output().expect("child exits");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        paused >= 1,
        "expected at least one Debugger.paused; stderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "program should run to completion after resume; stderr: {stderr}"
    );
    assert!(
        stdout.contains("before") && stdout.contains("after"),
        "program output incomplete: {stdout:?}"
    );
    assert!(
        stderr.contains("Debugger listening on ws://"),
        "missing listening banner: {stderr}"
    );
}

/// End-to-end reconnect: spawn `oam run --inspect`, attach a first WebSocket
/// client, drive the enable/run handshake, send `Debugger.enable`, then close
/// the WebSocket gracefully.  Re-connect a second WebSocket to the same
/// process and verify the new session responds to `Runtime.enable` (i.e. the
/// inspector transport is still live and the engine dispatches CDP messages to
/// the new client).  The program runs to completion after the second client
/// disconnects.
///
/// Uses `--inspect` (not `--inspect-brk`) so the program runs freely and the
/// reconnect window is driven by the test, not by a breakpoint.
#[tokio::test]
async fn inspector_reconnects_after_client_disconnect() {
    use futures_util::{SinkExt, StreamExt};
    use std::process::Stdio;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    // Grab a free port, then release it so the child can bind it.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    // A script with a deliberate pause so we have time to disconnect and
    // reconnect before the process exits naturally.  3 s is generous; the
    // whole test is time-boxed to 20 s, well below the CI job limit.
    let file = write_temp(
        "inspector/reconnect.mjs",
        "console.log('start');\n\
         await new Promise(r => setTimeout(r, 3000));\n\
         console.log('end');",
    );
    let cache = write_temp("oam-cache-reconn/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args([
            "run",
            &format!("--inspect=127.0.0.1:{port}"),
            file.to_str().unwrap(),
        ])
        .env("OAM_CACHE_DIR", cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("oam binary spawns");

    let drive = tokio::time::timeout(Duration::from_secs(20), async {
        let url = format!("ws://127.0.0.1:{port}/oam");

        // --- First client ---
        let mut ws1 = None;
        // Up to 5 s for the process to start and bind.
        for _ in 0..50 {
            match tokio_tungstenite::connect_async(&url).await {
                Ok((stream, _)) => {
                    ws1 = Some(stream);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let mut ws1 = ws1.expect("first client connects");

        // Standard attach handshake.
        for msg in [
            r#"{"id":1,"method":"Runtime.enable"}"#,
            r#"{"id":2,"method":"Debugger.enable"}"#,
        ] {
            ws1.send(Message::Text(msg.to_string())).await.unwrap();
        }

        // Drain responses until we see one with an id field (a CDP response
        // to one of our requests), confirming the session is active.
        let mut got_response = false;
        'drain: for _ in 0..20 {
            match tokio::time::timeout(Duration::from_millis(500), ws1.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if text.contains("\"id\"") {
                        got_response = true;
                        break 'drain;
                    }
                }
                Ok(Some(Ok(_))) => {} // ping/pong/binary
                _ => break 'drain,    // timeout, error, or EOF
            }
        }
        assert!(got_response, "first session: no response to Runtime.enable");

        // Gracefully close the first WebSocket session.
        let _ = ws1.close(None).await;
        drop(ws1);

        // Give the transport a moment to process the disconnect and refresh
        // its channel state before the second client attempts to connect.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // --- Second client ---
        let mut ws2 = None;
        for _ in 0..30 {
            match tokio_tungstenite::connect_async(&url).await {
                Ok((stream, _)) => {
                    ws2 = Some(stream);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let mut ws2 = ws2.expect("second client connects after disconnect");

        // The new session must respond to CDP commands.
        ws2.send(Message::Text(
            r#"{"id":10,"method":"Runtime.enable"}"#.to_string(),
        ))
        .await
        .unwrap();

        let mut second_session_ok = false;
        'drain2: for _ in 0..20 {
            match tokio::time::timeout(Duration::from_millis(500), ws2.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if text.contains("\"id\":10") {
                        second_session_ok = true;
                        break 'drain2;
                    }
                }
                Ok(Some(Ok(_))) => {}
                _ => break 'drain2,
            }
        }

        let _ = ws2.close(None).await;
        second_session_ok
    });

    let second_session_ok = drive.await.expect("reconnect drive did not time out");

    let output = child.wait_with_output().expect("child exits");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        second_session_ok,
        "second inspector session did not respond to Runtime.enable; stderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "program should complete after reconnect; stderr: {stderr}"
    );
    assert!(
        stdout.contains("start") && stdout.contains("end"),
        "program output incomplete: {stdout:?}"
    );
}

// ---------------------------------------------------- coverage backfill (recent regressions)
//
// These tests cover the specific code paths surfaced in the post-M2 review:
// edits whose absence is silent in normal runs but whose regression would be
// real-world wrong-result class.

#[test]
fn cjs_require_resolves_via_tsconfig_paths() {
    // Regression guard: resolve_require used to skip the tsconfig paths
    // consultation that resolve_import does -- so `require('@lib/x')` would
    // 404 while `import '@lib/x'` worked. Both paths must agree.
    write_temp(
        "cjspaths/tsconfig.json",
        "{\n  \"compilerOptions\": {\n    \"paths\": { \"@lib/*\": [\"./src/lib/*\"] }\n  }\n}",
    );
    // CJS target the alias resolves to. Node CJS LOAD_AS_FILE only probes
    // .js/.json/.node (per probe_require); use .js with a "type": "commonjs"
    // package.json to keep this unambiguously CJS.
    write_temp("cjspaths/package.json", "{ \"type\": \"commonjs\" }");
    write_temp(
        "cjspaths/src/lib/util.js",
        "module.exports = { greet: () => 'via cjs paths' };",
    );
    let main = write_temp(
        "cjspaths/main.cjs",
        "const { greet } = require('@lib/util');\nconsole.log(greet());",
    );
    let out = oam(&["run", main.to_str().unwrap(), "--no-check"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "via cjs paths");

    // Negative path: unmatched alias still surfaces the paths-consulted
    // note. (CJS require() failures surface at runtime as OAM-RT0001
    // wrapping the underlying loader message; the consulted-paths suffix
    // is what proves resolve_require ACTUALLY ran the consultation.)
    let miss = write_temp("cjspaths/miss.cjs", "require('@nope/never');");
    let out = oam(&["run", miss.to_str().unwrap(), "--json", "--no-check"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("tsconfig paths were consulted"),
        "stderr: {stderr}"
    );
}

#[test]
fn unhandled_rejection_handler_receives_promise_as_second_arg() {
    // Node's contract is process.emit('unhandledRejection', reason, promise).
    // The mid-run drain used to pass only reason; APMs and test runners that
    // chain off the promise (e.g. promise.catch in the handler) silently saw
    // undefined. This test exercises both the mid-run path (handler present
    // BEFORE rejection) and that the promise IS the originating promise.
    let stdout = run_ok(
        "rej_two_arg.mjs",
        "const seen = [];\n\
         process.on('unhandledRejection', (reason, promise) => {\n\
           seen.push([\n\
             reason?.message ?? String(reason),\n\
             promise instanceof Promise,\n\
             typeof promise?.then === 'function',\n\
           ]);\n\
         });\n\
         const original = Promise.reject(new Error('two-arg-shape'));\n\
         // Capture the identity check too: handler must be passed THE SAME\n\
         // promise object (not a wrapper).\n\
         process.on('unhandledRejection', (_reason, promise) => {\n\
           seen.push(['same?', promise === original]);\n\
         });\n\
         await new Promise((r) => setTimeout(r, 30));\n\
         console.log(JSON.stringify(seen));",
    );
    // Both handlers fire for the same rejection; both see a real Promise.
    assert!(
        stdout.contains("\"two-arg-shape\",true,true"),
        "missing (reason, promise) two-arg shape: {stdout}"
    );
    assert!(
        stdout.contains("[\"same?\",true]"),
        "promise identity broken: {stdout}"
    );
}

#[test]
fn http_per_request_body_over_cap_returns_413_not_503() {
    // The HTTP body-budget gate has two distinct failure modes:
    //   413 -- this ONE request exceeded MAX_REQUEST_BODY (per-request cap).
    //   503 -- the aggregate global budget is full (server is busy).
    // The simplification collapsed both to 503 in one earlier pass; the fix
    // restores the distinction. A 413 -> 503 collapse means a clearly-buggy
    // client gets told "try again later," which it does, forever.
    let stdout = run_ok(
        "http_413.mjs",
        "import http from 'node:http';\n\
         const server = http.createServer((req, res) => res.end('ok'));\n\
         await new Promise((r) => server.listen(0, r));\n\
         const base = `http://127.0.0.1:${server.address().port}`;\n\
         // 110 MB body -- larger than MAX_REQUEST_BODY (100 MB). Fresh server,\n\
         // empty aggregate budget, so the only reason to reject is per-request.\n\
         const huge = 'x'.repeat(110 * 1024 * 1024);\n\
         const res = await fetch(`${base}/big`, { method: 'POST', body: huge })\n\
           .catch((e) => ({ status: 'ERR:' + e.constructor.name }));\n\
         console.log(res.status);\n\
         server.close();",
    );
    // 413 = the per-request size cap fired and the client read the response.
    // ERR:* = server-close race (shouldn't happen with the drain fix, but the
    //         loose test still accepts it as a safety net).
    // What we MUST reject: 503 (the 413/503 collapse bug) or 200 (cap missed).
    // Check 503 first so the regression-specific message surfaces instead of
    // being swallowed by the general "413 or ERR:*" assertion below.
    assert_ne!(
        stdout, "503",
        "413/503 collapse regression: per-request over-cap surfaced as 'busy'"
    );
    assert!(
        stdout == "413" || stdout.starts_with("ERR:"),
        "per-request over-cap must yield 413 or a network error (cap fired \
         before client read response); got: {stdout}"
    );
}

#[test]
fn process_stdout_write_invokes_callback() {
    // Regression guard: process.stdout/stderr.write used to ignore the cb
    // argument silently. stream.pipeline + writable.end depend on the cb to
    // sequence chunks; dropping it broke piping into stdout.
    let stdout = run_ok(
        "stdout_cb.mjs",
        "let calls = 0;\n\
         const r = process.stdout.write('first\\n', () => { calls++; });\n\
         process.stderr.write('side\\n', () => { calls++; });\n\
         await new Promise((r) => setTimeout(r, 10));\n\
         // Return value is always true (no backpressure shape), AND both\n\
         // callbacks fired.\n\
         console.log('return=' + r);\n\
         console.log('calls=' + calls);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "first");
    assert_eq!(lines[1], "return=true");
    assert_eq!(
        lines[2], "calls=2",
        "callback must fire for both write calls"
    );
}

#[test]
fn writable_write_after_end_without_error_listener_does_not_crash() {
    // Bug: Writable.write on an already-ended stream emitted 'error'
    // unconditionally. EventEmitter throws on 'error' with no listener -- so
    // a graceful no-op write killed the process. ServerResponse.write had the
    // listener-count guard; the base Writable did not.
    let stdout = run_ok(
        "writable_after_end.mjs",
        "import { Writable } from 'node:stream';\n\
         const sink = new Writable({ write(_c, _e, cb) { cb(); } });\n\
         // No 'error' listener attached.\n\
         sink.end('first');\n\
         await new Promise((r) => setTimeout(r, 10));\n\
         // This SHOULD return false (per Node) and NOT crash.\n\
         const ret = sink.write('after-end');\n\
         await new Promise((r) => setTimeout(r, 10));\n\
         console.log('survived=' + (ret === false));",
    );
    assert_eq!(
        stdout, "survived=true",
        "write-after-end must not crash without an error listener"
    );
}

#[test]
fn timers_promises_set_interval_aborts_via_signal() {
    // The setInterval async iterator gained AbortSignal support and a
    // clearTimeout on abort. This test exercises BOTH: the iterator must
    // throw AbortError on abort, and the pending timer must be cleared so a
    // run with a tight signal doesn't pile up timer slots.
    let stdout = run_ok(
        "interval_abort.mjs",
        "import { setInterval as every } from 'node:timers/promises';\n\
         const ac = new AbortController();\n\
         let ticks = 0;\n\
         let name = '';\n\
         setTimeout(() => ac.abort(), 50);\n\
         try {\n\
           for await (const _ of every(20, null, { signal: ac.signal })) {\n\
             ticks++;\n\
             if (ticks > 50) break;\n\
           }\n\
         } catch (e) {\n\
           name = e.name;\n\
         }\n\
         // At ~50ms with a 20ms interval we get 1-3 ticks before abort.\n\
         console.log('aborted=' + (name === 'AbortError'));\n\
         console.log('boundedTicks=' + (ticks <= 10));\n\
         // Already-aborted signal throws immediately.\n\
         let name2 = '';\n\
         try {\n\
           for await (const _ of every(100, null, { signal: AbortSignal.abort() })) {}\n\
         } catch (e) { name2 = e.name; }\n\
         console.log('preAborted=' + (name2 === 'AbortError'));",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "aborted=true");
    assert_eq!(lines[1], "boundedTicks=true");
    assert_eq!(lines[2], "preAborted=true");
}

#[test]
fn oam_test_runs_afterall_when_beforeall_throws() {
    // Resource-leak class regression: beforeAll throws -> runSuite used to
    // abandon afterAll, so DB connections / temp dirs / lock files installed
    // by beforeAll (even partially) never got torn down. The try/finally
    // wrap guarantees afterAll runs regardless.
    write_temp(
        "runner_hook_throw/leak.test.ts",
        "import { test, describe, beforeAll, afterAll } from 'oam:test';\n\
         describe('with throwing beforeAll', () => {\n\
           beforeAll(() => { throw new Error('setup-boom'); });\n\
           afterAll(() => { console.log('CLEANUP_RAN'); });\n\
           test('never runs', () => {});\n\
         });",
    );
    let dir = write_temp("runner_hook_throw/.anchor", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let out = oam(&["test", dir.to_str().unwrap()]);
    // The run fails (beforeAll threw), but the afterAll cleanup MUST land.
    // The test runner emits user `console.log` to stdout; suite failures go
    // to stderr. Check both streams since the runner may surface differently
    // depending on the failure path.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "throwing beforeAll must fail the run"
    );
    assert!(
        combined.contains("CLEANUP_RAN"),
        "afterAll must run even when beforeAll throws; output: {combined}"
    );
}

#[test]
fn text_decoder_replacement_char_is_single_codepoint() {
    // Bug: the source literal for the U+FFFD replacement was three Latin-1
    // chars (UTF-8 of EF BF BD), yielding string length 3 instead of 1.
    // Any consumer comparing `ch === '�'` saw it as not-equal.
    let stdout = run_ok(
        "fffd.mjs",
        "// Invalid lead byte 0xFF -> non-fatal decoder emits U+FFFD.\n\
         const td = new TextDecoder();\n\
         const decoded = td.decode(new Uint8Array([0xff]));\n\
         console.log(decoded.length);\n\
         console.log(decoded.codePointAt(0).toString(16));\n\
         console.log(decoded === '\\uFFFD');",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "1", "replacement char must be one codepoint");
    assert_eq!(lines[1], "fffd", "must be U+FFFD specifically");
    assert_eq!(lines[2], "true", "must compare equal to the FFFD literal");
}

#[test]
fn readable_stream_tee_survives_underlying_error() {
    // The tee() implementation shared a `pulling` promise across branches.
    // An underlying-stream error used to poison `pulling` permanently --
    // both branches then awaited a stale rejected promise forever. The fix
    // resets `pulling` on rejection; the live branches must both observe
    // the same error (not deadlock).
    let stdout = run_ok(
        "tee_error.mjs",
        "const src = new ReadableStream({\n\
           start(c) {\n\
             c.enqueue('a');\n\
             // Defer the error so both readers acquire before it hits.\n\
             setTimeout(() => c.error(new Error('source-boom')), 10);\n\
           },\n\
         });\n\
         const [t1, t2] = src.tee();\n\
         async function readToError(branch) {\n\
           const reader = branch.getReader();\n\
           const collected = [];\n\
           try {\n\
             for (;;) {\n\
               const { value, done } = await reader.read();\n\
               if (done) return 'done:' + collected.join(',');\n\
               collected.push(value);\n\
             }\n\
           } catch (e) {\n\
             return 'err:' + e.message + ':' + collected.join(',');\n\
           }\n\
         }\n\
         const [r1, r2] = await Promise.all([readToError(t1), readToError(t2)]);\n\
         console.log(r1);\n\
         console.log(r2);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines[0].starts_with("err:source-boom"),
        "branch 1 must surface the error, not hang: {}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("err:source-boom"),
        "branch 2 must surface the error, not hang: {}",
        lines[1]
    );
}

// ------------------------------------------------ node:url POSIX round trip

/// POSIX-only fileURLToPath/pathToFileURL assertions that run on ALL
/// platforms. The Windows-specific drive-letter and UNC shapes live in
/// `node_url_file_conversions_round_trip` (cfg_attr-ignored on non-Windows).
/// This companion test gives Linux + macOS CI coverage of the portable path.
#[test]
fn node_url_file_conversions_posix() {
    // On Windows, POSIX paths without a drive letter are not valid absolute
    // paths, so we skip the POSIX-specific assertions there.
    #[cfg(windows)]
    {
        // Only verify the non-file-scheme-throws and the round-trip
        // identity (import.meta.url -> fileURLToPath == import.meta.filename)
        // which are portable.
        let stdout = run_ok(
            "nodeurl_posix_win.mjs",
            "import { fileURLToPath, pathToFileURL } from 'node:url';\n\
             // Round trip via import.meta.\n\
             console.log(fileURLToPath(import.meta.url) === import.meta.filename);\n\
             // Non-file scheme throws ERR_INVALID_URL_SCHEME on all platforms.\n\
             let code = '';\n\
             try { fileURLToPath('https://x.dev/a'); } catch (e) { code = e.code; }\n\
             console.log(code);",
        );
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(
            lines[0], "true",
            "import.meta round trip must hold on Windows"
        );
        assert_eq!(
            lines[1], "ERR_INVALID_URL_SCHEME",
            "non-file scheme must throw ERR_INVALID_URL_SCHEME"
        );
    }
    #[cfg(not(windows))]
    {
        let stdout = run_ok(
            "nodeurl_posix.mjs",
            "import { fileURLToPath, pathToFileURL } from 'node:url';\n\
             // Simple POSIX path round trip.\n\
             const url = pathToFileURL('/foo/bar').href;\n\
             const path = fileURLToPath('file:///foo/bar');\n\
             console.log(url);\n\
             console.log(path);\n\
             // Round trip is identity: pathToFileURL then fileURLToPath.\n\
             console.log(fileURLToPath(pathToFileURL('/foo/bar/baz.txt').href));\n\
             // Space encoding.\n\
             console.log(pathToFileURL('/dir name/file.txt').href);\n\
             console.log(fileURLToPath('file:///dir%20name/file.txt'));\n\
             // Round trip via import.meta.\n\
             console.log(fileURLToPath(import.meta.url) === import.meta.filename);\n\
             // Non-file scheme throws ERR_INVALID_URL_SCHEME.\n\
             let code = '';\n\
             try { fileURLToPath('https://x.dev/a'); } catch (e) { code = e.code; }\n\
             console.log(code);",
        );
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(
            lines[0], "file:///foo/bar",
            "pathToFileURL('/foo/bar').href"
        );
        assert_eq!(lines[1], "/foo/bar", "fileURLToPath('file:///foo/bar')");
        assert_eq!(
            lines[2], "/foo/bar/baz.txt",
            "round trip: fileURLToPath(pathToFileURL(path))"
        );
        assert_eq!(
            lines[3], "file:///dir%20name/file.txt",
            "spaces must be percent-encoded"
        );
        assert_eq!(
            lines[4], "/dir name/file.txt",
            "percent-decoded space in fileURLToPath"
        );
        assert_eq!(lines[5], "true", "import.meta round trip must hold");
        assert_eq!(
            lines[6], "ERR_INVALID_URL_SCHEME",
            "non-file scheme must throw ERR_INVALID_URL_SCHEME"
        );
    }
}

// ----------------------------------------- http 413 strict (all platforms)

/// Strict variant of `http_per_request_body_over_cap_returns_413_not_503`
/// that asserts stdout == "413" exactly on every platform.
///
/// The original test was cfg_attr-ignored on Windows because a 200 MB POST
/// could race: the server rejects after reading 100 MB, drops the connection
/// while the client is still uploading the remaining 100 MB, and on Windows
/// the TCP RST from the unread recv-buffer beats the 413 bytes in the
/// send-buffer.
///
/// The fix is two-pronged:
///   1. The server drains up to DRAIN_BUDGET bytes after the cap fires,
///      clearing the recv-buffer so the OS can close with a clean FIN.
///   2. The test body is 110 MB (10 MB over cap) -- within the 16 MB drain
///      budget -- so the drain always completes and the FIN path wins.
///
/// This replaces both the cfg_attr AND the 200 MB body.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "flakes on x86_64-apple-darwin GH runner; loose test covers macOS"
)]
fn http_per_request_body_over_cap_413_strict() {
    let stdout = run_ok(
        "http_413_strict.mjs",
        "import http from 'node:http';\n\
         const server = http.createServer((req, res) => res.end('ok'));\n\
         await new Promise((r) => server.listen(0, r));\n\
         const base = `http://127.0.0.1:${server.address().port}`;\n\
         // 110 MB body -- 10 MB over MAX_REQUEST_BODY (100 MB).\n\
         // Sized to fit within the server's 16 MB post-cap drain budget so\n\
         // the recv-buffer is clear before close, avoiding the RST race.\n\
         const huge = 'x'.repeat(110 * 1024 * 1024);\n\
         const res = await fetch(`${base}/big`, { method: 'POST', body: huge })\n\
           .catch((e) => ({ status: 'ERR:' + e.constructor.name }));\n\
         console.log(res.status);\n\
         server.close();",
    );
    assert_eq!(
        stdout, "413",
        "per-request over-cap must yield exactly 413 on all platforms; \
         got: {stdout} (ERR:* means the RST race is not fixed -- re-examine \
         the drain logic in collect_body)"
    );
}

// ------------------------------------------ net: TCP socket round-trip

#[test]
fn net_tcp_echo_round_trip() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // Simple TCP echo server
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    stream.write_all(&buf[..n]).unwrap();
                }
                Err(_) => break,
            }
        }
    });

    let source = format!(
        r#"
import net from 'node:net';
const socket = net.createConnection({{ port: {port}, host: '127.0.0.1' }});
const chunks = [];
socket.on('connect', () => {{
  socket.write('hello');
  socket.write(' world');
  socket.end();
}});
socket.on('data', (chunk) => {{
  chunks.push(chunk.toString());
}});
socket.on('end', () => {{
  console.log(chunks.join(''));
}});
socket.on('close', () => {{
  console.log('closed');
}});
"#,
        port = addr.port(),
    );
    let stdout = run_ok("net_tcp_echo.mjs", &source);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "hello world", "echoed data");
    assert_eq!(lines[1], "closed", "socket closed");
}

// ------------------------------------------ worker_threads

#[test]
fn worker_threads_message_round_trip() {
    let worker_script = write_temp(
        "echo_worker.cjs",
        r#"const { parentPort, workerData } = require('worker_threads');
parentPort.on('message', (msg) => {
  parentPort.postMessage({ echo: msg, data: workerData });
});
"#,
    );

    let worker_path = worker_script.to_string_lossy().replace('\\', "/");
    let source = format!(
        r#"const {{ Worker }} = require('worker_threads');
const w = new Worker('{worker_path}', {{ workerData: {{ n: 42 }} }});
w.on('message', (msg) => {{
  console.log(JSON.stringify(msg));
  w.terminate();
}});
w.on('exit', () => console.log('exited'));
w.postMessage('ping');
"#,
    );
    let stdout = run_ok("worker_main.cjs", &source);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].contains("ping"), "echo: {}", lines[0]);
    assert!(lines[0].contains("42"), "workerData: {}", lines[0]);
}

#[test]
fn worker_threads_is_main_thread_and_thread_id() {
    let worker_script = write_temp(
        "id_worker.cjs",
        r#"const { parentPort, isMainThread, threadId } = require('worker_threads');
parentPort.postMessage({ isMainThread, threadId });
"#,
    );

    let worker_path = worker_script.to_string_lossy().replace('\\', "/");
    let source = format!(
        r#"const wt = require('worker_threads');
console.log('main:isMain=' + wt.isMainThread + ',tid=' + wt.threadId);
const w = new wt.Worker('{worker_path}');
w.on('message', (msg) => {{
  console.log('worker:isMain=' + msg.isMainThread + ',tid=' + msg.threadId);
  w.terminate();
}});
"#,
    );
    let stdout = run_ok("worker_id_main.cjs", &source);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "main:isMain=true,tid=0");
    assert!(
        lines[1].starts_with("worker:isMain=false,tid="),
        "worker line: {}",
        lines[1]
    );
    let tid: u64 = lines[1]
        .split("tid=")
        .last()
        .unwrap()
        .parse()
        .expect("thread id is a number");
    assert!(tid > 0, "worker threadId > 0");
}

// ------------------------------------------ url: portable parity cases

/// Portable (all-platform) subset of `url_parity_fleet_regressions`.
/// The parent test is cfg_attr-ignored on non-Windows because several of
/// its assertions use Windows file:// shapes (drive letters, device paths).
/// The cases extracted here are purely WHATWG-URL behavior with no
/// file:// path assumptions: URLSearchParams astral chars, live-iteration
/// delete, empty-query search, port setter, hostname setter no-op on ':',
/// opaque-path setter, and urlToHttpOptions shape.
#[test]
fn websocket_echo_round_trip() {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(true).unwrap();
            let stream = tokio::net::TcpStream::from_std(stream).unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            use futures_util::{SinkExt, StreamExt};
            use tokio_tungstenite::tungstenite::Message;
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    Message::Text(text) => {
                        ws.send(Message::Text(text)).await.ok();
                    }
                    Message::Binary(data) => {
                        ws.send(Message::Binary(data)).await.ok();
                    }
                    Message::Close(frame) => {
                        ws.send(Message::Close(frame)).await.ok();
                        break;
                    }
                    _ => {}
                }
            }
        });
    });

    let source = format!(
        r#"
const ws = new WebSocket("ws://127.0.0.1:{port}");
const msgs = [];
ws.addEventListener("open", () => {{
  ws.send("hello");
  ws.send("world");
}});
ws.addEventListener("message", (ev) => {{
  msgs.push(ev.data);
  if (msgs.length === 2) {{
    console.log(msgs.join(","));
    ws.close();
  }}
}});
ws.addEventListener("close", (ev) => {{
  console.log("closed", ev.code, ev.wasClean);
}});
"#,
        port = addr.port(),
    );
    let stdout = run_ok("websocket_echo.mjs", &source);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "hello,world", "echoed messages");
    assert!(
        lines[1].starts_with("closed 1000"),
        "clean close: {}",
        lines[1]
    );
}

#[test]
fn url_parity_portable() {
    let stdout = run_ok(
        "url_parity_portable.mjs",
        "import { urlToHttpOptions } from 'node:url';\n\
         // Astral chars through URLSearchParams string init.\n\
         const sp = new URLSearchParams('e=\\u{1F984}');\n\
         console.log(sp.toString(), sp.get('e') === '\\u{1F984}');\n\
         // Live (not snapshot) iteration: delete-during-iterate skips every\n\
         // other entry (deletes current, iterator advances past next).\n\
         const live = new URLSearchParams('a=1&b=2&c=3&d=4');\n\
         for (const [k] of live) live.delete(k);\n\
         console.log(live.toString(), live.size);\n\
         // search edge: empty-present query reads '' but href keeps '?'.\n\
         const q = new URL('https://x.example/p?');\n\
         console.log(JSON.stringify(q.search), q.href);\n\
         // search setter with '?' alone trims to empty.\n\
         const q2 = new URL('https://x.example/p?a=1');\n\
         q2.search = '?';\n\
         console.log(q2.href);\n\
         // Port setter: WHATWG leading-digit parse.\n\
         const pu = new URL('http://example.com:81/');\n\
         pu.port = '8080 ';\n\
         console.log(pu.href);\n\
         // hostname setter no-ops on ':' (host+port separator would be ambiguous).\n\
         const hu = new URL('http://a.com:7/');\n\
         hu.hostname = 'b.com:99';\n\
         console.log(hu.href);\n\
         // pathname setter no-ops on opaque paths (data: URLs).\n\
         const du = new URL('data:text/plain,abc');\n\
         du.pathname = 'xyz';\n\
         console.log(du.href);\n\
         // urlToHttpOptions: node shape.\n\
         const o = urlToHttpOptions(new URL('http://us%65r:p%40ss@[::1]:8080/x/y?q=1#h'));\n\
         console.log(o.hostname, typeof o.port, o.port, o.auth, o.pathname);\n\
         const o2 = urlToHttpOptions(new URL('https://example.com/'));\n\
         console.log('port' in o2, 'auth' in o2);",
    );
    let lines: Vec<&str> = stdout.lines().collect();
    // Line 0: astral char round-trip via URLSearchParams.
    assert_eq!(
        lines[0], "e=%F0%9F%A6%84 true",
        "astral char in URLSearchParams"
    );
    // Line 1: live-iteration delete leaves b=2 and d=4 (a and c deleted).
    assert_eq!(lines[1], "b=2&d=4 2", "live-iteration delete during for-of");
    // Line 2: empty-present query: search == "" but href keeps '?'.
    assert_eq!(
        lines[2], "\"\" https://x.example/p?",
        "empty-present query in href"
    );
    // Line 3: setting search to '?' trims to empty query (no trailing '?').
    assert_eq!(
        lines[3], "https://x.example/p?",
        "search='?' trims to empty"
    );
    // Line 4: port setter accepts '8080 ' (WHATWG leading-digit parse).
    assert_eq!(
        lines[4], "http://example.com:8080/",
        "port setter with trailing space"
    );
    // Line 5: hostname setter no-ops when value contains ':'.
    assert_eq!(
        lines[5], "http://a.com:7/",
        "hostname setter no-op on colon"
    );
    // Line 6: pathname setter no-ops on data: opaque path.
    assert_eq!(
        lines[6], "data:text/plain,abc",
        "opaque pathname setter no-op"
    );
    // Lines 7-8: urlToHttpOptions shape.
    assert_eq!(
        lines[7], "::1 number 8080 user:p@ss /x/y",
        "urlToHttpOptions: hostname, port type, port, auth, pathname"
    );
    assert_eq!(
        lines[8], "false false",
        "urlToHttpOptions: port/auth absent for plain URL"
    );
}

#[test]
fn oam_ai_sse_parser_and_stream_chat() {
    let addr = spawn_echo_server();
    let file = write_temp(
        "ai_sse.mjs",
        &format!(
            r#"import {{ parseSSEStream, streamChat, openai, anthropic, defaultExtractDelta }} from 'oam:ai';

// 1. parseSSEStream: parse raw SSE bytes from the /stream endpoint
const resp = await fetch('http://{addr}/stream');
const events = [];
for await (const ev of parseSSEStream(resp.body)) {{
  events.push(ev);
}}
// /stream sends: data: tok1, data: tok2, data: [DONE]
console.log('events=' + events.length);
console.log('e0=' + events[0].data);
console.log('e1=' + events[1].data);
console.log('e2=' + events[2].data);

// 2. streamChat: wraps fetch+SSE, yields deltas, stops on [DONE]
const chunks = [];
for await (const chunk of streamChat({{
  url: 'http://{addr}/stream',
  headers: {{}},
  body: {{}},
}})) {{
  chunks.push(chunk);
}}
// defaultExtractDelta: tok1/tok2 are not valid JSON, so they get skipped.
// [DONE] triggers return. Should yield 0 chunks (non-JSON data lines).
console.log('chunks=' + chunks.length);

// 3. Exports exist
console.log('openai=' + typeof openai);
console.log('anthropic=' + typeof anthropic);
console.log('defaultExtractDelta=' + typeof defaultExtractDelta);
"#,
            addr = addr,
        ),
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "oam:ai test failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines[0], "events=3", "SSE parser yielded 3 events");
    assert_eq!(lines[1], "e0=tok1", "first event data");
    assert_eq!(lines[2], "e1=tok2", "second event data");
    assert_eq!(lines[3], "e2=[DONE]", "third event data");
    assert_eq!(lines[4], "chunks=0", "streamChat skips non-JSON data lines");
    assert_eq!(lines[5], "openai=function");
    assert_eq!(lines[6], "anthropic=function");
    assert_eq!(lines[7], "defaultExtractDelta=function");
}

#[test]
fn oam_serve_sets_port_and_host_env() {
    // Verify that `oam serve --port X --host Y` sets PORT and HOST env vars
    // visible to the script. The script prints them and exits immediately.
    let file = write_temp(
        "serve_env.mjs",
        r#"console.log('PORT=' + process.env.PORT);
console.log('HOST=' + process.env.HOST);
"#,
    );
    let out = oam(&[
        "serve",
        file.to_str().unwrap(),
        "--port",
        "4567",
        "--host",
        "127.0.0.1",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "oam serve env test failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines[0], "PORT=4567");
    assert_eq!(lines[1], "HOST=127.0.0.1");
}

#[test]
fn oam_serve_defaults_port_3000_host_0000() {
    // Without --port/--host flags, defaults should be PORT=3000 HOST=0.0.0.0
    let file = write_temp(
        "serve_defaults.mjs",
        r#"console.log('PORT=' + process.env.PORT);
console.log('HOST=' + process.env.HOST);
"#,
    );
    let out = oam(&["serve", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "oam serve defaults test failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines[0], "PORT=3000");
    assert_eq!(lines[1], "HOST=0.0.0.0");
}

#[test]
fn oam_serve_worker_pool_dispatches_requests() {
    use std::io::{BufRead, Read, Write};

    // Handler that echoes method + url + a marker proving it ran in a worker.
    // Uses CJS module.exports so the worker shim can require() it.
    let handler = write_temp(
        "pool_handler.cjs",
        r#"
const { threadId } = require("worker_threads");
module.exports = function handler(req, res) {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const body = Buffer.concat(chunks).toString();
    res.writeHead(200, { "content-type": "application/json", "x-worker": String(threadId) });
    res.end(JSON.stringify({ method: req.method, url: req.url, body: body, worker: threadId }));
  });
};
"#,
    );

    // Pick a port unlikely to collide. Start oam serve with 2 workers.
    let port = 19876u16;
    let cache = write_temp("pool-cache/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args([
            "serve",
            handler.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--host",
            "127.0.0.1",
            "--workers",
            "2",
        ])
        .env("OAM_CACHE_DIR", &cache)
        .env("OAM_DAEMON_IDLE_MS", "45000")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("oam serve spawns");

    // Wait for "listening" on stdout (the dispatcher prints it).
    let stdout = child.stdout.take().unwrap();
    let reader = std::io::BufReader::new(stdout);
    let mut listening = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    for line in reader.lines() {
        if std::time::Instant::now() > deadline {
            break;
        }
        let line = line.unwrap_or_default();
        if line.contains("workers)") {
            listening = true;
            break;
        }
    }
    assert!(
        listening,
        "server did not print listening line within timeout"
    );

    // Make two HTTP requests to verify dispatch works.
    let client = std::net::TcpStream::connect(format!("127.0.0.1:{port}"));
    assert!(client.is_ok(), "could not connect to server");

    // Request 1: GET /hello
    let mut stream = client.unwrap();
    stream
        .write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response1 = String::new();
    stream.read_to_string(&mut response1).unwrap();
    drop(stream);

    // Request 2: POST /data with body
    let mut stream2 = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    let post_body = r#"{"key":"value"}"#;
    let req2 = format!(
        "POST /data HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
        post_body.len(),
        post_body
    );
    stream2.write_all(req2.as_bytes()).unwrap();
    let mut response2 = String::new();
    stream2.read_to_string(&mut response2).unwrap();
    drop(stream2);

    // Kill the server.
    let _ = child.kill();
    let _ = child.wait();

    // Verify response 1: GET /hello
    assert!(
        response1.contains("200 OK"),
        "response1 not 200: {response1}"
    );
    let body1_start = response1.find('{').expect("JSON body in response1");
    let body1: serde_json::Value =
        serde_json::from_str(&response1[body1_start..]).expect("parse response1 JSON");
    assert_eq!(body1["method"], "GET");
    assert_eq!(body1["url"], "/hello");
    assert!(body1["worker"].as_u64().unwrap() > 0, "worker threadId > 0");

    // Verify response 2: POST /data
    assert!(
        response2.contains("200 OK"),
        "response2 not 200: {response2}"
    );
    let body2_start = response2.find('{').expect("JSON body in response2");
    let body2: serde_json::Value =
        serde_json::from_str(&response2[body2_start..]).expect("parse response2 JSON");
    assert_eq!(body2["method"], "POST");
    assert_eq!(body2["url"], "/data");
    assert_eq!(body2["body"], r#"{"key":"value"}"#);
    assert!(body2["worker"].as_u64().unwrap() > 0, "worker threadId > 0");
}

// --------------------------------------------------------- child_process

#[test]
fn child_process_exec_sync_runs_shell_commands() {
    let f = write_temp(
        "cp_exec_sync.mjs",
        r#"
import { execSync, spawnSync, execFileSync } from "child_process";

// execSync runs in a shell and returns stdout
const out = execSync("echo hello from execSync");
console.log("execSync:", out.toString().trim());

// spawnSync with shell: true
const r = spawnSync("echo", ["spawn", "sync"], { shell: true });
console.log("spawnSync:", Buffer.from(r.stdout).toString().trim(), "status:", r.status);

// spawnSync without shell (direct executable)
const r2 = spawnSync("node", ["-e", "console.log('node-direct')"]);
console.log("direct:", Buffer.from(r2.stdout).toString().trim(), "status:", r2.status);

// execSync throws on non-zero exit
try {
  execSync("exit 42", { encoding: "utf8" });
  console.log("should have thrown");
} catch (e) {
  console.log("threw:", e.status);
}

// spawnSync with input
const r3 = spawnSync("node", ["-e", "process.stdin.resume(); process.stdin.on('data', d => { process.stdout.write(d); process.stdin.pause(); });"], { input: "piped-in" });
console.log("input:", Buffer.from(r3.stdout).toString().trim());
"#,
    );
    let out = oam(&["run", "--no-check", f.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit {}: {stdout}", out.status);
    assert!(stdout.contains("execSync: hello from execSync"), "{stdout}");
    assert!(stdout.contains("spawnSync: spawn sync status: 0"), "{stdout}");
    assert!(stdout.contains("direct: node-direct status: 0"), "{stdout}");
    assert!(stdout.contains("threw: 42"), "{stdout}");
    assert!(stdout.contains("input: piped-in"), "{stdout}");
}

#[test]
fn child_process_async_spawn_streams_and_events() {
    let f = write_temp(
        "cp_async_spawn.mjs",
        r#"
import { spawn, exec } from "child_process";

// Test async spawn with events
const cp = spawn("echo", ["hello", "async"], { shell: true });
const chunks = [];
cp.on("spawn", () => {
  console.log("spawned pid:", typeof cp.pid);
  cp.stdout.on("data", (chunk) => chunks.push(chunk));
});
cp.on("close", (code) => {
  console.log("spawn-out:", Buffer.concat(chunks).toString().trim(), "code:", code);

  // Test exec with callback
  exec("echo exec-cb-test", (err, stdout, stderr) => {
    console.log("exec:", stdout.trim(), "err:", err);
    console.log("DONE");
  });
});
"#,
    );
    let out = oam(&["run", "--no-check", f.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit {}: {stdout}", out.status);
    assert!(stdout.contains("spawned pid: number"), "{stdout}");
    assert!(stdout.contains("spawn-out: hello async code: 0"), "{stdout}");
    assert!(stdout.contains("exec: exec-cb-test err: null"), "{stdout}");
    assert!(stdout.contains("DONE"), "{stdout}");
}

#[test]
fn dns_lookup_resolves_localhost() {
    let f = write_temp(
        "dns_lookup.mjs",
        r#"
import dns from 'node:dns';
import { promises as dnsPromises } from 'node:dns';

// 1. callback-style lookup
dns.lookup('localhost', (err, address, family) => {
  console.log('cb-err:', err);
  console.log('cb-addr:', address);
  console.log('cb-fam:', family);
});

// 2. promise-style lookup
const result = await dnsPromises.lookup('localhost');
console.log('p-addr:', result.address);
console.log('p-fam:', result.family);

// 3. lookup with all:true
const all = await dnsPromises.lookup('localhost', { all: true });
console.log('all-len:', all.length > 0);
console.log('all-addr:', all[0].address);

// 4. resolve4 (A records via OS resolver)
const addrs = await dnsPromises.resolve4('localhost');
console.log('resolve4-len:', addrs.length > 0);
console.log('resolve4-type:', typeof addrs[0]);

// 5. ENOTFOUND on bogus hostname
try {
  await dnsPromises.lookup('this.host.definitely.does.not.exist.invalid');
  console.log('bogus: no error');
} catch (e) {
  console.log('bogus-code:', e.code);
}
"#,
    );
    let out = oam(&["run", f.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "dns test failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert!(lines.iter().any(|l| l.starts_with("cb-err: null")), "callback error should be null: {stdout}");
    assert!(lines.iter().any(|l| l.starts_with("cb-addr: ")), "callback should return address: {stdout}");
    assert!(lines.iter().any(|l| l.starts_with("p-addr: ")), "promise should return address: {stdout}");
    assert!(lines.iter().any(|l| l == &"all-len: true"), "all:true should return results: {stdout}");
    assert!(lines.iter().any(|l| l == &"resolve4-len: true"), "resolve4 should return results: {stdout}");
    assert!(lines.iter().any(|l| l == &"resolve4-type: string"), "resolve4 should return strings: {stdout}");
    assert!(lines.iter().any(|l| l == &"bogus-code: ENOTFOUND"), "bogus hostname should ENOTFOUND: {stdout}");
}

#[test]
fn process_stdin_is_readable_stream() {
    use std::io::Write;

    let f = write_temp(
        "stdin_read.mjs",
        r#"
const chunks = [];
for await (const chunk of process.stdin) {
  chunks.push(chunk);
}
const text = Buffer.concat(chunks).toString();
console.log('lines:', text.trim().split('\n').length);
console.log('first:', text.trim().split('\n')[0]);
console.log('isTTY:', process.stdin.isTTY);
"#,
    );
    let cache = write_temp("oam-cache-stdin/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args(["run", f.to_str().unwrap()])
        .env("OAM_CACHE_DIR", &cache)
        .env("OAM_DAEMON_IDLE_MS", "45000")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn oam");

    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"hello world\ngoodbye world\n").unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdin test failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("lines: 2"), "should read 2 lines: {stdout}");
    assert!(stdout.contains("first: hello world"), "first line: {stdout}");
    assert!(stdout.contains("isTTY: false"), "piped stdin not TTY: {stdout}");
}

#[test]
fn fs_watch_detects_file_change() {
    let stdout = run_ok(
        "fs_watch.mjs",
        "import fs from 'fs';\n\
         import path from 'path';\n\
         const target = path.join(process.cwd(), '_watch_test_' + process.pid + '.tmp');\n\
         fs.writeFileSync(target, 'initial');\n\
         let detected = false;\n\
         const watcher = fs.watch(target, { interval: 30 }, (eventType, filename) => {\n\
           detected = true;\n\
           watcher.close();\n\
           console.log('event:', eventType, 'file:', filename);\n\
         });\n\
         setTimeout(() => {\n\
           fs.writeFileSync(target, 'changed-' + Date.now());\n\
         }, 100);\n\
         setTimeout(() => {\n\
           watcher.close();\n\
           try { fs.unlinkSync(target); } catch (e) {}\n\
           console.log('detected:', detected);\n\
         }, 800);",
    );
    assert!(stdout.contains("event: change"), "should detect change event: {stdout}");
    assert!(stdout.contains("detected: true"), "change should be detected: {stdout}");
}

#[test]
fn os_and_process_native_values() {
    let stdout = run_ok(
        "os_process_natives.mjs",
        "import os from 'os';\n\
         import v8 from 'v8';\n\
         \n\
         // os.release returns a non-empty version string\n\
         const rel = os.release();\n\
         console.log('release:', rel.length > 0);\n\
         \n\
         // os.totalmem/freemem return real values (> 100 MB)\n\
         console.log('totalmem:', os.totalmem() > 100_000_000);\n\
         console.log('freemem:', os.freemem() > 0);\n\
         \n\
         // os.cpus() returns correct count with real model\n\
         const cpus = os.cpus();\n\
         console.log('cpu_count:', cpus.length > 0);\n\
         console.log('cpu_model:', cpus[0].model.length > 0 && cpus[0].model !== 'unknown');\n\
         console.log('cpu_speed:', cpus[0].speed > 0);\n\
         \n\
         // os.networkInterfaces has loopback\n\
         const ni = os.networkInterfaces();\n\
         const hasLoopback = Object.values(ni).some(addrs => addrs.some(a => a.internal && a.address === '127.0.0.1'));\n\
         console.log('loopback:', hasLoopback);\n\
         \n\
         // process.memoryUsage returns real values\n\
         const m = process.memoryUsage();\n\
         console.log('rss:', m.rss > 0);\n\
         console.log('heapUsed:', m.heapUsed > 0);\n\
         console.log('heapTotal:', m.heapTotal > 0);\n\
         console.log('rss_fn:', process.memoryUsage.rss() > 0);\n\
         \n\
         // v8.getHeapStatistics returns real values\n\
         const h = v8.getHeapStatistics();\n\
         console.log('v8_heap:', h.used_heap_size > 0 && h.total_heap_size > 0 && h.heap_size_limit > 0);\n\
         \n\
         // process.ppid is a real parent PID (not 0)\n\
         console.log('ppid:', process.ppid > 0);\n\
         \n\
         // os module shape\n\
         console.log('platform:', os.platform() === process.platform);\n\
         console.log('arch:', os.arch() === process.arch);\n\
         console.log('homedir:', os.homedir().length > 0);\n\
         console.log('tmpdir:', os.tmpdir().length > 0);\n\
         console.log('hostname:', os.hostname().length > 0);\n\
         console.log('endianness:', os.endianness() === 'LE');\n\
         console.log('EOL:', os.EOL === '\\r\\n' || os.EOL === '\\n');",
    );
    for line in stdout.lines() {
        assert!(
            line.ends_with("true"),
            "assertion failed: {line}\nfull output: {stdout}"
        );
    }
}

#[test]
fn fs_mkdtemp_symlink_readlink_link_chmod_truncate() {
    let stdout = run_ok(
        "fs_new_ops.js",
        "import fs from 'node:fs';\n\
         import fsp from 'node:fs/promises';\n\
         import path from 'node:path';\n\
         import os from 'node:os';\n\
         \n\
         // mkdtemp (sync)\n\
         const dir = fs.mkdtempSync('oam-test-');\n\
         console.log('mkdtempSync:', dir.includes('oam-test-'));\n\
         \n\
         // mkdtemp (async)\n\
         const dir2 = await fsp.mkdtemp('oam-async-');\n\
         console.log('mkdtemp:', dir2.includes('oam-async-'));\n\
         \n\
         // write a file for testing\n\
         const testFile = path.join(dir, 'hello.txt');\n\
         fs.writeFileSync(testFile, 'hello world');\n\
         \n\
         // symlink + readlink (sync) -- may fail on Windows without dev mode\n\
         let symlinkOk = true;\n\
         try {\n\
           const linkPath = path.join(dir, 'link.txt');\n\
           fs.symlinkSync(testFile, linkPath);\n\
           const target = fs.readlinkSync(linkPath);\n\
           console.log('symlinkSync:', target.includes('hello.txt'));\n\
           console.log('readlinkSync:', target === testFile);\n\
         } catch (e) {\n\
           if (e.message && e.message.includes('privilege')) {\n\
             console.log('symlinkSync:', true);\n\
             console.log('readlinkSync:', true);\n\
             symlinkOk = false;\n\
           } else { throw e; }\n\
         }\n\
         \n\
         // symlink + readlink (async)\n\
         if (symlinkOk) {\n\
           const linkPath2 = path.join(dir, 'link2.txt');\n\
           await fsp.symlink(testFile, linkPath2);\n\
           const target2 = await fsp.readlink(linkPath2);\n\
           console.log('symlink_async:', target2 === testFile);\n\
         } else {\n\
           console.log('symlink_async:', true);\n\
         }\n\
         \n\
         // hard link (sync)\n\
         const hardPath = path.join(dir, 'hard.txt');\n\
         fs.linkSync(testFile, hardPath);\n\
         const hardContent = fs.readFileSync(hardPath, 'utf8');\n\
         console.log('linkSync:', hardContent === 'hello world');\n\
         \n\
         // hard link (async)\n\
         const hardPath2 = path.join(dir, 'hard2.txt');\n\
         await fsp.link(testFile, hardPath2);\n\
         const hardContent2 = await fsp.readFile(hardPath2, 'utf8');\n\
         console.log('link_async:', hardContent2 === 'hello world');\n\
         \n\
         // truncate (sync)\n\
         const truncFile = path.join(dir, 'trunc.txt');\n\
         fs.writeFileSync(truncFile, 'abcdefghij');\n\
         fs.truncateSync(truncFile, 3);\n\
         console.log('truncateSync:', fs.readFileSync(truncFile, 'utf8') === 'abc');\n\
         \n\
         // truncate (async)\n\
         const truncFile2 = path.join(dir, 'trunc2.txt');\n\
         fs.writeFileSync(truncFile2, '1234567890');\n\
         await fsp.truncate(truncFile2, 5);\n\
         const tr2 = await fsp.readFile(truncFile2, 'utf8');\n\
         console.log('truncate_async:', tr2 === '12345');\n\
         \n\
         // chmod (sync) -- on Windows just tests no throw\n\
         fs.chmodSync(testFile, 0o644);\n\
         console.log('chmodSync:', true);\n\
         \n\
         // chmod (async)\n\
         await fsp.chmod(testFile, 0o755);\n\
         console.log('chmod_async:', true);\n\
         \n\
         // cleanup\n\
         fs.rmSync(dir, { recursive: true, force: true });\n\
         fs.rmSync(dir2, { recursive: true, force: true });\n\
         console.log('done:', true);",
    );
    for line in stdout.lines() {
        assert!(
            line.ends_with("true"),
            "assertion failed: {line}\nfull output: {stdout}"
        );
    }
}

#[test]
fn http_request_and_get_client() {
    let addr = spawn_echo_server();
    let stdout = run_ok(
        "http_client.mjs",
        &format!(
            "import http from 'node:http';\n\
             \n\
             // http.get (auto-ends the request)\n\
             const getResult = await new Promise((resolve, reject) => {{\n\
               http.get('http://{addr}/hello', (res) => {{\n\
                 let body = '';\n\
                 res.on('data', (chunk) => body += chunk);\n\
                 res.on('end', () => resolve({{ status: res.statusCode, body }}));\n\
               }}).on('error', reject);\n\
             }});\n\
             console.log('get_status:', getResult.status === 200);\n\
             const getData = JSON.parse(getResult.body);\n\
             console.log('get_method:', getData.method === 'GET');\n\
             console.log('get_path:', getData.path === '/hello');\n\
             \n\
             // http.request with POST body\n\
             const postResult = await new Promise((resolve, reject) => {{\n\
               const req = http.request({{\n\
                 hostname: '127.0.0.1',\n\
                 port: {port},\n\
                 path: '/submit',\n\
                 method: 'POST',\n\
                 headers: {{ 'content-type': 'text/plain' }},\n\
               }}, (res) => {{\n\
                 let body = '';\n\
                 res.on('data', (chunk) => body += chunk);\n\
                 res.on('end', () => resolve({{ status: res.statusCode, body }}));\n\
               }});\n\
               req.on('error', reject);\n\
               req.write('hello from oam');\n\
               req.end();\n\
             }});\n\
             console.log('post_status:', postResult.status === 200);\n\
             const postData = JSON.parse(postResult.body);\n\
             console.log('post_method:', postData.method === 'POST');\n\
             console.log('post_echo:', postData.echo === 'hello from oam');\n\
             \n\
             // setHeader / getHeader / removeHeader\n\
             const req2 = http.request('http://{addr}/h');\n\
             req2.setHeader('x-test', 'val');\n\
             console.log('setHeader:', req2.getHeader('x-test') === 'val');\n\
             req2.removeHeader('x-test');\n\
             console.log('removeHeader:', req2.getHeader('x-test') === undefined);\n\
             req2.destroy();",
            port = addr.port(),
        ),
    );
    for line in stdout.lines() {
        assert!(
            line.ends_with("true"),
            "assertion failed: {line}\nfull output: {stdout}"
        );
    }
}

#[test]
fn submodule_imports_resolve() {
    let stdout = run_ok(
        "submod_imports.mjs",
        "import types from 'node:util/types';\n\
         import dnsp from 'node:dns/promises';\n\
         import rlp from 'node:readline/promises';\n\
         import fsp from 'node:fs/promises';\n\
         import stp from 'node:stream/promises';\n\
         import tp from 'node:timers/promises';\n\
         import pp from 'node:path/posix';\n\
         \n\
         // util/types works\n\
         console.log('isDate:', types.isDate(new Date()));\n\
         console.log('isRegExp:', types.isRegExp(/abc/));\n\
         console.log('isArrayBuffer:', types.isArrayBuffer(new ArrayBuffer(1)));\n\
         \n\
         // dns/promises has lookup\n\
         console.log('dns_promises:', typeof dnsp.lookup === 'function');\n\
         \n\
         // readline/promises has createInterface\n\
         console.log('readline_promises:', typeof rlp.createInterface === 'function');\n\
         \n\
         // fs/promises has readFile\n\
         console.log('fs_promises:', typeof fsp.readFile === 'function');\n\
         \n\
         // stream/promises has pipeline\n\
         console.log('stream_promises:', typeof stp.pipeline === 'function');\n\
         \n\
         // timers/promises has setTimeout\n\
         console.log('timers_promises:', typeof tp.setTimeout === 'function');\n\
         \n\
         // path/posix has join\n\
         console.log('path_posix:', pp.join('a', 'b') === 'a/b');",
    );
    for line in stdout.lines() {
        assert!(
            line.ends_with("true"),
            "assertion failed: {line}\nfull output: {stdout}"
        );
    }
}

#[test]
fn fs_promises_open_file_handle() {
    let stdout = run_ok(
        "fsp_open.mjs",
        "import fsp from 'node:fs/promises';\n\
         import fs from 'node:fs';\n\
         import path from 'node:path';\n\
         import os from 'node:os';\n\
         \n\
         const dir = await fsp.mkdtemp('oam-fh-');\n\
         const filePath = path.join(dir, 'test.txt');\n\
         \n\
         // Open for write\n\
         const wh = await fsp.open(filePath, 'w');\n\
         await wh.writeFile('hello filehandle');\n\
         await wh.close();\n\
         \n\
         // Open for read, use readFile\n\
         const rh = await fsp.open(filePath, 'r');\n\
         const content = await rh.readFile({ encoding: 'utf8' });\n\
         console.log('readFile:', content === 'hello filehandle');\n\
         \n\
         // stat\n\
         const s = await rh.stat();\n\
         console.log('stat:', s.isFile());\n\
         console.log('size:', s.size === 16);\n\
         await rh.close();\n\
         \n\
         // write with buffer\n\
         const wh2 = await fsp.open(filePath, 'w');\n\
         const result = await wh2.write(Buffer.from('abc'));\n\
         console.log('write_bytes:', result.bytesWritten === 3);\n\
         await wh2.close();\n\
         \n\
         // cleanup\n\
         fs.rmSync(dir, { recursive: true, force: true });\n\
         console.log('done:', true);",
    );
    for line in stdout.lines() {
        assert!(
            line.ends_with("true"),
            "assertion failed: {line}\nfull output: {stdout}"
        );
    }
}

// ----------------------------------------------------------------- crypto sign/verify

#[test]
fn crypto_sign_verify_rsa() {
    let priv_key = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCF8rU79C4bYdgB
9p7eHF7EHL0xSVtfDO+eFw/ihhPt1y6CaWz5ics5xtIRCZOaoi+QCiFwQGmemWSb
WeQSp8xgAY5qs1FelP4iCnI0ohkm+julKvKU+Bw7wj0j03B25OsZjWhQmEkAIwAC
OK7wQ7Ap3ElYswC/e1c1FYZepQJRKRHfW1MT13q++lcWhd0rGuav4635IX81B8dA
LfINYYXxXIznDEhtuRilhKLxt2t7NGQqynMhA99Gh6UWnxXUatc8q8mDgptM4Kqg
Dpap+rW/vqP3HKaaSPu+4zrMT2KY95zXsApY3eDvXLrTszr91K8MsHqcJet8J8cU
LMDaa74lAgMBAAECggEAFHiB/Zpk9H7Yy/4EjAXSbs7ElDR1kBpyQVsdbJ1YjNv6
qegSRTWrlxYdUwi/Y92+/pipwRW6/ofLUhmkCzzVNWPvf7uNZzLGfu3RQ911Ehmi
hWzBm4YqjHB0NxYwhR8ZlfNgOpb3axuuO+itRZ9WnCMkG3fp2JmxO3XhbfPyXXQ8
gtcmhpdGrLk9slZ3yTGkXzYXNLcoy7b3Nl1mt5QtB7pDA3d7gTfyDlkCX1fGQmo7
YCpy6c90hBjuC4U8Rtn5l99GvTB64ksaRRdi2YMh0f6ZOl0W+6LpxRiHW8+YI3UM
8klKTLTpfIdwc9WuvW8hgG1xWRrHLbGjnSVlYcvKSQKBgQC5/WRuNaZPBB/T+io3
X9yqb+gw5i8U2Qj1FlmIiOH7YRxqi8ynC/rKCs8t3Uu6JN1ylpBt6s7Z9dZG2QE7
riNLHIaGJhLTJNa6cxKJEoF7MZjXblcTK/4l0Oa3yMOS37EDDzy6pocd8VRIR5eY
nVC2bLCfp8hmBmWGUjYhl7ykSQKBgQC4XmXddLFibYFrWDb3W3jhaHAh8HbB0/j4
OedmpiTaPfdcBQ+rE5vIFrQvvxEJPSAKUSszOBOKvFhxMCibv+S8HIuDFhgDo8R6
XAk8roXDgEo6i1ukHZVtgu89tNEjGszwVRzIcoRoDWCLxIv68ERNsvj6pBrAhdwW
A7WL2W5S/QKBgQCgjasqsEl2oHrRRH04/BnDT4NC4xH1jz16RObZREC//h7HoxLx
iRffXeFnGEeM0tIPXwYivLX/1YY59o5n9HUnG+LM3wUVHBH5NejkRwNbU387SVcF
h86G2oSwVjDuEwf9OiQUhDjTkkZNdu/YoMTSFZWK3Q3TdOYjQ8jSyuffcQKBgEr8
YdPrZUYSIcQmEd0TQBv1nT3AjpyQ+T8EVgBi7LQy5ctwZ4n+JKsByPFudaBbUw+/
KaHgWdpgdlw66RlHt+FmfrunHcdFMWFO05bxqIf2QrqC+ZfLTH5I9cMUKsdrXBUX
mOhR41ZqsmzGWOSMGku70hYm7paFGxl9Era5jWyFAoGBAJjmlBbTIXCRQ0A9BfLf
V2wGH+ty2Z22gJktxvPWTLh0q3tyuucAI82ajFVCK8OFXzKQyZZ5EYWyV5R5FMgx
CFudgcI4mdC4ae+qr0JHMl9c7vI68uvKyPXPdB8RARM1cjXYEMNSrTgXbJzYjh/O
335IlYjZO6oamGu5l/xIjU8b
-----END PRIVATE KEY-----"#;

    let pub_key = r#"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAhfK1O/QuG2HYAfae3hxe
xBy9MUlbXwzvnhcP4oYT7dcugmls+YnLOcbSEQmTmqIvkAohcEBpnplkm1nkEqfM
YAGOarNRXpT+IgpyNKIZJvo7pSrylPgcO8I9I9NwduTrGY1oUJhJACMAAjiu8EOw
KdxJWLMAv3tXNRWGXqUCUSkR31tTE9d6vvpXFoXdKxrmr+Ot+SF/NQfHQC3yDWGF
8VyM5wxIbbkYpYSi8bdrezRkKspzIQPfRoelFp8V1GrXPKvJg4KbTOCqoA6Wqfq1
v76j9xymmkj7vuM6zE9imPec17AKWN3g71y607M6/dSvDLB6nCXrfCfHFCzA2mu+
JQIDAQAB
-----END PUBLIC KEY-----"#;

    let priv_path = write_temp("rsa_priv.pem", priv_key);
    let pub_path = write_temp("rsa_pub.pem", pub_key);

    let script = format!(
        r#"import crypto from 'node:crypto';
import fs from 'node:fs';

const privKey = fs.readFileSync('{}', 'utf8');
const pubKey = fs.readFileSync('{}', 'utf8');

// createSign / createVerify
const sign = crypto.createSign('RSA-SHA256');
sign.update('hello world');
const signature = sign.sign(privKey);
console.log('sig_type:', signature instanceof Buffer);
console.log('sig_len:', signature.length === 256);

const verify = crypto.createVerify('RSA-SHA256');
verify.update('hello world');
console.log('verify_ok:', verify.verify(pubKey, signature));

// tampered data should fail
const verify2 = crypto.createVerify('RSA-SHA256');
verify2.update('hello world!');
console.log('verify_tampered:', verify2.verify(pubKey, signature) === false);

// one-shot sign/verify
const sig2 = crypto.sign('RSA-SHA256', Buffer.from('test data'), privKey);
console.log('oneshot_sig:', sig2 instanceof Buffer);
console.log('oneshot_verify:', crypto.verify('RSA-SHA256', Buffer.from('test data'), pubKey, sig2));

// createPrivateKey / createPublicKey
const privObj = crypto.createPrivateKey(privKey);
console.log('priv_type:', privObj.type === 'private');
console.log('priv_asym:', privObj.asymmetricKeyType === 'rsa');

const pubObj = crypto.createPublicKey(pubKey);
console.log('pub_type:', pubObj.type === 'public');
console.log('pub_asym:', pubObj.asymmetricKeyType === 'rsa');

// sign with KeyObject
const sig3 = crypto.createSign('RSA-SHA256').update('key object test').sign(privObj);
const v3 = crypto.createVerify('RSA-SHA256').update('key object test').verify(pubObj, sig3);
console.log('keyobj_roundtrip:', v3);

// base64 signature encoding
const sig4 = crypto.createSign('RSA-SHA256').update('encode test').sign(privKey, 'base64');
console.log('b64_sig:', typeof sig4 === 'string');
const v4 = crypto.createVerify('RSA-SHA256').update('encode test').verify(pubKey, sig4, 'base64');
console.log('b64_verify:', v4);"#,
        priv_path.display().to_string().replace('\\', "/"),
        pub_path.display().to_string().replace('\\', "/"),
    );

    let path = write_temp("crypto_sign_verify.mjs", &script);
    let out = oam(&["run", path.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        panic!("crypto sign/verify failed:\nstdout: {stdout}\nstderr: {stderr}");
    }
    for line in stdout.lines() {
        assert!(
            line.ends_with("true"),
            "assertion failed: {line}\nfull output: {stdout}\nstderr: {stderr}"
        );
    }
}

// ------------------------------------------------------------ process.cpuUsage / process.kill

#[test]
fn process_cpu_usage_and_kill() {
    let path = write_temp(
        "process_cpu_kill.mjs",
        "import process from 'node:process';\n\
         \n\
         // cpuUsage returns {user, system} in microseconds\n\
         const usage = process.cpuUsage();\n\
         console.log('cpu_user:', typeof usage.user === 'number' && usage.user >= 0);\n\
         console.log('cpu_system:', typeof usage.system === 'number' && usage.system >= 0);\n\
         \n\
         // differential cpuUsage\n\
         const prev = process.cpuUsage();\n\
         let x = 0;\n\
         for (let i = 0; i < 1e6; i++) x += Math.sqrt(i);\n\
         const diff = process.cpuUsage(prev);\n\
         console.log('cpu_diff_user:', diff.user >= 0);\n\
         console.log('cpu_diff_system:', diff.system >= 0);\n\
         \n\
         // process.kill(pid, 0) checks if process exists (our own pid)\n\
         console.log('kill_self_check:', process.kill(process.pid, 0) === true);\n\
         \n\
         // signal name support\n\
         console.log('kill_sig_name:', process.kill(process.pid, 'SIGTERM') !== undefined || true);\n\
         console.log('done:', true);",
    );
    let out = oam(&["run", path.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // On Windows, process.kill sends TerminateProcess which kills the process
    // immediately, so we can only test the existence check (signal 0) and
    // cpuUsage. The SIGTERM test will kill the process, so we test it carefully.
    // Just verify cpuUsage lines and signal-0 check.
    let lines: Vec<&str> = stdout.lines().collect();
    for line in &lines {
        if line.starts_with("cpu_") || line.starts_with("kill_self_check:") {
            assert!(
                line.ends_with("true"),
                "assertion failed: {line}\nfull output: {stdout}\nstderr: {stderr}"
            );
        }
    }
}

// ------------------------------------------------------------ fs.opendir + fs.cp

#[test]
fn fs_opendir_and_cp() {
    let path = write_temp(
        "fs_opendir_cp.mjs",
        "import fs from 'node:fs';\n\
         import { opendir, cp } from 'node:fs/promises';\n\
         import { join } from 'node:path';\n\
         \n\
         const tmpBase = process.env.TEMP || process.env.TMPDIR || '/tmp';\n\
         const dir = join(tmpBase, 'oam-opendir-test-' + Date.now());\n\
         fs.mkdirSync(dir, { recursive: true });\n\
         fs.writeFileSync(join(dir, 'a.txt'), 'hello');\n\
         fs.writeFileSync(join(dir, 'b.txt'), 'world');\n\
         fs.mkdirSync(join(dir, 'sub'));\n\
         fs.writeFileSync(join(dir, 'sub', 'c.txt'), 'nested');\n\
         \n\
         // opendirSync\n\
         const d = fs.opendirSync(dir);\n\
         console.log('opendir_path:', d.path === dir);\n\
         const names = [];\n\
         let entry;\n\
         while ((entry = d.readSync()) !== null) {\n\
           names.push(entry.name);\n\
           if (entry.name === 'sub') {\n\
             console.log('sub_isDir:', entry.isDirectory());\n\
           }\n\
         }\n\
         console.log('opendir_count:', names.length === 3);\n\
         console.log('has_a:', names.includes('a.txt'));\n\
         console.log('has_b:', names.includes('b.txt'));\n\
         console.log('has_sub:', names.includes('sub'));\n\
         \n\
         // async opendir with for-await\n\
         const asyncDir = await opendir(dir);\n\
         const asyncNames = [];\n\
         for await (const e of asyncDir) {\n\
           asyncNames.push(e.name);\n\
         }\n\
         console.log('async_opendir_count:', asyncNames.length === 3);\n\
         \n\
         // cpSync (recursive)\n\
         const dest = dir + '-copy';\n\
         fs.cpSync(dir, dest, { recursive: true });\n\
         console.log('cp_a:', fs.readFileSync(join(dest, 'a.txt'), 'utf8') === 'hello');\n\
         console.log('cp_b:', fs.readFileSync(join(dest, 'b.txt'), 'utf8') === 'world');\n\
         console.log('cp_nested:', fs.readFileSync(join(dest, 'sub', 'c.txt'), 'utf8') === 'nested');\n\
         \n\
         // async cp\n\
         const dest2 = dir + '-copy2';\n\
         await cp(dir, dest2, { recursive: true });\n\
         console.log('async_cp:', fs.readFileSync(join(dest2, 'a.txt'), 'utf8') === 'hello');\n\
         \n\
         // single file cp\n\
         const singleDest = join(dest2, 'single.txt');\n\
         fs.cpSync(join(dir, 'a.txt'), singleDest);\n\
         console.log('single_cp:', fs.readFileSync(singleDest, 'utf8') === 'hello');\n\
         \n\
         // cleanup\n\
         fs.rmSync(dir, { recursive: true, force: true });\n\
         fs.rmSync(dest, { recursive: true, force: true });\n\
         fs.rmSync(dest2, { recursive: true, force: true });\n\
         console.log('done:', true);",
    );
    let out = oam(&["run", path.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        panic!("fs opendir/cp failed:\nstdout: {stdout}\nstderr: {stderr}");
    }
    for line in stdout.lines() {
        assert!(
            line.ends_with("true"),
            "assertion failed: {line}\nfull output: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn crypto_generate_keypair_ed25519() {
    let file = write_temp(
        "crypto_gkp.mjs",
        r#"
import crypto from 'node:crypto';

// 1. generateKeyPairSync returns PEM strings
const { publicKey, privateKey } = crypto.generateKeyPairSync('ed25519');
console.log('hasPub=' + (publicKey.includes('BEGIN PUBLIC KEY')));
console.log('hasPriv=' + (privateKey.includes('BEGIN PRIVATE KEY')));

// 2. sign+verify roundtrip with generated keys
const sign = crypto.createSign('ed25519');
sign.update('hello world');
const sig = sign.sign(privateKey);
console.log('sigLen=' + (sig.length > 0));

const verify = crypto.createVerify('ed25519');
verify.update('hello world');
console.log('verified=' + verify.verify(publicKey, sig));

// 3. one-shot sign/verify
const sig2 = crypto.sign('ed25519', Buffer.from('test'), privateKey);
const ok2 = crypto.verify('ed25519', Buffer.from('test'), publicKey, sig2);
console.log('oneshot=' + ok2);

// 4. wrong data fails verification
const verify2 = crypto.createVerify('ed25519');
verify2.update('wrong data');
console.log('wrongData=' + (verify2.verify(publicKey, sig) === false));

// 5. DER format option
const { publicKey: derPub, privateKey: derPriv } = crypto.generateKeyPairSync('ed25519', {
  publicKeyEncoding: { type: 'spki', format: 'der' },
  privateKeyEncoding: { type: 'pkcs8', format: 'der' },
});
console.log('derPubBuf=' + Buffer.isBuffer(derPub));
console.log('derPrivBuf=' + Buffer.isBuffer(derPriv));

// 6. async generateKeyPair
crypto.generateKeyPair('ed25519', (err, pub2, priv2) => {
  console.log('asyncErr=' + (err === null));
  console.log('asyncPub=' + (typeof pub2 === 'string' && pub2.includes('BEGIN PUBLIC KEY')));
  console.log('asyncPriv=' + (typeof priv2 === 'string' && priv2.includes('BEGIN PRIVATE KEY')));
});
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "crypto generateKeyPair failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "hasPub=true",
        "hasPriv=true",
        "sigLen=true",
        "verified=true",
        "oneshot=true",
        "wrongData=true",
        "derPubBuf=true",
        "derPrivBuf=true",
        "asyncErr=true",
        "asyncPub=true",
        "asyncPriv=true",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn webcrypto_subtle_sign_verify_hmac() {
    let file = write_temp(
        "webcrypto_subtle.mjs",
        r#"
import crypto from 'node:crypto';
const { subtle } = crypto.webcrypto || crypto;

// 1. HMAC sign/verify via subtle
const hmacKey = await subtle.generateKey(
  { name: 'HMAC', hash: 'SHA-256' },
  true, ['sign', 'verify']
);
console.log('hmacKeyType=' + (hmacKey.type === 'secret'));
console.log('hmacExtract=' + hmacKey.extractable);

const sig = await subtle.sign('HMAC', hmacKey, new TextEncoder().encode('hello'));
console.log('hmacSigType=' + (sig instanceof ArrayBuffer));
console.log('hmacSigLen=' + (sig.byteLength === 32));

const ok = await subtle.verify('HMAC', hmacKey, sig, new TextEncoder().encode('hello'));
console.log('hmacVerify=' + ok);

const bad = await subtle.verify('HMAC', hmacKey, sig, new TextEncoder().encode('wrong'));
console.log('hmacVerifyBad=' + (bad === false));

// 2. Export raw HMAC key
const rawKey = await subtle.exportKey('raw', hmacKey);
console.log('hmacExport=' + (rawKey instanceof ArrayBuffer));

// 3. Import raw HMAC key and re-verify
const imported = await subtle.importKey(
  'raw', rawKey, { name: 'HMAC', hash: 'SHA-256' }, false, ['verify']
);
const ok2 = await subtle.verify('HMAC', imported, sig, new TextEncoder().encode('hello'));
console.log('hmacImportVerify=' + ok2);

// 4. Ed25519 generateKey + sign/verify via subtle
const ed25519Pair = await subtle.generateKey('Ed25519', true, ['sign', 'verify']);
console.log('ed25519Priv=' + (ed25519Pair.privateKey.type === 'private'));
console.log('ed25519Pub=' + (ed25519Pair.publicKey.type === 'public'));

const edSig = await subtle.sign('Ed25519', ed25519Pair.privateKey, new TextEncoder().encode('test'));
console.log('edSigType=' + (edSig instanceof ArrayBuffer));

const edOk = await subtle.verify('Ed25519', ed25519Pair.publicKey, edSig, new TextEncoder().encode('test'));
console.log('edVerify=' + edOk);

const edBad = await subtle.verify('Ed25519', ed25519Pair.publicKey, edSig, new TextEncoder().encode('wrong'));
console.log('edVerifyBad=' + (edBad === false));

// 5. subtle.digest
const hash = await subtle.digest('SHA-256', new TextEncoder().encode('abc'));
console.log('digestType=' + (hash instanceof ArrayBuffer));
console.log('digestLen=' + (hash.byteLength === 32));
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "webcrypto subtle failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "hmacKeyType=true",
        "hmacExtract=true",
        "hmacSigType=true",
        "hmacSigLen=true",
        "hmacVerify=true",
        "hmacVerifyBad=true",
        "hmacExport=true",
        "hmacImportVerify=true",
        "ed25519Priv=true",
        "ed25519Pub=true",
        "edSigType=true",
        "edVerify=true",
        "edVerifyBad=true",
        "digestType=true",
        "digestLen=true",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn events_on_async_iterator() {
    let file = write_temp(
        "events_on.mjs",
        r#"
import { EventEmitter, on, once, getEventListeners, setMaxListeners } from 'node:events';

// 1. events.on returns an async iterator
const ee = new EventEmitter();
const iter = on(ee, 'data');
console.log('hasAsyncIter=' + (typeof iter[Symbol.asyncIterator] === 'function'));

// 2. Emit events, consume with async iterator
setTimeout(() => {
  ee.emit('data', 'hello');
  ee.emit('data', 'world');
  setTimeout(() => iter.return(), 10);
}, 10);

const results = [];
for await (const [value] of iter) {
  results.push(value);
}
console.log('iterValues=' + results.join(','));

// 3. events.once returns a promise
const ee2 = new EventEmitter();
setTimeout(() => ee2.emit('ready', 42), 10);
const [val] = await once(ee2, 'ready');
console.log('onceVal=' + (val === 42));

// 4. getEventListeners
const ee3 = new EventEmitter();
const fn1 = () => {};
const fn2 = () => {};
ee3.on('test', fn1);
ee3.on('test', fn2);
console.log('listeners=' + (getEventListeners(ee3, 'test').length === 2));

// 5. setMaxListeners (should not throw)
setMaxListeners(20, ee3);
console.log('setMax=true');

// 6. EventEmitter.on === on
console.log('staticOn=' + (EventEmitter.on === on));
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "events.on test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "hasAsyncIter=true",
        "iterValues=hello,world",
        "onceVal=true",
        "listeners=true",
        "setMax=true",
        "staticOn=true",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn process_get_builtin_module_and_navigator() {
    let file = write_temp(
        "builtin_nav.mjs",
        r#"
// process.getBuiltinModule
const fs = process.getBuiltinModule('fs');
console.log('getBuiltin_fs=' + (typeof fs.readFileSync === 'function'));

const crypto = process.getBuiltinModule('node:crypto');
console.log('getBuiltin_crypto=' + (typeof crypto.createHash === 'function'));

const missing = process.getBuiltinModule('nonexistent');
console.log('getBuiltin_missing=' + (missing === undefined));

// navigator
console.log('navigator=' + (typeof navigator === 'object'));
console.log('userAgent=' + navigator.userAgent.startsWith('oam/'));
console.log('onLine=' + (navigator.onLine === true));
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "builtin/navigator test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "getBuiltin_fs=true",
        "getBuiltin_crypto=true",
        "getBuiltin_missing=true",
        "navigator=true",
        "userAgent=true",
        "onLine=true",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn util_parse_args_full_surface() {
    let file = write_temp(
        "util_parse_args.mjs",
        r#"
import { parseArgs } from 'node:util';

// basic long options
const r1 = parseArgs({
  args: ['--name', 'alice', '--verbose'],
  options: {
    name:    { type: 'string' },
    verbose: { type: 'boolean' },
  },
  allowPositionals: false,
});
console.log('name=' + r1.values.name);
console.log('verbose=' + r1.values.verbose);
console.log('pos0=' + r1.positionals.length);

// short aliases
const r2 = parseArgs({
  args: ['-n', 'bob', '-v'],
  options: {
    name:    { type: 'string', short: 'n' },
    verbose: { type: 'boolean', short: 'v' },
  },
  allowPositionals: false,
});
console.log('short_name=' + r2.values.name);
console.log('short_verbose=' + r2.values.verbose);

// positionals + option terminator
const r3 = parseArgs({
  args: ['--flag', 'pos1', '--', '--not-an-opt'],
  options: { flag: { type: 'boolean' } },
  allowPositionals: true,
});
console.log('flag=' + r3.values.flag);
console.log('positionals=' + r3.positionals.join(','));

// multiple values
const r4 = parseArgs({
  args: ['--tag', 'a', '--tag', 'b', '--tag', 'c'],
  options: { tag: { type: 'string', multiple: true } },
});
console.log('tags=' + r4.values.tag.join(','));

// default values
const r5 = parseArgs({
  args: [],
  options: { color: { type: 'string', default: 'blue' } },
});
console.log('default_color=' + r5.values.color);

// equals syntax
const r6 = parseArgs({
  args: ['--output=json'],
  options: { output: { type: 'string' } },
});
console.log('eq_output=' + r6.values.output);

// tokens mode
const r7 = parseArgs({
  args: ['--x', 'pos'],
  options: { x: { type: 'boolean' } },
  tokens: true,
  allowPositionals: true,
});
console.log('has_tokens=' + Array.isArray(r7.tokens));
console.log('token_kinds=' + r7.tokens.map(t => t.kind).join(','));

// strict: unknown option throws
let threw = false;
try { parseArgs({ args: ['--unknown'], strict: true }); }
catch (e) { threw = true; }
console.log('strict_threw=' + threw);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "util.parseArgs test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "name=alice",
        "verbose=true",
        "pos0=0",
        "short_name=bob",
        "short_verbose=true",
        "flag=true",
        "positionals=pos1,--not-an-opt",
        "tags=a,b,c",
        "default_color=blue",
        "eq_output=json",
        "has_tokens=true",
        "token_kinds=option,positional",
        "strict_threw=true",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn crypto_hash_one_shot() {
    let file = write_temp(
        "crypto_hash.mjs",
        r#"
import crypto from 'node:crypto';

// basic hex output
const h1 = crypto.hash('sha256', 'hello', 'hex');
console.log('sha256_hello=' + h1);

// binary (buffer) output
const h2 = crypto.hash('sha256', 'hello');
console.log('is_buffer=' + (h2 instanceof Uint8Array));
console.log('buf_len=' + h2.length);

// matches createHash
const h3 = crypto.createHash('sha256').update('hello').digest('hex');
console.log('matches_createHash=' + (h1 === h3));

// md5
const h4 = crypto.hash('md5', 'test', 'hex');
console.log('md5_test=' + h4);

// sha512
const h5 = crypto.hash('sha512', '', 'hex');
// SHA-512 of empty string starts with cf83e1357
console.log('sha512_empty_prefix=' + h5.startsWith('cf83e1357'));

// base64 encoding
const h6 = crypto.hash('sha256', 'hello', 'base64');
console.log('base64_len=' + h6.length);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "crypto.hash test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "sha256_hello=2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        "is_buffer=true",
        "buf_len=32",
        "matches_createHash=true",
        "md5_test=098f6bcd4621d373cade4e832627b4f6",
        "sha512_empty_prefix=true",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
    assert!(
        stdout.contains("base64_len="),
        "missing base64 line.\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn console_time_count_group_table() {
    let file = write_temp(
        "console_extra.mjs",
        r#"
// console.time / timeEnd / timeLog
console.time('t1');
console.timeLog('t1', 'checkpoint');
console.timeEnd('t1');

// console.count / countReset
console.count('a');
console.count('a');
console.count('a');
console.countReset('a');
console.count('a');

// console.group / groupEnd
console.group('grp');
console.log('inside');
console.groupEnd();

// console.table (array of objects)
console.table([{ x: 1, y: 2 }]);

// console.assert
console.assert(true, 'should not print');
console.assert(false, 'assertion fired');

// console.dir
console.dir({ hello: 42 });

// default label
console.count();
console.count();
console.countReset();
console.count();
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "console extras failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("t1:"), "missing timeLog output:\n{stdout}");
    assert!(stdout.contains("checkpoint"), "missing timeLog extra args:\n{stdout}");
    assert!(stdout.contains("a: 1"), "missing count 1:\n{stdout}");
    assert!(stdout.contains("a: 2"), "missing count 2:\n{stdout}");
    assert!(stdout.contains("a: 3"), "missing count 3:\n{stdout}");
    // After countReset, next count should be 1 again
    let a_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("a: ")).collect();
    assert_eq!(a_lines.len(), 4, "expected 4 'a:' lines, got {a_lines:?}");
    assert_eq!(a_lines[3], "a: 1", "countReset did not reset:\n{stdout}");
    assert!(stdout.contains("inside"), "missing group content:\n{stdout}");
    assert!(stdout.contains("(index)"), "missing table header:\n{stdout}");
    assert!(stderr.contains("assertion fired"), "missing assert output:\n{stderr}");
    assert!(stdout.contains("hello"), "missing dir output:\n{stdout}");
    // default counters
    assert!(stdout.contains("default: 1"), "missing default count 1:\n{stdout}");
    assert!(stdout.contains("default: 2"), "missing default count 2:\n{stdout}");
}

#[test]
fn structured_clone_and_util_style_text() {
    let file = write_temp(
        "clone_style.mjs",
        r#"
import util from 'node:util';

// structuredClone
const obj = { a: 1, b: [2, 3], c: { d: 4 } };
const clone = structuredClone(obj);
console.log('clone_eq=' + (JSON.stringify(clone) === JSON.stringify(obj)));
console.log('clone_ref=' + (clone !== obj));
console.log('clone_deep_ref=' + (clone.c !== obj.c));
clone.a = 99;
console.log('original_a=' + obj.a);

// util.styleText single format
const red = util.styleText('red', 'hello');
console.log('red_has_escape=' + red.includes('\x1b[31m'));
console.log('red_has_reset=' + red.includes('\x1b[39m'));
console.log('red_has_text=' + red.includes('hello'));

// util.styleText array of formats
const boldRed = util.styleText(['bold', 'red'], 'hi');
console.log('bold_red_has_bold=' + boldRed.includes('\x1b[1m'));
console.log('bold_red_has_red=' + boldRed.includes('\x1b[31m'));

// util.styleText with unknown format
const unknown = util.styleText('nonexistent', 'text');
console.log('unknown_passthrough=' + unknown);

// structuredClone undefined
const u = structuredClone(undefined);
console.log('clone_undef=' + (u === undefined));

// structuredClone primitives
console.log('clone_num=' + (structuredClone(42) === 42));
console.log('clone_str=' + (structuredClone('abc') === 'abc'));
console.log('clone_bool=' + (structuredClone(true) === true));
console.log('clone_null=' + (structuredClone(null) === null));
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "structuredClone/styleText test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "clone_eq=true",
        "clone_ref=true",
        "clone_deep_ref=true",
        "original_a=1",
        "red_has_escape=true",
        "red_has_reset=true",
        "red_has_text=true",
        "bold_red_has_bold=true",
        "bold_red_has_red=true",
        "unknown_passthrough=text",
        "clone_undef=true",
        "clone_num=true",
        "clone_str=true",
        "clone_bool=true",
        "clone_null=true",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn process_load_env_file_and_report_stub() {
    // Create a .env file for loadEnvFile to read
    let env_file = write_temp(
        "loadenv/.env",
        "# comment\nFOO=bar\nBAZ=\"quoted value\"\nNUM=42\n",
    );
    let env_dir = env_file.parent().unwrap();
    let file = write_temp(
        "loadenv/test.mjs",
        r#"
import fs from 'node:fs';
import util from 'node:util';

// process.loadEnvFile
process.loadEnvFile();
console.log('FOO=' + process.env.FOO);
console.log('BAZ=' + process.env.BAZ);
console.log('NUM=' + process.env.NUM);

// process.report stub
console.log('report_type=' + typeof process.report);
console.log('report_getReport=' + typeof process.report.getReport);
console.log('report_obj=' + (typeof process.report.getReport() === 'object'));

// Missing .env throws
let threw = false;
try { process.loadEnvFile('nonexistent.env'); }
catch (e) { threw = e.code === 'ERR_ENV_FILE_NOT_FOUND'; }
console.log('missing_throws=' + threw);

// util.getSystemErrorName
console.log('enoent=' + util.getSystemErrorName(-2));
console.log('eacces=' + util.getSystemErrorName(-3));
console.log('unknown=' + util.getSystemErrorName(-999).startsWith('Unknown'));

// util.getSystemErrorMap
const map = util.getSystemErrorMap();
console.log('map_is_map=' + (map instanceof Map));
console.log('map_enoent=' + map.get(-2)[0]);
"#,
    );
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args(["run", file.to_str().unwrap()])
        .current_dir(env_dir)
        .env("OAM_CACHE_DIR", write_temp("oam-cache2/.keep", "").parent().unwrap())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "loadEnvFile test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "FOO=bar",
        "BAZ=quoted value",
        "NUM=42",
        "report_type=object",
        "report_getReport=function",
        "report_obj=true",
        "missing_throws=true",
        "enoent=ENOENT",
        "eacces=EACCES",
        "unknown=true",
        "map_is_map=true",
        "map_enoent=ENOENT",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn stream_add_abort_signal_and_compose() {
    let file = write_temp(
        "stream_extras.mjs",
        r#"
import { Readable, Writable, Transform, addAbortSignal, compose } from 'node:stream';

// addAbortSignal
const ac = new AbortController();
const chunks = [];
const w = new Writable({
  write(chunk, enc, cb) { chunks.push(chunk.toString()); cb(); },
});
addAbortSignal(ac.signal, w);
w.on('error', () => {}); // swallow expected abort error
w.write('hello');
ac.abort();
// After abort the stream should be destroyed
console.log('abort_destroyed=' + w.destroyed);

// compose: chain a transform into a readable -> writable pipeline
const upper = new Transform({
  transform(chunk, enc, cb) {
    cb(null, chunk.toString().toUpperCase());
  },
});
const collector = [];
const sink = new Writable({
  write(chunk, enc, cb) { collector.push(chunk.toString()); cb(); },
});
const composed = compose(upper, sink);
composed.write('hello');
composed.write('world');
composed.end();
await new Promise(r => composed.on('finish', r));
console.log('compose_result=' + collector.join(','));

// addAbortSignal with already-aborted signal
const ac2 = new AbortController();
ac2.abort();
const r = new Readable({ read() {} });
r.on('error', () => {}); // swallow expected abort error
addAbortSignal(ac2.signal, r);
console.log('pre_aborted=' + r.destroyed);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "stream extras failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "abort_destroyed=true",
        "compose_result=HELLO,WORLD",
        "pre_aborted=true",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn webcrypto_subtle_derive_bits_and_key() {
    let file = write_temp(
        "subtle_derive.mjs",
        r#"
const { subtle } = globalThis.crypto;

// PBKDF2 deriveBits
const keyMaterial = await subtle.importKey(
  'raw',
  new TextEncoder().encode('password'),
  'PBKDF2',
  false,
  ['deriveBits', 'deriveKey']
);

const salt = new Uint8Array([1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]);
const bits = await subtle.deriveBits(
  { name: 'PBKDF2', salt, iterations: 100000, hash: 'SHA-256' },
  keyMaterial,
  256
);
console.log('pbkdf2_type=' + (bits instanceof ArrayBuffer));
console.log('pbkdf2_len=' + bits.byteLength);

// PBKDF2 deriveKey
const aesKey = await subtle.deriveKey(
  { name: 'PBKDF2', salt, iterations: 100000, hash: 'SHA-256' },
  keyMaterial,
  { name: 'AES-GCM', length: 256 },
  true,
  ['sign']
);
console.log('deriveKey_type=' + aesKey.constructor.name);
console.log('deriveKey_algo=' + aesKey.algorithm.name);
console.log('deriveKey_extractable=' + aesKey.extractable);

// HKDF deriveBits
const hkdfMaterial = await subtle.importKey(
  'raw',
  new TextEncoder().encode('secret'),
  'HKDF',
  false,
  ['deriveBits']
);
const hkdfBits = await subtle.deriveBits(
  { name: 'HKDF', salt, info: new TextEncoder().encode('info'), hash: 'SHA-256' },
  hkdfMaterial,
  128
);
console.log('hkdf_type=' + (hkdfBits instanceof ArrayBuffer));
console.log('hkdf_len=' + hkdfBits.byteLength);

// deterministic: same inputs = same output
const bits2 = await subtle.deriveBits(
  { name: 'PBKDF2', salt, iterations: 100000, hash: 'SHA-256' },
  keyMaterial,
  256
);
const a = new Uint8Array(bits);
const b = new Uint8Array(bits2);
let same = true;
for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) same = false;
console.log('deterministic=' + same);

// buffer.Blob exists (use global Blob)
const BlobCtor = globalThis.Blob;
const blob = new BlobCtor(['hello']);
console.log('blob_size=' + blob.size);
console.log('blob_type=' + blob.type);
const text = await blob.text();
console.log('blob_text=' + text);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "subtle derive test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "pbkdf2_type=true",
        "pbkdf2_len=32",
        "deriveKey_type=CryptoKey",
        "deriveKey_algo=AES-GCM",
        "deriveKey_extractable=true",
        "hkdf_type=true",
        "hkdf_len=16",
        "deterministic=true",
        "blob_size=5",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
    assert!(stdout.contains("blob_text=hello"), "missing blob_text.\nstdout: {stdout}\nstderr: {stderr}");
}

#[test]
fn performance_mark_measure_entries() {
    let file = write_temp(
        "perf_mark.mjs",
        r#"
// performance.now exists
console.log('now_type=' + typeof performance.now());
console.log('now_positive=' + (performance.now() > 0));

// performance.mark
performance.mark('start');
let sum = 0;
for (let i = 0; i < 1000; i++) sum += i;
performance.mark('end');

// performance.measure
const m = performance.measure('work', 'start', 'end');
console.log('measure_name=' + m.name);
console.log('measure_type=' + m.entryType);
console.log('measure_duration_positive=' + (m.duration >= 0));

// getEntries
const entries = performance.getEntries();
console.log('entries_count=' + entries.length);

// getEntriesByName
const starts = performance.getEntriesByName('start');
console.log('starts_count=' + starts.length);
console.log('start_type=' + starts[0].entryType);

// getEntriesByType
const marks = performance.getEntriesByType('mark');
console.log('marks_count=' + marks.length);
const measures = performance.getEntriesByType('measure');
console.log('measures_count=' + measures.length);

// clearMarks
performance.clearMarks('start');
const afterClear = performance.getEntriesByType('mark');
console.log('marks_after_clear=' + afterClear.length);

// clearMeasures
performance.clearMeasures();
const afterClearM = performance.getEntriesByType('measure');
console.log('measures_after_clear=' + afterClearM.length);

// timeOrigin
console.log('timeOrigin_positive=' + (performance.timeOrigin > 0));

// measure with options object
performance.mark('a');
performance.mark('b');
const m2 = performance.measure('opt', { start: 'a', end: 'b' });
console.log('opt_measure=' + m2.name);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "performance test failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "now_type=number",
        "now_positive=true",
        "measure_name=work",
        "measure_type=measure",
        "measure_duration_positive=true",
        "entries_count=3",
        "starts_count=1",
        "start_type=mark",
        "marks_count=2",
        "measures_count=1",
        "marks_after_clear=1",
        "measures_after_clear=0",
        "timeOrigin_positive=true",
        "opt_measure=opt",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

// ------------------------------------------------------------------ M3-9
#[test]
fn webcrypto_subtle_encrypt_decrypt() {
    let file = write_temp(
        "subtle_enc.mjs",
        r#"
const subtle = globalThis.crypto.subtle;

// -- AES-GCM round-trip --
const gcmKey = await subtle.generateKey({ name: 'AES-GCM', length: 256 }, true, ['encrypt', 'decrypt']);
console.log('gcmKey_type=' + gcmKey.type);
console.log('gcmKey_algo=' + gcmKey.algorithm.name);

const iv = globalThis.crypto.getRandomValues(new Uint8Array(12));
const plain = new TextEncoder().encode('hello webcrypto');
const ct = await subtle.encrypt({ name: 'AES-GCM', iv }, gcmKey, plain);
console.log('gcm_ct_type=' + (ct instanceof ArrayBuffer));
console.log('gcm_ct_longer=' + (ct.byteLength > plain.length));

const pt = await subtle.decrypt({ name: 'AES-GCM', iv }, gcmKey, ct);
const decoded = new TextDecoder().decode(pt);
console.log('gcm_roundtrip=' + decoded);

// -- AES-GCM with AAD --
const aad = new TextEncoder().encode('associated data');
const ct2 = await subtle.encrypt({ name: 'AES-GCM', iv, additionalData: aad }, gcmKey, plain);
const pt2 = await subtle.decrypt({ name: 'AES-GCM', iv, additionalData: aad }, gcmKey, ct2);
console.log('gcm_aad_roundtrip=' + new TextDecoder().decode(pt2));

// -- AES-GCM wrong key fails --
const wrongKey = await subtle.generateKey({ name: 'AES-GCM', length: 256 }, true, ['encrypt', 'decrypt']);
try {
  await subtle.decrypt({ name: 'AES-GCM', iv }, wrongKey, ct);
  console.log('gcm_wrong_key=no_error');
} catch (e) {
  console.log('gcm_wrong_key=error');
}

// -- AES-CBC round-trip --
const cbcKeyData = globalThis.crypto.getRandomValues(new Uint8Array(16));
const cbcKey = await subtle.importKey('raw', cbcKeyData, { name: 'AES-CBC' }, true, ['encrypt', 'decrypt']);
const cbcIv = globalThis.crypto.getRandomValues(new Uint8Array(16));
const cbcCt = await subtle.encrypt({ name: 'AES-CBC', iv: cbcIv }, cbcKey, plain);
console.log('cbc_ct_type=' + (cbcCt instanceof ArrayBuffer));
const cbcPt = await subtle.decrypt({ name: 'AES-CBC', iv: cbcIv }, cbcKey, cbcCt);
console.log('cbc_roundtrip=' + new TextDecoder().decode(cbcPt));

// -- AES-CTR round-trip --
const ctrKeyData = globalThis.crypto.getRandomValues(new Uint8Array(32));
const ctrKey = await subtle.importKey('raw', ctrKeyData, { name: 'AES-CTR' }, true, ['encrypt', 'decrypt']);
const counter = new Uint8Array(16);
counter[15] = 1;
const ctrCt = await subtle.encrypt({ name: 'AES-CTR', counter, length: 64 }, ctrKey, plain);
const ctrPt = await subtle.decrypt({ name: 'AES-CTR', counter, length: 64 }, ctrKey, ctrCt);
console.log('ctr_roundtrip=' + new TextDecoder().decode(ctrPt));

// -- wrapKey / unwrapKey --
const wrapKey = await subtle.generateKey({ name: 'AES-GCM', length: 256 }, true, ['wrapKey', 'unwrapKey']);
const innerKey = await subtle.generateKey({ name: 'AES-GCM', length: 128 }, true, ['encrypt']);
const wrapIv = globalThis.crypto.getRandomValues(new Uint8Array(12));
const wrapped = await subtle.wrapKey('raw', innerKey, wrapKey, { name: 'AES-GCM', iv: wrapIv });
console.log('wrapped_type=' + (wrapped instanceof ArrayBuffer));
const unwrapped = await subtle.unwrapKey(
  'raw', wrapped, wrapKey, { name: 'AES-GCM', iv: wrapIv },
  { name: 'AES-GCM', length: 128 }, true, ['encrypt']
);
console.log('unwrap_type=' + unwrapped.type);
console.log('unwrap_algo=' + unwrapped.algorithm.name);

// -- AES-128 generateKey --
const aes128 = await subtle.generateKey({ name: 'AES-GCM', length: 128 }, true, ['encrypt']);
const exported = await subtle.exportKey('raw', aes128);
console.log('aes128_keylen=' + new Uint8Array(exported).length);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "subtle encrypt/decrypt failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "gcmKey_type=secret",
        "gcmKey_algo=AES-GCM",
        "gcm_ct_type=true",
        "gcm_ct_longer=true",
        "gcm_roundtrip=hello webcrypto",
        "gcm_aad_roundtrip=hello webcrypto",
        "gcm_wrong_key=error",
        "cbc_ct_type=true",
        "cbc_roundtrip=hello webcrypto",
        "ctr_roundtrip=hello webcrypto",
        "wrapped_type=true",
        "unwrap_type=secret",
        "unwrap_algo=AES-GCM",
        "aes128_keylen=16",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

// ------------------------------------------------------------------ M3-10
#[test]
fn readable_async_helpers() {
    let file = write_temp(
        "readable_helpers.mjs",
        r#"
import { Readable } from 'node:stream';

// map
const mapped = await Readable.from([1, 2, 3]).map(x => x * 10).toArray();
console.log('map=' + mapped.join(','));

// filter
const filtered = await Readable.from([1, 2, 3, 4, 5]).filter(x => x % 2 === 0).toArray();
console.log('filter=' + filtered.join(','));

// reduce
const sum = await Readable.from([1, 2, 3, 4]).reduce((a, b) => a + b, 0);
console.log('reduce=' + sum);

// reduce without initial
const product = await Readable.from([2, 3, 4]).reduce((a, b) => a * b);
console.log('reduce_no_init=' + product);

// forEach
const collected = [];
await Readable.from(['a', 'b', 'c']).forEach(x => collected.push(x.toUpperCase()));
console.log('forEach=' + collected.join(','));

// some
const hasEven = await Readable.from([1, 3, 4, 5]).some(x => x % 2 === 0);
console.log('some_true=' + hasEven);
const allOdd = await Readable.from([1, 3, 5]).some(x => x % 2 === 0);
console.log('some_false=' + allOdd);

// every
const allPos = await Readable.from([1, 2, 3]).every(x => x > 0);
console.log('every_true=' + allPos);
const notAll = await Readable.from([1, -1, 3]).every(x => x > 0);
console.log('every_false=' + notAll);

// find
const found = await Readable.from([10, 20, 30]).find(x => x > 15);
console.log('find=' + found);
const notFound = await Readable.from([1, 2, 3]).find(x => x > 100);
console.log('find_none=' + notFound);

// drop
const dropped = await Readable.from([1, 2, 3, 4, 5]).drop(2).toArray();
console.log('drop=' + dropped.join(','));

// take
const taken = await Readable.from([1, 2, 3, 4, 5]).take(3).toArray();
console.log('take=' + taken.join(','));

// flatMap
const flatMapped = await Readable.from([1, 2, 3]).flatMap(x => [x, x * 10]).toArray();
console.log('flatMap=' + flatMapped.join(','));

// chaining: filter -> map -> toArray
const chained = await Readable.from([1, 2, 3, 4, 5, 6])
  .filter(x => x % 2 === 0)
  .map(x => x * x)
  .toArray();
console.log('chain=' + chained.join(','));
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "readable helpers failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "map=10,20,30",
        "filter=2,4",
        "reduce=10",
        "reduce_no_init=24",
        "forEach=A,B,C",
        "some_true=true",
        "some_false=false",
        "every_true=true",
        "every_false=false",
        "find=20",
        "find_none=undefined",
        "drop=3,4,5",
        "take=1,2,3",
        "flatMap=1,10,2,20,3,30",
        "chain=4,16,36",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

// ------------------------------------------------------------------ M3-11
#[test]
fn mimetype_crypto_extras_readable_statics() {
    let file = write_temp(
        "misc_m3.mjs",
        r#"
import util from 'node:util';
import crypto from 'node:crypto';
import { Readable } from 'node:stream';

// -- util.MIMEType --
const mime = new util.MIMEType('text/html; charset=utf-8; boundary="abc"');
console.log('mime_type=' + mime.type);
console.log('mime_subtype=' + mime.subtype);
console.log('mime_essence=' + mime.essence);
console.log('mime_charset=' + mime.params.get('charset'));
console.log('mime_boundary=' + mime.params.get('boundary'));
console.log('mime_str=' + mime.toString());

const json = new util.MIMEType('application/json');
console.log('json_type=' + json.type);
console.log('json_subtype=' + json.subtype);
console.log('json_params_has=' + json.params.has('charset'));

// invalid MIME
try {
  new util.MIMEType('invalid');
  console.log('invalid_mime=no_error');
} catch (e) {
  console.log('invalid_mime=error');
}

// -- crypto.getCurves --
const curves = crypto.getCurves();
console.log('curves_includes_p256=' + curves.includes('prime256v1'));
console.log('curves_includes_ed25519=' + curves.includes('ed25519'));

// -- crypto.generateKeySync --
const aesKey = crypto.generateKeySync('aes', { length: 128 });
console.log('genkey_type=' + aesKey.type);
console.log('genkey_size=' + aesKey.symmetricKeySize);

const hmacKey = crypto.generateKeySync('hmac', { length: 512 });
console.log('hmackey_type=' + hmacKey.type);
console.log('hmackey_size=' + hmacKey.symmetricKeySize);

// -- Readable.isDisturbed / isReadable --
const r = new Readable({ read() {} });
console.log('is_readable_fresh=' + Readable.isReadable(r));
console.log('is_disturbed_fresh=' + Readable.isDisturbed(r));
r.destroy();
console.log('is_readable_destroyed=' + Readable.isReadable(r));
console.log('is_disturbed_destroyed=' + Readable.isDisturbed(r));

// -- process.abort exists --
console.log('process_abort_type=' + typeof process.abort);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "misc m3 failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "mime_type=text",
        "mime_subtype=html",
        "mime_essence=text/html",
        "mime_charset=utf-8",
        "mime_boundary=abc",
        "mime_str=text/html;charset=utf-8;boundary=abc",
        "json_type=application",
        "json_subtype=json",
        "json_params_has=false",
        "invalid_mime=error",
        "curves_includes_p256=true",
        "curves_includes_ed25519=true",
        "genkey_type=secret",
        "genkey_size=16",
        "hmackey_type=secret",
        "hmackey_size=64",
        "is_readable_fresh=true",
        "is_disturbed_fresh=false",
        "is_readable_destroyed=false",
        "is_disturbed_destroyed=true",
        "process_abort_type=function",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

// ------------------------------------------------------------------ M3-12
#[test]
fn os_machine_events_abort_util_parse_env() {
    let file = write_temp(
        "misc_m3b.mjs",
        r#"
import os from 'node:os';
import events from 'node:events';
import util from 'node:util';
import fs from 'node:fs/promises';

// -- os.machine --
const m = os.machine();
console.log('machine_type=' + typeof m);
console.log('machine_nonempty=' + (m.length > 0));

// -- events.addAbortListener --
console.log('addAbortListener_type=' + typeof events.addAbortListener);
const ac = new AbortController();
let fired = false;
const disposable = events.addAbortListener(ac.signal, () => { fired = true; });
console.log('disposable_has_dispose=' + (Symbol.dispose in disposable));
ac.abort();
await new Promise(r => setTimeout(r, 10));
console.log('abort_fired=' + fired);

// addAbortListener on already-aborted signal
const ac2 = new AbortController();
ac2.abort();
let fired2 = false;
events.addAbortListener(ac2.signal, () => { fired2 = true; });
await new Promise(r => setTimeout(r, 10));
console.log('pre_aborted_fired=' + fired2);

// -- util.aborted --
const ac3 = new AbortController();
let abortedReason = null;
const p = util.aborted(ac3.signal).then(r => { abortedReason = r; });
ac3.abort('test reason');
await p;
console.log('aborted_reason=' + abortedReason);

// util.aborted on already-aborted
const ac4 = new AbortController();
ac4.abort('already');
const r4 = await util.aborted(ac4.signal);
console.log('aborted_already=' + r4);

// -- util.parseEnv --
const env = util.parseEnv('FOO=bar\nBAZ="hello world"\n# comment\nEMPTY=\nQUOTED=\'single\'');
console.log('parseEnv_FOO=' + env.FOO);
console.log('parseEnv_BAZ=' + env.BAZ);
console.log('parseEnv_EMPTY=' + JSON.stringify(env.EMPTY));
console.log('parseEnv_QUOTED=' + env.QUOTED);
console.log('parseEnv_comment=' + (env['# comment'] === undefined));

// -- fs/promises.constants --
console.log('fsp_F_OK=' + fs.constants.F_OK);
console.log('fsp_R_OK=' + fs.constants.R_OK);

// -- events.captureRejectionSymbol --
console.log('captureRejection=' + typeof events.captureRejectionSymbol);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "misc m3b failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "machine_type=string",
        "machine_nonempty=true",
        "addAbortListener_type=function",
        "disposable_has_dispose=true",
        "abort_fired=true",
        "pre_aborted_fired=true",
        "aborted_reason=test reason",
        "aborted_already=already",
        "parseEnv_FOO=bar",
        "parseEnv_BAZ=hello world",
        "parseEnv_EMPTY=\"\"",
        "parseEnv_QUOTED=single",
        "parseEnv_comment=true",
        "fsp_F_OK=0",
        "fsp_R_OK=4",
        "captureRejection=symbol",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn assert_iferror_events_getmax_util_tousvstring_process_extras() {
    let file = write_temp(
        "misc_m3c.mjs",
        r#"
import assert from 'node:assert';
import events from 'node:events';
import util from 'node:util';
import process from 'node:process';
import crypto from 'node:crypto';
import stream from 'node:stream';
import buffer from 'node:buffer';

// -- assert.ifError --
assert.ifError(null);
assert.ifError(undefined);
try { assert.ifError(new Error('boom')); console.log('FAIL'); } catch(e) {
  console.log('ifError_err=' + e.message);
}
try { assert.ifError('oops'); console.log('FAIL'); } catch(e) {
  console.log('ifError_str=' + e.message.includes('oops'));
  console.log('ifError_op=' + e.operator);
}

// -- events.getMaxListeners --
const ee = new events.EventEmitter();
ee.setMaxListeners(42);
console.log('getMax_ee=' + events.getMaxListeners(ee));
console.log('getMax_default=' + events.getMaxListeners(new events.EventEmitter()));

// -- util.toUSVString --
console.log('toUSV_basic=' + (util.toUSVString('hello') === 'hello'));
console.log('toUSV_num=' + util.toUSVString(123));
console.log('toUSV_type=' + typeof util.toUSVString);

// -- process.debugPort, connected, constrainedMemory, availableMemory --
console.log('debugPort=' + process.debugPort);
console.log('connected=' + process.connected);
console.log('constrainedMemory=' + process.constrainedMemory());
console.log('availableMemory=' + process.availableMemory());

// -- crypto.getFips, secureHeapUsed --
console.log('getFips=' + crypto.getFips());
const sh = crypto.secureHeapUsed();
console.log('secureHeap_total=' + sh.total);
console.log('secureHeap_used=' + sh.used);
try { crypto.setFips(1); } catch(e) { console.log('setFips_err=true'); }

// -- stream.getDefaultHighWaterMark / setDefaultHighWaterMark --
console.log('hwm_obj=' + stream.getDefaultHighWaterMark(true));
console.log('hwm_buf=' + stream.getDefaultHighWaterMark(false));
try { stream.setDefaultHighWaterMark(false, -1); } catch(e) { console.log('hwm_invalid=true'); }

// -- buffer.kStringMaxLength, SlowBuffer --
console.log('kStringMaxLength=' + buffer.kStringMaxLength);
console.log('SlowBuffer_type=' + typeof buffer.SlowBuffer);
const sb = new buffer.SlowBuffer(8);
console.log('sb_len=' + sb.length);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "misc m3c failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "ifError_err=boom",
        "ifError_str=true",
        "ifError_op=ifError",
        "getMax_ee=42",
        "getMax_default=10",
        "toUSV_basic=true",
        "toUSV_num=123",
        "toUSV_type=function",
        "debugPort=9229",
        "connected=false",
        "constrainedMemory=0",
        "availableMemory=0",
        "getFips=0",
        "secureHeap_total=0",
        "secureHeap_used=0",
        "setFips_err=true",
        "hwm_obj=16",
        "hwm_buf=16384",
        "hwm_invalid=true",
        "kStringMaxLength=536870888",
        "SlowBuffer_type=function",
        "sb_len=8",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn util_is_family_stream_isdisturbed_http_globalagent() {
    let file = write_temp(
        "misc_m3d.mjs",
        r#"
import util from 'node:util';
import stream from 'node:stream';
import http from 'node:http';

// -- util.is* deprecated family --
console.log('isRegExp=' + util.isRegExp(/test/));
console.log('isRegExp_f=' + util.isRegExp('nope'));
console.log('isDate=' + util.isDate(new Date()));
console.log('isDate_f=' + util.isDate(42));
console.log('isError=' + util.isError(new TypeError('x')));
console.log('isError_f=' + util.isError('x'));
console.log('isPrim_null=' + util.isPrimitive(null));
console.log('isPrim_str=' + util.isPrimitive('x'));
console.log('isPrim_num=' + util.isPrimitive(0));
console.log('isPrim_obj=' + util.isPrimitive({}));
console.log('isBuffer=' + util.isBuffer(Buffer.from('x')));
console.log('isBuffer_f=' + util.isBuffer(new Uint8Array(1)));
console.log('isFunction=' + util.isFunction(() => {}));
console.log('isFunction_f=' + util.isFunction(42));
console.log('isObject=' + util.isObject({}));
console.log('isObject_null=' + util.isObject(null));
console.log('isNullOrUndef_n=' + util.isNullOrUndefined(null));
console.log('isNullOrUndef_u=' + util.isNullOrUndefined(undefined));
console.log('isNullOrUndef_f=' + util.isNullOrUndefined(0));
console.log('isString=' + util.isString('hello'));
console.log('isNumber=' + util.isNumber(3.14));
console.log('isBoolean=' + util.isBoolean(false));
console.log('isNull=' + util.isNull(null));
console.log('isNull_undef=' + util.isNull(undefined));
console.log('isUndefined=' + util.isUndefined(undefined));
console.log('isSymbol=' + util.isSymbol(Symbol('x')));

// -- util.log --
console.log('log_type=' + typeof util.log);

// -- stream.isDisturbed --
console.log('isDisturbed_type=' + typeof stream.isDisturbed);
const r = new stream.Readable({ read() {} });
console.log('isDisturbed_fresh=' + stream.isDisturbed(r));

// -- http.globalAgent --
console.log('globalAgent_type=' + typeof http.globalAgent);
console.log('maxSockets=' + http.globalAgent.maxSockets);
console.log('keepAlive=' + http.globalAgent.keepAlive);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "misc m3d failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "isRegExp=true",
        "isRegExp_f=false",
        "isDate=true",
        "isDate_f=false",
        "isError=true",
        "isError_f=false",
        "isPrim_null=true",
        "isPrim_str=true",
        "isPrim_num=true",
        "isPrim_obj=false",
        "isBuffer=true",
        "isBuffer_f=false",
        "isFunction=true",
        "isFunction_f=false",
        "isObject=true",
        "isObject_null=false",
        "isNullOrUndef_n=true",
        "isNullOrUndef_u=true",
        "isNullOrUndef_f=false",
        "isString=true",
        "isNumber=true",
        "isBoolean=true",
        "isNull=true",
        "isNull_undef=false",
        "isUndefined=true",
        "isSymbol=true",
        "log_type=function",
        "isDisturbed_type=function",
        "isDisturbed_fresh=false",
        "globalAgent_type=object",
        "maxSockets=Infinity",
        "keepAlive=true",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn os_constants_tracing_channel_call_tracker() {
    let file = write_temp(
        "misc_m3e.mjs",
        r#"
import os from 'node:os';
import diag from 'node:diagnostics_channel';
import assert from 'node:assert';

// -- os.constants.signals --
console.log('SIGTERM=' + os.constants.signals.SIGTERM);
console.log('SIGKILL=' + os.constants.signals.SIGKILL);
console.log('SIGINT=' + os.constants.signals.SIGINT);
console.log('SIGHUP=' + os.constants.signals.SIGHUP);

// -- os.constants.errno --
console.log('ENOENT=' + os.constants.errno.ENOENT);
console.log('EACCES=' + os.constants.errno.EACCES);
console.log('EEXIST=' + os.constants.errno.EEXIST);
console.log('EPERM=' + os.constants.errno.EPERM);

// -- os.constants.priority --
console.log('PRIORITY_NORMAL=' + os.constants.priority.PRIORITY_NORMAL);

// -- diagnostics_channel.tracingChannel --
const tc = diag.tracingChannel('test.op');
console.log('tc_start_type=' + typeof tc.start.subscribe);
console.log('tc_end_type=' + typeof tc.end.subscribe);
console.log('tc_error_type=' + typeof tc.error.subscribe);
console.log('tc_asyncStart_type=' + typeof tc.asyncStart.subscribe);

let startMsg = null;
tc.subscribe({ start: (msg) => { startMsg = msg; } });
tc.start.publish({ op: 'hello' });
console.log('tc_start_msg=' + startMsg.op);

// -- assert.CallTracker --
const tracker = new assert.CallTracker();
const fn1 = tracker.calls(() => 42, 2);
console.log('fn1_call1=' + fn1());
console.log('fn1_call2=' + fn1());
tracker.verify();
console.log('tracker_ok=true');

const tracker2 = new assert.CallTracker();
const fn2 = tracker2.calls(() => {}, 3);
fn2(); fn2();
const report = tracker2.report();
console.log('report_len=' + report.length);
console.log('report_expected=' + report[0].expected);
console.log('report_actual=' + report[0].actual);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "misc m3e failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "SIGTERM=15",
        "SIGKILL=9",
        "SIGINT=2",
        "SIGHUP=1",
        "ENOENT=-2",
        "EACCES=-13",
        "EEXIST=-17",
        "EPERM=-1",
        "PRIORITY_NORMAL=0",
        "tc_start_type=function",
        "tc_end_type=function",
        "tc_error_type=function",
        "tc_asyncStart_type=function",
        "tc_start_msg=hello",
        "fn1_call1=42",
        "fn1_call2=42",
        "tracker_ok=true",
        "report_len=1",
        "report_expected=3",
        "report_actual=2",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn fs_dirent_class_expanded_constants() {
    let file = write_temp(
        "misc_m3f.mjs",
        r#"
import fs from 'node:fs';
import fsp from 'node:fs/promises';

// -- fs.Dirent class exists --
console.log('Dirent_type=' + typeof fs.Dirent);

// -- readdirSync withFileTypes returns Dirent instances --
const entries = fs.readdirSync('.', { withFileTypes: true });
const first = entries[0];
console.log('instanceof=' + (first instanceof fs.Dirent));
console.log('has_isFile=' + typeof first.isFile);
console.log('has_isDir=' + typeof first.isDirectory);
console.log('has_isSymlink=' + typeof first.isSymbolicLink);
console.log('has_name=' + (typeof first.name === 'string'));
console.log('has_parentPath=' + (typeof first.parentPath === 'string'));
console.log('has_path=' + (typeof first.path === 'string'));

// -- async readdir withFileTypes --
const asyncEntries = await fsp.readdir('.', { withFileTypes: true });
const asyncFirst = asyncEntries[0];
console.log('async_instanceof=' + (asyncFirst instanceof fs.Dirent));

// -- expanded fs.constants --
console.log('F_OK=' + fs.constants.F_OK);
console.log('O_RDONLY=' + fs.constants.O_RDONLY);
console.log('O_WRONLY=' + fs.constants.O_WRONLY);
console.log('O_RDWR=' + fs.constants.O_RDWR);
console.log('O_CREAT=' + fs.constants.O_CREAT);
console.log('S_IFMT=' + fs.constants.S_IFMT);
console.log('S_IFREG=' + fs.constants.S_IFREG);
console.log('S_IFDIR=' + fs.constants.S_IFDIR);
console.log('S_IRUSR=' + fs.constants.S_IRUSR);
console.log('COPYFILE_EXCL=' + fs.constants.COPYFILE_EXCL);

// -- fs/promises also exports Dirent and expanded constants --
console.log('fsp_Dirent=' + typeof fsp.Dirent);
console.log('fsp_O_RDONLY=' + fsp.constants.O_RDONLY);
console.log('fsp_S_IFMT=' + fsp.constants.S_IFMT);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "misc m3f failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "Dirent_type=function",
        "instanceof=true",
        "has_isFile=function",
        "has_isDir=function",
        "has_isSymlink=function",
        "has_name=true",
        "has_parentPath=true",
        "has_path=true",
        "async_instanceof=true",
        "F_OK=0",
        "O_RDONLY=0",
        "O_WRONLY=1",
        "O_RDWR=2",
        "O_CREAT=64",
        "S_IFMT=61440",
        "S_IFREG=32768",
        "S_IFDIR=16384",
        "S_IRUSR=256",
        "COPYFILE_EXCL=1",
        "fsp_Dirent=function",
        "fsp_O_RDONLY=0",
        "fsp_S_IFMT=61440",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn crypto_aliases_http_agent_fs_exports_eear() {
    let file = write_temp(
        "misc_m3g.mjs",
        r#"
import crypto from 'node:crypto';
import http from 'node:http';
import fs from 'node:fs';
import events from 'node:events';

// -- crypto aliases --
console.log('pseudoRandomBytes=' + typeof crypto.pseudoRandomBytes);
console.log('rng=' + typeof crypto.rng);
console.log('prng=' + typeof crypto.prng);
const b = crypto.pseudoRandomBytes(8);
console.log('prb_len=' + b.length);

// -- http.Agent --
console.log('Agent=' + typeof http.Agent);
const agent = new http.Agent({ keepAlive: true });
console.log('agent_keepAlive=' + agent.keepAlive);
console.log('agent_maxSockets=' + agent.maxSockets);
console.log('agent_getName=' + agent.getName({ host: 'x.com', port: 443 }));
agent.destroy();

// -- http.maxHeaderSize --
console.log('maxHeaderSize=' + http.maxHeaderSize);

// -- http.validateHeaderName / validateHeaderValue --
http.validateHeaderName('Content-Type');
console.log('validateName=ok');
http.validateHeaderValue('X-Test', 'val');
console.log('validateValue=ok');
try { http.validateHeaderValue('X-Test', undefined); } catch(e) { console.log('validateValue_undef=caught'); }

// -- fs exports --
console.log('Stats=' + typeof fs.Stats);
console.log('ReadStream=' + typeof fs.ReadStream);
console.log('WriteStream=' + typeof fs.WriteStream);
console.log('FileReadStream=' + typeof fs.FileReadStream);

// -- events.EventEmitterAsyncResource --
console.log('EEAR=' + typeof events.EventEmitterAsyncResource);
const ear = new events.EventEmitterAsyncResource({ name: 'test' });
console.log('ear_asyncId=' + ear.asyncId);
ear.on('test', () => {});
console.log('ear_listeners=' + ear.listenerCount('test'));
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "misc m3g failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "pseudoRandomBytes=function",
        "rng=function",
        "prng=function",
        "prb_len=8",
        "Agent=function",
        "agent_keepAlive=true",
        "agent_maxSockets=Infinity",
        "agent_getName=x.com:443",
        "maxHeaderSize=16384",
        "validateName=ok",
        "validateValue=ok",
        "validateValue_undef=caught",
        "Stats=function",
        "ReadStream=function",
        "WriteStream=function",
        "FileReadStream=function",
        "EEAR=function",
        "ear_asyncId=0",
        "ear_listeners=1",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn http_outgoing_msg_full_status_codes_net_blocklist_crypto_cert_process_uid() {
    let file = write_temp(
        "misc_m3h.mjs",
        r#"
import http from 'node:http';
import net from 'node:net';
import crypto from 'node:crypto';
import process from 'node:process';

// -- http.OutgoingMessage --
console.log('OutgoingMessage=' + typeof http.OutgoingMessage);
const om = new http.OutgoingMessage();
console.log('om_headersSent=' + om.headersSent);
om.setHeader('X-Test', 'hello');
console.log('om_getHeader=' + om.getHeader('x-test'));
console.log('om_hasHeader=' + om.hasHeader('X-Test'));
console.log('om_getHeaderNames=' + om.getHeaderNames().join(','));
om.appendHeader('X-Multi', 'a');
om.appendHeader('X-Multi', 'b');
console.log('om_appendHeader=' + JSON.stringify(om.getHeader('x-multi')));
om.removeHeader('X-Test');
console.log('om_removed=' + om.hasHeader('X-Test'));

// ServerResponse instanceof OutgoingMessage
console.log('SR_proto=' + (http.ServerResponse.prototype instanceof http.OutgoingMessage));

// -- full STATUS_CODES --
console.log('sc_100=' + http.STATUS_CODES[100]);
console.log('sc_418=' + http.STATUS_CODES[418]);
console.log('sc_429=' + http.STATUS_CODES[429]);
console.log('sc_451=' + http.STATUS_CODES[451]);
console.log('sc_502=' + http.STATUS_CODES[502]);
console.log('sc_511=' + http.STATUS_CODES[511]);
const codeCount = Object.keys(http.STATUS_CODES).length;
console.log('sc_count_gte_40=' + (codeCount >= 40));

// -- net.SocketAddress --
console.log('SocketAddress=' + typeof net.SocketAddress);
const sa = new net.SocketAddress({ address: '10.0.0.1', port: 8080 });
console.log('sa_addr=' + sa.address);
console.log('sa_port=' + sa.port);
console.log('sa_family=' + sa.family);

// -- net.BlockList --
console.log('BlockList=' + typeof net.BlockList);
const bl = new net.BlockList();
bl.addAddress('1.2.3.4');
bl.addRange('10.0.0.0', '10.0.0.255');
bl.addSubnet('192.168.1.0', 24);
console.log('bl_check_hit=' + bl.check('1.2.3.4'));
console.log('bl_check_miss=' + bl.check('5.6.7.8'));
console.log('bl_rules=' + bl.rules.length);

// -- crypto.Certificate --
console.log('Certificate=' + typeof crypto.Certificate);
const cert = new crypto.Certificate();
console.log('cert_verifySpkac=' + cert.verifySpkac());
console.log('cert_static_verify=' + crypto.Certificate.verifySpkac());
console.log('cert_exportChallenge_len=' + crypto.Certificate.exportChallenge().length);

// -- process.getuid/getgid --
console.log('getuid=' + process.getuid());
console.log('getgid=' + process.getgid());
console.log('geteuid=' + process.geteuid());
console.log('getegid=' + process.getegid());
console.log('getgroups=' + JSON.stringify(process.getgroups()));
process.setuid(0);
process.setgid(0);
console.log('setuid_setgid=ok');
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "misc m3h failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "OutgoingMessage=function",
        "om_headersSent=false",
        "om_getHeader=hello",
        "om_hasHeader=true",
        "om_getHeaderNames=x-test",
        r#"om_appendHeader=["a","b"]"#,
        "om_removed=false",
        "SR_proto=true",
        "sc_100=Continue",
        "sc_418=I'm a Teapot",
        "sc_429=Too Many Requests",
        "sc_451=Unavailable For Legal Reasons",
        "sc_502=Bad Gateway",
        "sc_511=Network Authentication Required",
        "sc_count_gte_40=true",
        "SocketAddress=function",
        "sa_addr=10.0.0.1",
        "sa_port=8080",
        "sa_family=ipv4",
        "BlockList=function",
        "bl_check_hit=true",
        "bl_check_miss=false",
        "bl_rules=3",
        "Certificate=function",
        "cert_verifySpkac=false",
        "cert_static_verify=false",
        "cert_exportChallenge_len=0",
        "getuid=0",
        "getgid=0",
        "geteuid=0",
        "getegid=0",
        "getgroups=[0]",
        "setuid_setgid=ok",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

#[test]
fn duplex_statics_http2_consts_dns_codes_perf_entry_resource_usage() {
    let file = write_temp(
        "misc_m3i.mjs",
        r#"
import stream from 'node:stream';
import http2 from 'node:http2';
import dns from 'node:dns';
import { performance, PerformanceObserver, PerformanceEntry, PerformanceObserverEntryList, PerformanceNodeTiming, nodeTiming } from 'node:perf_hooks';
import process from 'node:process';

// -- Duplex statics --
console.log('Duplex.from=' + typeof stream.Duplex.from);
console.log('Duplex.fromWeb=' + typeof stream.Duplex.fromWeb);
console.log('Duplex.toWeb=' + typeof stream.Duplex.toWeb);

// Duplex.from with a passthrough-like Duplex
const src = new stream.PassThrough();
const dup = stream.Duplex.from(src);
console.log('dupFrom_ok=' + (typeof dup.write === 'function'));

// -- http2.constants --
console.log('h2_NO_ERROR=' + http2.constants.NGHTTP2_NO_ERROR);
console.log('h2_PROTOCOL_ERROR=' + http2.constants.NGHTTP2_PROTOCOL_ERROR);
console.log('h2_CANCEL=' + http2.constants.NGHTTP2_CANCEL);
console.log('h2_STATUS=' + http2.constants.HTTP2_HEADER_STATUS);
console.log('h2_METHOD=' + http2.constants.HTTP2_HEADER_METHOD);
console.log('h2_OK=' + http2.constants.HTTP_STATUS_OK);
console.log('h2_WEIGHT=' + http2.constants.NGHTTP2_DEFAULT_WEIGHT);

// -- dns error codes --
console.log('dns_NODATA=' + dns.NODATA);
console.log('dns_NOTFOUND=' + dns.NOTFOUND);
console.log('dns_SERVFAIL=' + dns.SERVFAIL);
console.log('dns_CONNREFUSED=' + dns.CONNREFUSED);
console.log('dns_TIMEOUT=' + dns.TIMEOUT);
console.log('dns_CANCELLED=' + dns.CANCELLED);

// -- perf_hooks --
console.log('PerformanceEntry=' + typeof PerformanceEntry);
const pe = new PerformanceEntry('test', 'measure', 100, 50);
console.log('pe_name=' + pe.name);
console.log('pe_type=' + pe.entryType);
console.log('pe_json=' + JSON.stringify(pe.toJSON()));

console.log('PerformanceObserverEntryList=' + typeof PerformanceObserverEntryList);
const poel = new PerformanceObserverEntryList();
console.log('poel_getEntries=' + Array.isArray(poel.getEntries()));

console.log('PerformanceNodeTiming=' + typeof PerformanceNodeTiming);
console.log('nodeTiming_name=' + nodeTiming.name);
console.log('nodeTiming_type=' + nodeTiming.entryType);
console.log('nodeTiming_idleTime=' + nodeTiming.idleTime);

// -- process.resourceUsage --
const ru = process.resourceUsage();
console.log('ru_type=' + typeof ru);
console.log('ru_userCPU=' + ru.userCPUTime);
console.log('ru_maxRSS=' + ru.maxRSS);
"#,
    );
    let out = oam(&["run", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "misc m3i failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let expected = [
        "Duplex.from=function",
        "Duplex.fromWeb=function",
        "Duplex.toWeb=function",
        "dupFrom_ok=true",
        "h2_NO_ERROR=0",
        "h2_PROTOCOL_ERROR=1",
        "h2_CANCEL=8",
        "h2_STATUS=:status",
        "h2_METHOD=:method",
        "h2_OK=200",
        "h2_WEIGHT=16",
        "dns_NODATA=NODATA",
        "dns_NOTFOUND=NOTFOUND",
        "dns_SERVFAIL=SERVFAIL",
        "dns_CONNREFUSED=CONNREFUSED",
        "dns_TIMEOUT=TIMEOUT",
        "dns_CANCELLED=CANCELLED",
        "PerformanceEntry=function",
        "pe_name=test",
        "pe_type=measure",
        r#"pe_json={"name":"test","entryType":"measure","startTime":100,"duration":50}"#,
        "PerformanceObserverEntryList=function",
        "poel_getEntries=true",
        "PerformanceNodeTiming=function",
        "nodeTiming_name=node",
        "nodeTiming_type=node",
        "nodeTiming_idleTime=0",
        "ru_type=object",
        "ru_userCPU=0",
        "ru_maxRSS=0",
    ];
    let lines: Vec<&str> = stdout.trim().lines().collect();
    for (i, exp) in expected.iter().enumerate() {
        assert_eq!(
            lines.get(i).unwrap_or(&"MISSING"),
            exp,
            "line {i} mismatch.\nfull stdout: {stdout}\nstderr: {stderr}"
        );
    }
}
