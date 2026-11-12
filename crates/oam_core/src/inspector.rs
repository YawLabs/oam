//! v8-free CDP transport: a localhost WebSocket + discovery HTTP server on a
//! dedicated thread, bridged to the engine thread by channels.
//!
//! The engine (oam_engine) owns the actual V8 inspector and drives the
//! protocol; this module only moves bytes. Keeping it here preserves
//! oam_core's v8-free boundary and reuses the in-tree tokio runtime stack.
//!
//! Model: ONE active debugger session at a time, running as a task beside
//! the accept loop. A second concurrent client while one is already connected
//! is rejected with 503, and discovery stays answered throughout. After the
//! connected client disconnects, the accept loop refreshes its channel state
//! and waits for the next client -- reconnect is supported for the lifetime
//! of the process. The Chrome DevTools Protocol is JSON over a single
//! WebSocket; the discovery endpoints (`/json`, `/json/version`) let
//! `chrome://inspect` and VS Code find the target.
//!
//! Who is answered, as node's inspector decides it: a request whose `Host`
//! header is an IP address or `localhost` (any other name is refused with
//! 400, discovery included), and a WebSocket upgrade only at the target's
//! own path, `/<uuid>` -- the id discovery and the startup banner hand out.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;

use futures_util::{SinkExt, StreamExt};

/// Opaque zero-cost wrapper around `tokio::sync::mpsc::UnboundedSender<String>`.
///
/// Defined here so that crates above `oam_core` (e.g. `oam_engine`) can name
/// the reconnect sender type without taking a direct `tokio` dependency.
/// `#[repr(transparent)]` makes this a zero-cost abstraction: the compiler
/// treats it identically to the inner type in every layout and ABI sense.
#[repr(transparent)]
pub struct OutboundSender(UnboundedSender<String>);

impl OutboundSender {
    /// Forward a CDP message to the attached debugger client.
    /// Silently discards if the channel is closed (receiver gone).
    #[inline]
    pub fn send(&self, message: String) {
        let _ = self.0.send(message);
    }
}

/// How long a peer may take to send its full HTTP head before we drop the
/// connection. Bounds head-of-line blocking on the single accept loop and
/// guarantees a stalled client can never wedge process exit.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// The magic GUID every WebSocket server appends to the client key before
/// hashing (RFC 6455).
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// A lifecycle/data event from the debugger client to the engine.
pub enum FromClient {
    /// The single client finished the WebSocket handshake.
    Connected,
    /// A CDP message (JSON text) arrived from the client.
    Message(String),
    /// The client went away (close frame, socket error, or EOF).
    Disconnected,
    /// A new client connected after the previous one disconnected (reconnect).
    /// Carries the fresh `OutboundSender` the engine must use for outbound
    /// CDP messages going forward; the old sender is now dead.
    Reconnected(OutboundSender),
}

/// Handle to a running inspector transport. The engine holds this for the
/// life of the run; dropping it shuts the server thread down.
///
/// The handle is created on one thread and then held exclusively by the
/// engine (isolate) thread, so the interior-mutable sender uses `RefCell`
/// rather than a lock. `InspectorHandle` is therefore `!Sync` (which is
/// already the case via `Receiver<FromClient>`).
pub struct InspectorHandle {
    /// The bound address (useful when the caller asked for port 0).
    pub addr: SocketAddr,
    /// The target id embedded in the WebSocket debugger URL.
    pub uuid: String,
    /// CDP messages + lifecycle from the client. The engine drains this
    /// (blocking while paused, non-blocking while running).
    pub from_client: Receiver<FromClient>,
    /// CDP messages to the current client. On reconnect the engine replaces
    /// this sender via `swap_sender`; the old sender is dead (recv side went
    /// away with the previous session's channel).
    to_client: RefCell<OutboundSender>,
    /// Level-triggered shutdown. A `watch` (not a `Notify`) so the signal
    /// cannot be lost: the server task sees `changed()` resolve even if it
    /// registers AFTER `send(true)`, which a lost-wakeup `notify_waiters`
    /// would miss and hang the join below forever.
    shutdown: watch::Sender<bool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InspectorHandle {
    /// The `ws://host:port/uuid` URL a debugger connects to.
    pub fn ws_url(&self) -> String {
        format!("ws://{}/{}", self.addr, self.uuid)
    }

    /// Forward one outbound CDP message to the attached client. Silently
    /// drops if no client is attached (the protocol tolerates this: V8 only
    /// emits while a session is connected).
    pub fn send_to_client(&self, message: String) {
        self.to_client.borrow().send(message);
    }

    /// Replace the outbound sender with a new one after a reconnect. The
    /// previous sender is dropped here (its receive side went away when the
    /// prior session's channel was discarded by the transport).
    pub fn swap_sender(&self, new_tx: OutboundSender) {
        *self.to_client.borrow_mut() = new_tx;
    }
}

impl Drop for InspectorHandle {
    fn drop(&mut self) {
        // Level-triggered: even if every server task is mid-read/mid-write
        // (off any `changed()` await) right now, the stored `true` is seen
        // the instant it next awaits, and every long await is wrapped in a
        // shutdown-select, so the task returns and the join cannot hang.
        let _ = self.shutdown.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start the inspector transport. Binds `addr` (use port 0 for an
/// OS-assigned port), spins a dedicated thread with a current-thread tokio
/// runtime, and returns once the listener is bound (so `addr`/`uuid` are
/// final and a debugger can attach immediately).
pub fn start(addr: SocketAddr, uuid: String) -> std::io::Result<InspectorHandle> {
    let (from_client_tx, from_client_rx) = std::sync::mpsc::channel::<FromClient>();
    let (to_client_tx, to_client_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Bind synchronously on this thread so we can report the real addr back
    // before the server thread starts accepting.
    let std_listener = std::net::TcpListener::bind(addr)?;
    std_listener.set_nonblocking(true)?;
    let bound_addr = std_listener.local_addr()?;

    let thread_uuid = uuid.clone();
    let thread = std::thread::Builder::new()
        .name("oam-inspector".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            runtime.block_on(async move {
                let listener = match TcpListener::from_std(std_listener) {
                    Ok(l) => l,
                    Err(_) => return,
                };
                serve(
                    listener,
                    bound_addr,
                    thread_uuid,
                    from_client_tx,
                    to_client_rx,
                    shutdown_rx,
                )
                .await;
            });
        })?;

    Ok(InspectorHandle {
        addr: bound_addr,
        uuid,
        from_client: from_client_rx,
        to_client: RefCell::new(OutboundSender(to_client_tx)),
        shutdown: shutdown_tx,
        thread: Some(thread),
    })
}

/// Accept loop: serve discovery GETs, hand the first WebSocket upgrade for
/// this target to a bridge task. The loop keeps accepting while that session
/// runs, so discovery stays answered and a second concurrent client is
/// rejected with 503. After the client disconnects, a fresh channel pair is
/// created and the engine is notified via `FromClient::Reconnected` so that
/// the next client starts with a live sender; reconnect only fires AFTER the
/// prior session ends.
///
/// Every await of the loop is wrapped in a `shutdown.changed()` select and
/// the session task is aborted on the way out, so a parked read/write is
/// cancelled the instant the handle is dropped and the dedicated thread's
/// `block_on` can always return (no hang-on-exit). The head read
/// additionally has its own timeout so one slow client cannot
/// head-of-line-block the (single) accept loop even absent shutdown.
async fn serve(
    listener: TcpListener,
    addr: SocketAddr,
    uuid: String,
    from_client: Sender<FromClient>,
    to_client_rx: UnboundedReceiver<String>,
    mut shutdown: watch::Receiver<bool>,
) {
    // The outbound receiver is here while no debugger is attached and inside
    // the session task while one is, so `None` means "a client is attached".
    let mut to_client_rx = Some(to_client_rx);
    // The attached client's bridge. A task of its own, so the accept loop
    // keeps answering discovery GETs (and refusing a second client) for as
    // long as the session lasts.
    let mut session: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let stream = tokio::select! {
            _ = shutdown.changed() => break,
            _ = session_done(&mut session) => {
                session = None;
                eprintln!("inspector: debugger disconnected -- waiting for reconnect");
                // Create a fresh channel pair for the next session.  Send the
                // new sender to the engine so it can swap out the dead one;
                // keep the new receiver here for the next WebSocket bridge.
                let (new_tx, new_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                to_client_rx = Some(new_rx);
                // If the engine has already exited (sender disconnected), stop.
                if from_client
                    .send(FromClient::Reconnected(OutboundSender(new_tx)))
                    .is_err()
                {
                    break;
                }
                continue;
            },
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => continue,
            },
        };

        // Read the head and answer it, abandoning the connection the instant
        // shutdown fires. Bounded by HEAD_TIMEOUT, so a session that ends
        // meanwhile is noticed on the next turn of the loop.
        let upgraded = tokio::select! {
            _ = shutdown.changed() => break,
            upgraded = handle_connection(stream, addr, &uuid, &mut to_client_rx) => upgraded,
        };
        if let Some((stream, rx)) = upgraded {
            let from_client = from_client.clone();
            session = Some(tokio::spawn(async move {
                let ws = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
                let _ = from_client.send(FromClient::Connected);
                bridge(ws, &from_client, rx).await;
                let _ = from_client.send(FromClient::Disconnected);
            }));
        }
    }
    // Shutdown (or the engine is gone): end the session with the loop. The
    // runtime drops its tasks when `block_on` returns; the abort makes it
    // explicit that a parked bridge cannot outlive this function.
    if let Some(session) = session {
        session.abort();
    }
}

/// Resolves when the attached client's session task has ended; never while
/// no client is attached.
async fn session_done(session: &mut Option<tokio::task::JoinHandle<()>>) {
    match session {
        Some(handle) => {
            let _ = handle.await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Handle one accepted connection: a discovery GET (answered here), or a
/// WebSocket upgrade. An accepted upgrade returns the stream, its 101
/// already written, with the outbound receiver the session's bridge drains;
/// everything else is answered and closed, and returns `None`.
///
/// Two checks come before either, both node's (inspector_socket.cc,
/// inspector_socket_server.cc): the `Host` header must name an IP address or
/// `localhost`, and an upgrade's path must be this target's `/<uuid>`. The
/// transport serves a debugging session, so it answers only a client that
/// addressed it directly and, for the session itself, one that was given the
/// target id.
async fn handle_connection(
    mut stream: TcpStream,
    addr: SocketAddr,
    uuid: &str,
    to_client_rx: &mut Option<UnboundedReceiver<String>>,
) -> Option<(TcpStream, UnboundedReceiver<String>)> {
    const BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n";
    let head = read_http_head(&mut stream).await?;

    let host = header_value(&head, "host").unwrap_or_default();
    if !is_allowed_host(&host) {
        let _ = stream.write_all(BAD_REQUEST).await;
        return None;
    }

    if !is_ws_upgrade(&head) {
        // Discovery GET (or junk): answer and close.
        let body = discovery_response(&head, addr, uuid);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=UTF-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return None;
    }

    if !is_target_path(request_path(&head), uuid) {
        let _ = stream.write_all(BAD_REQUEST).await;
        return None;
    }
    let Some(key) = header_value(&head, "sec-websocket-key") else {
        let _ = stream.write_all(BAD_REQUEST).await;
        return None;
    };
    let Some(rx) = to_client_rx.take() else {
        // Already have our one client; refuse politely and keep serving.
        let _ = stream
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
            .await;
        return None;
    };

    let accept_key = ws_accept_key(&key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept_key}\r\n\r\n"
    );
    if stream.write_all(response.as_bytes()).await.is_err() {
        *to_client_rx = Some(rx); // hand the slot back; the upgrade failed
        return None;
    }
    Some((stream, rx))
}

/// Whether a request's `Host` header is one this transport answers: an IP
/// address literal or `localhost`, with or without a port, or no host at all
/// -- node's rule (`IsAllowedHost`). A host NAME other than `localhost` is
/// refused: the listener is reached by address, so such a name says the
/// client resolved some other name to get here.
fn is_allowed_host(host_with_port: &str) -> bool {
    let host = host_with_port.trim();
    // Drop a port: the last ':' unless it sits inside an IPv6 literal's
    // brackets.
    let host = match (host.rfind(':'), host.rfind(']')) {
        (Some(colon), None) => &host[..colon],
        (Some(colon), Some(bracket)) if colon > bracket => &host[..colon],
        _ => host,
    };
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // An address has to be one a client can actually have dialled: node
    // refuses the unspecified ones (`0.0.0.0`, `[::]`) along with names.
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        let parsed = v6.parse::<std::net::Ipv6Addr>();
        return parsed.is_ok_and(|ip| !ip.is_unspecified());
    }
    let parsed = host.parse::<std::net::Ipv4Addr>();
    parsed.is_ok_and(|ip| !ip.is_unspecified())
}

/// Whether an upgrade's request path names this target: exactly `/<uuid>`,
/// the path of the `webSocketDebuggerUrl` discovery hands out.
fn is_target_path(path: &str, uuid: &str) -> bool {
    path.strip_prefix('/') == Some(uuid)
}

/// Pump frames both ways until the client disconnects. Shutdown is handled by
/// the accept loop (which aborts the session task), so no shutdown branch
/// here.
async fn bridge(
    ws: WebSocketStream<TcpStream>,
    from_client: &Sender<FromClient>,
    mut to_client_rx: UnboundedReceiver<String>,
) {
    let (mut sink, mut source) = ws.split();
    loop {
        tokio::select! {
            outbound = to_client_rx.recv() => match outbound {
                Some(text) => {
                    if sink.send(Message::Text(text)).await.is_err() {
                        return;
                    }
                }
                None => return, // engine dropped the sender (run finished)
            },
            inbound = source.next() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    if from_client.send(FromClient::Message(text)).is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    // tungstenite queued the Close reply when it read the
                    // client's frame; it is written on a flush. Bounded, so
                    // a client that stopped reading cannot hold the session.
                    let _ = tokio::time::timeout(Duration::from_secs(1), sink.flush()).await;
                    return;
                }
                None => return,
                Some(Ok(_)) => {} // ping/pong/binary: tungstenite handles control frames
                Some(Err(_)) => return,
            },
        }
    }
}

/// Read an HTTP request head (everything up to the blank line). Bounded in
/// BOTH bytes (16KB) and time (`HEAD_TIMEOUT`): the byte cap stops a hostile
/// peer buffering forever; the time cap stops a slow/silent peer parking the
/// single accept loop and, with the caller's shutdown-select, guarantees the
/// transport thread can never wedge process exit.
async fn read_http_head(stream: &mut TcpStream) -> Option<String> {
    const MAX_HEAD: usize = 16 * 1024;
    tokio::time::timeout(HEAD_TIMEOUT, async {
        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return Some(String::from_utf8_lossy(&buf).into_owned());
            }
            if buf.len() > MAX_HEAD {
                return None;
            }
        }
    })
    .await
    .ok()
    .flatten()
}

fn is_ws_upgrade(head: &str) -> bool {
    header_value(head, "upgrade")
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

/// Case-insensitive header lookup over a raw request head.
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines()
        .skip(1) // request line
        .filter_map(|line| line.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
}

/// The request target (path) from the request line, defaulting to "/".
fn request_path(head: &str) -> &str {
    head.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
}

/// `Sec-WebSocket-Accept` = base64(sha1(key + GUID)).
fn ws_accept_key(client_key: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    base64_encode(&hasher.finalize())
}

/// Standard base64 with padding (enough for the 20-byte WS accept hash; not
/// a general-purpose codec, but correct for any input).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The JSON body for a discovery GET. `/json/version` returns the protocol
/// banner; `/json` and `/json/list` return the single target.
fn discovery_response(head: &str, addr: SocketAddr, uuid: &str) -> String {
    let path = request_path(head);
    if path.starts_with("/json/version") {
        return format!(
            "{{\"Browser\":\"oam/{}\",\"Protocol-Version\":\"1.3\"}}",
            env!("CARGO_PKG_VERSION")
        );
    }
    // /json and /json/list: one target descriptor.
    let ws = format!("{addr}/{uuid}");
    format!(
        "[{{\
           \"description\":\"oam instance\",\
           \"id\":\"{uuid}\",\
           \"title\":\"oam\",\
           \"type\":\"node\",\
           \"webSocketDebuggerUrl\":\"ws://{ws}\",\
           \"devtoolsFrontendUrl\":\"devtools://devtools/bundled/js_app.html?experiments=true&v8only=true&ws={ws}\"\
         }}]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn ws_accept_matches_rfc6455_example() {
        // From RFC 6455 section 1.3.
        assert_eq!(
            ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let head = "GET /x HTTP/1.1\r\nUpgrade: websocket\r\nSec-WebSocket-Key: abc\r\n\r\n";
        assert!(is_ws_upgrade(head));
        assert_eq!(
            header_value(head, "sec-websocket-key").as_deref(),
            Some("abc")
        );
        assert_eq!(request_path(head), "/x");
    }

    #[test]
    fn only_an_address_or_localhost_is_an_allowed_host() {
        for host in [
            "",
            "localhost",
            "LOCALHOST:9229",
            "127.0.0.1",
            "127.0.0.1:9229",
            "[::1]",
            "[::1]:9229",
        ] {
            assert!(is_allowed_host(host), "{host:?} must be allowed");
        }
        for host in [
            "example.test",
            "example.test:9229",
            "localhost.example.test",
            "127.0.0.1.example.test:9229",
            "[example.test]:9229",
            "1.2.3",
            "0.0.0.0",
            "0.0.0.0:9229",
            "[::]:9229",
        ] {
            assert!(!is_allowed_host(host), "{host:?} must be refused");
        }
    }

    #[test]
    fn an_upgrade_path_must_be_the_target_id() {
        assert!(is_target_path("/abc-123", "abc-123"));
        for path in ["/", "/x", "/abc-123/", "/abc-1234", "abc-123", "//abc-123"] {
            assert!(!is_target_path(path, "abc-123"), "{path:?}");
        }
    }

    /// One request against a running transport: the stream (kept open by the
    /// caller for a session) and the response head.
    fn request(addr: SocketAddr, head: &str) -> (std::net::TcpStream, String) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        (stream, String::from_utf8_lossy(&buf).into_owned())
    }

    fn upgrade(addr: SocketAddr, path: &str, host: &str) -> (std::net::TcpStream, String) {
        request(
            addr,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\r\n"
            ),
        )
    }

    /// The transport end to end: a session starts only at the target's path
    /// from an allowed host, a second client is refused with 503 while it
    /// lasts, discovery is answered throughout, and the next client attaches
    /// once the first has gone.
    #[test]
    fn a_session_needs_the_target_path_and_leaves_the_loop_serving() {
        let handle = start("127.0.0.1:0".parse().unwrap(), "abc-123".to_string()).unwrap();
        let addr = handle.addr;
        let host = addr.to_string();
        let wait = Duration::from_secs(10);

        let (_, response) = upgrade(addr, "/somewhere-else", &host);
        assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
        let (_, response) = upgrade(addr, "/abc-123", "example.test:9229");
        assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
        let (_, response) = request(addr, "GET /json HTTP/1.1\r\nHost: example.test\r\n\r\n");
        assert!(response.starts_with("HTTP/1.1 400 "), "{response}");

        let (first, response) = upgrade(addr, "/abc-123", &host);
        assert!(response.starts_with("HTTP/1.1 101 "), "{response}");
        assert!(matches!(
            handle.from_client.recv_timeout(wait),
            Ok(FromClient::Connected)
        ));

        // While the session lasts: no second client, discovery still served.
        let (_, response) = upgrade(addr, "/abc-123", &host);
        assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
        let (_, response) = request(
            addr,
            &format!("GET /json/version HTTP/1.1\r\nHost: {host}\r\n\r\n"),
        );
        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");

        // The first client leaves; the transport reports it and re-arms.
        drop(first);
        assert!(matches!(
            handle.from_client.recv_timeout(wait),
            Ok(FromClient::Disconnected)
        ));
        assert!(matches!(
            handle.from_client.recv_timeout(wait),
            Ok(FromClient::Reconnected(_))
        ));
        let (_second, response) = upgrade(addr, "/abc-123", &host);
        assert!(response.starts_with("HTTP/1.1 101 "), "{response}");
        assert!(matches!(
            handle.from_client.recv_timeout(wait),
            Ok(FromClient::Connected)
        ));
    }
}
