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

use std::collections::HashMap;
use std::future::Future;
use std::sync::mpsc;
use std::time::Instant;

use futures_util::FutureExt;

pub use oam_diagnostics as diagnostics;

pub mod child;
pub mod cluster;
pub mod dns;
pub mod http_server;
pub mod inspector;
pub mod tcp;
pub mod tls;
pub mod udp;
pub mod websocket;
pub mod worker;

/// io_uring FS fast path (Linux-only, opt-in via OAM_IO_URING). See
/// docs/design/io_uring.md. cfg'd out on every other platform.
#[cfg(target_os = "linux")]
mod io_uring_fs;

pub type OpId = u64;

/// What an async op produced. v8-free by design; the engine maps these to
/// promise resolutions (Done -> undefined, Text -> string, Json -> the
/// parsed value via V8's own JSON parser, Failed -> reject with
/// Error(message)).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

/// Live streaming response bodies, keyed by handle. A std (not tokio)
/// Mutex on purpose: readers REMOVE the response under a short lock, await
/// the chunk with no lock held, then reinsert — no guard ever crosses an
/// await. Single-reader discipline is guaranteed by ReadableStream's lock.
pub type BodyRegistry = std::sync::Arc<std::sync::Mutex<HashMap<u64, reqwest::Response>>>;

/// Open file handles for fs streams -- same remove-await-reinsert
/// discipline as BodyRegistry (node:stream's write queue serializes
/// access per handle). The `closed` set is the generation guard: a chunk
/// op removes the File, awaits IO unlocked, then reinserts -- but if
/// fsClose landed during that await (stream.destroy() racing an in-flight
/// read), the reinsert would resurrect a leaked fd. closed tracks ids
/// retired mid-flight so the reinsert drops the File instead.
#[derive(Default)]
pub struct FileState {
    pub files: HashMap<u64, tokio::fs::File>,
    pub closed: std::collections::HashSet<u64>,
}
pub type FileRegistry = std::sync::Arc<std::sync::Mutex<FileState>>;

/// Incremental zlib/brotli stream state. Each entry is an encoder or decoder
/// that accepts chunks one at a time. The JS Transform wires _transform to
/// zlibStreamWrite and _flush to zlibStreamFlush.
///
/// Variants:
/// - Compress/Decompress: flate2 gzip/deflate/deflateRaw, truly incremental.
/// - BrotliCompress/BrotliDecompress: pure-Rust brotli via the `brotli` crate.
pub enum ZlibStream {
    Compress(zlib::StreamCompressor),
    Decompress(zlib::StreamDecompressor),
    // Brotli state is large (~5 KB for the compressor); Box keeps the enum
    // discriminant compact so the gzip/deflate variants (the hot path) don't
    // pay for brotli's footprint in the HashMap registry. Heap indirection
    // is paid once per brotli stream, never on the per-chunk write path.
    BrotliCompress(Box<BrotliCompressor>),
    BrotliDecompress(Box<BrotliDecompressor>),
    HandleCompress(flate2::Compress),
    HandleDecompress(flate2::Decompress),
}

pub type ZlibRegistry = std::sync::Arc<std::sync::Mutex<HashMap<u64, ZlibStream>>>;

pub struct CoreRuntime {
    /// Option so Drop can take it for shutdown_background (see below).
    tokio: Option<tokio::runtime::Runtime>,
    /// Shared HTTP client (connection pool). Owned per CoreRuntime so pooled
    /// connections never outlive the tokio runtime they were spawned on.
    http: reqwest::Client,
    tx: mpsc::Sender<OpCompletion>,
    rx: mpsc::Receiver<OpCompletion>,
    next_id: OpId,
    inflight: usize,
    bodies: BodyRegistry,
    files: FileRegistry,
    zlib_streams: ZlibRegistry,
    http_state: std::sync::Arc<http_server::HttpState>,
    tcp: tcp::TcpRegistry,
    tls: tls::TlsRegistry,
    udp: udp::UdpRegistry,
    ws: websocket::WsRegistry,
    workers: worker::WorkerRegistry,
    children: child::ChildRegistry,
    next_body: std::sync::Arc<std::sync::atomic::AtomicU64>,
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
            tokio: Some(tokio),
            http,
            tx,
            rx,
            next_id: 1,
            inflight: 0,
            bodies: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            files: std::sync::Arc::new(std::sync::Mutex::new(FileState::default())),
            zlib_streams: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            http_state: std::sync::Arc::new(http_server::HttpState::default()),
            tcp: std::sync::Arc::new(std::sync::Mutex::new(tcp::TcpState::default())),
            tls: std::sync::Arc::new(std::sync::Mutex::new(tls::TlsState::default())),
            udp: std::sync::Arc::new(std::sync::Mutex::new(udp::UdpState::default())),
            ws: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            workers: std::sync::Arc::new(std::sync::Mutex::new(worker::WorkerState::default())),
            children: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            next_body: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        })
    }

    /// Cheap Arc clone for ops that need the pooled HTTP client.
    pub fn http_client(&self) -> reqwest::Client {
        self.http.clone()
    }

    /// The streaming-body registry (Arc clone). Dies with the CoreRuntime,
    /// so per-run resets drop any unread bodies.
    pub fn bodies(&self) -> BodyRegistry {
        self.bodies.clone()
    }

    /// Allocator handle for new streaming bodies AND file handles (one id
    /// space; the registries are separate).
    pub fn body_ids(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.next_body.clone()
    }

    /// Open-file registry for fs streams (Arc clone; dies with the run).
    pub fn files(&self) -> FileRegistry {
        self.files.clone()
    }

    /// Incremental zlib stream registry (Arc clone; dies with the run).
    pub fn zlib_streams(&self) -> ZlibRegistry {
        self.zlib_streams.clone()
    }

    /// HTTP server state (Arc clone; servers die with the run).
    pub fn http(&self) -> std::sync::Arc<http_server::HttpState> {
        self.http_state.clone()
    }

    /// TCP socket registry (Arc clone; dies with the run).
    pub fn tcp(&self) -> tcp::TcpRegistry {
        self.tcp.clone()
    }

    /// TLS socket registry (Arc clone; dies with the run).
    pub fn tls(&self) -> tls::TlsRegistry {
        self.tls.clone()
    }

    /// UDP socket registry (Arc clone; dies with the run).
    pub fn udp(&self) -> udp::UdpRegistry {
        self.udp.clone()
    }

    /// WebSocket connection registry (Arc clone; dies with the run).
    pub fn ws(&self) -> websocket::WsRegistry {
        self.ws.clone()
    }

    /// Worker thread registry (Arc clone; parent side).
    pub fn workers(&self) -> worker::WorkerRegistry {
        self.workers.clone()
    }

    /// Child process registry (Arc clone; dies with the run).
    pub fn children(&self) -> child::ChildRegistry {
        self.children.clone()
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
        self.tokio
            .as_ref()
            .expect("runtime alive")
            .spawn(async move {
                let outcome = match std::panic::AssertUnwindSafe(op).catch_unwind().await {
                    Ok(outcome) => outcome,
                    Err(payload) => {
                        let msg = payload
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
                            .unwrap_or("internal panic in async op");
                        OpOutcome::Failed(format!("panic: {msg}"))
                    }
                };
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

impl Drop for CoreRuntime {
    fn drop(&mut self) {
        // The run is over: nothing on the IO runtime may block process
        // exit. A plain Runtime::drop WAITS — an idle keep-alive
        // connection (reqwest pool, 90s idle timeout; a hyper server
        // conn) turned exit into a 90-second hang. shutdown_background
        // drops everything without waiting.
        if let Some(runtime) = self.tokio.take() {
            runtime.shutdown_background();
        }
    }
}

pub async fn stdin_read() -> OpOutcome {
    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 65536];
    match stdin.read(&mut buf).await {
        Ok(0) => OpOutcome::Done,
        Ok(n) => {
            buf.truncate(n);
            OpOutcome::Bytes(buf)
        }
        Err(e) => OpOutcome::Failed(format!("stdin read: {e}")),
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

/// fs.access semantics: existence always; W_OK (mode & 2) additionally
/// requires the file not be read-only — Node throws EPERM on Windows for
/// W_OK against a read-only file, and programs gate writes on exactly this
/// call. X_OK is approximated as existence (wave 1). Err is (code, message).
pub fn check_access(path: &str, mode: i32) -> Result<(), (String, String)> {
    let meta = std::fs::metadata(path).map_err(|e| {
        let code = node_error_code(&e);
        (
            code.to_string(),
            node_error_message(code, "access", path, &e),
        )
    })?;
    if mode & 2 != 0 && meta.permissions().readonly() {
        let code = if cfg!(windows) { "EPERM" } else { "EACCES" };
        return Err((
            code.to_string(),
            format!("{code}: operation not permitted, access '{path}'"),
        ));
    }
    Ok(())
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

/// node:zlib backend (flate2 + brotli). Sync fns serve the *Sync natives
/// directly; the async ops below wrap them in spawn_blocking -- compression
/// is CPU work and must not sit on the isolate thread for the callback forms.
///
/// Incremental streaming (StreamCompressor / StreamDecompressor /
/// BrotliCompressor / BrotliDecompressor) backs the JS Transform classes.
/// Each JS Transform stream creates one handle in the ZlibRegistry;
/// _transform feeds chunks via zlibStreamWrite and _flush finalizes via
/// zlibStreamFlush.
pub mod zlib {
    use flate2::Compression;
    use std::io::{Read, Write};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Format {
        Gzip,
        Deflate,
        DeflateRaw,
    }

    impl Format {
        pub fn parse(name: &str) -> Option<Self> {
            Some(match name {
                "gzip" => Format::Gzip,
                "deflate" => Format::Deflate,
                "deflateRaw" => Format::DeflateRaw,
                _ => return None,
            })
        }
    }

    pub fn compress(bytes: &[u8], format: Format, level: i32) -> std::io::Result<Vec<u8>> {
        // Node levels: -1 default, 0..=9. flate2 default is 6, same as zlib.
        let level = if (0..=9).contains(&level) {
            Compression::new(level as u32)
        } else {
            Compression::default()
        };
        match format {
            Format::Gzip => {
                let mut encoder = flate2::write::GzEncoder::new(Vec::new(), level);
                encoder.write_all(bytes)?;
                encoder.finish()
            }
            Format::Deflate => {
                let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), level);
                encoder.write_all(bytes)?;
                encoder.finish()
            }
            Format::DeflateRaw => {
                let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), level);
                encoder.write_all(bytes)?;
                encoder.finish()
            }
        }
    }

    pub fn decompress(bytes: &[u8], format: Format) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        match format {
            Format::Gzip => {
                flate2::read::GzDecoder::new(bytes).read_to_end(&mut out)?;
            }
            Format::Deflate => {
                flate2::read::ZlibDecoder::new(bytes).read_to_end(&mut out)?;
            }
            Format::DeflateRaw => {
                flate2::read::DeflateDecoder::new(bytes).read_to_end(&mut out)?;
            }
        }
        Ok(out)
    }

    /// Node's unzip*: auto-detect gzip (1f 8b magic) vs zlib-wrapped.
    pub fn unzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        if bytes.starts_with(&[0x1f, 0x8b]) {
            decompress(bytes, Format::Gzip)
        } else {
            decompress(bytes, Format::Deflate)
        }
    }

    // ----------------------------------------------------------------
    // Incremental streaming: gzip / deflate / deflateRaw
    //
    // We use flate2's write-based encoders (GzEncoder, ZlibEncoder,
    // DeflateEncoder) for compression, draining the backing Vec<u8>
    // via get_mut() + mem::take() after each write_all. This is truly
    // incremental: compressed bytes are emitted per-chunk with no need
    // to buffer the full input.
    //
    // For decompression we likewise use the write-based decoders
    // (GzDecoder, ZlibDecoder, DeflateDecoder). Each decoder accepts
    // a chunk, runs it through the inflate state machine, and appends
    // decompressed bytes to the inner Vec<u8>. We drain via mem::take
    // after each write so memory stays bounded (~64 kB per stream plus
    // the decompressed output for that chunk).
    //
    // The "unzip" auto-detect variant peeks at the first two bytes on
    // the initial write_chunk call to resolve the format, then creates
    // the appropriate decoder.
    //
    // Send requirement: all flate2 encoder/decoder types are Send, and
    // our wrappers hold no thread-local state.
    // ----------------------------------------------------------------

    /// Wraps any of the three flate2 write-encoders behind a uniform
    /// interface. Created via `StreamCompressor::new`; consumes chunks via
    /// `write_chunk`; finalizes via `finish` (emits the trailing CRC /
    /// checksum bytes the format requires).
    pub struct StreamCompressor {
        inner: CompressorInner,
    }

    enum CompressorInner {
        Gzip(flate2::write::GzEncoder<Vec<u8>>),
        Deflate(flate2::write::ZlibEncoder<Vec<u8>>),
        DeflateRaw(flate2::write::DeflateEncoder<Vec<u8>>),
    }

    impl StreamCompressor {
        pub fn new(format: Format, level: i32) -> Self {
            let level = if (0..=9).contains(&level) {
                Compression::new(level as u32)
            } else {
                Compression::default()
            };
            let inner = match format {
                Format::Gzip => {
                    CompressorInner::Gzip(flate2::write::GzEncoder::new(Vec::new(), level))
                }
                Format::Deflate => {
                    CompressorInner::Deflate(flate2::write::ZlibEncoder::new(Vec::new(), level))
                }
                Format::DeflateRaw => CompressorInner::DeflateRaw(
                    flate2::write::DeflateEncoder::new(Vec::new(), level),
                ),
            };
            Self { inner }
        }

        /// Feed a chunk. Returns whatever bytes the encoder produced
        /// immediately (may be empty -- the encoder buffers internally
        /// until it has a full deflate block ready).
        #[inline]
        pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
            match &mut self.inner {
                CompressorInner::Gzip(enc) => {
                    enc.write_all(chunk)?;
                    Ok(std::mem::take(enc.get_mut()))
                }
                CompressorInner::Deflate(enc) => {
                    enc.write_all(chunk)?;
                    Ok(std::mem::take(enc.get_mut()))
                }
                CompressorInner::DeflateRaw(enc) => {
                    enc.write_all(chunk)?;
                    Ok(std::mem::take(enc.get_mut()))
                }
            }
        }

        /// Flush and finalize. Consumes self; returns the tail bytes
        /// (including the gzip/zlib trailer). After this the stream handle
        /// is dropped -- close is implicit.
        pub fn finish(self) -> std::io::Result<Vec<u8>> {
            match self.inner {
                CompressorInner::Gzip(enc) => enc.finish(),
                CompressorInner::Deflate(enc) => enc.finish(),
                CompressorInner::DeflateRaw(enc) => enc.finish(),
            }
        }
    }

    // Manual Send -- CompressorInner holds encoder types that are all Send.
    unsafe impl Send for StreamCompressor {}

    // ----------------------------------------------------------------
    // Truly incremental decompressor -- slice A.
    //
    // Uses flate2's write-based decoders (GzDecoder, ZlibDecoder,
    // DeflateDecoder) so each write_chunk call invokes the inflate state
    // machine immediately and returns whatever bytes were decoded, bounded
    // by the chunk size. The full compressed stream never needs to live
    // in memory simultaneously.
    //
    // The `Unzip` variant defers decoder creation until the first
    // non-empty write_chunk, at which point it peeks the magic bytes to
    // choose Gzip or Deflate.
    // ----------------------------------------------------------------

    /// Truly incremental flate2 decompressor: memory usage bounded by
    /// ~64 kB scratch per stream regardless of input size.
    pub struct StreamDecompressor {
        inner: DecompressorInner,
    }

    enum DecompressorInner {
        Gzip(flate2::write::GzDecoder<Vec<u8>>),
        Deflate(flate2::write::ZlibDecoder<Vec<u8>>),
        DeflateRaw(flate2::write::DeflateDecoder<Vec<u8>>),
        /// Pending auto-detect: first chunk resolves to Gzip or Deflate.
        Unzip,
    }

    impl StreamDecompressor {
        pub fn new_gzip() -> Self {
            Self {
                inner: DecompressorInner::Gzip(flate2::write::GzDecoder::new(Vec::new())),
            }
        }
        pub fn new_deflate() -> Self {
            Self {
                inner: DecompressorInner::Deflate(flate2::write::ZlibDecoder::new(Vec::new())),
            }
        }
        pub fn new_deflate_raw() -> Self {
            Self {
                inner: DecompressorInner::DeflateRaw(
                    flate2::write::DeflateDecoder::new(Vec::new()),
                ),
            }
        }
        pub fn new_unzip() -> Self {
            Self {
                inner: DecompressorInner::Unzip,
            }
        }

        /// Feed one chunk of compressed data. Returns the decompressed bytes
        /// produced by this chunk (may be smaller than expected if the
        /// deflate block spans multiple chunks -- the remaining bytes arrive
        /// on subsequent calls). Memory usage stays bounded: we drain the
        /// inner Vec via mem::take after each write.
        #[inline]
        pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
            if chunk.is_empty() {
                return Ok(Vec::new());
            }
            // Resolve auto-detect on first non-empty chunk.
            if matches!(self.inner, DecompressorInner::Unzip) {
                if chunk.starts_with(&[0x1f, 0x8b]) {
                    self.inner = DecompressorInner::Gzip(flate2::write::GzDecoder::new(Vec::new()));
                } else {
                    self.inner =
                        DecompressorInner::Deflate(flate2::write::ZlibDecoder::new(Vec::new()));
                }
            }
            match &mut self.inner {
                DecompressorInner::Gzip(dec) => {
                    dec.write_all(chunk)?;
                    Ok(std::mem::take(dec.get_mut()))
                }
                DecompressorInner::Deflate(dec) => {
                    dec.write_all(chunk)?;
                    Ok(std::mem::take(dec.get_mut()))
                }
                DecompressorInner::DeflateRaw(dec) => {
                    dec.write_all(chunk)?;
                    Ok(std::mem::take(dec.get_mut()))
                }
                DecompressorInner::Unzip => unreachable!("resolved above"),
            }
        }

        /// Finalize: flush the inflate state and return any remaining
        /// decompressed bytes. For gzip this verifies the CRC/ISIZE trailer.
        pub fn finish(self) -> std::io::Result<Vec<u8>> {
            match self.inner {
                DecompressorInner::Gzip(dec) => dec.finish(),
                DecompressorInner::Deflate(dec) => dec.finish(),
                DecompressorInner::DeflateRaw(dec) => dec.finish(),
                // No data was ever written (empty stream).
                DecompressorInner::Unzip => Ok(Vec::new()),
            }
        }
    }

    unsafe impl Send for StreamDecompressor {}
}

// ----------------------------------------------------------------
// Brotli incremental stream types -- slice B.
//
// Uses the `brotli` crate (pure-Rust, default features: std +
// alloc-stdlib; no ffi-api, no simd, no native deps) with its
// write-based CompressorWriter / DecompressorWriter API.
//
// Memory: both types use a 64 kB internal buffer; decompressed output
// drains via mem::take on every write_chunk. The full input never
// needs to live in memory.
// ----------------------------------------------------------------

/// Buffer size for both brotli encoder and decoder: 64 kB scratch.
const BROTLI_BUF: usize = 65536;

/// Brotli quality: 4 is a good default (fast, reasonable ratio).
/// Node's default is 11 (max quality) but that is very slow for large
/// streams -- 4 gives ~10x the throughput at reasonable compression.
const BROTLI_QUALITY: u32 = 4;

/// lgwin: log2 of the sliding window. 22 = 4 MB window (brotli default).
const BROTLI_LGWIN: u32 = 22;

/// Incremental brotli compressor wrapping `brotli::CompressorWriter`.
pub struct BrotliCompressor {
    inner: brotli::CompressorWriter<Vec<u8>>,
}

impl Default for BrotliCompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl BrotliCompressor {
    pub fn new() -> Self {
        Self {
            inner: brotli::CompressorWriter::new(
                Vec::new(),
                BROTLI_BUF,
                BROTLI_QUALITY,
                BROTLI_LGWIN,
            ),
        }
    }

    /// Feed one chunk. Returns compressed bytes produced so far.
    /// Memory stays bounded: inner Vec is drained via mem::take.
    #[inline]
    pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
        use std::io::Write;
        self.inner.write_all(chunk)?;
        Ok(std::mem::take(self.inner.get_mut()))
    }

    /// Finalize: write the brotli stream-end marker and return all tail bytes.
    /// `into_inner()` calls BROTLI_OPERATION_FINISH internally and returns
    /// the inner Vec containing the final compressed bytes (stream-end marker).
    pub fn finish(self) -> std::io::Result<Vec<u8>> {
        Ok(self.inner.into_inner())
    }
}

// CompressorWriter<Vec<u8>> is Send -- Vec<u8> is Send and the brotli
// state is self-contained with no thread-local references.
unsafe impl Send for BrotliCompressor {}

/// Incremental brotli decompressor wrapping `brotli::DecompressorWriter`.
pub struct BrotliDecompressor {
    inner: brotli::DecompressorWriter<Vec<u8>>,
}

impl Default for BrotliDecompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl BrotliDecompressor {
    pub fn new() -> Self {
        Self {
            inner: brotli::DecompressorWriter::new(Vec::new(), BROTLI_BUF),
        }
    }

    /// Feed one chunk of compressed data. Returns decompressed bytes
    /// produced so far. Memory stays bounded: inner Vec is drained via
    /// mem::take after each write.
    #[inline]
    pub fn write_chunk(&mut self, chunk: &[u8]) -> std::io::Result<Vec<u8>> {
        use std::io::Write;
        self.inner.write_all(chunk)?;
        Ok(std::mem::take(self.inner.get_mut()))
    }

    /// Finalize: close the brotli decompressor and return any remaining
    /// output bytes. `into_inner()` calls `close()` and returns the inner
    /// Vec; on decompressor error it returns `Err(Vec)` which we convert
    /// to an io::Error (the partial bytes are discarded on corruption).
    pub fn finish(self) -> std::io::Result<Vec<u8>> {
        self.inner.into_inner().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "brotli decompressor: stream is incomplete or corrupt",
            )
        })
    }
}

unsafe impl Send for BrotliDecompressor {}

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
            // ctime not available on stable Rust; approximating with mtime
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

    pub async fn fs_read_file(path: String) -> OpOutcome {
        // Always raw bytes: encodings decode JS-side via Buffer#toString
        // (a Rust-side utf8-lossy decode was silently wrong for base64/
        // hex/latin1 requests).
        //
        // io_uring fast path (Linux, opt-in via OAM_IO_URING): on success use
        // the bytes; on ANY error (incl. io_uring unavailable / worker gone)
        // fall through to the std path below, which is authoritative for the
        // node-shaped error mapping. io_uring is a pure optimization here.
        #[cfg(target_os = "linux")]
        {
            if let Some(uring) = crate::io_uring_fs::global()
                && let Ok(bytes) = uring.read_file(path.clone()).await
            {
                return OpOutcome::Bytes(bytes);
            }
        }
        match tokio::fs::read(&path).await {
            Ok(bytes) => OpOutcome::Bytes(bytes),
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

    pub async fn fs_access(path: String, mode: i32) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || match super::check_access(&path, mode) {
            Ok(()) => OpOutcome::Done,
            Err((code, message)) => OpOutcome::NodeFailed { code, message },
        })
        .await;
        result.unwrap_or_else(|e| OpOutcome::Failed(format!("access: {e}")))
    }

    pub async fn fs_realpath(path: String) -> OpOutcome {
        match tokio::fs::canonicalize(&path).await {
            Ok(real) => OpOutcome::Text(super::strip_unc_prefix(&real)),
            Err(e) => node_fail(e, "realpath", &path),
        }
    }

    pub async fn fs_mkdtemp(prefix: String) -> OpOutcome {
        let dir = std::env::temp_dir().join(format!(
            "{}{}",
            prefix,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        match tokio::fs::create_dir(&dir).await {
            Ok(()) => OpOutcome::Text(super::strip_unc_prefix(&dir)),
            Err(e) => node_fail(e, "mkdtemp", &prefix),
        }
    }

    pub async fn fs_symlink(target: String, path: String) -> OpOutcome {
        #[cfg(windows)]
        let result = {
            let is_dir = tokio::fs::metadata(&target)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if is_dir {
                tokio::fs::symlink_dir(&target, &path).await
            } else {
                tokio::fs::symlink_file(&target, &path).await
            }
        };
        #[cfg(not(windows))]
        let result = tokio::fs::symlink(&target, &path).await;
        match result {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "symlink", &path),
        }
    }

    pub async fn fs_readlink(path: String) -> OpOutcome {
        match tokio::fs::read_link(&path).await {
            Ok(target) => OpOutcome::Text(super::strip_unc_prefix(&target)),
            Err(e) => node_fail(e, "readlink", &path),
        }
    }

    pub async fn fs_link(existing: String, new_path: String) -> OpOutcome {
        match tokio::fs::hard_link(&existing, &new_path).await {
            Ok(()) => OpOutcome::Done,
            Err(e) => node_fail(e, "link", &new_path),
        }
    }

    pub async fn fs_chmod(path: String, mode: u32) -> OpOutcome {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).await {
                Ok(()) => OpOutcome::Done,
                Err(e) => node_fail(e, "chmod", &path),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            let readonly = mode & 0o200 == 0;
            match tokio::fs::metadata(&path).await {
                Ok(meta) => {
                    let mut perms = meta.permissions();
                    perms.set_readonly(readonly);
                    match tokio::fs::set_permissions(&path, perms).await {
                        Ok(()) => OpOutcome::Done,
                        Err(e) => node_fail(e, "chmod", &path),
                    }
                }
                Err(e) => node_fail(e, "chmod", &path),
            }
        }
    }

    pub async fn fs_truncate(path: String, len: u64) -> OpOutcome {
        match tokio::fs::OpenOptions::new().write(true).open(&path).await {
            Ok(f) => match f.set_len(len).await {
                Ok(()) => OpOutcome::Done,
                Err(e) => node_fail(e, "truncate", &path),
            },
            Err(e) => node_fail(e, "truncate", &path),
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
        #[serde(default)]
        pub body_base64: Option<String>,
        /// Connection pin: dial this IP for the URL's host instead of
        /// resolving DNS, while keeping Host header + TLS SNI = the host.
        /// Set by globalThis.fetch when an undici dispatcher carries a
        /// connect.lookup hook (DNS-rebind / SSRF pinning).
        #[serde(default)]
        pub pin: Option<FetchPin>,
    }

    /// A DNS/connect pin: `host` (the URL hostname, kept for Host + SNI) ->
    /// `ip` (the literal address to dial).
    #[derive(serde::Deserialize)]
    pub struct FetchPin {
        pub host: String,
        pub ip: String,
    }

    /// Build a one-off reqwest client that resolves `pin.host` to `pin.ip`
    /// (port from the URL), keeping the shared client's config. Used only for
    /// pinned requests, so the normal fetch path keeps the pooled client.
    fn build_pinned_client(url: &str, pin: &FetchPin) -> Result<reqwest::Client, String> {
        let parsed = reqwest::Url::parse(url).map_err(|e| format!("bad url: {e}"))?;
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| "url has no port and no known default".to_string())?;
        let ip: std::net::IpAddr = pin
            .ip
            .parse()
            .map_err(|e| format!("pin ip '{}' is not an IP: {e}", pin.ip))?;
        let addr = std::net::SocketAddr::new(ip, port);
        reqwest::Client::builder()
            .user_agent(concat!("oam/", env!("CARGO_PKG_VERSION")))
            .resolve(&pin.host, addr)
            .build()
            .map_err(|e| format!("pinned http client: {e}"))
    }

    /// Streaming fetch: resolves at HEADERS time with the response shape
    /// plus a body handle. The body streams through fetch_body_read one
    /// chunk per op — `for await (const chunk of response.body)` sees
    /// tokens as the server flushes them (the SSE/AI-client path).
    pub async fn fetch(
        client: reqwest::Client,
        req: FetchRequest,
        bodies: super::BodyRegistry,
        ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> OpOutcome {
        let method = req.method.as_deref().unwrap_or("GET");
        let method = match reqwest::Method::from_bytes(method.as_bytes()) {
            Ok(m) => m,
            Err(_) => return OpOutcome::Failed(format!("fetch: invalid method '{method}'")),
        };
        // Connection pin (DNS-rebind / SSRF protection): a one-off client that
        // dials the verified IP for this host. Normal requests keep the
        // pooled client untouched.
        let client = match &req.pin {
            Some(pin) => match build_pinned_client(&req.url, pin) {
                Ok(c) => c,
                Err(e) => return OpOutcome::Failed(format!("fetch: connect pin failed: {e}")),
            },
            None => client,
        };
        let mut builder = client.request(method, &req.url);
        for (name, value) in &req.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = req.body {
            builder = builder.body(body);
        } else if let Some(b64) = req.body_base64 {
            use base64::Engine;
            match base64::engine::general_purpose::STANDARD.decode(&b64) {
                Ok(bytes) => {
                    builder = builder.body(bytes);
                }
                Err(_) => return OpOutcome::Failed("fetch: malformed base64 body".into()),
            }
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
        let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        bodies
            .lock()
            .expect("body registry lock")
            .insert(handle, response);
        let payload = serde_json::json!({
            "status": status,
            "statusText": status_text,
            "url": url,
            "redirected": redirected,
            "headers": headers,
            "bodyHandle": handle,
        });
        OpOutcome::Json(payload.to_string())
    }

    /// zlibStreamCreate: allocate an incremental compressor or decompressor.
    /// Returns Json {handle} on success. compress=true for encoding,
    /// false for decoding. format must be "gzip", "deflate", "deflateRaw",
    /// "unzip" (decompress only), or "brotli".
    pub async fn zlib_stream_create(
        streams: super::ZlibRegistry,
        ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
        format: String,
        level: i32,
        compress: bool,
    ) -> OpOutcome {
        // Stream allocation is cheap: do it inline (no IO).
        let stream = if format == "brotli" {
            // Brotli: pure-Rust incremental backend.
            if compress {
                super::ZlibStream::BrotliCompress(Box::default())
            } else {
                super::ZlibStream::BrotliDecompress(Box::default())
            }
        } else if compress {
            let Some(fmt) = super::zlib::Format::parse(&format) else {
                return OpOutcome::Failed(format!("zlib stream: unknown format '{format}'"));
            };
            super::ZlibStream::Compress(super::zlib::StreamCompressor::new(fmt, level))
        } else {
            let dec = match format.as_str() {
                "gzip" => super::zlib::StreamDecompressor::new_gzip(),
                "deflate" => super::zlib::StreamDecompressor::new_deflate(),
                "deflateRaw" => super::zlib::StreamDecompressor::new_deflate_raw(),
                "unzip" => super::zlib::StreamDecompressor::new_unzip(),
                _ => return OpOutcome::Failed(format!("zlib stream: unknown format '{format}'")),
            };
            super::ZlibStream::Decompress(dec)
        };
        let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        streams
            .lock()
            .expect("zlib stream registry lock")
            .insert(handle, stream);
        OpOutcome::Json(serde_json::json!({ "handle": handle }).to_string())
    }

    /// zlibStreamWrite: feed one chunk into an incremental stream.
    /// Resolves with the bytes produced immediately (may be empty for
    /// compressors that buffer internally). Runs on spawn_blocking --
    /// the CPU work is non-trivial for large chunks.
    pub async fn zlib_stream_write(
        streams: super::ZlibRegistry,
        handle: u64,
        chunk: Vec<u8>,
    ) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = streams.lock().unwrap_or_else(|e| e.into_inner());
            let Some(stream) = guard.get_mut(&handle) else {
                return Err(format!("zlib stream: handle {handle} not found"));
            };
            match stream {
                super::ZlibStream::Compress(enc) => enc
                    .write_chunk(&chunk)
                    .map_err(|e| format!("zlib stream write: {e}")),
                super::ZlibStream::Decompress(dec) => dec
                    .write_chunk(&chunk)
                    .map_err(|e| format!("zlib stream write: {e}")),
                super::ZlibStream::BrotliCompress(enc) => enc
                    .write_chunk(&chunk)
                    .map_err(|e| format!("brotli stream write: {e}")),
                super::ZlibStream::BrotliDecompress(dec) => dec
                    .write_chunk(&chunk)
                    .map_err(|e| format!("brotli stream write: {e}")),
                super::ZlibStream::HandleCompress(_) | super::ZlibStream::HandleDecompress(_) => {
                    Err("zlib handle: use zlibHandleWriteSync, not zlibStreamWrite".into())
                }
            }
        })
        .await;
        match result {
            Ok(Ok(bytes)) => OpOutcome::Bytes(bytes),
            Ok(Err(msg)) => OpOutcome::Failed(msg),
            Err(e) => OpOutcome::Failed(format!("zlib stream write task: {e}")),
        }
    }

    /// zlibStreamFlush: finalize and remove the stream. Returns the tail
    /// bytes. For compressors, this emits the format trailer (CRC etc.).
    /// For decompressors, this finalizes the inflate/brotli state machine
    /// and returns any remaining output bytes.
    pub async fn zlib_stream_flush(streams: super::ZlibRegistry, handle: u64) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            let stream = streams
                .lock()
                .expect("zlib stream registry lock")
                .remove(&handle);
            let Some(stream) = stream else {
                return Err(format!("zlib stream: handle {handle} not found"));
            };
            match stream {
                super::ZlibStream::Compress(enc) => {
                    enc.finish().map_err(|e| format!("zlib stream flush: {e}"))
                }
                super::ZlibStream::Decompress(dec) => {
                    dec.finish().map_err(|e| format!("zlib stream flush: {e}"))
                }
                super::ZlibStream::BrotliCompress(enc) => enc
                    .finish()
                    .map_err(|e| format!("brotli stream flush: {e}")),
                super::ZlibStream::BrotliDecompress(dec) => dec
                    .finish()
                    .map_err(|e| format!("brotli stream flush: {e}")),
                super::ZlibStream::HandleCompress(_) | super::ZlibStream::HandleDecompress(_) => {
                    Err("zlib handle: use close(), not zlibStreamFlush".into())
                }
            }
        })
        .await;
        match result {
            Ok(Ok(bytes)) => OpOutcome::Bytes(bytes),
            Ok(Err(msg)) => OpOutcome::Failed(msg),
            Err(e) => OpOutcome::Failed(format!("zlib stream flush task: {e}")),
        }
    }

    /// zlibStreamClose: drop a stream without flushing. Synchronous --
    /// just removes the entry from the registry. Called by the JS side
    /// when the Transform stream is destroyed before completion.
    pub fn zlib_stream_close(streams: &super::ZlibRegistry, handle: u64) {
        streams
            .lock()
            .expect("zlib stream registry lock")
            .remove(&handle);
    }

    /// zlibHandleCreate: allocate a low-level flate2 Compress or Decompress
    /// handle for Node's zlib binding interface (used by ssh2 etc.).
    /// mode: 1=DEFLATE, 2=INFLATE, 5=DEFLATERAW, 6=INFLATERAW.
    pub fn zlib_handle_create(
        streams: &super::ZlibRegistry,
        ids: &std::sync::Arc<std::sync::atomic::AtomicU64>,
        mode: i32,
        level: i32,
    ) -> Result<u64, String> {
        let zlib_header = mode == 1 || mode == 2;
        let stream = match mode {
            1 | 5 => {
                let lvl = if (0..=9).contains(&level) {
                    flate2::Compression::new(level as u32)
                } else {
                    flate2::Compression::default()
                };
                super::ZlibStream::HandleCompress(flate2::Compress::new(lvl, zlib_header))
            }
            2 | 6 => super::ZlibStream::HandleDecompress(flate2::Decompress::new(zlib_header)),
            _ => return Err(format!("zlib handle: unknown mode {mode}")),
        };
        let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        streams
            .lock()
            .expect("zlib registry lock")
            .insert(handle, stream);
        Ok(handle)
    }

    /// zlibHandleWriteSync: synchronous incremental compress/decompress.
    /// Returns (availOutAfter, availInAfter). The caller provides a mutable
    /// output slice; compressed/decompressed bytes are written into it.
    pub fn zlib_handle_write_sync(
        streams: &super::ZlibRegistry,
        handle: u64,
        flush: i32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize), String> {
        let mut guard = streams.lock().unwrap_or_else(|e| e.into_inner());
        let stream = guard
            .get_mut(&handle)
            .ok_or_else(|| format!("zlib handle {handle} not found"))?;
        match stream {
            super::ZlibStream::HandleCompress(c) => {
                let before_in = c.total_in();
                let before_out = c.total_out();
                let fl = match flush {
                    0 => flate2::FlushCompress::None,
                    1 => flate2::FlushCompress::Partial,
                    2 => flate2::FlushCompress::Sync,
                    3 => flate2::FlushCompress::Full,
                    4 => flate2::FlushCompress::Finish,
                    _ => flate2::FlushCompress::None,
                };
                c.compress(input, output, fl)
                    .map_err(|e| format!("zlib handle compress: {e}"))?;
                let consumed = (c.total_in() - before_in) as usize;
                let produced = (c.total_out() - before_out) as usize;
                Ok((output.len() - produced, input.len() - consumed))
            }
            super::ZlibStream::HandleDecompress(d) => {
                let before_in = d.total_in();
                let before_out = d.total_out();
                let fl = match flush {
                    2 => flate2::FlushDecompress::Sync,
                    4 => flate2::FlushDecompress::Finish,
                    _ => flate2::FlushDecompress::None,
                };
                d.decompress(input, output, fl)
                    .map_err(|e| format!("zlib handle decompress: {e}"))?;
                let consumed = (d.total_in() - before_in) as usize;
                let produced = (d.total_out() - before_out) as usize;
                Ok((output.len() - produced, input.len() - consumed))
            }
            _ => Err(format!("zlib handle {handle} is not a handle variant")),
        }
    }

    /// Async zlib: CPU-bound, so spawn_blocking off the op channel
    /// (Node's threadpool model). compress=true encodes, false decodes;
    /// format "unzip" auto-detects on the decode side.
    pub async fn zlib_transform(
        bytes: Vec<u8>,
        format: String,
        level: i32,
        compress: bool,
    ) -> OpOutcome {
        let result = tokio::task::spawn_blocking(move || {
            if !compress && format == "unzip" {
                return super::zlib::unzip(&bytes);
            }
            let Some(parsed) = super::zlib::Format::parse(&format) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown zlib format '{format}'"),
                ));
            };
            if compress {
                super::zlib::compress(&bytes, parsed, level)
            } else {
                super::zlib::decompress(&bytes, parsed)
            }
        })
        .await;
        match result {
            Ok(Ok(out)) => OpOutcome::Bytes(out),
            Ok(Err(e)) => OpOutcome::Failed(format!("zlib: {e}")),
            Err(e) => OpOutcome::Failed(format!("zlib task: {e}")),
        }
    }

    /// Open a file for streaming. Mode: "r" read, "w" truncate-create,
    /// "a" append-create. Resolves with Json {handle}.
    pub async fn fs_open(
        files: super::FileRegistry,
        ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
        path: String,
        mode: String,
    ) -> OpOutcome {
        let mut options = tokio::fs::OpenOptions::new();
        match mode.as_str() {
            "r" => options.read(true),
            "w" => options.write(true).create(true).truncate(true),
            "a" => options.append(true).create(true),
            other => {
                return OpOutcome::Failed(format!("fs_open: unknown mode '{other}'"));
            }
        };
        match options.open(&path).await {
            Ok(file) => {
                let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                files
                    .lock()
                    .expect("file registry lock")
                    .files
                    .insert(handle, file);
                OpOutcome::Json(serde_json::json!({ "handle": handle }).to_string())
            }
            Err(e) => node_fail(e, "open", &path),
        }
    }

    /// Reinsert a File ONLY if it was not closed mid-flight. Returns
    /// whether it was kept (false = the handle was retired by fsClose
    /// during the IO await, so the File is dropped here, closing the fd).
    fn reinsert_file(files: &super::FileRegistry, handle: u64, file: tokio::fs::File) -> bool {
        let mut guard = files.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed.remove(&handle) {
            drop(file); // closed during the await: do not resurrect
            false
        } else {
            guard.files.insert(handle, file);
            true
        }
    }

    /// Read up to `len` bytes. Bytes = data, Done = EOF (handle stays open
    /// until fs_close — the JS side closes explicitly).
    pub async fn fs_read_chunk(files: super::FileRegistry, handle: u64, len: usize) -> OpOutcome {
        use tokio::io::AsyncReadExt;
        let file = files
            .lock()
            .expect("file registry lock")
            .files
            .remove(&handle);
        let Some(mut file) = file else {
            return OpOutcome::Failed(format!("fs stream: handle {handle} is gone"));
        };
        let mut buf = vec![0u8; len.clamp(1, 8 * 1024 * 1024)];
        match file.read(&mut buf).await {
            Ok(0) => {
                reinsert_file(&files, handle, file);
                OpOutcome::Done
            }
            Ok(n) => {
                reinsert_file(&files, handle, file);
                buf.truncate(n);
                OpOutcome::Bytes(buf)
            }
            Err(e) => node_fail(e, "read", &handle.to_string()),
        }
    }

    /// Append one chunk to an open handle (the node:stream write queue
    /// serializes callers).
    pub async fn fs_write_chunk(
        files: super::FileRegistry,
        handle: u64,
        bytes: Vec<u8>,
    ) -> OpOutcome {
        use tokio::io::AsyncWriteExt;
        let file = files
            .lock()
            .expect("file registry lock")
            .files
            .remove(&handle);
        let Some(mut file) = file else {
            return OpOutcome::Failed(format!("fs stream: handle {handle} is gone"));
        };
        match file.write_all(&bytes).await {
            Ok(()) => {
                reinsert_file(&files, handle, file);
                OpOutcome::Done
            }
            Err(e) => node_fail(e, "write", &handle.to_string()),
        }
    }

    /// Read one chunk from a streaming body. Bytes = a chunk, Done = EOF
    /// (handle dropped). Remove-await-reinsert keeps the lock short; the
    /// JS ReadableStream lock guarantees a single reader per handle.
    pub async fn fetch_body_read(bodies: super::BodyRegistry, handle: u64) -> OpOutcome {
        let response = bodies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&handle);
        let Some(mut response) = response else {
            return OpOutcome::Failed(format!("fetch: body handle {handle} is gone"));
        };
        match response.chunk().await {
            Ok(Some(bytes)) => {
                bodies
                    .lock()
                    .expect("body registry lock")
                    .insert(handle, response);
                OpOutcome::Bytes(bytes.to_vec())
            }
            Ok(None) => OpOutcome::Done,
            Err(e) => OpOutcome::Failed(format!("fetch: body read failed: {e}")),
        }
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
