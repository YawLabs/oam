//! UDP datagram ops (node:dgram).
//!
//! Sockets are stored as `Arc<UdpSocket>` so send and recv can run
//! concurrently without removing from the map. UDP is connectionless
//! and tokio's send_to/recv_from take `&self`.

use crate::{OpOutcome, node_error_code, node_error_message};
use std::collections::HashMap;
use std::sync::{Arc, atomic::Ordering};

#[derive(Default)]
pub struct UdpState {
    sockets: HashMap<u64, Arc<tokio::net::UdpSocket>>,
    /// One close signal per socket with a receive in flight. A `watch`, not a
    /// `Notify`: it is level-triggered and reaches every receiver, so a
    /// close ends each receive on the socket, including one that has not
    /// been polled yet. An entry exists only while its socket does.
    cancel: HashMap<u64, tokio::sync::watch::Sender<bool>>,
}

pub type UdpRegistry = Arc<std::sync::Mutex<UdpState>>;

fn udp_fail(error: std::io::Error, syscall: &str, target: &str) -> OpOutcome {
    let code = node_error_code(&error);
    // syscall + errno, but no `path`: a host:port is not a filesystem path,
    // and node does not put one on a net error.
    OpOutcome::node_failed_io(
        code,
        node_error_message(code, syscall, target, &error),
        syscall,
        None,
        &error,
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

/// Whether a receive error is one libuv's UDP receive ignores. On Windows a
/// datagram this socket sent to a port nobody listens on comes back (ICMP
/// port unreachable) as WSAECONNRESET -- WSAENETRESET for an expired TTL --
/// on the NEXT receive, unless SIO_UDP_CONNRESET is turned off. libuv skips
/// both and goes on reading, so node's socket keeps receiving; so does this
/// one. Nothing is ignored elsewhere.
fn ignored_recv_error(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock::{WSAECONNRESET, WSAENETRESET};
        matches!(error.raw_os_error(), Some(code) if code == WSAECONNRESET || code == WSAENETRESET)
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

/// socket.on('message'): receive one datagram.
/// Returns Json {data (base64), rinfo: {address, port, family, size}}
/// or Done if the socket was closed.
pub async fn udp_recv(registry: UdpRegistry, handle: u64, len: usize) -> OpOutcome {
    let (socket, mut closed) = {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        // The socket first: a handle that is closed or unknown gets no close
        // signal of its own, which nothing would ever remove.
        let Some(socket) = guard.sockets.get(&handle).cloned() else {
            return OpOutcome::Done;
        };
        // Subscribed under the lock udp_close sends under, so the close is
        // always seen as a change.
        let closed = guard
            .cancel
            .entry(handle)
            .or_insert_with(|| tokio::sync::watch::channel(false).0)
            .subscribe();
        (socket, closed)
    };

    let mut buf = vec![0u8; len.clamp(1, 65536)];

    let received = async {
        loop {
            match socket.recv_from(&mut buf).await {
                Err(e) if ignored_recv_error(&e) => continue,
                result => break result,
            }
        }
    };

    tokio::select! {
        result = received => {
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
                // node's onMessage: `new ErrnoException(nread, 'recvmsg')`,
                // whose message is the syscall and the code alone -- no
                // target (this side's handle id meant nothing to a script).
                Err(e) => {
                    let code = node_error_code(&e);
                    OpOutcome::node_failed_io(code, format!("recvmsg {code}"), "recvmsg", None, &e)
                }
            }
        }
        // Resolves on the close (a change), and on the sender going away.
        _ = closed.changed() => {
            OpOutcome::Done
        }
    }
}

/// socket.close(): close the UDP socket. Ends every receive blocked on it,
/// so none keeps the socket (and its port) alive.
pub fn udp_close(registry: &UdpRegistry, handle: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard.sockets.remove(&handle);
    if let Some(closed) = guard.cancel.remove(&handle) {
        let _ = closed.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closing a socket ends EVERY receive in flight on it, and a receive on
    /// a handle that is gone leaves nothing behind in the registry.
    #[tokio::test]
    async fn close_ends_every_receive_and_a_gone_handle_leaves_no_entry() {
        let registry: UdpRegistry = Default::default();
        let ids = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let bound = udp_bind(registry.clone(), ids, "127.0.0.1".into(), 0).await;
        let OpOutcome::Json(json) = bound else {
            panic!("bind must succeed");
        };
        let handle = serde_json::from_str::<serde_json::Value>(&json).unwrap()["handle"]
            .as_u64()
            .unwrap();

        let first = tokio::spawn(udp_recv(registry.clone(), handle, 16));
        let second = tokio::spawn(udp_recv(registry.clone(), handle, 16));
        // Both receives are parked once each holds the socket.
        loop {
            let parked = registry
                .lock()
                .unwrap()
                .sockets
                .get(&handle)
                .map(Arc::strong_count);
            if parked == Some(3) {
                break;
            }
            tokio::task::yield_now().await;
        }
        udp_close(&registry, handle);
        for recv in [first, second] {
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), recv)
                .await
                .expect("close must end the receive")
                .unwrap();
            assert!(matches!(outcome, OpOutcome::Done));
        }

        assert!(matches!(
            udp_recv(registry.clone(), handle, 16).await,
            OpOutcome::Done
        ));
        let guard = registry.lock().unwrap();
        assert!(guard.sockets.is_empty());
        assert!(guard.cancel.is_empty());
    }

    fn bound_handle(outcome: OpOutcome) -> (u64, u16) {
        let OpOutcome::Json(json) = outcome else {
            panic!("bind must succeed");
        };
        let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();
        (
            value["handle"].as_u64().unwrap(),
            value["port"].as_u64().unwrap() as u16,
        )
    }

    /// A datagram sent to a closed port does not end the sender's receives:
    /// the reset Windows reports for it on the next receive is skipped, as
    /// libuv skips it, and the next datagram still arrives.
    #[tokio::test]
    async fn a_send_to_a_closed_port_does_not_end_receiving() {
        let registry: UdpRegistry = Default::default();
        let ids = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let bind = || udp_bind(registry.clone(), ids.clone(), "127.0.0.1".into(), 0);
        let (receiver, receiver_port) = bound_handle(bind().await);
        let (closed, closed_port) = bound_handle(bind().await);
        udp_close(&registry, closed);
        let sent = udp_send(
            registry.clone(),
            receiver,
            b"x".to_vec(),
            "127.0.0.1".into(),
            closed_port,
        )
        .await;
        assert!(matches!(sent, OpOutcome::Json(_)));
        // Time for the port-unreachable answer to come back.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (sender, _) = bound_handle(bind().await);
        let sent = udp_send(
            registry.clone(),
            sender,
            b"hello".to_vec(),
            "127.0.0.1".into(),
            receiver_port,
        )
        .await;
        assert!(matches!(sent, OpOutcome::Json(_)));
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            udp_recv(registry.clone(), receiver, 16),
        )
        .await
        .expect("the datagram arrives");
        let OpOutcome::Json(json) = outcome else {
            panic!("the receive failed instead of delivering the datagram");
        };
        let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();
        assert_eq!(value["rinfo"]["size"], 5);
    }
}
