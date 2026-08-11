//! TCP client and server ops (node:net).
//!
//! Streams are split into independent read and write halves via
//! `TcpStream::into_split()`. Each half uses the remove-await-reinsert
//! pattern with its own map, so reads and writes proceed concurrently
//! without blocking each other. The closed set prevents handle
//! resurrection when a close races an in-flight read/write.

use crate::{OpOutcome, node_errno, node_error_code, node_error_message};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

#[derive(Default)]
pub struct TcpState {
    readers: HashMap<u64, OwnedReadHalf>,
    writers: HashMap<u64, OwnedWriteHalf>,
    listeners: HashMap<u64, tokio::net::TcpListener>,
    closed: HashSet<u64>,
    cancel: HashMap<u64, std::sync::Arc<tokio::sync::Notify>>,
}

impl TcpState {
    pub fn register_stream(&mut self, handle: u64, reader: OwnedReadHalf, writer: OwnedWriteHalf) {
        self.readers.insert(handle, reader);
        self.writers.insert(handle, writer);
    }

    pub fn take_halves(&mut self, handle: u64) -> Option<(OwnedReadHalf, OwnedWriteHalf)> {
        let reader = self.readers.remove(&handle)?;
        let writer = self.writers.remove(&handle)?;
        self.closed.insert(handle);
        Some((reader, writer))
    }
}

pub type TcpRegistry = std::sync::Arc<std::sync::Mutex<TcpState>>;

fn reinsert_reader(registry: &TcpRegistry, handle: u64, reader: OwnedReadHalf) -> bool {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.closed.contains(&handle) {
        drop(reader);
        false
    } else {
        guard.readers.insert(handle, reader);
        true
    }
}

fn reinsert_writer(registry: &TcpRegistry, handle: u64, writer: OwnedWriteHalf) -> bool {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.closed.contains(&handle) {
        drop(writer);
        false
    } else {
        guard.writers.insert(handle, writer);
        true
    }
}

/// Reinsert a TcpListener ONLY if it was not closed mid-flight.
fn reinsert_listener(
    registry: &TcpRegistry,
    server_id: u64,
    listener: tokio::net::TcpListener,
) -> bool {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    if guard.closed.remove(&server_id) {
        drop(listener);
        false
    } else {
        guard.listeners.insert(server_id, listener);
        true
    }
}

/// Format a SocketAddr as a Node-style structured address object.
fn addr_to_json(addr: std::net::SocketAddr) -> serde_json::Value {
    serde_json::json!({
        "address": addr.ip().to_string(),
        "port": addr.port(),
        "family": if addr.is_ipv6() { "IPv6" } else { "IPv4" },
    })
}

/// Map an IO error to a NodeFailed outcome with the appropriate syscall.
fn tcp_fail(error: std::io::Error, syscall: &str, target: &str) -> OpOutcome {
    let code = node_error_code(&error);
    // syscall + errno, but no `path`: a host:port is not a filesystem path,
    // and node does not put one on a net error.
    OpOutcome::node_failed_at(
        code,
        node_error_message(code, syscall, target, &error),
        syscall,
        None,
        node_errno(code, &error),
    )
}

/// net.connect / net.createConnection: establish a TCP client connection.
/// Returns Json {handle, localAddr, remoteAddr}.
pub async fn tcp_connect(
    registry: TcpRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    host: String,
    port: u16,
) -> OpOutcome {
    let addr = format!("{host}:{port}");
    let stream = match tokio::net::TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => return tcp_fail(e, "connect", &addr),
    };

    let local_addr = stream.local_addr().ok();
    let remote_addr = stream.peer_addr().ok();
    let handle = ids.fetch_add(1, Ordering::Relaxed);
    let (reader, writer) = stream.into_split();
    {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        guard.readers.insert(handle, reader);
        guard.writers.insert(handle, writer);
    }

    let mut payload = serde_json::json!({ "handle": handle });
    if let Some(la) = local_addr {
        payload["localAddr"] = addr_to_json(la);
    }
    if let Some(ra) = remote_addr {
        payload["remoteAddr"] = addr_to_json(ra);
    }
    OpOutcome::Json(payload.to_string())
}

/// Read up to `len` bytes from a TCP stream. Remove-await-reinsert on the
/// read half only -- writes proceed independently.
pub async fn tcp_read(registry: TcpRegistry, handle: u64, len: usize) -> OpOutcome {
    let reader = registry
        .lock()
        .expect("tcp registry lock")
        .readers
        .remove(&handle);
    let Some(mut reader) = reader else {
        return OpOutcome::Failed(format!("tcp: read handle {handle} is gone"));
    };

    let mut buf = vec![0u8; len.clamp(1, 8 * 1024 * 1024)];
    match reader.read(&mut buf).await {
        Ok(0) => {
            reinsert_reader(&registry, handle, reader);
            OpOutcome::Done
        }
        Ok(n) => {
            reinsert_reader(&registry, handle, reader);
            buf.truncate(n);
            OpOutcome::Bytes(buf)
        }
        Err(e) => tcp_fail(e, "read", &handle.to_string()),
    }
}

/// Write bytes to a TCP stream. Remove-await-reinsert on the write half
/// only -- reads proceed independently.
pub async fn tcp_write(registry: TcpRegistry, handle: u64, data: Vec<u8>) -> OpOutcome {
    let writer = registry
        .lock()
        .expect("tcp registry lock")
        .writers
        .remove(&handle);
    let Some(mut writer) = writer else {
        return OpOutcome::Failed(format!("tcp: write handle {handle} is gone"));
    };

    match writer.write_all(&data).await {
        Ok(()) => {
            reinsert_writer(&registry, handle, writer);
            OpOutcome::Done
        }
        Err(e) => tcp_fail(e, "write", &handle.to_string()),
    }
}

/// Half-close the write side (sends FIN). Removes the write half and
/// drops it after shutdown -- no further writes are possible.
pub async fn tcp_shutdown(registry: TcpRegistry, handle: u64) -> OpOutcome {
    let writer = registry
        .lock()
        .expect("tcp registry lock")
        .writers
        .remove(&handle);
    let Some(mut writer) = writer else {
        return OpOutcome::Done;
    };
    let _ = writer.shutdown().await;
    drop(writer);
    OpOutcome::Done
}

/// Close a TCP stream. Remove both halves and mark the handle closed so
/// any in-flight read/write does not resurrect it.
pub fn tcp_close(registry: &TcpRegistry, handle: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard.readers.remove(&handle);
    guard.writers.remove(&handle);
    guard.closed.insert(handle);
}

/// net.createServer + server.listen: bind a TCP listener.
/// Returns Json {serverId, port, hostname}.
pub async fn tcp_listen(
    registry: TcpRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    host: String,
    port: u16,
) -> OpOutcome {
    let addr = format!("{host}:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => return tcp_fail(e, "listen", &addr),
    };

    let local_addr = listener.local_addr().ok();
    let bound_port = local_addr.map(|a| a.port()).unwrap_or(port);
    let hostname = local_addr.map(|a| a.ip().to_string()).unwrap_or(host);

    let server_id = ids.fetch_add(1, Ordering::Relaxed);
    registry
        .lock()
        .expect("tcp registry lock")
        .listeners
        .insert(server_id, listener);

    OpOutcome::Json(
        serde_json::json!({
            "serverId": server_id,
            "port": bound_port,
            "hostname": hostname,
        })
        .to_string(),
    )
}

/// Accept one connection from a TCP listener. Remove-await-reinsert on the
/// LISTENER. Splits and stores the accepted stream's read/write halves.
/// Returns Json {handle, remoteAddr} or Done if listener was closed.
pub async fn tcp_accept(
    registry: TcpRegistry,
    server_id: u64,
    stream_ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> OpOutcome {
    let (listener, notify) = {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        let listener = guard.listeners.remove(&server_id);
        let notify = guard
            .cancel
            .entry(server_id)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Notify::new()))
            .clone();
        (listener, notify)
    };
    let Some(listener) = listener else {
        return OpOutcome::Done;
    };

    tokio::select! {
        result = listener.accept() => {
            match result {
                Ok((stream, peer_addr)) => {
                    reinsert_listener(&registry, server_id, listener);

                    let handle = stream_ids.fetch_add(1, Ordering::Relaxed);
                    let (reader, writer) = stream.into_split();
                    {
                        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                        guard.readers.insert(handle, reader);
                        guard.writers.insert(handle, writer);
                    }

                    OpOutcome::Json(
                        serde_json::json!({
                            "handle": handle,
                            "remoteAddr": addr_to_json(peer_addr),
                        })
                        .to_string(),
                    )
                }
                Err(e) => {
                    reinsert_listener(&registry, server_id, listener);
                    tcp_fail(e, "accept", &server_id.to_string())
                }
            }
        }
        _ = notify.notified() => {
            drop(listener);
            OpOutcome::Done
        }
    }
}

/// Close a TCP server. Remove the listener AND add to closed set so any
/// in-flight accept does not resurrect it. Notifies any blocked accept.
pub fn tcp_server_close(registry: &TcpRegistry, server_id: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard.listeners.remove(&server_id);
    guard.closed.insert(server_id);
    if let Some(notify) = guard.cancel.remove(&server_id) {
        notify.notify_one();
    }
}
