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
    // prefixed or bare. (worker_threads is the long-horizon example —
    // earlier picks kept SHIPPING and flipping this test.)
    std::fs::write(
        proj.join("builtin_main.ts"),
        "import 'node:worker_threads';",
    )
    .unwrap();
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
        stderr.contains("wave 1 ships"),
        "gate lists wave 1: {stderr}"
    );

    std::fs::write(proj.join("bare_builtin.ts"), "import 'http';").unwrap();
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
    // rmdir on a FILE throws (ENOENT on Windows) and deletes nothing.
    assert_eq!(lines[1], "ENOENT true");

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
