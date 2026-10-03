//! WebSocket client: the browser-standard WebSocket global.
//!
//! Architecture: `ws_connect` dials with the connector net, tls, fetch and
//! http share (`net_connect`) and runs the handshake over that stream -- or
//! over the connection an undici dispatcher's `connect` function handed JS
//! (a MockAgent's in-memory socket, an Agent's own connector), piped in as
//! a `byte_pipe` the way a connector-mode fetch gets its connection -- then
//! a bridge task runs on the tokio runtime pumping frames between two mpsc
//! channels and the underlying stream. The JS side sends/receives through
//! the channels via ops; the bridge task owns all async I/O.
//!
//! Channel-based rather than split-stream: avoids storing complex split
//! types in the registry and lets send+recv proceed independently without
//! contention on the same Mutex entry.

use crate::OpOutcome;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use tokio_tungstenite::tungstenite::Message;

pub enum WsFrame {
    Text(String),
    Binary(Vec<u8>),
    Close { code: u16, reason: String },
}

pub struct WsConnection {
    outbound: tokio::sync::mpsc::UnboundedSender<Message>,
    inbound: Option<tokio::sync::mpsc::UnboundedReceiver<WsFrame>>,
}

pub type WsRegistry = std::sync::Arc<std::sync::Mutex<HashMap<u64, WsConnection>>>;

async fn bridge(
    ws: impl futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
    + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
    + Unpin,
    mut outbound_rx: tokio::sync::mpsc::UnboundedReceiver<Message>,
    inbound_tx: tokio::sync::mpsc::UnboundedSender<WsFrame>,
) {
    let (mut sink, mut source) = ws.split();
    loop {
        tokio::select! {
            msg = outbound_rx.recv() => match msg {
                Some(msg) => {
                    if sink.send(msg).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            frame = source.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    if inbound_tx.send(WsFrame::Text(text)).is_err() { break; }
                }
                Some(Ok(Message::Binary(data))) => {
                    if inbound_tx.send(WsFrame::Binary(data)).is_err() { break; }
                }
                Some(Ok(Message::Close(close))) => {
                    let (code, reason) = close
                        .map(|c| (c.code.into(), c.reason.to_string()))
                        .unwrap_or((1005, String::new()));
                    let _ = inbound_tx.send(WsFrame::Close { code, reason });
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
}

/// The host and port a WebSocket URL is dialled at: the URL's own, the
/// scheme's default port (80 for `ws:`, 443 for `wss:`) without one, and an
/// IPv6 literal without its URI brackets. `None` for a URL with no host or
/// another scheme.
fn dial_target(uri: &tokio_tungstenite::tungstenite::http::Uri) -> Option<(String, u16)> {
    let host = uri.host()?;
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    let port = match (uri.port_u16(), uri.scheme_str()) {
        (Some(port), _) => port,
        (None, Some("ws")) => 80,
        (None, Some("wss")) => 443,
        (None, _) => return None,
    };
    Some((host.to_string(), port))
}

/// One `wsConnect`: where the handshake goes, and what it carries.
pub struct WsConnect {
    /// The URL the handshake is for: its host (and port) is the connection's
    /// and the `host` the request carries. JS sends the socket's own URL, or
    /// the origin a Client or Pool dispatcher pins it to (with the URL's
    /// path).
    pub url: String,
    pub protocols: Vec<String>,
    /// `WebSocketInit.headers`, added to the handshake request.
    pub headers: Vec<(String, String)>,
    /// The connection a dispatcher's `connect` function made, already
    /// through any TLS it does: the handshake runs over it as it is, and
    /// nothing is dialled.
    pub supplied: Option<tokio::io::DuplexStream>,
}

pub async fn ws_connect(
    registry: WsRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    connect: WsConnect,
) -> OpOutcome {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http;
    let WsConnect {
        url,
        protocols,
        headers,
        supplied,
    } = connect;
    let mut request = match url.as_str().into_client_request() {
        Ok(request) => request,
        Err(e) => return OpOutcome::Failed(format!("WebSocket: invalid request: {e}")),
    };
    if !protocols.is_empty() {
        match http::HeaderValue::from_str(&protocols.join(", ")) {
            Ok(value) => {
                request
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", value);
            }
            Err(e) => return OpOutcome::Failed(format!("WebSocket: invalid request: {e}")),
        }
    }
    for (name, value) in &headers {
        let name = match http::HeaderName::from_bytes(name.as_bytes()) {
            Ok(name) => name,
            Err(e) => return OpOutcome::Failed(format!("WebSocket: invalid request: {e}")),
        };
        match http::HeaderValue::from_str(value) {
            Ok(value) => {
                request.headers_mut().append(name, value);
            }
            Err(e) => return OpOutcome::Failed(format!("WebSocket: invalid request: {e}")),
        }
    }
    if let Some(io) = supplied {
        let result = tokio_tungstenite::client_async_with_config(request, io, None).await;
        return established(registry, ids, result);
    }
    // The dial is oam's own (#161), the one net, tls, fetch and http share:
    // node's per-address connect algorithm, and on Windows no SYN retransmit
    // to a loopback peer -- `connect_async` opened a plain tokio stream, and
    // a refused loopback port took ~2 s per resolved address there. The
    // handshake (and TLS for `wss:`) then runs over that stream as before.
    let Some((host, port)) = dial_target(request.uri()) else {
        return OpOutcome::Failed(format!("WebSocket: no host to connect to in {url}"));
    };
    let options = crate::net_connect::ConnectOptions::default();
    let stream = match crate::net_connect::connect(&host, port, &options).await {
        Ok(connected) => connected.stream,
        Err(e) => return OpOutcome::Failed(format!("WebSocket connection failed: {e}")),
    };
    let result = tokio_tungstenite::client_async_tls_with_config(request, stream, None, None).await;
    established(registry, ids, result)
}

/// A handshake's outcome: the socket registered and its bridge running, or
/// the failure.
fn established<S>(
    registry: WsRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    result: Result<
        (
            tokio_tungstenite::WebSocketStream<S>,
            tokio_tungstenite::tungstenite::handshake::client::Response,
        ),
        tokio_tungstenite::tungstenite::Error,
    >,
) -> OpOutcome
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (ws_stream, response) = match result {
        Ok(pair) => pair,
        Err(e) => return OpOutcome::Failed(format!("WebSocket connection failed: {e}")),
    };
    let protocol = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let extensions = response
        .headers()
        .get("sec-websocket-extensions")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let (inbound_tx, inbound_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(bridge(ws_stream, outbound_rx, inbound_tx));

    let handle = ids.fetch_add(1, Ordering::Relaxed);
    registry.lock().unwrap_or_else(|e| e.into_inner()).insert(
        handle,
        WsConnection {
            outbound: outbound_tx,
            inbound: Some(inbound_rx),
        },
    );

    OpOutcome::Json(
        serde_json::json!({
            "handle": handle,
            "protocol": protocol,
            "extensions": extensions,
        })
        .to_string(),
    )
}

pub fn ws_send_sync(registry: &WsRegistry, handle: u64, message: Message) -> Result<(), String> {
    let sender = {
        let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get(&handle) {
            Some(conn) => conn.outbound.clone(),
            None => return Err(format!("WebSocket: handle {handle} not found")),
        }
    };
    if sender.send(message).is_err() {
        return Err("WebSocket: connection closed".to_string());
    }
    Ok(())
}

pub async fn ws_recv(registry: WsRegistry, handle: u64) -> OpOutcome {
    let rx = registry
        .lock()
        .expect("ws registry lock")
        .get_mut(&handle)
        .and_then(|conn| conn.inbound.take());
    let Some(mut rx) = rx else {
        return OpOutcome::Failed(format!("WebSocket: handle {handle} recv not available"));
    };

    let frame = rx.recv().await;

    if let Some(conn) = registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&handle)
    {
        conn.inbound = Some(rx);
    }

    match frame {
        Some(WsFrame::Text(text)) => {
            OpOutcome::Json(serde_json::json!({"type":"text","data":text}).to_string())
        }
        Some(WsFrame::Binary(data)) => OpOutcome::Bytes(data),
        Some(WsFrame::Close { code, reason }) => OpOutcome::Json(
            serde_json::json!({"type":"close","code":code,"reason":reason}).to_string(),
        ),
        None => OpOutcome::Done,
    }
}

pub async fn ws_close(registry: WsRegistry, handle: u64, code: u16, reason: String) -> OpOutcome {
    let sender = {
        let guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        match guard.get(&handle) {
            Some(conn) => conn.outbound.clone(),
            None => return OpOutcome::Done,
        }
    };
    let close = Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: code.into(),
        reason: reason.into(),
    }));
    let _ = sender.send(close);
    OpOutcome::Done
}

pub fn ws_drop(registry: &WsRegistry, handle: u64) {
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::http::Uri;

    #[test]
    fn dial_target_takes_the_urls_host_and_the_schemes_default_port() {
        let target = |url: &str| dial_target(&url.parse::<Uri>().unwrap());
        assert_eq!(
            target("ws://example.test/chat"),
            Some(("example.test".into(), 80))
        );
        assert_eq!(
            target("wss://example.test/chat"),
            Some(("example.test".into(), 443))
        );
        assert_eq!(
            target("ws://127.0.0.1:8080/"),
            Some(("127.0.0.1".into(), 8080))
        );
        // An IPv6 literal is dialled without its URI brackets: glibc's
        // getaddrinfo does not resolve `[::1]`.
        assert_eq!(target("ws://[::1]:9000/"), Some(("::1".into(), 9000)));
        assert_eq!(target("wss://[::1]/"), Some(("::1".into(), 443)));
        assert_eq!(target("http://example.test/"), None);
        assert_eq!(target("/no-host"), None);
    }

    /// A supplied connection -- what a dispatcher's `connect` function made,
    /// piped in -- carries the handshake as it is: nothing is dialled (the
    /// URL's host does not exist), no TLS is added for `wss:` (the
    /// connector's is the connection's), and the init headers go out.
    #[tokio::test]
    async fn a_supplied_connection_carries_the_handshake_as_it_is() {
        let registry: WsRegistry = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let ids = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let (near, far) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let mut seen = None;
            let callback =
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    seen = request
                        .headers()
                        .get("x-extra")
                        .map(|v| v.to_str().unwrap().to_string());
                    Ok(response)
                };
            let ws = tokio_tungstenite::accept_hdr_async(far, callback).await;
            (ws.is_ok(), seen)
        });
        let outcome = ws_connect(
            registry.clone(),
            ids,
            WsConnect {
                url: "wss://no-such-host.invalid/chat".into(),
                protocols: Vec::new(),
                headers: vec![("x-extra".into(), "yes".into())],
                supplied: Some(near),
            },
        )
        .await;
        assert!(
            matches!(outcome, OpOutcome::Json(_)),
            "the handshake must succeed"
        );
        let (accepted, seen) = server.await.unwrap();
        assert!(accepted);
        assert_eq!(seen.as_deref(), Some("yes"));
        assert_eq!(registry.lock().unwrap().len(), 1);
    }

    /// #161: a refused loopback connect fails at once. Through tokio's own
    /// connect it took ~2 s per address on Windows (the SYN is retransmitted
    /// to a loopback peer that already refused it); the shared connector
    /// turns that off. `localhost` resolves to two addresses there, so the
    /// bound covers both attempts.
    #[tokio::test]
    async fn a_refused_loopback_connect_fails_fast() {
        let registry: WsRegistry = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let ids = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        for host in ["127.0.0.1", "localhost"] {
            // A port nothing listens on: bound, read, released.
            let port = {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                listener.local_addr().unwrap().port()
            };
            let started = std::time::Instant::now();
            let outcome = ws_connect(
                registry.clone(),
                ids.clone(),
                WsConnect {
                    url: format!("ws://{host}:{port}/"),
                    protocols: Vec::new(),
                    headers: Vec::new(),
                    supplied: None,
                },
            )
            .await;
            let elapsed = started.elapsed();
            let OpOutcome::Failed(message) = outcome else {
                panic!("a connect to a closed port must fail");
            };
            assert!(
                message.starts_with("WebSocket connection failed: "),
                "{message}"
            );
            assert!(
                elapsed < std::time::Duration::from_millis(1500),
                "{host}: refused after {elapsed:?}; the loopback SYN retransmit is back"
            );
        }
        assert!(registry.lock().unwrap().is_empty());
    }
}
