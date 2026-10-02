//! `oam self-update`: replace this oam with a release whose files are signed by
//! a committed release key.
//!
//! The chain, in order (signing plan 6.3; scripts/lib/signing.sh holds the same
//! rules for the release side, and the installers check them too):
//!
//!   1. Resolve the tag ONCE: `--version`, or the `/releases/latest` redirect.
//!      Every file then comes from that one tag's `/download/<tag>/`, so a
//!      "latest" that moves mid-update cannot mix two releases.
//!   2. From `FIRST_MANIFEST_SIG_TAG` on, fetch RELEASE-MANIFEST and its .sig
//!      and verify the SSHSIG against the key set compiled into this binary
//!      (`release-keys/allowed_signers` + `ranges`, by include_str!): a trusted
//!      principal, namespace `oam-release`, version 1, sha256/sha512, the
//!      embedded public key equal to the pinned one, the signed tag equal to
//!      the requested one, and inside that key's range. Before the cutoff no
//!      release was signed, so the SHA-256 of its published SHA256SUMS must
//!      equal the one pinned in `release-keys/presigning-sums`; a pre-cutoff
//!      tag not in that bounded table is refused.
//!   3. Anti-rollback: without `--version`, the signed tag must not be older
//!      than this binary. The version compared is the SIGNED one, never what a
//!      download says about itself.
//!   4. Stream the asset for this target into a temp file IN the install dir,
//!      hashing as it lands, and compare with the verified manifest line.
//!   5. Rename it into place (Windows: move the running exe aside first, as
//!      install.ps1 does), then run `--version` as a smoke. A failed smoke puts
//!      the previous binary back.
//!
//! Any failure deletes the temp file, leaves the installed binary as it was,
//! and exits non-zero with the expected and actual key fingerprints or digests.
//! There is no flag that skips a check: a skip knob is exactly what an attacker
//! able to serve a fake release would ask a user to set.

use sha2::{Digest as _, Sha256};
use ssh_key::{HashAlg, PublicKey, SshSig};
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// The committed trust root, byte-for-byte. Compiled in on purpose: a trust
/// root read at runtime is one a local attacker (or a stray env var) could
/// swap. The tests below assert these parse and agree with each other.
const ALLOWED_SIGNERS: &str = include_str!("../../../release-keys/allowed_signers");
const RANGES: &str = include_str!("../../../release-keys/ranges");
const PRESIGNING_SUMS: &str = include_str!("../../../release-keys/presigning-sums");

const NAMESPACE: &str = "oam-release";
const MANIFEST_HEADER: &str = "oam-release-manifest v1";
const PRINCIPAL_PREFIX: &str = "oam-release-";
/// The first release that carries RELEASE-MANIFEST(.sig). From here on a
/// missing manifest is a refusal, never a fallback to SHA256SUMS.
const FIRST_MANIFEST_SIG_TAG: Tag = Tag {
    major: 0,
    minor: 18,
    patch: 0,
};
const RELEASES_ROOT: &str = "https://github.com/YawLabs/oam/releases";

/// RELEASE-MANIFEST, its .sig and SHA256SUMS are a few KB; anything far past
/// that is not one of them, and is not worth holding in memory to find out.
const META_CAP: u64 = 1 << 20;
/// THIRD_PARTY_LICENSES.md is ~0.5 MB today.
const LICENSE_CAP: u64 = 16 << 20;
/// A release binary is ~100 MB today. The cap only stops an endless body.
const ASSET_CAP: u64 = 2 << 30;

// --- tags ----------------------------------------------------------------------

/// A plain `vMAJOR.MINOR.PATCH` tag, the only shape this repo releases.
/// Ordered numerically per field (derived, in field order): v0.10.0 > v0.9.0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Tag {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Tag {
    /// `vX.Y.Z`, digits only in each field (signing.sh's `_rs_plain_tag`).
    pub(crate) fn parse(s: &str) -> Option<Tag> {
        let mut fields = s.strip_prefix('v')?.split('.');
        let mut next = || -> Option<u64> {
            let f = fields.next()?;
            if f.is_empty() || f.len() > 9 || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            f.parse().ok()
        };
        let tag = Tag {
            major: next()?,
            minor: next()?,
            patch: next()?,
        };
        fields.next().is_none().then_some(tag)
    }

    /// A user's `--version`: the tag, with the leading `v` optional.
    fn parse_user(s: &str) -> Option<Tag> {
        Tag::parse(s).or_else(|| Tag::parse(&format!("v{s}")))
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

// --- trust root ----------------------------------------------------------------

struct TrustedKey {
    /// The principal's suffix: "k1" for oam-release-k1, the name ranges uses.
    id: String,
    key: PublicKey,
}

struct KeyRange {
    id: String,
    from: Tag,
    /// None: open-ended ("-").
    to: Option<Tag>,
}

/// allowed_signers + ranges + presigning-sums, parsed and linted with the
/// rules of signing.sh's `release_keys_lint`. Hand-edited files must never
/// degrade into "trust everything" or "nothing verifies, skip", so every
/// malformed line is an error, not a skipped line.
pub(crate) struct TrustRoot {
    keys: Vec<TrustedKey>,
    ranges: Vec<KeyRange>,
    presigning: Vec<(Tag, [u8; 32])>,
}

/// The non-comment, non-blank lines (signing.sh: `!/^[[:space:]]*(#|$)/`).
fn data_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|l| {
        let t = l.trim_start();
        !t.is_empty() && !t.starts_with('#')
    })
}

impl TrustRoot {
    pub(crate) fn embedded() -> Result<TrustRoot, String> {
        TrustRoot::parse(ALLOWED_SIGNERS, RANGES, PRESIGNING_SUMS)
    }

    pub(crate) fn parse(
        allowed: &str,
        ranges: &str,
        presigning: &str,
    ) -> Result<TrustRoot, String> {
        let mut keys: Vec<TrustedKey> = Vec::new();
        for line in data_lines(allowed) {
            let mut f = line.split_whitespace();
            let (principal, opts, ktype, blob) = (f.next(), f.next(), f.next(), f.next());
            let (Some(principal), Some(opts), Some(ktype), Some(blob)) =
                (principal, opts, ktype, blob)
            else {
                return Err(format!(
                    "allowed_signers: '{line}' is not '<principal> namespaces=\"{NAMESPACE}\" <type> <base64>'"
                ));
            };
            // Anything after the blob is the key comment; ssh-keygen ignores it.
            let id = principal.strip_prefix(PRINCIPAL_PREFIX).ok_or_else(|| {
                format!("allowed_signers: principal '{principal}' does not start with '{PRINCIPAL_PREFIX}'")
            })?;
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return Err(format!(
                    "allowed_signers: principal '{principal}' must be a single literal name (no patterns or lists)"
                ));
            }
            if opts != format!("namespaces=\"{NAMESPACE}\"") {
                return Err(format!(
                    "allowed_signers: '{principal}' must carry namespaces=\"{NAMESPACE}\" as its only option (got '{opts}')"
                ));
            }
            let algorithm = match ktype {
                "ssh-ed25519" => ssh_key::Algorithm::Ed25519,
                "sk-ssh-ed25519@openssh.com" => ssh_key::Algorithm::SkEd25519,
                _ => {
                    return Err(format!(
                        "allowed_signers: '{principal}' has key type '{ktype}' -- release keys are ssh-ed25519 (or sk-ssh-ed25519@openssh.com)"
                    ));
                }
            };
            let key = PublicKey::from_openssh(&format!("{ktype} {blob}")).map_err(|e| {
                format!("allowed_signers: '{principal}' has no valid key blob ({e})")
            })?;
            if key.algorithm() != algorithm {
                return Err(format!(
                    "allowed_signers: '{principal}' says {ktype} but its blob is {}",
                    key.algorithm()
                ));
            }
            if keys.iter().any(|k| k.id == id) {
                return Err(format!(
                    "allowed_signers: principal '{principal}' appears twice"
                ));
            }
            keys.push(TrustedKey {
                id: id.to_string(),
                key,
            });
        }
        if keys.is_empty() {
            return Err("allowed_signers holds no key -- nothing could verify a release".into());
        }

        let mut parsed_ranges: Vec<KeyRange> = Vec::new();
        for line in data_lines(ranges) {
            let f: Vec<&str> = line.split_whitespace().collect();
            let [id, from, to] = f[..] else {
                return Err(format!(
                    "ranges: '{line}' must be exactly '<id> <from-tag> <to-tag|->'"
                ));
            };
            let from_tag = Tag::parse(from).ok_or_else(|| {
                format!("ranges: '{id}' starts at '{from}', not a plain vX.Y.Z tag")
            })?;
            let to_tag = if to == "-" {
                None
            } else {
                let t = Tag::parse(to).ok_or_else(|| {
                    format!("ranges: '{id}' ends at '{to}', neither '-' nor a plain vX.Y.Z tag")
                })?;
                if t < from_tag {
                    return Err(format!("ranges: '{id}' runs backwards ({from} > {to})"));
                }
                Some(t)
            };
            if parsed_ranges.iter().any(|r| r.id == id) {
                return Err(format!(
                    "ranges: '{id}' has more than one line -- one range per key, or a forgotten open line keeps a closed key alive"
                ));
            }
            parsed_ranges.push(KeyRange {
                id: id.to_string(),
                from: from_tag,
                to: to_tag,
            });
        }

        let mut pinned: Vec<(Tag, [u8; 32])> = Vec::new();
        for line in data_lines(presigning) {
            let f: Vec<&str> = line.split_whitespace().collect();
            let [tag, digest] = f[..] else {
                return Err(format!(
                    "presigning-sums: '{line}' must be exactly '<tag> <sha256-hex>'"
                ));
            };
            let t = Tag::parse(tag)
                .ok_or_else(|| format!("presigning-sums: '{tag}' is not a plain vX.Y.Z tag"))?;
            if t >= FIRST_MANIFEST_SIG_TAG {
                return Err(format!(
                    "presigning-sums: {t} is not before {FIRST_MANIFEST_SIG_TAG} -- a signed-era tag is verified by its signature, never a pinned digest"
                ));
            }
            let d = parse_sha256_hex(digest).ok_or_else(|| {
                format!("presigning-sums: {tag}'s digest is not 64 lowercase hex digits")
            })?;
            if pinned.iter().any(|(p, _)| *p == t) {
                return Err(format!("presigning-sums: {tag} appears twice"));
            }
            pinned.push((t, d));
        }

        Ok(TrustRoot {
            keys,
            ranges: parsed_ranges,
            presigning: pinned,
        })
    }

    /// signing.sh's `release_tag_in_range`, with the same reasons.
    fn range_allows(&self, id: &str, tag: Tag) -> Result<(), String> {
        let Some(r) = self.ranges.iter().find(|r| r.id == id) else {
            return Err(format!(
                "key {PRINCIPAL_PREFIX}{id} has no line in release-keys/ranges -- a staged next key signs nothing until its range opens"
            ));
        };
        if tag < r.from {
            return Err(format!(
                "key {PRINCIPAL_PREFIX}{id} may sign {} onward (release-keys/ranges), and {tag} is before that",
                r.from
            ));
        }
        if let Some(to) = r.to
            && tag > to
        {
            return Err(format!(
                "key {PRINCIPAL_PREFIX}{id} was retired after {to} (release-keys/ranges) -- it may not sign {tag}"
            ));
        }
        Ok(())
    }

    /// "oam-release-k1 SHA256:..., oam-release-k2 SHA256:..." -- what a
    /// signature was expected to be made by, for the failure message.
    fn expected_keys(&self) -> String {
        self.keys
            .iter()
            .map(|k| {
                format!(
                    "{PRINCIPAL_PREFIX}{} {}",
                    k.id,
                    k.key.fingerprint(HashAlg::Sha256)
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Check RELEASE-MANIFEST against its .sig for `tag` and return the
    /// SHA256SUMS section. signing.sh's `release_verify_manifest`, minus the
    /// SHA256SUMS comparison (the manifest's copy is the one used here).
    /// The content checks come AFTER the signature: until it verifies, the
    /// content is attacker-controlled and its parse is not worth reporting.
    pub(crate) fn verify_manifest<'m>(
        &self,
        manifest: &'m [u8],
        sig: &[u8],
        tag: Tag,
    ) -> Result<Verified<'m>, String> {
        let sig = SshSig::from_pem(sig).map_err(|e| {
            format!("RELEASE-MANIFEST.sig is not a well-formed SSH signature ({e})")
        })?;
        // ssh-key refuses versions above 1 but takes a 0. The field is not
        // under the signature, and nothing has ever written a 0: refuse it.
        if sig.version() != 1 {
            return Err(format!(
                "RELEASE-MANIFEST.sig is SSHSIG version {}, not 1",
                sig.version()
            ));
        }
        if sig.namespace() != NAMESPACE {
            return Err(format!(
                "RELEASE-MANIFEST.sig is for namespace '{}', not '{NAMESPACE}' -- a signature made for something else",
                printable(sig.namespace().as_bytes())
            ));
        }
        // Belt and braces: the decoder knows no other algorithm today, but a
        // later ssh-key that learns sha384 must not widen what verifies here.
        match sig.hash_alg() {
            HashAlg::Sha256 | HashAlg::Sha512 => {}
            other => {
                return Err(format!(
                    "RELEASE-MANIFEST.sig hashes with {other}, not sha256/sha512"
                ));
            }
        }
        let actual_fp = sig.public_key().fingerprint(HashAlg::Sha256);
        let Some(trusted) = self
            .keys
            .iter()
            .find(|k| k.key.key_data() == sig.public_key())
        else {
            return Err(format!(
                "RELEASE-MANIFEST.sig was made by key {actual_fp}, which is not a release key (expected one of: {})",
                self.expected_keys()
            ));
        };
        let principal = format!("{PRINCIPAL_PREFIX}{}", trusted.id);
        if sig.algorithm() != trusted.key.algorithm() {
            return Err(format!(
                "RELEASE-MANIFEST.sig carries a {} signature for {principal}'s {} key",
                sig.algorithm(),
                trusted.key.algorithm()
            ));
        }
        // PublicKey::verify also re-checks the embedded key and the namespace.
        trusted.key.verify(NAMESPACE, manifest, &sig).map_err(|e| {
            format!(
                "RELEASE-MANIFEST.sig names {principal} ({actual_fp}) but does not verify for it ({e}) -- the manifest or its signature was altered"
            )
        })?;

        // Header: compared as BYTES, then diagnosed line by line.
        let prefix = format!("{MANIFEST_HEADER}\ntag {tag}\n");
        if !manifest.starts_with(prefix.as_bytes()) {
            let mut lines = manifest.split(|&b| b == b'\n');
            let line1 = lines.next().unwrap_or_default();
            let line2 = lines.next().unwrap_or_default();
            if line1.ends_with(b"\r") || line2.ends_with(b"\r") {
                return Err(
                    "RELEASE-MANIFEST's header has CR line endings -- it is LF-only, byte for byte"
                        .into(),
                );
            }
            if line1 != MANIFEST_HEADER.as_bytes() {
                return Err(format!(
                    "RELEASE-MANIFEST line 1 is '{}', not '{MANIFEST_HEADER}'",
                    printable(line1)
                ));
            }
            return Err(match line2.strip_prefix(b"tag ") {
                Some(signed) => format!(
                    "RELEASE-MANIFEST is signed for tag '{}', not {tag} -- a replayed or misfiled release",
                    printable(signed)
                ),
                None => format!(
                    "RELEASE-MANIFEST line 2 is '{}', not 'tag {tag}'",
                    printable(line2)
                ),
            });
        }
        self.range_allows(&trusted.id, tag)?;
        Ok(Verified {
            principal,
            fingerprint: actual_fp.to_string(),
            sums: &manifest[prefix.len()..],
        })
    }

    /// A pre-cutoff release: the SHA-256 of its published SHA256SUMS must be
    /// the one pinned for its tag.
    pub(crate) fn verify_presigning(&self, tag: Tag, sums: &[u8]) -> Result<(), String> {
        if tag >= FIRST_MANIFEST_SIG_TAG {
            return Err(format!(
                "{tag} is a signed-era release ({FIRST_MANIFEST_SIG_TAG} on) -- it is verified by its RELEASE-MANIFEST.sig, never a pinned digest"
            ));
        }
        let Some((_, pinned)) = self.presigning.iter().find(|(t, _)| *t == tag) else {
            return Err(format!(
                "{tag} predates release signing and is not in the pinned digest table (release-keys/presigning-sums) -- an unknown pre-{FIRST_MANIFEST_SIG_TAG} tag is refused"
            ));
        };
        let actual: [u8; 32] = Sha256::digest(sums).into();
        if &actual != pinned {
            return Err(format!(
                "{tag}'s SHA256SUMS does not match the digest pinned for it (expected {}, got {})",
                hex(pinned),
                hex(&actual)
            ));
        }
        Ok(())
    }
}

/// A verified manifest: who signed it, and the SHA256SUMS lines it vouches for.
pub(crate) struct Verified<'m> {
    principal: String,
    fingerprint: String,
    sums: &'m [u8],
}

// --- small parsers -------------------------------------------------------------

fn parse_sha256_hex(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64
        || !b
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
    {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in b.chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bytes from a download, made safe to print: no terminal escapes, bounded.
fn printable(b: &[u8]) -> String {
    let s: String = String::from_utf8_lossy(b).escape_debug().collect();
    if s.chars().count() > 80 {
        format!("{}...", s.chars().take(80).collect::<String>())
    } else {
        s
    }
}

/// The digest SHA256SUMS lists for `asset`: "<hex> *<name>" (sha256sum's
/// binary marker) or "<hex>  <name>", first match wins -- install.sh's awk.
fn sum_for(sums: &[u8], asset: &str) -> Result<[u8; 32], String> {
    let text =
        std::str::from_utf8(sums).map_err(|_| "the signed SHA256SUMS is not text".to_string())?;
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(digest), Some(name)) = (f.next(), f.next()) else {
            continue;
        };
        if name.strip_prefix('*').unwrap_or(name) == asset {
            return parse_sha256_hex(&digest.to_ascii_lowercase()).ok_or_else(|| {
                format!(
                    "the SHA256SUMS line for {asset} carries no sha256 ('{}')",
                    printable(digest.as_bytes())
                )
            });
        }
    }
    Err(format!(
        "the release lists no {asset} -- no build of it for this platform"
    ))
}

/// This build's release asset name (install.sh / install.ps1 must agree; see
/// install/README.md). None for a platform no release ships.
fn asset_name() -> Option<&'static str> {
    Some(match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "oam-x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "oam-aarch64-unknown-linux-gnu",
        ("x86_64", "macos") => "oam-x86_64-apple-darwin",
        ("aarch64", "macos") => "oam-aarch64-apple-darwin",
        ("x86_64", "windows") => "oam-x86_64-pc-windows-msvc.exe",
        ("aarch64", "windows") => "oam-aarch64-pc-windows-msvc.exe",
        _ => return None,
    })
}

// --- HTTP ----------------------------------------------------------------------

pub(crate) struct Http {
    rt: tokio::runtime::Runtime,
    client: reqwest::Client,
}

enum Fetched {
    Body(Vec<u8>),
    NotFound,
}

impl Http {
    pub(crate) fn new() -> Result<Http, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("could not start the network runtime: {e}"))?;
        let client = oam_loader::install::build_http_client()
            .map_err(|e| format!("could not build the HTTPS client: {e}"))?;
        Ok(Http { rt, client })
    }

    /// One GET, retried once on a transport error or a 5xx; a 404 is an
    /// answer, not a failure, so the caller can say which file is missing.
    async fn send(
        &self,
        url: &str,
        timeout: Duration,
    ) -> Result<Option<reqwest::Response>, String> {
        let mut last = String::new();
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            match self.client.get(url).timeout(timeout).send().await {
                Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => return Ok(None),
                Ok(r) if r.status().is_success() => return Ok(Some(r)),
                Ok(r) if r.status().is_server_error() => {
                    last = format!("HTTP {} for {url}", r.status())
                }
                Ok(r) => return Err(format!("HTTP {} for {url}", r.status())),
                Err(e) => last = format!("request to {url}: {e}"),
            }
        }
        Err(last)
    }

    /// A small file, whole, refused past `cap` bytes.
    fn get(&self, url: &str, cap: u64) -> Result<Fetched, String> {
        self.rt.block_on(async {
            let Some(mut resp) = self.send(url, Duration::from_secs(120)).await? else {
                return Ok(Fetched::NotFound);
            };
            let mut body = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| format!("reading {url}: {e}"))?
            {
                body.extend_from_slice(&chunk);
                if body.len() as u64 > cap {
                    return Err(format!(
                        "{url} is over {cap} bytes -- not the file expected"
                    ));
                }
            }
            Ok(Fetched::Body(body))
        })
    }

    /// The tag `/releases/latest` redirects to (`.../releases/tag/<tag>`).
    fn latest_tag(&self, releases_root: &str) -> Result<Tag, String> {
        let url = format!("{releases_root}/latest");
        self.rt.block_on(async {
            let resp = self
                .send(&url, Duration::from_secs(120))
                .await?
                .ok_or_else(|| format!("{url} answered 404 -- no published release"))?;
            tag_from_release_url(resp.url().as_str())
        })
    }

    /// Stream `url` into `out`, hashing as it goes; returns the SHA-256.
    fn download(&self, url: &str, out: &mut std::fs::File) -> Result<[u8; 32], String> {
        self.rt.block_on(async {
            // The binary is large; 30 minutes is a slow link, not a hang.
            let mut resp = self
                .send(url, Duration::from_secs(1800))
                .await?
                .ok_or_else(|| format!("{url} answered 404 -- the release has no such file"))?;
            let mut hasher = Sha256::new();
            let mut total: u64 = 0;
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| format!("reading {url}: {e}"))?
            {
                total += chunk.len() as u64;
                if total > ASSET_CAP {
                    return Err(format!(
                        "{url} is over {ASSET_CAP} bytes -- not an oam binary"
                    ));
                }
                hasher.update(&chunk);
                out.write_all(&chunk)
                    .map_err(|e| format!("writing the download: {e}"))?;
            }
            Ok(hasher.finalize().into())
        })
    }
}

/// The tag in a release page URL: the path's last `/tag/<tag>`.
fn tag_from_release_url(url: &str) -> Result<Tag, String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit_once("/tag/")
        .and_then(|(_, t)| Tag::parse(t))
        .ok_or_else(|| {
            format!(
                "the latest-release redirect went to {}, not a .../releases/tag/vX.Y.Z page",
                printable(url.as_bytes())
            )
        })
}

// --- the update ----------------------------------------------------------------

pub(crate) struct Request {
    /// `--version`: install exactly this tag, older ones included.
    pub(crate) pinned: Option<Tag>,
    /// This binary's version, the anti-rollback floor when nothing is pinned.
    pub(crate) current: Tag,
    /// Where `latest` and `download/<tag>/` live.
    pub(crate) releases_root: String,
    /// OAM_SELF_UPDATE_URL: fetch the tag's files from here instead.
    pub(crate) asset_base: Option<String>,
    pub(crate) asset: String,
    /// The file to replace.
    pub(crate) target: PathBuf,
    pub(crate) dry_run: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    UpToDate(Tag),
    DryRun(Tag),
    Updated(Tag),
}

/// The `--version` smoke the new binary has to pass once it is in place.
pub(crate) type Smoke<'a> = &'a dyn Fn(&Path) -> Result<String, String>;

pub(crate) fn run(
    req: &Request,
    trust: &TrustRoot,
    http: &Http,
    smoke: Smoke<'_>,
) -> Result<Outcome, String> {
    let tag = match (req.pinned, &req.asset_base) {
        (Some(t), _) => t,
        // Like the installers' OAM_INSTALL_BASE: a base names ONE release's
        // files, so the tag they must be signed for has to come from the user.
        (None, Some(_)) => {
            return Err("OAM_SELF_UPDATE_URL names one release's files, so it needs --version <tag> (the tag its signature must be for)".into());
        }
        (None, None) => {
            let t = http.latest_tag(&req.releases_root)?;
            println!("oam self-update: the latest release is {t}");
            t
        }
    };
    let base = match &req.asset_base {
        Some(b) => b.trim_end_matches('/').to_string(),
        None => format!("{}/download/{tag}", req.releases_root),
    };
    let fetch = |name: &str| -> Result<Option<Vec<u8>>, String> {
        match http.get(&format!("{base}/{name}"), META_CAP)? {
            Fetched::Body(b) => Ok(Some(b)),
            Fetched::NotFound => Ok(None),
        }
    };

    // Verify first: everything below trusts only what this vouches for.
    let manifest_bytes;
    let sums: &[u8] = if tag >= FIRST_MANIFEST_SIG_TAG {
        let (Some(m), Some(sig)) = (fetch("RELEASE-MANIFEST")?, fetch("RELEASE-MANIFEST.sig")?)
        else {
            return Err(format!(
                "{tag} has no RELEASE-MANIFEST / RELEASE-MANIFEST.sig at {base} -- every release from {FIRST_MANIFEST_SIG_TAG} on is signed, so this one is refused"
            ));
        };
        manifest_bytes = m;
        let v = trust.verify_manifest(&manifest_bytes, &sig, tag)?;
        println!(
            "oam self-update: RELEASE-MANIFEST for {tag} is signed by {} ({})",
            v.principal, v.fingerprint
        );
        v.sums
    } else {
        let Some(s) = fetch("SHA256SUMS")? else {
            return Err(format!("{tag} has no SHA256SUMS at {base}"));
        };
        trust.verify_presigning(tag, &s)?;
        println!(
            "oam self-update: {tag} predates signing; its SHA256SUMS matches the digest pinned for it"
        );
        manifest_bytes = s;
        &manifest_bytes
    };

    // Anti-rollback, on the verified tag.
    if req.pinned.is_none() {
        if tag == req.current {
            println!("oam self-update: already up to date ({tag})");
            return Ok(Outcome::UpToDate(tag));
        }
        if tag < req.current {
            return Err(format!(
                "the latest release is {tag}, older than this oam ({}) -- refusing a downgrade (pass --version {tag} to install it on purpose)",
                req.current
            ));
        }
    }

    let expected = sum_for(sums, &req.asset)?;
    let url = format!("{base}/{}", req.asset);
    if req.dry_run {
        println!(
            "oam self-update: (dry-run) would download {url} (sha256 {}) and replace {}",
            hex(&expected),
            req.target.display()
        );
        return Ok(Outcome::DryRun(tag));
    }

    let dir = req
        .target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", req.target.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let staged = Staged::create(&req.target)?;
    println!("oam self-update: downloading {url}");
    let actual = {
        let mut f = staged.file()?;
        let d = http.download(&url, &mut f)?;
        f.sync_all()
            .map_err(|e| format!("flushing {}: {e}", staged.path.display()))?;
        d
    };
    if actual != expected {
        return Err(format!(
            "{} does not match the signed manifest (expected sha256 {}, got {})",
            req.asset,
            hex(&expected),
            hex(&actual)
        ));
    }
    // Same mode bits as what it replaces (or as this oam, for a fresh dir):
    // executable on Unix without a cfg(unix) block.
    let mode_from = if req.target.exists() {
        req.target.clone()
    } else {
        std::env::current_exe().map_err(|e| format!("cannot find this oam: {e}"))?
    };
    let perms = std::fs::metadata(&mode_from)
        .map_err(|e| format!("reading {}: {e}", mode_from.display()))?
        .permissions();
    std::fs::set_permissions(&staged.path, perms)
        .map_err(|e| format!("setting permissions on {}: {e}", staged.path.display()))?;

    let reported = replace(&req.target, &staged.path, smoke)?;
    refresh_licenses(http, &base, dir);
    println!(
        "oam self-update: installed {tag} to {} ({reported})",
        req.target.display()
    );
    Ok(Outcome::Updated(tag))
}

/// The notices install.sh / install.ps1 put in `<install dir>/licenses`: the
/// binary redistributes V8, ICU and ~400 crates, and their notices travel with
/// it, so a new binary brings its own. Best-effort, as in the installers: a
/// release before v0.8.1 ships none, and a missing notice is reported, not
/// worth undoing a good update over. They are outside SHA256SUMS (it covers
/// binaries), so they are unverified text -- written, never executed.
fn refresh_licenses(http: &Http, base: &str, dir: &Path) {
    let licenses = dir.join("licenses");
    let mut got = false;
    for name in ["LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.md"] {
        let Ok(Fetched::Body(body)) = http.get(&format!("{base}/{name}"), LICENSE_CAP) else {
            continue;
        };
        let tmp = licenses.join(format!(".{name}.update-{}", std::process::id()));
        let written = std::fs::create_dir_all(&licenses)
            .and_then(|()| std::fs::write(&tmp, &body))
            .and_then(|()| std::fs::rename(&tmp, licenses.join(name)));
        match written {
            Ok(()) => got = true,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                println!(
                    "oam self-update: note: could not write {name} to {}: {e}",
                    licenses.display()
                );
            }
        }
    }
    if got {
        println!(
            "oam self-update: license and attribution files in {}",
            licenses.display()
        );
    } else {
        println!(
            "oam self-update: note: this release ships no license assets; see https://github.com/YawLabs/oam"
        );
    }
}

/// The download's temp file, in the install dir so the final rename never
/// crosses a filesystem. Removed on drop unless it was renamed into place.
struct Staged {
    path: PathBuf,
}

impl Staged {
    fn create(target: &Path) -> Result<Staged, String> {
        let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("oam");
        let path = target.with_file_name(format!(".{name}.update-{}", std::process::id()));
        // A leftover from a killed run with a recycled pid.
        let _ = std::fs::remove_file(&path);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        Ok(Staged { path })
    }

    fn file(&self) -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(|e| format!("cannot open {}: {e}", self.path.display()))
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        // Gone already after a successful rename; that is the success path.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `<target>.old`, install.ps1's move-aside name.
fn backup_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".old");
    PathBuf::from(s)
}

/// Put `staged` at `target`, smoke it, and put the old binary back if the
/// smoke fails. Returns what the new binary's `--version` printed.
///
/// Windows cannot replace or delete a running exe, but it can rename one, so
/// the running oam moves aside to `<target>.old` first (install.ps1) and that
/// file is cleared by the next update. Unix renames over the running binary
/// atomically; a hard link (or copy) keeps the old one for a rollback.
fn replace(target: &Path, staged: &Path, smoke: Smoke<'_>) -> Result<String, String> {
    let backup = backup_path(target);
    let _ = std::fs::remove_file(&backup);
    let had_old = target.exists();
    if had_old {
        let kept = if cfg!(windows) {
            std::fs::rename(target, &backup)
        } else {
            std::fs::hard_link(target, &backup)
                .or_else(|_| std::fs::copy(target, &backup).map(|_| ()))
        };
        kept.map_err(|e| {
            format!(
                "cannot keep the current binary at {}: {e}",
                backup.display()
            )
        })?;
    }
    if let Err(e) = std::fs::rename(staged, target) {
        if had_old && cfg!(windows) {
            let _ = std::fs::rename(&backup, target);
        } else {
            let _ = std::fs::remove_file(&backup);
        }
        return Err(format!(
            "cannot move the new binary to {}: {e}",
            target.display()
        ));
    }
    match smoke(target) {
        Ok(v) => {
            // Fails on Windows while the old exe still runs; the next update
            // clears it.
            let _ = std::fs::remove_file(&backup);
            Ok(v)
        }
        Err(e) => {
            let restored = if had_old {
                std::fs::rename(&backup, target)
            } else {
                std::fs::remove_file(target)
            };
            Err(match restored {
                Ok(()) => format!(
                    "the new binary failed its --version smoke ({e}); the previous one was put back"
                ),
                Err(re) => format!(
                    "the new binary failed its --version smoke ({e}), and putting the previous one back from {} failed too ({re})",
                    backup.display()
                ),
            })
        }
    }
}

/// Run `<path> --version`; it must succeed and say it is oam.
fn smoke_version(path: &Path) -> Result<String, String> {
    let out = std::process::Command::new(path)
        .arg("--version")
        .output()
        .map_err(|e| format!("could not run it: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || !stdout.starts_with("oam ") {
        return Err(format!(
            "exit {}, stdout '{}'",
            out.status,
            printable(stdout.as_bytes())
        ));
    }
    Ok(stdout)
}

/// The binary to replace: `$OAM_INSTALL_DIR/oam[.exe]` when set (the
/// installers' knob), else the running binary, through any symlink (a
/// symlinked oam is updated where the link points, not replaced by a file).
fn target_path() -> Result<PathBuf, String> {
    if let Some(d) = std::env::var_os("OAM_INSTALL_DIR") {
        return Ok(PathBuf::from(d).join(if cfg!(windows) { "oam.exe" } else { "oam" }));
    }
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot resolve the current binary location: {e}"))?;
    let is_link = std::fs::symlink_metadata(&exe)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_link {
        return std::fs::canonicalize(&exe)
            .map_err(|e| format!("cannot resolve {}: {e}", exe.display()));
    }
    Ok(exe)
}

/// `oam self-update [--version <tag>] [--dry-run]`.
pub(crate) fn command(version: Option<&str>, dry_run: bool) -> ExitCode {
    match command_inner(version, dry_run) {
        Ok(_) => ExitCode::SUCCESS,
        Err((msg, target)) => {
            eprintln!("oam self-update: error: {msg}");
            if let Some(t) = target {
                eprintln!("oam self-update: {} was left as it was", t.display());
            }
            ExitCode::FAILURE
        }
    }
}

fn command_inner(
    version: Option<&str>,
    dry_run: bool,
) -> Result<Outcome, (String, Option<PathBuf>)> {
    let pinned = match version {
        Some(v) => Some(Tag::parse_user(v).ok_or_else(|| {
            (
                format!("--version '{v}' is not a release tag (vMAJOR.MINOR.PATCH)"),
                None,
            )
        })?),
        None => None,
    };
    let current = Tag::parse_user(env!("CARGO_PKG_VERSION")).ok_or_else(|| {
        (
            "this build's version is not a plain MAJOR.MINOR.PATCH".to_string(),
            None,
        )
    })?;
    let asset = asset_name().ok_or_else(|| {
        (
            format!(
                "no oam release is built for {}-{}",
                std::env::consts::ARCH,
                std::env::consts::OS
            ),
            None,
        )
    })?;
    let target = target_path().map_err(|e| (e, None))?;
    let trust = TrustRoot::embedded().map_err(|e| {
        (
            format!("the compiled-in release keys do not parse: {e}"),
            None,
        )
    })?;
    let asset_base = std::env::var("OAM_SELF_UPDATE_URL")
        .ok()
        .filter(|s| !s.is_empty());

    println!("oam self-update: current version {current}");
    println!("oam self-update: updating {}", target.display());
    match (pinned, &asset_base) {
        (Some(t), _) => println!("oam self-update: pinning to {t}"),
        (None, _) => println!("oam self-update: targeting the latest release"),
    }
    if let Some(b) = &asset_base {
        println!(
            "oam self-update: fetching release files from {b} (OAM_SELF_UPDATE_URL; still signature-checked)"
        );
    }

    let req = Request {
        pinned,
        current,
        releases_root: RELEASES_ROOT.to_string(),
        asset_base,
        asset: asset.to_string(),
        target: target.clone(),
        dry_run,
    };
    let http = Http::new().map_err(|e| (e, Some(target.clone())))?;
    run(&req, &trust, &http, &smoke_version).map_err(|e| (e, Some(target)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader};
    use std::net::TcpListener;
    use std::sync::Arc;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/self_update");

    fn fixture(rel: &str) -> Vec<u8> {
        std::fs::read(format!("{FIX}/{rel}")).unwrap_or_else(|e| panic!("{FIX}/{rel}: {e}"))
    }

    fn fixture_str(rel: &str) -> String {
        String::from_utf8(fixture(rel)).unwrap()
    }

    fn t(s: &str) -> Tag {
        Tag::parse(s).unwrap()
    }

    /// The test trust root: t1 and tsk sign from v0.18.0, t2 is staged, and
    /// v0.17.1 is pinned to the fixture SHA256SUMS.
    fn test_root() -> TrustRoot {
        let pinned = format!("v0.17.1 {}\n", hex(&Sha256::digest(fixture("SHA256SUMS"))));
        TrustRoot::parse(
            &fixture_str("allowed_signers"),
            &fixture_str("ranges"),
            &pinned,
        )
        .unwrap()
    }

    fn verify_case(case: &str, manifest: Option<&str>) -> Result<String, String> {
        let m = match manifest {
            Some(m) => fixture(m),
            None => fixture("manifest-v0.18.0"),
        };
        let sig = fixture(&format!("cases/{case}.sig"));
        test_root()
            .verify_manifest(&m, &sig, t("v0.18.0"))
            .map(|v| v.principal)
    }

    fn assert_rejects(case: &str, manifest: Option<&str>, why: &str) {
        match verify_case(case, manifest) {
            Ok(p) => panic!("{case}: verified as {p}, expected a rejection mentioning '{why}'"),
            Err(e) => assert!(
                e.contains(why),
                "{case}: rejected, but '{e}' does not mention '{why}'"
            ),
        }
    }

    // --- the embedded trust root (drift: the copies ARE the files) ----------

    #[test]
    fn embedded_trust_root_parses_and_lints() {
        let root = TrustRoot::embedded().expect("release-keys/* must parse");
        assert!(
            root.keys.iter().any(|k| k.id == "k1"),
            "k1 is the current release key"
        );
        // Every key the ranges open is a committed key.
        for r in &root.ranges {
            assert!(
                root.keys.iter().any(|k| k.id == r.id),
                "ranges names unknown key {}",
                r.id
            );
        }
        // The cutoff is where signing began: the earliest range start, as
        // signing.sh's release_tag_predates_signing reads it.
        let earliest = root.ranges.iter().map(|r| r.from).min().expect("a range");
        assert_eq!(earliest, FIRST_MANIFEST_SIG_TAG);
        // The pinned table covers exactly the unsigned era, which is closed.
        assert_eq!(root.presigning.len(), 25);
        assert!(
            root.presigning
                .iter()
                .all(|(t, _)| *t < FIRST_MANIFEST_SIG_TAG)
        );
        assert!(
            root.presigning
                .iter()
                .any(|(t, _)| *t == Tag::parse("v0.17.1").unwrap())
        );
    }

    #[test]
    fn embedded_copies_are_the_committed_files() {
        // include_str! of the files themselves; this pins the paths, so a move
        // of release-keys/ cannot silently leave a stale copy behind.
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../release-keys");
        for (name, embedded) in [
            ("allowed_signers", ALLOWED_SIGNERS),
            ("ranges", RANGES),
            ("presigning-sums", PRESIGNING_SUMS),
        ] {
            let on_disk = std::fs::read_to_string(format!("{root}/{name}")).unwrap();
            assert_eq!(on_disk, embedded, "release-keys/{name}");
        }
        assert!(PRESIGNING_SUMS.starts_with(
            "# SHA-256 of each pre-signing release's published SHA256SUMS (tags before v0.18.0).\n"
        ));
    }

    // --- tags ------------------------------------------------------------------

    #[test]
    fn tags_parse_plain_and_compare_numerically() {
        assert!(t("v0.10.0") > t("v0.9.0"));
        assert!(t("v1.0.0") > t("v0.99.99"));
        assert_eq!(t("v0.018.0"), t("v0.18.0"));
        assert_eq!(t("v0.18.0").to_string(), "v0.18.0");
        for bad in [
            "0.18.0",
            "v0.18",
            "v0.18.0.1",
            "v0.18.0-rc1",
            "v0.18.x",
            "v.1.2",
            "v1..2",
            "",
            "v+1.2.3",
        ] {
            assert!(Tag::parse(bad).is_none(), "{bad}");
        }
        assert_eq!(Tag::parse_user("0.18.0"), Some(t("v0.18.0")));
        assert!(Tag::parse_user("latest").is_none());
    }

    #[test]
    fn latest_redirect_url_yields_its_tag() {
        assert_eq!(
            tag_from_release_url("https://github.com/YawLabs/oam/releases/tag/v0.17.1"),
            Ok(t("v0.17.1"))
        );
        assert_eq!(
            tag_from_release_url("http://127.0.0.1:1/releases/tag/v0.18.0?x=1"),
            Ok(t("v0.18.0"))
        );
        assert!(tag_from_release_url("https://github.com/YawLabs/oam/releases").is_err());
        assert!(
            tag_from_release_url("https://github.com/YawLabs/oam/releases/tag/nightly").is_err()
        );
    }

    // --- trust-root lint -------------------------------------------------------

    #[test]
    fn ranges_gate_each_key() {
        let root = TrustRoot::parse(
            &fixture_str("allowed_signers"),
            "t1 v0.18.0 v0.21.3\ntsk v0.21.4 -\n",
            "",
        )
        .unwrap();
        assert!(root.range_allows("t1", t("v0.18.0")).is_ok());
        assert!(root.range_allows("t1", t("v0.21.3")).is_ok());
        assert!(
            root.range_allows("t1", t("v0.17.9"))
                .unwrap_err()
                .contains("before that")
        );
        assert!(
            root.range_allows("t1", t("v0.21.4"))
                .unwrap_err()
                .contains("retired after v0.21.3")
        );
        assert!(root.range_allows("tsk", t("v9.0.0")).is_ok());
        assert!(
            root.range_allows("t2", t("v0.18.0"))
                .unwrap_err()
                .contains("staged next key")
        );
    }

    #[test]
    fn malformed_trust_files_are_errors_not_skips() {
        let good = fixture_str("allowed_signers");
        let key = good
            .lines()
            .nth(1)
            .unwrap()
            .split_once(' ')
            .unwrap()
            .1
            .to_string();
        let as_bad = |s: &str| TrustRoot::parse(s, "", "").err().unwrap_or_default();
        assert!(as_bad("").contains("no key"));
        assert!(as_bad(&format!("someone {key}")).contains("does not start with"));
        assert!(as_bad(&format!("oam-release-* {key}")).contains("single literal name"));
        assert!(as_bad(&format!("oam-release-a,b {key}")).contains("single literal name"));
        let nsless = key.replacen("namespaces=\"oam-release\"", "cert-authority", 1);
        assert!(as_bad(&format!("oam-release-x {nsless}")).contains("namespaces"));
        let nsfile = key.replacen("\"oam-release\"", "\"file\"", 1);
        assert!(as_bad(&format!("oam-release-x {nsfile}")).contains("namespaces"));
        let rsa = key.replacen("ssh-ed25519", "ssh-rsa", 1);
        assert!(as_bad(&format!("oam-release-x {rsa}")).contains("key type"));
        let mislabelled = key.replacen("ssh-ed25519", "sk-ssh-ed25519@openssh.com", 1);
        assert!(!as_bad(&format!("oam-release-x {mislabelled}")).is_empty());
        let nob64 = format!("{} !!!!", key.rsplit_once(' ').unwrap().0);
        assert!(as_bad(&format!("oam-release-x {nob64}")).contains("key blob"));
        assert!(as_bad(&format!("oam-release-x {key}\noam-release-x {key}")).contains("twice"));

        let ranges_bad = |r: &str| TrustRoot::parse(&good, r, "").err().unwrap_or_default();
        assert!(ranges_bad("t1 v0.18.0").contains("exactly"));
        assert!(ranges_bad("t1 v0.18.0 - extra").contains("exactly"));
        assert!(ranges_bad("t1 0.18.0 -").contains("plain"));
        assert!(ranges_bad("t1 v0.18.0 later").contains("neither"));
        assert!(ranges_bad("t1 v0.19.0 v0.18.0").contains("backwards"));
        assert!(ranges_bad("t1 v0.18.0 -\nt1 v0.20.0 -").contains("more than one line"));

        let pins_bad = |p: &str| TrustRoot::parse(&good, "", p).err().unwrap_or_default();
        let d = "a".repeat(64);
        assert!(pins_bad(&format!("v0.17.1 {d} x")).contains("exactly"));
        assert!(pins_bad(&format!("v0.18.0 {d}")).contains("signed-era"));
        assert!(pins_bad(&format!("v0.17.1 {}", "A".repeat(64))).contains("lowercase hex"));
        assert!(pins_bad("v0.17.1 abc").contains("lowercase hex"));
        assert!(pins_bad(&format!("v0.17.1 {d}\nv0.17.1 {d}")).contains("twice"));
        // Comments and blank lines are not data.
        assert!(
            TrustRoot::parse(
                &good,
                "  # c\n\nt1 v0.18.0 -\n",
                &format!("# c\n  \nv0.1.0 {d}\n")
            )
            .is_ok()
        );
    }

    // --- the ssh-keygen vectors ------------------------------------------------
    // Every .sig here was made (or, for the mutations, checked) by a real
    // ssh-keygen -Y; generate.mjs records its verdict. oam must agree with it,
    // except where the name says oam is stricter (version-0).

    #[test]
    fn valid_signatures_verify() {
        assert_eq!(verify_case("valid", None).unwrap(), "oam-release-t1");
        assert_eq!(verify_case("valid-sha256", None).unwrap(), "oam-release-t1");
        assert_eq!(verify_case("sk-valid", None).unwrap(), "oam-release-tsk");
        // ssh-keygen accepts an sk signature without the user-presence flag;
        // so must oam, or the two verifiers would disagree on one release.
        assert_eq!(
            verify_case("sk-no-presence", None).unwrap(),
            "oam-release-tsk"
        );
    }

    #[test]
    fn verified_manifest_yields_the_sums_section() {
        let m = fixture("manifest-v0.18.0");
        let sig = fixture("cases/valid.sig");
        let root = test_root();
        let v = root.verify_manifest(&m, &sig, t("v0.18.0")).unwrap();
        assert_eq!(v.sums, &fixture("SHA256SUMS")[..]);
        assert!(v.fingerprint.starts_with("SHA256:"));
        let payload_sum: [u8; 32] = Sha256::digest(fixture("payload.bin")).into();
        assert_eq!(
            sum_for(v.sums, "oam-x86_64-pc-windows-msvc.exe").unwrap(),
            payload_sum
        );
        assert_eq!(
            sum_for(v.sums, "oam-aarch64-apple-darwin").unwrap(),
            payload_sum
        );
        assert!(
            sum_for(v.sums, "oam-riscv64")
                .unwrap_err()
                .contains("lists no oam-riscv64")
        );
        // Two-space (text-mode) lines work too.
        assert!(sum_for(format!("{}  oam-x\n", "b".repeat(64)).as_bytes(), "oam-x").is_ok());
    }

    #[test]
    fn mutated_and_foreign_signatures_are_rejected() {
        assert_rejects("wrong-namespace", None, "namespace 'file'");
        assert_rejects("untrusted-key", None, "not a release key");
        assert_rejects("truncated", None, "not a well-formed");
        assert_rejects("trailing-bytes", None, "not a well-formed");
        assert_rejects("bad-preamble", None, "not a well-formed");
        assert_rejects("version-0", None, "version 0");
        assert_rejects("version-2", None, "not a well-formed");
        assert_rejects("hash-sha384", None, "not a well-formed");
        assert_rejects("sig-bitflip", None, "does not verify");
        assert_rejects("pubkey-swapped", None, "does not verify");
        assert_rejects("pubkey-claims-trusted", None, "does not verify");
    }

    #[test]
    fn untrusted_key_failure_names_both_fingerprints() {
        let e = verify_case("untrusted-key", None).unwrap_err();
        let untrusted = PublicKey::from_openssh(fixture_str("untrusted.pub").trim()).unwrap();
        assert!(
            e.contains(&untrusted.fingerprint(HashAlg::Sha256).to_string()),
            "{e}"
        );
        let t1 = test_root().keys.into_iter().find(|k| k.id == "t1").unwrap();
        assert!(
            e.contains(&t1.key.fingerprint(HashAlg::Sha256).to_string()),
            "{e}"
        );
    }

    #[test]
    fn validly_signed_wrong_content_is_rejected() {
        // ssh-keygen accepts each of these; the content checks are oam's.
        assert_rejects("staged-key", None, "staged next key");
        assert_rejects(
            "tag-mismatch",
            Some("cases/tag-mismatch.manifest"),
            "signed for tag 'v0.18.1'",
        );
        assert_rejects(
            "crlf-header",
            Some("cases/crlf-header.manifest"),
            "CR line endings",
        );
        assert_rejects("bad-header", Some("cases/bad-header.manifest"), "line 1");
        // Signed for v0.17.9 and asked for v0.17.9: the key's range starts later.
        let m = fixture("cases/out-of-range.manifest");
        let sig = fixture("cases/out-of-range.sig");
        let e = test_root()
            .verify_manifest(&m, &sig, t("v0.17.9"))
            .err()
            .unwrap();
        assert!(e.contains("may sign v0.18.0 onward"), "{e}");
    }

    #[test]
    fn a_manifest_is_bound_to_the_requested_tag() {
        // The valid v0.18.0 manifest, replayed as v0.19.0.
        let e = test_root()
            .verify_manifest(
                &fixture("manifest-v0.18.0"),
                &fixture("cases/valid.sig"),
                t("v0.19.0"),
            )
            .err()
            .unwrap();
        assert!(e.contains("signed for tag 'v0.18.0', not v0.19.0"), "{e}");
    }

    #[test]
    fn presigning_digests_are_pinned_per_tag() {
        let root = test_root();
        let sums = fixture("SHA256SUMS");
        assert!(root.verify_presigning(t("v0.17.1"), &sums).is_ok());
        let mut tampered = sums.clone();
        tampered[0] ^= 1;
        let e = root.verify_presigning(t("v0.17.1"), &tampered).unwrap_err();
        assert!(e.contains("expected ") && e.contains(", got "), "{e}");
        assert!(
            root.verify_presigning(t("v0.16.0"), &sums)
                .unwrap_err()
                .contains("is refused")
        );
        assert!(
            root.verify_presigning(t("v0.18.0"), &sums)
                .unwrap_err()
                .contains("signed-era")
        );
    }

    #[test]
    fn this_platform_has_a_release_asset_name() {
        if let Some(a) = asset_name() {
            assert!(a.starts_with("oam-"));
            assert_eq!(a.ends_with(".exe"), cfg!(windows));
        }
    }

    // --- end to end, against a local fixture server ---------------------------

    type Routes = Vec<(String, u16, Vec<u8>)>;

    /// A minimal HTTP/1.1 server on 127.0.0.1: one response per connection.
    /// A 302 route's body is its Location. Lives until the test process ends.
    fn serve(routes: Routes) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(routes);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let routes = Arc::clone(&routes);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        return;
                    }
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let resp = match routes.iter().find(|(p, _, _)| *p == path) {
                        Some((_, 302, loc)) => format!(
                            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            String::from_utf8_lossy(loc)
                        )
                        .into_bytes(),
                        Some((_, status, body)) => {
                            let mut r = format!(
                                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            r.extend_from_slice(body);
                            r
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = stream.write_all(&resp);
                });
            }
        });
        base
    }

    /// A signed v0.18.0 release under /releases, with latest pointing at it.
    fn release_routes(sig_case: &str, payload: Vec<u8>) -> Routes {
        let d = "/releases/download/v0.18.0";
        vec![
            (
                "/releases/latest".into(),
                302,
                b"/releases/tag/v0.18.0".to_vec(),
            ),
            (
                "/releases/tag/v0.18.0".into(),
                200,
                b"<html>release page</html>".to_vec(),
            ),
            (
                format!("{d}/RELEASE-MANIFEST"),
                200,
                fixture("manifest-v0.18.0"),
            ),
            (
                format!("{d}/RELEASE-MANIFEST.sig"),
                200,
                fixture(&format!("cases/{sig_case}.sig")),
            ),
            (format!("{d}/SHA256SUMS"), 200, fixture("SHA256SUMS")),
            (
                format!("{d}/{}", asset_name().unwrap_or("oam-none")),
                200,
                payload,
            ),
            // Pre-signing: v0.17.1 has SHA256SUMS only.
            (
                "/releases/download/v0.17.1/SHA256SUMS".into(),
                200,
                fixture("SHA256SUMS"),
            ),
            (
                format!(
                    "/releases/download/v0.17.1/{}",
                    asset_name().unwrap_or("oam-none")
                ),
                200,
                fixture("payload.bin"),
            ),
        ]
    }

    struct Scene {
        dir: PathBuf,
        target: PathBuf,
    }

    const OLD: &[u8] = b"the installed oam, which must survive any failure\n";

    impl Scene {
        fn new(name: &str) -> Scene {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "oam-self-update-{name}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let target = dir.join("oam");
            std::fs::write(&target, OLD).unwrap();
            Scene { dir, target }
        }

        fn request(&self, base: &str, pinned: Option<&str>, current: &str) -> Request {
            Request {
                pinned: pinned.map(t),
                current: t(current),
                releases_root: format!("{base}/releases"),
                asset_base: None,
                asset: asset_name().unwrap_or("oam-none").to_string(),
                target: self.target.clone(),
                dry_run: false,
            }
        }

        /// The installed bytes, and every other file left in the dir.
        fn state(&self) -> (Vec<u8>, Vec<String>) {
            let mut others: Vec<String> = std::fs::read_dir(&self.dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n != "oam")
                .collect();
            others.sort();
            (std::fs::read(&self.target).unwrap(), others)
        }

        fn assert_untouched(&self) {
            assert_eq!(
                self.state(),
                (OLD.to_vec(), vec![]),
                "the installed binary must be left exactly as it was"
            );
        }
    }

    impl Drop for Scene {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn smoke_ok(_: &Path) -> Result<String, String> {
        Ok("oam 0.18.0".into())
    }

    fn update(req: &Request) -> Result<Outcome, String> {
        run(req, &test_root(), &Http::new().unwrap(), &smoke_ok)
    }

    #[test]
    fn e2e_latest_signed_release_replaces_the_binary() {
        let mut routes = release_routes("valid", fixture("payload.bin"));
        let license = b"the release's LICENSE\n".to_vec();
        routes.push((
            "/releases/download/v0.18.0/LICENSE".into(),
            200,
            license.clone(),
        ));
        let base = serve(routes);
        let s = Scene::new("ok");
        assert_eq!(
            update(&s.request(&base, None, "v0.17.1")),
            Ok(Outcome::Updated(t("v0.18.0")))
        );
        assert_eq!(s.state(), (fixture("payload.bin"), vec!["licenses".into()]));
        // The notices travel with the binary, as the installers place them.
        assert_eq!(
            std::fs::read(s.dir.join("licenses/LICENSE")).unwrap(),
            license
        );
        assert_eq!(
            std::fs::read_dir(s.dir.join("licenses")).unwrap().count(),
            1
        );
    }

    #[test]
    fn e2e_sk_signed_release_installs() {
        let base = serve(release_routes("sk-valid", fixture("payload.bin")));
        let s = Scene::new("sk");
        assert_eq!(
            update(&s.request(&base, None, "v0.17.1")),
            Ok(Outcome::Updated(t("v0.18.0")))
        );
    }

    #[test]
    fn e2e_bad_signature_leaves_the_binary_intact() {
        for case in [
            "sig-bitflip",
            "untrusted-key",
            "wrong-namespace",
            "staged-key",
            "truncated",
        ] {
            let base = serve(release_routes(case, fixture("payload.bin")));
            let s = Scene::new(case);
            let e = update(&s.request(&base, None, "v0.17.1")).unwrap_err();
            assert!(!e.is_empty(), "{case}");
            s.assert_untouched();
        }
    }

    #[test]
    fn e2e_tampered_binary_is_refused_and_cleaned_up() {
        let base = serve(release_routes("valid", b"not the signed bytes".to_vec()));
        let s = Scene::new("tampered");
        let e = update(&s.request(&base, None, "v0.17.1")).unwrap_err();
        assert!(e.contains("does not match the signed manifest"), "{e}");
        assert!(
            e.contains(&hex(&Sha256::digest(fixture("payload.bin")))),
            "{e}"
        );
        assert!(
            e.contains(&hex(&Sha256::digest(b"not the signed bytes"))),
            "{e}"
        );
        s.assert_untouched();
    }

    #[test]
    fn e2e_missing_manifest_on_a_signed_era_tag_is_refused() {
        let mut routes = release_routes("valid", fixture("payload.bin"));
        routes.retain(|(p, _, _)| !p.ends_with("/RELEASE-MANIFEST.sig"));
        let base = serve(routes);
        let s = Scene::new("nosig");
        let e = update(&s.request(&base, None, "v0.17.1")).unwrap_err();
        assert!(e.contains("has no RELEASE-MANIFEST"), "{e}");
        s.assert_untouched();
    }

    #[test]
    fn e2e_downgrade_is_refused_unless_pinned() {
        let base = serve(release_routes("valid", fixture("payload.bin")));
        let s = Scene::new("downgrade");
        let e = update(&s.request(&base, None, "v0.19.0")).unwrap_err();
        assert!(e.contains("refusing a downgrade"), "{e}");
        s.assert_untouched();
        // The same release, asked for by name, installs.
        assert_eq!(
            update(&s.request(&base, Some("v0.18.0"), "v0.19.0")),
            Ok(Outcome::Updated(t("v0.18.0")))
        );
    }

    #[test]
    fn e2e_up_to_date_downloads_nothing() {
        let base = serve(release_routes("valid", fixture("payload.bin")));
        let s = Scene::new("current");
        assert_eq!(
            update(&s.request(&base, None, "v0.18.0")),
            Ok(Outcome::UpToDate(t("v0.18.0")))
        );
        s.assert_untouched();
    }

    #[test]
    fn e2e_dry_run_verifies_but_writes_nothing() {
        let base = serve(release_routes("valid", fixture("payload.bin")));
        let s = Scene::new("dry");
        let mut req = s.request(&base, None, "v0.17.1");
        req.dry_run = true;
        assert_eq!(update(&req), Ok(Outcome::DryRun(t("v0.18.0"))));
        s.assert_untouched();
        let bad = serve(release_routes("sig-bitflip", fixture("payload.bin")));
        let mut req = s.request(&bad, None, "v0.17.1");
        req.dry_run = true;
        assert!(update(&req).is_err(), "a dry run checks the signature too");
    }

    #[test]
    fn e2e_failed_smoke_puts_the_old_binary_back() {
        let base = serve(release_routes("valid", fixture("payload.bin")));
        let s = Scene::new("smoke");
        let e = run(
            &s.request(&base, None, "v0.17.1"),
            &test_root(),
            &Http::new().unwrap(),
            &|_: &Path| Err("exit 1".to_string()),
        )
        .unwrap_err();
        assert!(e.contains("put back"), "{e}");
        s.assert_untouched();
    }

    #[test]
    fn e2e_presigning_release_is_checked_against_its_pinned_digest() {
        let base = serve(release_routes("valid", fixture("payload.bin")));
        let s = Scene::new("pre");
        assert_eq!(
            update(&s.request(&base, Some("v0.17.1"), "v0.17.1")),
            Ok(Outcome::Updated(t("v0.17.1")))
        );
        assert_eq!(s.state().0, fixture("payload.bin"));

        // The embedded table pins the REAL v0.17.1 SHA256SUMS, not the fixture.
        let s = Scene::new("pre-real");
        let e = run(
            &s.request(&base, Some("v0.17.1"), "v0.17.1"),
            &TrustRoot::embedded().unwrap(),
            &Http::new().unwrap(),
            &smoke_ok,
        )
        .unwrap_err();
        assert!(e.contains("does not match the digest pinned"), "{e}");
        s.assert_untouched();
    }

    #[test]
    fn e2e_asset_base_override_still_needs_a_valid_signature() {
        let good = serve(release_routes("valid", fixture("payload.bin")));
        let s = Scene::new("base");
        let mut req = s.request("http://127.0.0.1:9", Some("v0.18.0"), "v0.17.1");
        req.asset_base = Some(format!("{good}/releases/download/v0.18.0/"));
        assert_eq!(update(&req), Ok(Outcome::Updated(t("v0.18.0"))));

        let bad = serve(release_routes("untrusted-key", fixture("payload.bin")));
        let s = Scene::new("base-bad");
        let mut req = s.request("http://127.0.0.1:9", Some("v0.18.0"), "v0.17.1");
        req.asset_base = Some(format!("{bad}/releases/download/v0.18.0"));
        assert!(update(&req).unwrap_err().contains("not a release key"));
        s.assert_untouched();

        let mut req = s.request("http://127.0.0.1:9", None, "v0.17.1");
        req.asset_base = Some(good);
        assert!(update(&req).unwrap_err().contains("needs --version"));
        s.assert_untouched();
    }
}
