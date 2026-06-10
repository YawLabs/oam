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
    let bootstrap_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/bootstrap.js");
    println!("cargo:rerun-if-changed={bootstrap_path}");
    let bootstrap = std::fs::read_to_string(bootstrap_path).expect("bootstrap.js readable");

    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::initialize();

    let mut isolate = v8::Isolate::snapshot_creator(None, None);
    {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, v8::ContextOptions::default());
        {
            let scope = &mut v8::ContextScope::new(scope, context);
            let source = v8::String::new(scope, &bootstrap).expect("bootstrap fits");
            let script = v8::Script::compile(scope, source, None).expect("bootstrap compiles");
            script.run(scope).expect("bootstrap evaluates");
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
