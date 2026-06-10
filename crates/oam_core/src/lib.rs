//! oam_core: the engine-agnostic async substrate.
//!
//! Owns the tokio runtime and the op-completion channel. Deliberately
//! v8-free: ops are spawned as plain futures producing [`OpOutcome`]
//! payloads; oam_engine bridges completions to V8 promises on the isolate
//! thread. Completions travel over a std::sync::mpsc channel because the
//! event loop consumes them from synchronous code (recv_timeout doubles as
//! the loop's idle sleep).
//!
//! Roadmap: the #[op] macro, the io_uring/IOCP completion IoDriver, and
//! per-workload tokio tuning land here as the op surface grows.

use std::future::Future;
use std::sync::mpsc;
use std::time::Instant;

pub use oam_diagnostics as diagnostics;

pub type OpId = u64;

/// What an async op produced. v8-free by design; the engine maps these to
/// promise resolutions (Done -> undefined, Text -> string, Json -> the
/// parsed value via V8's own JSON parser, Failed -> reject with
/// Error(message)).
#[derive(Debug)]
pub enum OpOutcome {
    Done,
    Text(String),
    /// A JSON document; the engine parses it on the isolate thread. The
    /// structured-payload path until a zero-copy transfer lands with the
    /// op-macro work.
    Json(String),
    /// Raw bytes; the engine surfaces a Uint8Array over a fresh backing
    /// store (JS wraps it in Buffer where node: semantics apply).
    Bytes(Vec<u8>),
    Failed(String),
    /// A failure carrying a Node errno code (ENOENT, EACCES, ...). The
    /// engine rejects with an Error whose `.code` property is set —
    /// ecosystem code branches on err.code constantly (graceful-fs et al).
    NodeFailed {
        code: String,
        message: String,
    },
}

#[derive(Debug)]
pub struct OpCompletion {
    pub id: OpId,
    pub outcome: OpOutcome,
}

pub struct CoreRuntime {
    tokio: tokio::runtime::Runtime,
    /// Shared HTTP client (connection pool). Owned per CoreRuntime so pooled
    /// connections never outlive the tokio runtime they were spawned on.
    http: reqwest::Client,
    tx: mpsc::Sender<OpCompletion>,
    rx: mpsc::Receiver<OpCompletion>,
    next_id: OpId,
    inflight: usize,
}

impl CoreRuntime {
    pub fn new() -> Result<Self, String> {
        // Process-wide TLS provider: ring (see workspace Cargo.toml for why
        // not aws-lc-rs). Err means a provider is already installed: fine.
        static TLS_PROVIDER: std::sync::Once = std::sync::Once::new();
        TLS_PROVIDER.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("oam-io")
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("oam/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let (tx, rx) = mpsc::channel();
        Ok(Self {
            tokio,
            http,
            tx,
            rx,
            next_id: 1,
            inflight: 0,
        })
    }

    /// Cheap Arc clone for ops that need the pooled HTTP client.
    pub fn http_client(&self) -> reqwest::Client {
        self.http.clone()
    }

    /// Spawn an async op; its completion will surface via try_recv /
    /// recv_deadline tagged with the returned id.
    pub fn spawn_op<F>(&mut self, op: F) -> OpId
    where
        F: Future<Output = OpOutcome> + Send + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        self.inflight += 1;
        let tx = self.tx.clone();
        self.tokio.spawn(async move {
            let outcome = op.await;
            // Receiver dropped means the runtime is shutting down: fine.
            let _ = tx.send(OpCompletion { id, outcome });
        });
        id
    }

    pub fn has_inflight(&self) -> bool {
        self.inflight > 0
    }

    pub fn try_recv(&mut self) -> Option<OpCompletion> {
        let completion = self.rx.try_recv().ok();
        if completion.is_some() {
            self.inflight -= 1;
        }
        completion
    }

    /// Block until a completion arrives or `deadline` passes (None = wait
    /// indefinitely — only call that way when has_inflight() is true, or
    /// it blocks forever).
    pub fn recv_deadline(&mut self, deadline: Option<Instant>) -> Option<OpCompletion> {
        let completion = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if deadline <= now {
                    return self.try_recv();
                }
                self.rx.recv_timeout(deadline - now).ok()
            }
            None => self.rx.recv().ok(),
        };
        if completion.is_some() {
            self.inflight -= 1;
        }
        completion
    }
}

/// Map an I/O error to the Node errno code ecosystem code branches on.
/// Shared by the async fs ops below and oam_engine's sync fs natives.
pub fn node_error_code(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        ErrorKind::NotADirectory => "ENOTDIR",
        ErrorKind::IsADirectory => "EISDIR",
        ErrorKind::InvalidInput => "EINVAL",
        ErrorKind::TimedOut => "ETIMEDOUT",
        ErrorKind::Interrupted => "EINTR",
        ErrorKind::Unsupported => "ENOSYS",
        ErrorKind::BrokenPipe => "EPIPE",
        ErrorKind::WouldBlock => "EAGAIN",
        _ => "EIO",
    }
}

/// Node-style error message: "ENOENT: no such file or directory, open 'p'".
pub fn node_error_message(code: &str, syscall: &str, path: &str, error: &std::io::Error) -> String {
    let reason = match code {
        "ENOENT" => "no such file or directory".to_string(),
        "EACCES" => "permission denied".to_string(),
        "EEXIST" => "file already exists".to_string(),
        "ENOTEMPTY" => "directory not empty".to_string(),
        "ENOTDIR" => "not a directory".to_string(),
        "EISDIR" => "illegal operation on a directory".to_string(),
        _ => error.to_string(),
    };
    format!("{code}: {reason}, {syscall} '{path}'")
}

/// rm with Node semantics: file or directory, optional recursion.
pub fn remove_path(path: &str, recursive: bool) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    }
}

/// std::fs::canonicalize returns \\?\-prefixed paths on Windows, which leak
/// into user-visible strings and break naive comparisons — strip the prefix.
pub fn strip_unc_prefix(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// Built-in op implementations. Plain futures; the engine decides how their
/// outcomes surface in JS.
pub mod ops {
    use super::OpOutcome;
    use std::time::Duration;

    pub async fn sleep(ms: u64) -> OpOutcome {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        OpOutcome::Done
    }

    fn node_fail(error: std::io::Error, syscall: &str, path: &str) -> OpOutcome {
        let code = super::node_error_code(&error);
        OpOutcome::NodeFailed {
            code: code.to_string(),
            message: super::node_error_message(code, syscall, path, &error),
        }
    }

    /// stat/lstat payload, shared with the sync native in oam_engine for a
    /// single wire shape ({kind, size, mtimeMs, ...}).
    pub fn stat_to_json(meta: &std::fs::Metadata) -> String {
        fn ms(time: std::io::Result<std::time::SystemTime>) -> f64 {
            time.ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0)
        }
        let kind = if meta.is_symlink() {
            "symlink"
        } else if meta.is_dir() {
            "dir"
        } else {
            "file"
        };
        serde_json::json!({
            "kind": kind,
            "size": meta.len(),
            "mtimeMs": ms(meta.modified()),
            "atimeMs": ms(meta.accessed()),
            "ctimeMs": ms(meta.modified()),
            "birthtimeMs": ms(meta.created()),
            "mode": 0,
        })
        .to_string()
    }

    pub fn readdir_to_json(path: &str) -> std::io::Result<String> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = match entry.file_type() {
                Ok(t) if t.is_symlink() => "symlink",
                Ok(t) if t.is_dir() => "dir",
                _ => "file",
            };
            entries.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": kind,
            }));
        }
        Ok(serde_json::Value::Array(entries).to_string())
    }

    pub async fn fs_read_file(path: String, encoding: Option<String>) -> OpOutcome {
        match tokio::fs::read(&path).await {
            Ok(bytes) => match encoding.as_deref() {
                None => OpOutcome::Bytes(bytes),
                Some(_) => OpOutcome::Text(String::from_utf8_lossy(&bytes).into_owned()),
            },
            Err(e) => node_fail(e, "open", &path),
        }
    }

    pub async fn fs_write_file(path: String, data: Vec<u8>, append: bool) -> OpOutcome {
        let result = if append {
            tokio::task::spawn_blocking({
                let path = path.clone();
                move || {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .and_then(|mut f| f.write_all(&data))
                }
            })
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        } else {
            tokio::fs::write(&path, data).await
        };
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "open", &path),
        }
    }

    pub async fn fs_stat(path: String, lstat: bool) -> OpOutcome {
        let meta = if lstat {
            tokio::fs::symlink_metadata(&path).await
        } else {
            tokio::fs::metadata(&path).await
        };
        match meta {
            Ok(meta) => OpOutcome::Json(stat_to_json(&meta)),
            Err(e) => node_fail(e, if lstat { "lstat" } else { "stat" }, &path),
        }
    }

    pub async fn fs_readdir(path: String) -> OpOutcome {
        match tokio::task::spawn_blocking({
            let path = path.clone();
            move || readdir_to_json(&path)
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)))
        {
            Ok(json) => OpOutcome::Json(json),
            Err(e) => node_fail(e, "scandir", &path),
        }
    }

    pub async fn fs_mkdir(path: String, recursive: bool) -> OpOutcome {
        let result = if recursive {
            tokio::fs::create_dir_all(&path).await
        } else {
            tokio::fs::create_dir(&path).await
        };
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "mkdir", &path),
        }
    }

    pub async fn fs_rm(path: String, recursive: bool, force: bool) -> OpOutcome {
        let result = tokio::task::spawn_blocking({
            let path = path.clone();
            move || super::remove_path(&path, recursive)
        })
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)));
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => OpOutcome::Done,
            Err(e) => node_fail(e, "rm", &path),
        }
    }

    pub async fn fs_unlink(path: String) -> OpOutcome {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "unlink", &path),
        }
    }

    pub async fn fs_rename(from: String, to: String) -> OpOutcome {
        match tokio::fs::rename(&from, &to).await {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "rename", &from),
        }
    }

    pub async fn fs_copy_file(from: String, to: String) -> OpOutcome {
        match tokio::fs::copy(&from, &to).await {
            Ok(_) => OpOutcome::Done,
            Err(e) => node_fail(e, "copyfile", &from),
        }
    }

    pub async fn fs_access(path: String) -> OpOutcome {
        match tokio::fs::metadata(&path).await {
            Ok(_) => OpOutcome::Done,
            Err(e) => node_fail(e, "access", &path),
        }
    }

    pub async fn fs_realpath(path: String) -> OpOutcome {
        match tokio::fs::canonicalize(&path).await {
            Ok(real) => OpOutcome::Text(super::strip_unc_prefix(&real)),
            Err(e) => node_fail(e, "realpath", &path),
        }
    }

    pub async fn read_text_file(path: String) -> OpOutcome {
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => OpOutcome::Text(text),
            Err(e) => OpOutcome::Failed(format!("could not read {path}: {e}")),
        }
    }

    /// Parse the wire JSON from the JS `fetch` wrapper. Kept here so the
    /// engine never needs a serde dependency.
    pub fn parse_fetch_request(json: &str) -> Result<FetchRequest, String> {
        serde_json::from_str(json).map_err(|e| format!("fetch: malformed request: {e}"))
    }

    /// The wire shape `fetch` sends down from JS (serialized JSON).
    #[derive(serde::Deserialize)]
    pub struct FetchRequest {
        pub url: String,
        #[serde(default)]
        pub method: Option<String>,
        #[serde(default)]
        pub headers: Vec<(String, String)>,
        #[serde(default)]
        pub body: Option<String>,
    }

    /// Buffered fetch (M1 subset: full-body, UTF-8-lossy text; streaming
    /// bodies arrive with ReadableStream). Returns Json with
    /// {status, statusText, url, headers: [[k,v]], body}.
    pub async fn fetch(client: reqwest::Client, req: FetchRequest) -> OpOutcome {
        let method = req.method.as_deref().unwrap_or("GET");
        let method = match reqwest::Method::from_bytes(method.as_bytes()) {
            Ok(m) => m,
            Err(_) => return OpOutcome::Failed(format!("fetch: invalid method '{method}'")),
        };
        let mut builder = client.request(method, &req.url);
        for (name, value) in &req.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = req.body {
            builder = builder.body(body);
        }
        let response = match builder.send().await {
            Ok(r) => r,
            Err(e) => return OpOutcome::Failed(format!("fetch failed: {e}")),
        };
        let status = response.status().as_u16();
        let status_text = response
            .status()
            .canonical_reason()
            .unwrap_or_default()
            .to_string();
        let url = response.url().to_string();
        // Compare PARSED urls: string comparison false-positives on
        // normalization (trailing slash, default port, percent-casing).
        let redirected = reqwest::Url::parse(&req.url)
            .map(|original| *response.url() != original)
            .unwrap_or(false);
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => return OpOutcome::Failed(format!("fetch: body read failed: {e}")),
        };
        let body = String::from_utf8_lossy(&bytes).into_owned();
        let payload = serde_json::json!({
            "status": status,
            "statusText": status_text,
            "url": url,
            "redirected": redirected,
            "headers": headers,
            "body": body,
        });
        OpOutcome::Json(payload.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ops_complete_and_inflight_tracks() {
        let mut core = CoreRuntime::new().unwrap();
        assert!(!core.has_inflight());
        let id = core.spawn_op(ops::sleep(5));
        assert!(core.has_inflight());
        let completion = core
            .recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("op completes");
        assert_eq!(completion.id, id);
        assert!(matches!(completion.outcome, OpOutcome::Done));
        assert!(!core.has_inflight());
    }

    #[test]
    fn recv_deadline_times_out_without_completion() {
        let mut core = CoreRuntime::new().unwrap();
        let _id = core.spawn_op(ops::sleep(60_000));
        let start = Instant::now();
        let completion = core.recv_deadline(Some(start + Duration::from_millis(30)));
        assert!(completion.is_none());
        assert!(core.has_inflight());
    }

    #[test]
    fn read_text_file_fails_cleanly_on_missing_path() {
        let mut core = CoreRuntime::new().unwrap();
        core.spawn_op(ops::read_text_file("/definitely/not/here.txt".into()));
        let completion = core
            .recv_deadline(Some(Instant::now() + Duration::from_secs(5)))
            .expect("op completes");
        assert!(matches!(completion.outcome, OpOutcome::Failed(_)));
    }
}
