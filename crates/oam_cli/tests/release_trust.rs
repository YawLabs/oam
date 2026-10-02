//! `oam self-update` through the real binary, against a local fixture server.
//!
//! The success path is covered in src/self_update.rs, where a test trust root
//! can be injected. What only the binary can show is that its COMPILED-IN
//! trust root is the one in force: a release signed by a throwaway key (the
//! fixtures' t1) is refused, the installed file survives byte for byte, and
//! the refusal names both the key that signed and the keys that may.
//!
//! Not named self_update.rs: Windows' installer detection asks for elevation
//! before running any unmanifested exe whose name contains "update", and a
//! test binary is named after its file (os error 740, the test never runs).

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/self_update");
const OLD: &[u8] = b"the installed oam, which must survive any failure\n";

fn fixture(rel: &str) -> Vec<u8> {
    std::fs::read(format!("{FIX}/{rel}")).unwrap_or_else(|e| panic!("{FIX}/{rel}: {e}"))
}

/// One response per connection; unknown paths 404. Lives until the test
/// process ends.
fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("");
            let resp = match routes.iter().find(|(p, _)| p == path) {
                Some((_, body)) => {
                    let mut r = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    r.extend_from_slice(body);
                    r
                }
                None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            };
            let _ = stream.write_all(&resp);
        }
    });
    base
}

struct Scene {
    dir: PathBuf,
}

impl Scene {
    fn new(name: &str) -> Scene {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "oam-self-update-bin-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(exe_name()), OLD).unwrap();
        Scene { dir }
    }

    fn assert_untouched(&self) {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![exe_name().to_string()],
            "no temp or backup file left behind"
        );
        assert_eq!(std::fs::read(self.dir.join(exe_name())).unwrap(), OLD);
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn exe_name() -> &'static str {
    if cfg!(windows) { "oam.exe" } else { "oam" }
}

fn self_update(dir: &Path, base: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oam"))
        .arg("self-update")
        .args(args)
        .env("OAM_INSTALL_DIR", dir)
        .env("OAM_SELF_UPDATE_URL", base)
        .output()
        .unwrap()
}

/// The t1 public key's fingerprint, as `ssh-keygen -lf` prints it.
const T1_FP: &str = "SHA256:HGTWpLiMBRFgpurFRx8SMBmCf5kXKrDVpMv+0IhBWhE";
/// release-keys/README.md's k1.
const K1_FP: &str = "SHA256:zB7Aq4Ky/U90VJ4sAEp0e2A65KfQpiyQXJI4FuT2oss";

#[test]
fn a_release_signed_by_a_non_release_key_is_refused_and_nothing_changes() {
    let base = serve(vec![
        ("/RELEASE-MANIFEST".into(), fixture("manifest-v0.18.0")),
        ("/RELEASE-MANIFEST.sig".into(), fixture("cases/valid.sig")),
        ("/SHA256SUMS".into(), fixture("SHA256SUMS")),
        (
            "/oam-x86_64-pc-windows-msvc.exe".into(),
            fixture("payload.bin"),
        ),
        (
            "/oam-aarch64-pc-windows-msvc.exe".into(),
            fixture("payload.bin"),
        ),
        (
            "/oam-x86_64-unknown-linux-gnu".into(),
            fixture("payload.bin"),
        ),
        ("/oam-aarch64-apple-darwin".into(), fixture("payload.bin")),
        ("/oam-x86_64-apple-darwin".into(), fixture("payload.bin")),
    ]);
    for args in [
        &["--version", "v0.18.0"][..],
        &["--version", "v0.18.0", "--dry-run"],
    ] {
        let s = Scene::new("untrusted");
        let out = self_update(&s.dir, &base, args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?}: must fail\n{stderr}");
        assert!(stderr.contains("not a release key"), "{args:?}: {stderr}");
        assert!(
            stderr.contains(T1_FP),
            "{args:?}: names the signing key\n{stderr}"
        );
        assert!(
            stderr.contains(K1_FP),
            "{args:?}: names the trusted keys\n{stderr}"
        );
        assert!(stderr.contains("was left as it was"), "{args:?}: {stderr}");
        s.assert_untouched();
    }
}

#[test]
fn a_presigning_release_whose_sums_are_not_the_pinned_ones_is_refused() {
    // v0.17.1 predates signing; the compiled-in table pins the REAL release's
    // SHA256SUMS digest, which the fixture's is not.
    let base = serve(vec![("/SHA256SUMS".into(), fixture("SHA256SUMS"))]);
    let s = Scene::new("pinned");
    let out = self_update(&s.dir, &base, &["--version", "v0.17.1"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("d13c587307519995fabe05a999abdfae9292921b261e5808082b53e7acbe79a9"),
        "the expected (pinned) digest is printed\n{stderr}"
    );
    s.assert_untouched();
}

#[test]
fn a_signed_era_tag_without_a_manifest_is_refused() {
    let base = serve(vec![("/SHA256SUMS".into(), fixture("SHA256SUMS"))]);
    let s = Scene::new("nomanifest");
    let out = self_update(&s.dir, &base, &["--version", "v0.18.0"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("has no RELEASE-MANIFEST"), "{stderr}");
    s.assert_untouched();
}

#[test]
fn bad_arguments_fail_before_any_network() {
    let s = Scene::new("args");
    // Port 9 (discard) on loopback: nothing should ever be asked of it.
    let out = self_update(&s.dir, "http://127.0.0.1:9", &["--version", "latest"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a release tag"));
    let out = self_update(&s.dir, "http://127.0.0.1:9", &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs --version"));
    s.assert_untouched();
}
