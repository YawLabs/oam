//! Build-time startup-snapshot generation (plan §2.4, stage one).
//!
//! Evaluates js/bootstrap.js into a fresh V8 context and serializes the
//! heap (FunctionCodeHandling::Keep — compiled code travels too), so
//! JsRuntime::new() deserializes a ready context instead of parsing and
//! evaluating JS at every startup. The snapshotted context contains ONLY
//! pure JS (bootstrap looks __oam up at call time); native bindings are
//! installed after restore, which keeps external-reference bookkeeping out
//! of the blob entirely. The win grows as js/ grows (ECMA-429 surface,
//! node: shims land here).

/// Real dependency versions for `process.versions`, read from the workspace
/// lockfile at build time so the published numbers can never drift from what
/// is actually linked. A missing crate is a BUILD error, not a silent
/// absence: dropping a dependency must consciously update this list. Only
/// libraries oam genuinely ships are listed -- Node's dependency names (uv,
/// openssl, llhttp, ares, napi, modules, ...) are deliberately never
/// published for software that is not in the binary
/// (docs/node-divergences.md divergence 1).
fn dep_versions_json() -> String {
    // (process.versions key, Cargo.lock package name), alphabetical by key.
    const DEPS: &[(&str, &str)] = &[
        ("brotli", "brotli"),
        ("flate2", "flate2"),
        ("h2", "h2"),
        ("hickory", "hickory-resolver"),
        ("hyper", "hyper"),
        ("oxc", "oxc_parser"),
        ("reqwest", "reqwest"),
        ("ring", "ring"),
        ("rustls", "rustls"),
        ("tokio", "tokio"),
        ("url", "url"),
    ];
    let lock_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    let lock = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", lock_path.display()));
    let mut json = String::from("{");
    for (i, (key, package)) in DEPS.iter().enumerate() {
        let versions = lock_package_versions(&lock, package);
        let version = match versions.as_slice() {
            [one] => one,
            [] => panic!(
                "{package} not found in Cargo.lock -- process.versions.{key} would go stale; \
                 update DEPS in oam_engine/build.rs to match the real dependency set"
            ),
            many => panic!(
                "{package} appears {} times in Cargo.lock ({many:?}) -- process.versions.{key} \
                 would be ambiguous; deduplicate the dependency or pick explicitly in build.rs",
                many.len()
            ),
        };
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!("\"{key}\":\"{version}\""));
    }
    json.push('}');
    json
}

/// Every version of `package` pinned in the lockfile text (lockfile v3/v4
/// shape: a `[[package]]` block with `name = "..."` then `version = "..."`).
fn lock_package_versions(lock: &str, package: &str) -> Vec<String> {
    let needle = format!("name = \"{package}\"");
    let mut versions = Vec::new();
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() != needle {
            continue;
        }
        for follow in lines.by_ref() {
            let follow = follow.trim();
            if let Some(v) = follow.strip_prefix("version = \"") {
                versions.push(v.trim_end_matches('"').to_string());
                break;
            }
            if follow.starts_with("[[") {
                break;
            }
        }
    }
    versions
}

fn main() {
    println!("cargo:rustc-env=OAM_DEP_VERSIONS={}", dep_versions_json());

    // V8 snapshots are architecture-specific, and this build script runs on
    // the HOST: cross-compiling would silently embed a host-arch blob that
    // crashes the target binary at first isolate creation. Fail loudly until
    // target-arch snapshot generation is implemented (the `oam compile`
    // cross-target workstream owns that).
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch != std::env::consts::ARCH {
        panic!(
            "oam cross-compilation is not supported yet: the startup snapshot \
             must be generated on the target architecture (host: {}, target: {})",
            std::env::consts::ARCH,
            target_arch
        );
    }

    // Evaluated in order: web surface first, then the node: compat layer
    // (node_compat.js installs Buffer/TextEncoder globals and the builtin
    // factory registry -- all pure JS, natives looked up at call time),
    // then the test runner (registers the oam:test factory on that
    // registry, so it must come after node_compat), then the permissions
    // module (registers oam:permissions factory, also after node_compat).
    //
    // Entries with Some(specifier) are the vendored Node streams port
    // (docs/design/streams-port.md section 3.2): their bodies are evaluated
    // wrapped in `globalThis.__oamVendor.define(specifier, function (require,
    // module, exports, process, primordials) { <verbatim body> })`, keyed on
    // Node's own specifier strings so the upstream files run byte-identical
    // (zero path rewrites). define() only STORES the factory at snapshot
    // time; bodies execute at the first runtime require. The loader prelude
    // must precede every define; vendored order past that is cosmetic (kept
    // in dependency order for readability).
    let js_files: &[(&str, Option<&str>)] = &[
        (
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/bootstrap.js"),
            None,
        ),
        (
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/node_compat.js"),
            None,
        ),
        // Vendored streams port: loader + primordials run as plain scripts...
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/oam-shims/loader-prelude.js"
            ),
            None,
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/oam-shims/primordials.js"
            ),
            None,
        ),
        // ...shims + upstream bodies are define()d under Node's specifiers.
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/oam-shims/errors.js"
            ),
            Some("internal/errors"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/oam-shims/validators.js"
            ),
            Some("internal/validators"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/oam-shims/util.js"
            ),
            Some("internal/util"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/utils.js"
            ),
            Some("internal/streams/utils"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/state.js"
            ),
            Some("internal/streams/state"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/legacy.js"
            ),
            Some("internal/streams/legacy"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/from.js"
            ),
            Some("internal/streams/from"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/destroy.js"
            ),
            Some("internal/streams/destroy"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/end-of-stream.js"
            ),
            Some("internal/streams/end-of-stream"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/add-abort-signal.js"
            ),
            Some("internal/streams/add-abort-signal"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/readable.js"
            ),
            Some("internal/streams/readable"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/writable.js"
            ),
            Some("internal/streams/writable"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/duplex.js"
            ),
            Some("internal/streams/duplex"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/transform.js"
            ),
            Some("internal/streams/transform"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/passthrough.js"
            ),
            Some("internal/streams/passthrough"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/duplexify.js"
            ),
            Some("internal/streams/duplexify"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/duplexpair.js"
            ),
            Some("internal/streams/duplexpair"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/pipeline.js"
            ),
            Some("internal/streams/pipeline"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/compose.js"
            ),
            Some("internal/streams/compose"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/internal/streams/operators.js"
            ),
            Some("internal/streams/operators"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/stream.js"
            ),
            Some("stream"),
        ),
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/node-streams/stream/promises.js"
            ),
            Some("stream/promises"),
        ),
        // seal.js: closes __oamVendor.define and freezes the registry -- MUST
        // stay the last define-emitting entry of the vendor block.
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/oam-shims/seal.js"
            ),
            None,
        ),
        // register.js: installs the vendored Node v22 port as THE stream
        // factories (slices 3+5; the legacy streams and OAM_LEGACY_STREAMS
        // switch are gone). After seal (it only assigns __oamNode
        // factories), before everything that may consume streams.
        (
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../js/vendor/oam-shims/register.js"
            ),
            None,
        ),
        // streams.js: TextDecoderStream needs node_compat's TextDecoder.
        (
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/streams.js"),
            None,
        ),
        (
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/test_runner.js"),
            None,
        ),
        // permissions.js: oam:permissions factory (after node_compat).
        (
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/permissions.js"),
            None,
        ),
        // ai.js: oam:ai factory (SSE parser, streaming chat, tool-use loop).
        (concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/ai.js"), None),
        // mcp.js: oam:mcp factory (MCP server primitives, JSON-RPC, transports).
        (
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/mcp.js"),
            None,
        ),
        // undici.js: native undici-API shim (fetch/request/Agent/dispatchers)
        // shadowing the npm package; backed by oam's fetch.
        (
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/undici.js"),
            None,
        ),
    ];
    // Require-closure check: every require('...') string literal inside a
    // wrapped vendored body must resolve at runtime -- to another define()d
    // specifier, one of loader-prelude's inline shim ids, or a public builtin
    // the __oamNode fallback serves. Without this, a re-vendor that adds a
    // new upstream require (or a new file missing from the list above) builds
    // green and fails only at first runtime require, with a misleading
    // builtin-registry error.
    let defined: std::collections::HashSet<&str> =
        js_files.iter().filter_map(|(_, wrap)| *wrap).collect();
    let prelude_inline = [
        "internal/util/debuglog",
        "internal/util/types",
        "internal/buffer",
        "internal/event_target",
        "internal/abort_controller",
        "internal/events/abort_listener",
        "internal/blob",
        "internal/assert",
        "internal/webstreams/adapters",
    ];
    let public_fallback = ["events", "buffer", "string_decoder", "async_hooks"];
    for (path, wrap) in js_files.iter() {
        if wrap.is_none() {
            continue;
        }
        let body = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        for quote in ['\'', '"'] {
            let needle = format!("require({quote}");
            let mut rest: &str = &body;
            while let Some(pos) = rest.find(&needle) {
                let after = &rest[pos + needle.len()..];
                let Some(end) = after.find(quote) else { break };
                let id = &after[..end];
                if !(defined.contains(id)
                    || prelude_inline.contains(&id)
                    || public_fallback.contains(&id))
                {
                    panic!(
                        "{path}: require({id:?}) has no vendor define, prelude shim, \
                         or public fallback -- update js_files / loader-prelude"
                    );
                }
                rest = &after[end..];
            }
        }
    }

    let sources: Vec<(String, String)> = js_files
        .iter()
        .map(|(path, wrap)| {
            println!("cargo:rerun-if-changed={path}");
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let text = match wrap {
                Some(id) => format!(
                    "globalThis.__oamVendor.define({id:?}, \
                     function (require, module, exports, process, primordials) {{\n{text}\n}});"
                ),
                None => text,
            };
            (path.to_string(), text)
        })
        .collect();

    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();

    let mut isolate = v8::Isolate::snapshot_creator(None, None);
    {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, v8::ContextOptions::default());
        {
            let scope = &mut v8::ContextScope::new(scope, context);
            for (path, text) in &sources {
                let source = v8::String::new(scope, text)
                    .unwrap_or_else(|| panic!("{path}: too long for V8 string"));
                let script = v8::Script::compile(scope, source, None)
                    .unwrap_or_else(|| panic!("{path}: does not compile"));
                script
                    .run(scope)
                    .unwrap_or_else(|| panic!("{path}: failed to evaluate"));
            }
        }
        scope.set_default_context(context);
    }
    let blob = isolate
        .create_blob(v8::FunctionCodeHandling::Keep)
        .expect("snapshot serializes");

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"))
        .join("oam_snapshot.bin");
    std::fs::write(&out, &*blob).expect("snapshot written");
    println!("cargo:warning=oam startup snapshot: {} bytes", blob.len());
}
