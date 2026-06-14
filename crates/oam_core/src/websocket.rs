//! WebSocket client: the browser-standard WebSocket global.
//!
//! Architecture: `connect_async` establishes the connection, then a bridge
//! task runs on the tokio runtime pumping frames between two mpsc channels
//! and the underlying stream. The JS side sends/receives through the
//! channels via ops; the bridge task owns all async I/O.
//!
//! Channel-based rather than split-stream: avoids storing complex split
//! types in the registry and lets send+recv proceed independently without
//! contention on the same Mutex entry.

use crate::OpOutcome;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use tokio_tungstenite::connect_async;
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

pub async fn ws_connect(
    registry: WsRegistry,
    ids: std::sync::Arc<std::sync::atomic::AtomicU64>,
    url: String,
    protocols: Vec<String>,
) -> OpOutcome {
    let result = if protocols.is_empty() {
        connect_async(&url).await
    } else {
        use tokio_tungstenite::tungstenite::http;
        let mut builder = http::Request::builder().method("GET").uri(&url);
        builder = builder.header("Sec-WebSocket-Protocol", protocols.join(", "));
        match builder.body(()) {
            Ok(req) => connect_async(req).await,
            Err(e) => return OpOutcome::Failed(format!("WebSocket: invalid request: {e}")),
        }
    };
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
    registry.lock().expect("ws registry lock").insert(
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
        let guard = registry.lock().expect("ws registry lock");
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

    if let Some(conn) = registry.lock().expect("ws registry lock").get_mut(&handle) {
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
        let guard = registry.lock().expect("ws registry lock");
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
    registry.lock().expect("ws registry lock").remove(&handle);
}
