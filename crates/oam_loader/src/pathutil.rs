//! Path finalization for resolved modules + file: URL conversion.
//!
//! Node resolves every module to its REAL path (`fs.realpath`) before using
//! it as the module's identity (`--preserve-symlinks` off is the default).
//! That is what makes pnpm-style layouts work: `node_modules/pkg-a` is a
//! symlink (junction on Windows) into `.pnpm/pkg-a@1/node_modules/pkg-a`,
//! and pkg-a's own dependency walk must start from the REAL location — where
//! `.pnpm/pkg-a@1/node_modules/pkg-b` is a sibling — not from the symlink.
//! Keying modules by realpath also gives one identity per file regardless of
//! how many links point at it (Node semantics).

use std::path::{Path, PathBuf};

/// Finalize a successful resolution: realpath the file so the module's
/// identity (cache key, referrer for its own imports, `__filename`,
/// `import.meta.url`) is the real location. Virtual modules (`node:` /
/// `oam:`) pass through. A canonicalize failure falls back to the probed
/// path — the file was just confirmed to exist, so failures here are
/// transient races, not resolution errors.
pub(crate) fn finalize_resolved(path: PathBuf) -> PathBuf {
    if is_virtual(&path) {
        return path;
    }
    match std::fs::canonicalize(&path) {
        Ok(real) => strip_verbatim(real),
        Err(_) => path,
    }
}

pub(crate) fn is_virtual(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|s| s.starts_with("node:") || s.starts_with("oam:"))
}

/// `std::fs::canonicalize` returns `\\?\`-prefixed verbatim paths on
/// Windows. Those leak into JS-visible strings (`__filename`,
/// `import.meta.url`, error messages) where Node shows plain drive paths,
/// and they break naive string handling in userland. Convert
/// `\\?\C:\x` -> `C:\x` and `\\?\UNC\srv\share\x` -> `\\srv\share\x`.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_string());
        }
        path
    }
    #[cfg(not(windows))]
    {
        path
    }
}

/// Convert a `file:` URL string to a filesystem path — Node's
/// `url.fileURLToPath` for the string form the resolver sees. Handles
/// empty / `localhost` hosts, Windows drive letters (`/C:/x`), Windows UNC
/// hosts (`file://server/share`), percent-decoding, and rejects encoded
/// path separators (`%2F` / `%5C`, Node's path-smuggling guard).
pub(crate) fn file_url_to_path(url: &str) -> Result<PathBuf, String> {
    let rest = url
        .get(..5)
        .filter(|p| p.eq_ignore_ascii_case("file:"))
        .map(|_| &url[5..])
        .ok_or_else(|| "not a file: URL".to_string())?;
    let rest = rest
        .strip_prefix("//")
        .ok_or_else(|| "file: URL must use the file://... form".to_string())?;
    let (host, raw_path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    // Query / fragment are not part of the filesystem path.
    let raw_path = raw_path.split(['?', '#']).next().unwrap_or("");
    if raw_path.is_empty() {
        return Err("file: URL has no path".to_string());
    }
    let lower = raw_path.to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") {
        return Err("file: URL path must not include encoded / or \\ characters".to_string());
    }
    let decoded = percent_decode(raw_path)?;
    #[cfg(windows)]
    {
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            // UNC: file://server/share/x -> \\server\share\x
            return Ok(PathBuf::from(format!(
                r"\\{host}{}",
                decoded.replace('/', r"\")
            )));
        }
        // Drive form: /C:/x -> C:/x
        let bytes = decoded.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
        {
            return Ok(PathBuf::from(&decoded[1..]));
        }
        Err("file: URL must carry an absolute path (file:///C:/...)".to_string())
    }
    #[cfg(not(windows))]
    {
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return Err(format!(
                "file: URL host '{host}' must be empty or localhost"
            ));
        }
        Ok(PathBuf::from(decoded))
    }
}

fn percent_decode(s: &str) -> Result<String, String> {
    if !s.contains('%') {
        return Ok(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let value = bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .ok_or_else(|| "invalid percent escape in file: URL".to_string())?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "file: URL path is not valid UTF-8".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_encoded_separators() {
        assert!(file_url_to_path("file:///a%2Fb").is_err());
        assert!(file_url_to_path("file:///a%5Cb").is_err());
    }

    #[test]
    fn decodes_percent_escapes() {
        #[cfg(windows)]
        let (url, expected) = ("file:///C:/a%20dir/b.js", PathBuf::from("C:/a dir/b.js"));
        #[cfg(not(windows))]
        let (url, expected) = ("file:///a%20dir/b.js", PathBuf::from("/a dir/b.js"));
        assert_eq!(file_url_to_path(url).unwrap(), expected);
    }

    #[test]
    fn strips_query_and_fragment() {
        #[cfg(windows)]
        let (url, expected) = ("file:///C:/x.js?v=1#frag", PathBuf::from("C:/x.js"));
        #[cfg(not(windows))]
        let (url, expected) = ("file:///x.js?v=1#frag", PathBuf::from("/x.js"));
        assert_eq!(file_url_to_path(url).unwrap(), expected);
    }

    #[test]
    fn scheme_is_case_insensitive_and_non_file_rejected() {
        #[cfg(windows)]
        assert_eq!(
            file_url_to_path("FILE:///C:/x.js").unwrap(),
            PathBuf::from("C:/x.js")
        );
        #[cfg(not(windows))]
        assert_eq!(
            file_url_to_path("FILE:///x.js").unwrap(),
            PathBuf::from("/x.js")
        );
        assert!(file_url_to_path("https://example.com/x.js").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_unc_host_maps_to_unc_path() {
        assert_eq!(
            file_url_to_path("file://server/share/x.js").unwrap(),
            PathBuf::from(r"\\server\share\x.js")
        );
    }

    #[cfg(unix)]
    #[test]
    fn realpath_resolves_symlinked_package_identity() {
        // temp_dir + pid, matching the fixture idiom in npm.rs (no tempfile
        // dep); the symlink is re-created fresh in case of a prior run.
        let dir = std::env::temp_dir().join(format!("oam-pathutil-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let target = real.join("mod.js");
        std::fs::write(&target, "").unwrap();
        let link_dir = dir.join("link");
        let _ = std::fs::remove_file(&link_dir);
        std::os::unix::fs::symlink(&real, &link_dir).unwrap();
        let via_link = link_dir.join("mod.js");
        let finalized = finalize_resolved(via_link);
        assert_eq!(finalized, std::fs::canonicalize(&target).unwrap());
    }

    #[test]
    fn virtual_paths_pass_through() {
        assert_eq!(
            finalize_resolved(PathBuf::from("node:fs")),
            PathBuf::from("node:fs")
        );
        assert_eq!(
            finalize_resolved(PathBuf::from("oam:test")),
            PathBuf::from("oam:test")
        );
    }
}
