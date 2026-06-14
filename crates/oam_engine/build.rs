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

fn main() {
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
    let js_files = [
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/bootstrap.js"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/node_compat.js"),
        // streams.js: TextDecoderStream needs node_compat's TextDecoder.
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/streams.js"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/test_runner.js"),
        // permissions.js: oam:permissions factory (after node_compat).
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/permissions.js"),
        // ai.js: oam:ai factory (SSE parser, streaming chat, tool-use loop).
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/ai.js"),
    ];
    let sources: Vec<(String, String)> = js_files
        .iter()
        .map(|path| {
            println!("cargo:rerun-if-changed={path}");
            (
                path.to_string(),
                std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}")),
            )
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
