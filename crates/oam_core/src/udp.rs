//! UDP datagram ops (node:dgram).
//!
//! Sockets are stored as `Arc<UdpSocket>` so send and recv can run
//! concurrently without removing from the map. UDP is connectionless
//! and tokio's send_to/recv_from take `&self`.

use crate::{OpOutcome, node_errno, node_error_code, node_error_message};
use std::collections::HashMap;
use std::sync::{Arc, atomic::Ordering};

#[derive(Default)]
pub struct UdpState {
    sockets: HashMap<u64, Arc<tokio::net::UdpSocket>>,
    cancel: HashMap<u64, Arc<tokio::sync::Notify>>,
}

pub type UdpRegistry = Arc<std::sync::Mutex<UdpState>>;

fn udp_fail(error: std::io::Error, syscall: &str, target: &str) -> OpOutcome {
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

/// dgram.createSocket('udp4') + socket.bind(port, address):
/// Bind a UDP socket and return {handle, address, port, family}.
pub async fn udp_bind(
    registry: UdpRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    host: String,
    port: u16,
) -> OpOutcome {
    let addr = format!("{host}:{port}");
    let socket = match tokio::net::UdpSocket::bind(&addr).await {
        Ok(s) => s,
        Err(e) => return udp_fail(e, "bind", &addr),
    };

    let local_addr = match socket.local_addr() {
        Ok(a) => a,
        Err(e) => return udp_fail(e, "getsockname", &addr),
    };

    let handle = ids.fetch_add(1, Ordering::Relaxed);
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sockets
        .insert(handle, Arc::new(socket));

    OpOutcome::Json(
        serde_json::json!({
            "handle": handle,
            "address": local_addr.ip().to_string(),
            "port": local_addr.port(),
            "family": if local_addr.is_ipv6() { "IPv6" } else { "IPv4" },
        })
        .to_string(),
    )
}

/// socket.send(msg, offset, length, port, address): send a datagram.
/// Returns Json {bytesSent}.
pub async fn udp_send(
    registry: UdpRegistry,
    handle: u64,
    data: Vec<u8>,
    host: String,
    port: u16,
) -> OpOutcome {
    let socket = registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sockets
        .get(&handle)
        .cloned();
    let Some(socket) = socket else {
        return OpOutcome::Failed(format!("udp: send handle {handle} is gone"));
    };

    let target = format!("{host}:{port}");
    match socket.send_to(&data, &target).await {
        Ok(n) => OpOutcome::Json(serde_json::json!({ "bytesSent": n }).to_string()),
        Err(e) => udp_fail(e, "send", &target),
    }
}

/// socket.on('message'): receive one datagram.
/// Returns Json {data (base64), rinfo: {address, port, family, size}}
/// or Done if the socket was closed.
pub async fn udp_recv(registry: UdpRegistry, handle: u64, len: usize) -> OpOutcome {
    let (socket, notify) = {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        let socket = guard.sockets.get(&handle).cloned();
        let notify = guard
            .cancel
            .entry(handle)
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone();
        (socket, notify)
    };
    let Some(socket) = socket else {
        return OpOutcome::Done;
    };

    let mut buf = vec![0u8; len.clamp(1, 65536)];

    tokio::select! {
        result = socket.recv_from(&mut buf) => {
            match result {
                Ok((n, peer)) => {
                    buf.truncate(n);
                    use base64::Engine;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
                    OpOutcome::Json(
                        serde_json::json!({
                            "data": b64,
                            "rinfo": {
                                "address": peer.ip().to_string(),
                                "port": peer.port(),
                                "family": if peer.is_ipv6() { "IPv6" } else { "IPv4" },
                                "size": n,
                            }
                        })
                        .to_string(),
                    )
                }
                Err(e) => udp_fail(e, "recvmsg", &handle.to_string()),
            }
        }
        _ = notify.notified() => {
            OpOutcome::Done
        }
    }
}

/// socket.close(): close the UDP socket. Notifies any blocked recv.
pub fn udp_close(registry: &UdpRegistry, handle: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard.sockets.remove(&handle);
    if let Some(notify) = guard.cancel.remove(&handle) {
        notify.notify_one();
    }
}
