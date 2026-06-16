#!/usr/bin/env python3
"""Append e2e tests from install, compile, and crypto worktrees."""
import pathlib

p = pathlib.Path("crates/oam_cli/tests/e2e.rs")
src = p.read_text(encoding="utf-8")

new_tests = r'''
// ── oam install ─────────────────────────────────────────────────────────

#[test]
fn install_missing_lockfile_fails() {
    // Run `oam install` in a temp dir with no package-lock.json.
    let tmp = write_temp("install-nolock/.keep", "");
    let dir = tmp.parent().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args(["install"])
        .current_dir(dir)
        .env("OAM_CACHE_DIR", dir.join("oam-cache"))
        .output()
        .expect("oam binary runs");
    assert!(
        !out.status.success(),
        "should fail without lockfile; stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("OAM-PKG0001"),
        "expected OAM-PKG0001 diagnostic; stderr: {stderr}"
    );
}

#[test]
fn install_parses_empty_lockfile_v3() {
    // A valid v3 lockfile with no deps should succeed with 0 packages.
    let lockfile = write_temp(
        "install-empty/package-lock.json",
        r#"{
        "name": "empty-project",
        "version": "1.0.0",
        "lockfileVersion": 3,
        "packages": {
            "": {
                "name": "empty-project",
                "version": "1.0.0"
            }
        }
    }"#,
    );
    let dir = lockfile.parent().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_oam"))
        .args(["install"])
        .current_dir(dir)
        .env("OAM_CACHE_DIR", dir.join("oam-cache"))
        .output()
        .expect("oam binary runs");
    assert!(
        out.status.success(),
        "should succeed with empty lockfile; stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("0 package(s)"),
        "expected 0 packages; stderr: {stderr}"
    );
}

// ── oam compile ──

#[test]
fn compile_produces_standalone_binary_that_runs() {
    let entry = write_temp("compile_hello.js", "console.log('hello from compiled oam');");
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let output = std::env::temp_dir().join(format!(
        "oam-compile-test-{}-{nanos}{ext}",
        std::process::id()
    ));
    // Compile
    let compile_out = oam(&[
        "compile",
        entry.to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(
        compile_out.status.success(),
        "oam compile failed: stdout={} stderr={}",
        String::from_utf8_lossy(&compile_out.stdout),
        String::from_utf8_lossy(&compile_out.stderr)
    );
    // Run the compiled binary -- it should execute the embedded JS
    // without any arguments.
    let run_out = std::process::Command::new(&output)
        .output()
        .expect("compiled binary runs");
    let _ = std::fs::remove_file(&output);
    let stdout = String::from_utf8_lossy(&run_out.stdout);
    assert!(
        run_out.status.success(),
        "compiled binary failed: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&run_out.stderr)
    );
    assert!(
        stdout.contains("hello from compiled oam"),
        "expected greeting in stdout, got: {stdout}"
    );
}

#[test]
fn compile_binary_passes_script_args() {
    let entry = write_temp(
        "compile_args.js",
        "console.log('args=' + process.argv.slice(1).join(','));",
    );
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let output = std::env::temp_dir().join(format!(
        "oam-compile-args-{}-{nanos}{ext}",
        std::process::id()
    ));
    let compile_out = oam(&[
        "compile",
        entry.to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(compile_out.status.success());
    let run_out = std::process::Command::new(&output)
        .args(["--", "foo", "bar"])
        .output()
        .expect("compiled binary runs");
    let _ = std::fs::remove_file(&output);
    let stdout = String::from_utf8_lossy(&run_out.stdout);
    assert!(
        stdout.contains("args=foo,bar"),
        "expected script args, got: {stdout}"
    );
}

#[test]
fn compile_missing_entry_fails() {
    let output = std::env::temp_dir().join("oam-compile-missing-output.exe");
    let out = oam(&["compile", "/nonexistent/file.js", "--output", output.to_str().unwrap()]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not read"),
        "expected read error, got: {stderr}"
    );
}

// ── Wave 9: Crypto Phase A ──────────────────────────────────────────

#[test]
fn crypto_ec_jwk_import_export() {
    let stdout = run_ok(
        "ec_jwk_test.cjs",
        r#"
const crypto = require("crypto");

// Generate an EC P-256 key pair
const { publicKey: pubPem, privateKey: privPem } = crypto.generateKeyPairSync("ec", {
  namedCurve: "P-256",
  publicKeyEncoding: { type: "spki", format: "pem" },
  privateKeyEncoding: { type: "pkcs8", format: "pem" },
});

// Create KeyObjects and export to JWK
const privKey = crypto.createPrivateKey(privPem);
const pubKey = crypto.createPublicKey(pubPem);

const privJwk = privKey.export({ format: "jwk" });
const pubJwk = pubKey.export({ format: "jwk" });

console.log("priv_kty=" + privJwk.kty);
console.log("priv_crv=" + privJwk.crv);
console.log("priv_has_x=" + (typeof privJwk.x === "string" && privJwk.x.length > 0));
console.log("priv_has_y=" + (typeof privJwk.y === "string" && privJwk.y.length > 0));
console.log("priv_has_d=" + (typeof privJwk.d === "string" && privJwk.d.length > 0));

console.log("pub_kty=" + pubJwk.kty);
console.log("pub_crv=" + pubJwk.crv);
console.log("pub_has_x=" + (typeof pubJwk.x === "string" && pubJwk.x.length > 0));
console.log("pub_has_y=" + (typeof pubJwk.y === "string" && pubJwk.y.length > 0));
console.log("pub_no_d=" + (pubJwk.d === undefined));

// x,y should match between pub and priv
console.log("x_match=" + (privJwk.x === pubJwk.x));
console.log("y_match=" + (privJwk.y === pubJwk.y));

// Round-trip: import JWK back and sign/verify
const privKey2 = crypto.createPrivateKey({ format: "jwk", key: privJwk });
const pubKey2 = crypto.createPublicKey({ format: "jwk", key: pubJwk });

const signer = crypto.createSign("SHA256");
signer.update("ec jwk round trip");
const sig = signer.sign(privKey2.export());

const verifier = crypto.createVerify("SHA256");
verifier.update("ec jwk round trip");
const valid = verifier.verify(pubKey2.export(), sig);
console.log("ec_jwk_round_trip=" + valid);
"#,
    );
    assert!(stdout.contains("priv_kty=EC"), "stdout: {stdout}");
    assert!(stdout.contains("priv_crv=P-256"), "stdout: {stdout}");
    assert!(stdout.contains("priv_has_x=true"), "stdout: {stdout}");
    assert!(stdout.contains("priv_has_y=true"), "stdout: {stdout}");
    assert!(stdout.contains("priv_has_d=true"), "stdout: {stdout}");
    assert!(stdout.contains("pub_kty=EC"), "stdout: {stdout}");
    assert!(stdout.contains("pub_crv=P-256"), "stdout: {stdout}");
    assert!(stdout.contains("pub_no_d=true"), "stdout: {stdout}");
    assert!(stdout.contains("x_match=true"), "stdout: {stdout}");
    assert!(stdout.contains("y_match=true"), "stdout: {stdout}");
    assert!(
        stdout.contains("ec_jwk_round_trip=true"),
        "stdout: {stdout}"
    );
}

#[test]
fn crypto_ec_jwk_subtle_import() {
    let file = write_temp(
        "ec_jwk_subtle.mjs",
        r#"
import crypto from "node:crypto";
const { subtle } = crypto.webcrypto || crypto;

// Generate EC P-256 keys
const { publicKey: pubPem, privateKey: privPem } = crypto.generateKeyPairSync("ec", {
  namedCurve: "P-256",
  publicKeyEncoding: { type: "spki", format: "pem" },
  privateKeyEncoding: { type: "pkcs8", format: "pem" },
});

// Export to JWK via KeyObject
const privKey = crypto.createPrivateKey(privPem);
const pubKey = crypto.createPublicKey(pubPem);
const privJwk = privKey.export({ format: "jwk" });
const pubJwk = pubKey.export({ format: "jwk" });

// Import into subtle via JWK
const subtlePriv = await subtle.importKey(
  "jwk", privJwk,
  { name: "ECDSA", namedCurve: "P-256" },
  true, ["sign"]
);
const subtlePub = await subtle.importKey(
  "jwk", pubJwk,
  { name: "ECDSA", namedCurve: "P-256" },
  true, ["verify"]
);

console.log("priv_type=" + subtlePriv.type);
console.log("pub_type=" + subtlePub.type);

// Sign and verify through subtle
const data = new TextEncoder().encode("subtle ec jwk test");
const sig = await subtle.sign({ name: "ECDSA", hash: "SHA-256" }, subtlePriv, data);
console.log("sig_len=" + new Uint8Array(sig).length);

const valid = await subtle.verify({ name: "ECDSA", hash: "SHA-256" }, subtlePub, sig, data);
console.log("verify=" + valid);
console.log("all_ok=true");
"#,
    );
    let output = oam(&["run", file.to_str().unwrap(), "--no-check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "test failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("priv_type=private"), "stdout: {stdout}");
    assert!(stdout.contains("pub_type=public"), "stdout: {stdout}");
    assert!(stdout.contains("verify=true"), "stdout: {stdout}");
    assert!(stdout.contains("all_ok=true"), "stdout: {stdout}");
}

#[test]
fn crypto_rsa_pss_sign_verify() {
    let stdout = run_ok(
        "rsa_pss_test.cjs",
        r#"
const crypto = require("crypto");

// Generate RSA key pair
const { publicKey, privateKey } = crypto.generateKeyPairSync("rsa", {
  modulusLength: 2048,
  publicKeyEncoding: { type: "spki", format: "pem" },
  privateKeyEncoding: { type: "pkcs8", format: "pem" },
});

// Sign with RSA-PSS (padding=6 is RSA_PKCS1_PSS_PADDING)
const signer = crypto.createSign("SHA256");
signer.update("rsa pss test data");
const sig = signer.sign({
  key: privateKey,
  padding: crypto.constants.RSA_PKCS1_PSS_PADDING,
  saltLength: 32,
});
console.log("pss_sig_len=" + sig.length);

// Verify with RSA-PSS
const verifier = crypto.createVerify("SHA256");
verifier.update("rsa pss test data");
const valid = verifier.verify({
  key: publicKey,
  padding: crypto.constants.RSA_PKCS1_PSS_PADDING,
  saltLength: 32,
}, sig);
console.log("pss_verify=" + valid);

// Verify fails with wrong data
const verifier2 = crypto.createVerify("SHA256");
verifier2.update("wrong data");
const invalid = verifier2.verify({
  key: publicKey,
  padding: crypto.constants.RSA_PKCS1_PSS_PADDING,
  saltLength: 32,
}, sig);
console.log("pss_wrong_data=" + invalid);
"#,
    );
    assert!(stdout.contains("pss_sig_len=256"), "stdout: {stdout}");
    assert!(stdout.contains("pss_verify=true"), "stdout: {stdout}");
    assert!(stdout.contains("pss_wrong_data=false"), "stdout: {stdout}");
}

#[test]
fn crypto_subtle_rsa_pss() {
    let file = write_temp(
        "subtle_rsa_pss.mjs",
        r#"
import crypto from "node:crypto";
const { subtle } = crypto.webcrypto || crypto;

// Generate RSA keys as JWK
const { publicKey, privateKey } = crypto.generateKeyPairSync("rsa", {
  modulusLength: 2048,
  publicKeyEncoding: { type: "spki", format: "jwk" },
  privateKeyEncoding: { type: "pkcs8", format: "jwk" },
});

// Import into subtle via JWK
const privKey = await subtle.importKey(
  "jwk", privateKey,
  { name: "RSA-PSS", hash: "SHA-256" },
  false, ["sign"]
);
const pubKey = await subtle.importKey(
  "jwk", publicKey,
  { name: "RSA-PSS", hash: "SHA-256" },
  false, ["verify"]
);

console.log("priv_type=" + privKey.type);
console.log("pub_type=" + pubKey.type);

// Sign with RSA-PSS
const data = new TextEncoder().encode("subtle pss test");
const sig = await subtle.sign(
  { name: "RSA-PSS", saltLength: 32 },
  privKey, data
);
console.log("sig_len=" + new Uint8Array(sig).length);

// Verify
const valid = await subtle.verify(
  { name: "RSA-PSS", saltLength: 32 },
  pubKey, sig, data
);
console.log("verify=" + valid);

// Verify fails with wrong data
const wrongData = new TextEncoder().encode("wrong");
const invalid = await subtle.verify(
  { name: "RSA-PSS", saltLength: 32 },
  pubKey, sig, wrongData
);
console.log("wrong_verify=" + invalid);
console.log("all_ok=true");
"#,
    );
    let output = oam(&["run", file.to_str().unwrap(), "--no-check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "test failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("priv_type=private"), "stdout: {stdout}");
    assert!(stdout.contains("pub_type=public"), "stdout: {stdout}");
    assert!(stdout.contains("sig_len=256"), "stdout: {stdout}");
    assert!(stdout.contains("verify=true"), "stdout: {stdout}");
    assert!(stdout.contains("wrong_verify=false"), "stdout: {stdout}");
    assert!(stdout.contains("all_ok=true"), "stdout: {stdout}");
}
'''

# Append to end of file
src = src.rstrip() + "\n" + new_tests.strip() + "\n"
p.write_text(src, encoding="utf-8")
print("OK -- appended 9 e2e tests (2 install + 3 compile + 4 crypto)")
