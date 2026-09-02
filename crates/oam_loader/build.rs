//! Build-time capture of the oxc crate versions that shape
//! `transpile_typescript` output, read from the workspace lockfile so
//! `oam_loader::transpile_fingerprint` can never drift from what is actually
//! linked. Same approach (and the same lockfile parser) as
//! `oam_engine/build.rs` uses for `process.versions`: a missing or duplicated
//! crate is a BUILD error, never a silently stale number.

fn main() {
    let lock_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    let lock = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", lock_path.display()));
    // (env var consumed by lib.rs, Cargo.lock package name)
    const CRATES: &[(&str, &str)] = &[
        ("OAM_OXC_TRANSFORMER_VERSION", "oxc_transformer"),
        ("OAM_OXC_CODEGEN_VERSION", "oxc_codegen"),
    ];
    for (env, package) in CRATES {
        let versions = lock_package_versions(&lock, package);
        let version = match versions.as_slice() {
            [one] => one.clone(),
            [] => panic!(
                "{package} not found in Cargo.lock -- transpile_fingerprint would go stale; \
                 update CRATES in oam_loader/build.rs to match the real dependency set"
            ),
            many => panic!(
                "{package} appears {} times in Cargo.lock ({many:?}) -- transpile_fingerprint \
                 would be ambiguous; deduplicate the dependency or pick explicitly in build.rs",
                many.len()
            ),
        };
        println!("cargo:rustc-env={env}={version}");
    }
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
