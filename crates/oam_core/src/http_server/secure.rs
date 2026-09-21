//! http2.createSecureServer's connections: an accepted TLS connection
//! (node:tls did the handshake, and JS decided what it is from its ALPN)
//! served natively as one HTTP/2 session, or -- `allowHTTP1` and a client
//! that negotiated `http/1.1` or nothing -- as HTTP/1.1.
//!
//! Each connection gets its own accept queue (a server entry of its own, so
//! everything on the queue belongs to that session and JS needs no
//! connection id per request), and a `ConnWatch` that is how JS closes it:
//! `httpConnDestroy(connId, true)` is node's `session.close()` (GOAWAY, the
//! open streams finish), `false` is `session.destroy()`. The queue ends
//! when the connection does, which is the session's 'close'.
//!
//! Node's `Http2SecureServer` has no HTTP timeouts on its HTTP/2 sessions
//! (their watch never fires); an HTTP/1 connection it takes under
//! `allowHTTP1` is held to the http server's (headersTimeout 60 s,
//! requestTimeout 300 s, checked every 30 s), checked here.

use super::{
    BoxedBody, ConnAddrs, HttpState, ServerEntry, ServerEvent, Upgrades, check_connections,
    handle_request, serve_http1,
};
use crate::http_conn::{CloseReason, ConnWatch, ServerTimeouts, TimeoutSettings};
use crate::http_head::HeadPolicy;
use crate::tls::server::ServerIo;
use bytes::Bytes;
use http_body_util::BodyExt;
use std::sync::Arc;
use tokio::sync::mpsc;

/// What the session carries: `session_id` names its accept queue
/// (`httpAccept`, `httpClose` once it is drained), `conn_id` its watch
/// (`httpConnDestroy`).
pub struct SecureSession {
    pub session_id: u64,
    pub conn_id: u64,
}

/// Serve an accepted TLS connection as an HTTP/2 session (`http1` false) or
/// as HTTP/1.1 (`http1` true, node's `allowHTTP1` fallback).
pub fn http2_serve_tls(
    state: Arc<HttpState>,
    stream: tokio_rustls::server::TlsStream<ServerIo>,
    http1: bool,
    policy: HeadPolicy,
    // The http server's timeouts, for an HTTP/1 connection.
    http1_timeouts: TimeoutSettings,
) -> std::io::Result<SecureSession> {
    let tcp = stream.get_ref().0.tcp();
    let addrs = ConnAddrs {
        remote: tcp.peer_addr()?,
        local: tcp.local_addr().ok(),
    };
    let settings = if http1 {
        TimeoutSettings {
            js_driven: false,
            ..http1_timeouts
        }
    } else {
        // Nothing fires: node's http2 sessions have none of the http
        // server's timeouts. JS-driven only so that an exchange that ends
        // without its response (a stream the client reset) is reported.
        TimeoutSettings {
            headers_ms: 0,
            request_ms: 0,
            keep_alive_ms: 0,
            socket_ms: 0,
            js_driven: true,
            ..TimeoutSettings::default()
        }
    };
    let timeouts = ServerTimeouts::new(settings);
    let session_id = state.next_id();
    let (queue_tx, queue_rx) = mpsc::channel::<ServerEvent>(64);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    state
        .servers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            session_id,
            ServerEntry {
                queue: Some(queue_rx),
                shutdown: Some(shutdown_tx),
                // A session serves one connection JavaScript already accepted:
                // there is no listening socket for close() to wait on.
                listener_gone: None,
                timeouts: Some(Arc::clone(&timeouts)),
                tls: None,
            },
        );
    let watch = ConnWatch::new(state.next_id(), session_id, Arc::clone(&timeouts));
    let conn_id = watch.id;
    let registration = state.register_conn(Arc::clone(&watch));

    if http1 {
        tokio::spawn(check_connections(
            Arc::clone(&state),
            session_id,
            Arc::clone(&timeouts),
            shutdown_rx.clone(),
        ));
        tokio::spawn(async move {
            let _registration = registration;
            let service_state = Arc::clone(&state);
            let service_queue = queue_tx.clone();
            let service_watch = Arc::clone(&watch);
            let service = hyper::service::service_fn(move |req| {
                handle_request(
                    Arc::clone(&service_state),
                    service_queue.clone(),
                    req,
                    true,
                    addrs,
                    None,
                    policy,
                    Some(Arc::clone(&service_watch)),
                    Upgrades::CloseConnect,
                )
            });
            serve_http1(
                stream,
                watch,
                policy,
                service,
                queue_tx,
                false,
                addrs,
                shutdown_rx,
                None,
            )
            .await;
        });
    } else {
        tokio::spawn(serve_h2(
            state,
            stream,
            registration,
            watch,
            queue_tx,
            addrs,
            policy,
            shutdown_rx,
        ));
    }
    Ok(SecureSession {
        session_id,
        conn_id,
    })
}

/// One HTTP/2 session: hyper's server connection until it ends, gracefully
/// on `session.close()` (and when JS forgets the session), at once on
/// `session.destroy()`.
#[allow(clippy::too_many_arguments)]
async fn serve_h2(
    state: Arc<HttpState>,
    stream: tokio_rustls::server::TlsStream<ServerIo>,
    registration: super::ConnRegistration,
    watch: Arc<ConnWatch>,
    queue: mpsc::Sender<ServerEvent>,
    addrs: ConnAddrs,
    policy: HeadPolicy,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let _registration = registration;
    let service_watch = Arc::clone(&watch);
    let service = hyper::service::service_fn(move |req| {
        let exchange = handle_request(
            Arc::clone(&state),
            queue.clone(),
            req,
            true,
            addrs,
            None,
            policy,
            Some(Arc::clone(&service_watch)),
            Upgrades::Serve,
        );
        async move { exchange.await.map(without_length) }
    });
    let io = hyper_util::rt::TokioIo::new(stream);
    let conn = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection(io, service);
    let mut conn = std::pin::pin!(conn);
    let mut closing = false;
    loop {
        tokio::select! {
            _ = conn.as_mut() => break,
            reason = watch.closed(if closing { CloseReason::Destroy } else { CloseReason::End }) => {
                if reason == CloseReason::End {
                    closing = true;
                    conn.as_mut().graceful_shutdown();
                } else {
                    // session.destroy(): the connection goes now.
                    break;
                }
            }
            _ = shutdown.changed(), if !closing => {
                closing = true;
                conn.as_mut().graceful_shutdown();
            }
        }
    }
}

/// A response body that does not announce its length. hyper's HTTP/2
/// server adds `content-length` for a body of known size; node's
/// `stream.respond()` sends only the headers it was given.
struct NoLength(BoxedBody);

impl hyper::body::Body for NoLength {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        std::pin::Pin::new(&mut self.0).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }
}

fn without_length(response: hyper::Response<BoxedBody>) -> hyper::Response<BoxedBody> {
    response.map(|body| NoLength(body).boxed())
}
