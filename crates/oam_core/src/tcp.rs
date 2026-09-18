//! TCP client and server ops (node:net).
//!
//! Streams are split into independent read and write halves via
//! `TcpStream::into_split()`. Each half uses the remove-await-reinsert
//! pattern with its own map, so reads and writes proceed concurrently
//! without blocking each other. The closed set prevents handle
//! resurrection when a close races an in-flight read/write -- and holds
//! nothing else: a marker lives exactly as long as the await it guards.

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
    /// Stream handles closed while a half was checked out for an await
    /// (the last half back removes the marker), plus server ids closed
    /// mid-accept (`reinsert_listener` removes those). A stream id used to
    /// stay here for the life of the process once closed (#139).
    closed: HashSet<u64>,
    /// Halves currently out of the maps for an await, per stream handle.
    in_flight: HashMap<u64, u32>,
    cancel: HashMap<u64, std::sync::Arc<tokio::sync::Notify>>,
}

impl TcpState {
    pub fn register_stream(&mut self, handle: u64, reader: OwnedReadHalf, writer: OwnedWriteHalf) {
        self.readers.insert(handle, reader);
        self.writers.insert(handle, writer);
    }

    /// Both halves, for a TLS upgrade. Both present means nothing is in
    /// flight, so there is no await to guard and no marker to leave.
    pub fn take_halves(&mut self, handle: u64) -> Option<(OwnedReadHalf, OwnedWriteHalf)> {
        let reader = self.readers.remove(&handle)?;
        let writer = self.writers.remove(&handle)?;
        Some((reader, writer))
    }

    fn take_reader(&mut self, handle: u64) -> Option<OwnedReadHalf> {
        let reader = self.readers.remove(&handle)?;
        *self.in_flight.entry(handle).or_insert(0) += 1;
        Some(reader)
    }

    fn take_writer(&mut self, handle: u64) -> Option<OwnedWriteHalf> {
        let writer = self.writers.remove(&handle)?;
        *self.in_flight.entry(handle).or_insert(0) += 1;
        Some(writer)
    }

    /// One checked-out half is back (reinserted or dropped). The last one
    /// back after a `tcp_close` clears the closed marker.
    fn release(&mut self, handle: u64) {
        if let Some(n) = self.in_flight.get_mut(&handle) {
            *n -= 1;
            if *n == 0 {
                self.in_flight.remove(&handle);
                self.closed.remove(&handle);
            }
        }
    }

    /// (closed markers, cancel notifies, handles with a half in flight,
    /// readers, writers): all 0 once every stream handle is closed.
    #[cfg(test)]
    pub(crate) fn bookkeeping(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.closed.len(),
            self.cancel.len(),
            self.in_flight.len(),
            self.readers.len(),
            self.writers.len(),
        )
    }
}

pub type TcpRegistry = std::sync::Arc<std::sync::Mutex<TcpState>>;

/// A half checked out of the registry for an await. Dropping it releases
/// the handle's in-flight count on every exit path (reinserted, or dropped
/// after an error), so the closed marker cannot outlive the await.
struct InFlight {
    registry: TcpRegistry,
    handle: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release(self.handle);
    }
}

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

/// Format a SocketAddr as a Node-style structured address object. Shared
/// with the TLS ops, whose sockets carry the same `localAddr`/`remoteAddr`.
pub(crate) fn addr_to_json(addr: std::net::SocketAddr) -> serde_json::Value {
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
///
/// The connect itself is node's algorithm (crate::net_connect, shared with
/// tls_connect): `attempt_timeout` is `net.getDefaultAutoSelectFamilyAttemptTimeout()`
/// as JS read it for this call, and a failure rejects with node's own shape --
/// `connect ECONNREFUSED 127.0.0.1:8080` with errno, code, syscall, address and
/// port, a `getaddrinfo ENOTFOUND host` DNS error, or the NodeAggregateError of
/// a name whose every address failed.
pub async fn tcp_connect(
    registry: TcpRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    host: String,
    port: u16,
    attempt_timeout: std::time::Duration,
) -> OpOutcome {
    let opts = crate::net_connect::ConnectOptions {
        attempt_timeout,
        pin: None,
    };
    let stream = match crate::net_connect::connect(&host, port, &opts).await {
        Ok(connected) => connected.stream,
        Err(e) => return e.to_outcome(),
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
        .unwrap_or_else(|e| e.into_inner())
        .take_reader(handle);
    let Some(mut reader) = reader else {
        return OpOutcome::Failed(format!("tcp: read handle {handle} is gone"));
    };
    let _in_flight = InFlight {
        registry: registry.clone(),
        handle,
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
        .unwrap_or_else(|e| e.into_inner())
        .take_writer(handle);
    let Some(mut writer) = writer else {
        return OpOutcome::Failed(format!("tcp: write handle {handle} is gone"));
    };
    let _in_flight = InFlight {
        registry: registry.clone(),
        handle,
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
        .unwrap_or_else(|e| e.into_inner())
        .take_writer(handle);
    let Some(mut writer) = writer else {
        return OpOutcome::Done;
    };
    let _in_flight = InFlight {
        registry: registry.clone(),
        handle,
    };
    let _ = writer.shutdown().await;
    drop(writer);
    OpOutcome::Done
}

/// Close a TCP stream. Remove both halves and, if one is out for an await,
/// mark the handle closed so that await does not resurrect it -- the mark
/// is cleared when the await returns (#139).
pub fn tcp_close(registry: &TcpRegistry, handle: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    guard.readers.remove(&handle);
    guard.writers.remove(&handle);
    if guard.in_flight.get(&handle).is_some_and(|n| *n > 0) {
        guard.closed.insert(handle);
    }
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
/// Returns Json {handle, remoteAddr, localAddr?} or Done if listener was
/// closed -- the accepted socket's own address too, so `socket.address()`,
/// `localAddress` and `localPort` on a server-side net.Socket answer as
/// node's do.
pub async fn tcp_accept(
    registry: TcpRegistry,
    server_id: u64,
    stream_ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> OpOutcome {
    let (listener, notify) = {
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        // Closed, or another accept already holds the listener. Checked
        // BEFORE the cancel Notify is created: an accept that lands after
        // `tcp_server_close` removed the server's Notify would otherwise
        // insert a fresh one that nothing ever removes (#139).
        let Some(listener) = guard.listeners.remove(&server_id) else {
            return OpOutcome::Done;
        };
        let notify = guard
            .cancel
            .entry(server_id)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Notify::new()))
            .clone();
        (listener, notify)
    };

    tokio::select! {
        result = listener.accept() => {
            match result {
                Ok((stream, peer_addr)) => {
                    reinsert_listener(&registry, server_id, listener);

                    let local_addr = stream.local_addr().ok();
                    let handle = stream_ids.fetch_add(1, Ordering::Relaxed);
                    let (reader, writer) = stream.into_split();
                    {
                        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
                        guard.readers.insert(handle, reader);
                        guard.writers.insert(handle, writer);
                    }

                    let mut payload = serde_json::json!({
                        "handle": handle,
                        "remoteAddr": addr_to_json(peer_addr),
                    });
                    if let Some(la) = local_addr {
                        payload["localAddr"] = addr_to_json(la);
                    }
                    OpOutcome::Json(payload.to_string())
                }
                Err(e) => {
                    reinsert_listener(&registry, server_id, listener);
                    tcp_fail(e, "accept", &server_id.to_string())
                }
            }
        }
        _ = notify.notified() => {
            // The close that woke this accept set the marker for the two
            // accept-completed branches, which reinsert the listener; this
            // one drops it itself, so the marker has nothing left to guard.
            drop(listener);
            registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed
                .remove(&server_id);
            OpOutcome::Done
        }
    }
}

/// Close a TCP server. Remove the listener; if an accept is parked on it, mark
/// it closed so the accept does not resurrect it, and wake the accept. The
/// marker lives only while an accept is in flight: `reinsert_listener`
/// removes it when that accept observes it, and a server closed with no
/// accept parked leaves nothing behind (#139).
pub fn tcp_server_close(registry: &TcpRegistry, server_id: u64) {
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    let had_listener = guard.listeners.remove(&server_id).is_some();
    if let Some(notify) = guard.cancel.remove(&server_id) {
        // An accept holds the listener while it is parked, so "no listener
        // in the map but a cancel Notify registered" is exactly "an accept is
        // in flight": the marker is what stops it re-inserting the listener.
        if !had_listener {
            guard.closed.insert(server_id);
        }
        notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// Accepts N connections; each echoes its first read, then waits for the
    /// client's FIN (so a second client read can park) before closing.
    async fn echo_server(listener: tokio::net::TcpListener, connections: usize) {
        for _ in 0..connections {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf).await {
                    let _ = stream.write_all(&buf[..n]).await;
                }
                let _ = stream.read(&mut buf).await;
            });
        }
    }

    /// #139's tcp.rs twin: every closed stream leaves the registry empty,
    /// whether the close came after EOF, while a read was parked, or before
    /// a late read.
    #[tokio::test]
    async fn closed_streams_leave_no_bookkeeping_behind() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        const N: usize = 9;
        tokio::spawn(echo_server(listener, N));

        for i in 0..N {
            let OpOutcome::Json(payload) = tcp_connect(
                registry.clone(),
                ids.clone(),
                "127.0.0.1".into(),
                port,
                crate::net_connect::DEFAULT_ATTEMPT_TIMEOUT,
            )
            .await
            else {
                panic!("connect failed");
            };
            let info: serde_json::Value = serde_json::from_str(&payload).unwrap();
            let handle = info["handle"].as_u64().unwrap();

            assert!(matches!(
                tcp_write(registry.clone(), handle, b"ping".to_vec()).await,
                OpOutcome::Done
            ));
            assert!(matches!(
                tcp_read(registry.clone(), handle, 64).await,
                OpOutcome::Bytes(b) if b == b"ping"
            ));
            match i % 3 {
                0 => {
                    // Our FIN, the server's close, EOF, then close.
                    assert!(matches!(
                        tcp_shutdown(registry.clone(), handle).await,
                        OpOutcome::Done
                    ));
                    assert!(matches!(
                        tcp_read(registry.clone(), handle, 64).await,
                        OpOutcome::Done
                    ));
                    tcp_close(&registry, handle);
                }
                1 => {
                    // Close while a read is parked: the marker's one
                    // legitimate use. Dropping the write half sends FIN, the
                    // server closes, the parked read sees EOF and must NOT
                    // reinsert its half.
                    let parked = tokio::spawn(tcp_read(registry.clone(), handle, 64));
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    tcp_close(&registry, handle);
                    assert!(matches!(parked.await.unwrap(), OpOutcome::Done));
                }
                _ => {
                    // Close first; a read that lands afterwards fails.
                    tcp_close(&registry, handle);
                    assert!(matches!(
                        tcp_read(registry.clone(), handle, 64).await,
                        OpOutcome::Failed(_)
                    ));
                }
            }
        }

        let bookkeeping = registry.lock().unwrap().bookkeeping();
        assert_eq!(
            bookkeeping,
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// The server-side twin of the test above (#139): a listener closed with
    /// no accept parked, closed while one is parked, or closed after accepts
    /// completed leaves nothing behind, and an accept issued after the close
    /// creates no cancel Notify. Also pins the accepted socket's own address
    /// in the accept payload.
    #[tokio::test]
    async fn closed_servers_leave_no_bookkeeping_behind() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));

        for shape in 0..3 {
            let OpOutcome::Json(payload) =
                tcp_listen(registry.clone(), ids.clone(), "127.0.0.1".into(), 0).await
            else {
                panic!("listen failed");
            };
            let info: serde_json::Value = serde_json::from_str(&payload).unwrap();
            let server_id = info["serverId"].as_u64().unwrap();
            let port = u16::try_from(info["port"].as_u64().unwrap()).unwrap();
            match shape {
                0 => {
                    // Closed with nothing parked: no marker may be left.
                    tcp_server_close(&registry, server_id);
                }
                1 => {
                    // Closed while an accept is parked: the marker's one use.
                    let parked = tokio::spawn(tcp_accept(registry.clone(), server_id, ids.clone()));
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    tcp_server_close(&registry, server_id);
                    assert!(matches!(parked.await.unwrap(), OpOutcome::Done));
                }
                _ => {
                    // An accept completes (its Notify stays registered), the
                    // stream is closed, then the server.
                    let client = tokio::net::TcpStream::connect(("127.0.0.1", port))
                        .await
                        .unwrap();
                    let OpOutcome::Json(accepted) =
                        tcp_accept(registry.clone(), server_id, ids.clone()).await
                    else {
                        panic!("accept failed");
                    };
                    let accepted: serde_json::Value = serde_json::from_str(&accepted).unwrap();
                    assert_eq!(
                        accepted["localAddr"]["port"].as_u64(),
                        Some(u64::from(port)),
                        "the accept payload carries the accepted socket's own address"
                    );
                    tcp_close(&registry, accepted["handle"].as_u64().unwrap());
                    drop(client);
                    tcp_server_close(&registry, server_id);
                }
            }
            // An accept after the close must neither resurrect the server
            // nor register a cancel Notify for it.
            assert!(matches!(
                tcp_accept(registry.clone(), server_id, ids.clone()).await,
                OpOutcome::Done
            ));
        }

        let bookkeeping = registry.lock().unwrap().bookkeeping();
        assert_eq!(
            bookkeeping,
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }

    /// #139, the duplex case the single-half tests miss: a handle closed
    /// while BOTH a read and a write are in flight (in_flight == 2) keeps
    /// its closed marker until the SECOND half returns, not the first. This
    /// is a net.Socket writing a request body while reading the response,
    /// destroyed mid-flight: the read is woken by the close and returns
    /// first, the write drains later. If the marker cleared on the first
    /// half back, the second would see no marker, reinsert its half after
    /// close, and resurrect the handle -- the surviving half (and the TCP
    /// connection, and the event loop) leaking at exit, the exact #139 hang.
    /// Driven through the real take/close/reinsert/release primitives the
    /// ops use (each op holds a half out of the maps behind an `InFlight`
    /// guard); the awaits between are irrelevant to the bookkeeping.
    #[tokio::test]
    async fn a_close_with_both_halves_in_flight_clears_only_on_the_last() {
        let registry: TcpRegistry = std::sync::Arc::new(std::sync::Mutex::new(TcpState::default()));
        let ids = std::sync::Arc::new(AtomicU64::new(1));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(echo_server(listener, 1));

        let OpOutcome::Json(payload) = tcp_connect(
            registry.clone(),
            ids.clone(),
            "127.0.0.1".into(),
            port,
            crate::net_connect::DEFAULT_ATTEMPT_TIMEOUT,
        )
        .await
        else {
            panic!("connect failed");
        };
        let info: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let handle = info["handle"].as_u64().unwrap();

        // A read op and a write op each check out their half (in_flight ==
        // 2), each behind an `InFlight` guard, exactly as tcp_read/tcp_write.
        let reader = registry.lock().unwrap().take_reader(handle).unwrap();
        let g_read = InFlight {
            registry: registry.clone(),
            handle,
        };
        let writer = registry.lock().unwrap().take_writer(handle).unwrap();
        let g_write = InFlight {
            registry: registry.clone(),
            handle,
        };
        assert_eq!(
            registry.lock().unwrap().in_flight.get(&handle).copied(),
            Some(2),
            "both halves in flight for this handle"
        );

        // Close: the marker is set because a half is still out.
        tcp_close(&registry, handle);
        assert_eq!(
            registry.lock().unwrap().bookkeeping().0,
            1,
            "marker set while a half is in flight"
        );

        // The read op returns first (its select woken by the close): the
        // reinsert drops the half because the handle is closed, and the
        // guard's drop releases -> in_flight 2->1, marker MUST stay.
        assert!(
            !reinsert_reader(&registry, handle, reader),
            "read half dropped, not reinserted"
        );
        drop(g_read);
        let bk = registry.lock().unwrap().bookkeeping();
        assert_eq!(bk.0, 1, "marker must survive the first half back");
        assert_eq!(bk.2, 1, "one half still in flight");

        // The write op returns: the last half back clears the marker.
        assert!(
            !reinsert_writer(&registry, handle, writer),
            "write half dropped, not reinserted"
        );
        drop(g_write);
        assert_eq!(
            registry.lock().unwrap().bookkeeping(),
            (0, 0, 0, 0, 0),
            "(closed, cancel, in_flight, readers, writers)"
        );
    }
}
