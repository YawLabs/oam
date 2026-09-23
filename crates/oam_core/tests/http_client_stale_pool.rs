//! A request sent onto a pooled connection whose server has just closed it
//! must settle -- with a response or an error -- and never hang (L-3).
//!
//! hyper 1.10.1 could lose that race outright. hyper-util checks the idle
//! connection out of the pool and `try_send`s the request into the
//! connection's dispatch channel while, on the other worker, the connection's
//! h1 dispatcher reads the server's FIN, finishes and is dropped. The send
//! reserved its slot in the channel just before the dispatcher's receiver
//! closed it, and published the request just after tokio's close-time drain
//! looked, so the request stayed in the channel. Only dropping the channel's
//! last sender would have answered it, and hyper-util holds that sender while
//! it awaits the answer. No socket, no timer: `client.request` never returned
//! and the fetch never settled. The fix is the close-and-drain in
//! `vendor/hyper-1.10.1/src/client/dispatch.rs` (`Receiver::drop`).
//!
//! Two tests, because the window is a few dozen nanoseconds wide:
//!
//! * `a_send_racing_the_dispatcher_teardown_is_answered` is the race itself,
//!   on an in-memory connection, with the two threads released together.
//!   Stock hyper 1.10.1 strands about one send in a hundred here, so it
//!   fails every run.
//! * `a_request_racing_a_closing_pooled_connection_settles` is the shape a
//!   user meets, through oam's own transport and a real server. It hits the
//!   window far less often (stock hyper failed it in about one run in five
//!   on the Windows arm64 dev box) and stays as the end-to-end guard.

mod common;

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::http1;
use hyper::rt::{Read, ReadBufCursor, Write};
use oam_core::http_client::transport::empty_body;
use oam_core::http_client::{HttpTransport, ProxySource, Route};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ------------------------------------------------------------ the race

/// Rounds of the in-memory race. Stock hyper strands a couple of hundred of
/// these; the whole test takes well under a second.
const ROUNDS: usize = 20_000;

/// An in-memory connection to a server that answers each request
/// `200, content-length: 0` and closes the connection once `eof` is set.
struct ScriptedIo {
    eof: Arc<AtomicBool>,
    unread: Vec<u8>,
}

impl Read for ScriptedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.unread.is_empty() {
            let bytes = std::mem::take(&mut self.unread);
            buf.put_slice(&bytes);
            Poll::Ready(Ok(()))
        } else if self.eof.load(Ordering::SeqCst) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl Write for ScriptedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.ends_with(b"\r\n\r\n") {
            self.unread
                .extend_from_slice(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

type Connection = http1::Connection<ScriptedIo, Empty<Bytes>>;

fn poll_once<F: Future + ?Sized>(fut: Pin<&mut F>) -> Poll<F::Output> {
    fut.poll(&mut Context::from_waker(Waker::noop()))
}

/// Two threads leave `wait` within nanoseconds of each other. A
/// `std::sync::Barrier` wakes its waiter through the OS, microseconds after
/// the last arrival -- far wider than the window.
#[derive(Default)]
struct SpinBarrier {
    arrived: AtomicUsize,
    generation: AtomicUsize,
}

impl SpinBarrier {
    fn wait(&self) {
        let generation = self.generation.load(Ordering::SeqCst);
        if self.arrived.fetch_add(1, Ordering::SeqCst) == 1 {
            self.arrived.store(0, Ordering::SeqCst);
            self.generation.fetch_add(1, Ordering::SeqCst);
        } else {
            let mut spins = 0u32;
            while self.generation.load(Ordering::SeqCst) == generation {
                spins += 1;
                if spins.is_multiple_of(1024) {
                    std::thread::yield_now();
                } else {
                    std::hint::spin_loop();
                }
            }
        }
    }
}

/// The production interleaving, one step at a time: a connection serves a
/// request, goes idle (its sender now reports ready, as a pooled connection
/// does at checkout), then reads the server's FIN and finishes. One thread
/// then drops it -- as the runtime drops a finished connection task -- while
/// another sends the next request on it. Whatever the order, the request
/// must be answered: accepted, or handed back as never sent.
#[test]
fn a_send_racing_the_dispatcher_teardown_is_answered() {
    let barrier = Arc::new(SpinBarrier::default());
    let (to_dropper, connections) = mpsc::channel::<Connection>();
    let (dropped, dropped_rx) = mpsc::channel::<()>();
    let dropper = {
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            while let Ok(connection) = connections.recv() {
                barrier.wait();
                drop(connection);
                dropped.send(()).unwrap();
            }
        })
    };

    let started = Instant::now();
    let mut stranded = 0;
    for _ in 0..ROUNDS {
        let eof = Arc::new(AtomicBool::new(false));
        let io = ScriptedIo {
            eof: eof.clone(),
            unread: Vec::new(),
        };
        let Poll::Ready(Ok((mut sender, mut connection))) =
            poll_once(std::pin::pin!(http1::handshake::<_, Empty<Bytes>>(io)))
        else {
            panic!("an in-memory handshake completes at once");
        };

        // One exchange, so the connection is idle rather than never used.
        let mut first = Box::pin(sender.send_request(http::Request::new(Empty::new())));
        let mut polls = 0;
        let response = loop {
            assert!(poll_once(Pin::new(&mut connection)).is_pending());
            if let Poll::Ready(response) = poll_once(first.as_mut()) {
                break response.expect("the first request is answered");
            }
            polls += 1;
            assert!(polls < 8, "the first exchange did not complete");
        };
        drop(response);
        assert!(poll_once(Pin::new(&mut connection)).is_pending());

        // The server's FIN on the idle connection: the dispatcher finishes.
        eof.store(true, Ordering::SeqCst);
        assert!(matches!(
            poll_once(Pin::new(&mut connection)),
            Poll::Ready(Ok(()))
        ));
        assert!(
            sender.is_ready(),
            "a finished but undropped connection still looks idle to the pool"
        );

        to_dropper.send(connection).unwrap();
        barrier.wait();
        let mut second = Box::pin(sender.try_send_request(http::Request::new(Empty::new())));
        dropped_rx.recv().unwrap();
        if poll_once(second.as_mut()).is_pending() {
            stranded += 1;
        }
    }
    drop(to_dropper);
    dropper.join().unwrap();

    assert_eq!(
        stranded,
        0,
        "{stranded} of {ROUNDS} requests sent as their connection was dropped were never \
         answered ({:?})",
        started.elapsed()
    );
}

// ------------------------------------------------------------ end to end

/// Concurrent request chains. Each keeps reusing the connection its last
/// response arrived on, so every request after a chain's first is a draw at
/// the race.
const CHAINS: usize = 2;
const REQUESTS_PER_CHAIN: usize = 5_000;
/// A loopback request takes well under a millisecond; a stranded one never
/// finishes. The deadline only has to separate the two on a loaded box.
const DEADLINE: Duration = Duration::from_secs(5);

/// A server that answers every request `200, content-length: 0` and then
/// shuts its write side, the way a server with a short keep-alive timeout
/// ends a connection. It runs on a runtime of its own, as a real server
/// would, so the client's two workers are the client's alone.
fn answer_then_fin() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    std::thread::spawn(move || {
        runtime.block_on(async move {
            let listener = TcpListener::from_std(listener).unwrap();
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(answer_once_then_fin(stream));
            }
        })
    });
    port
}

async fn answer_once_then_fin(mut stream: tokio::net::TcpStream) {
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
    if stream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
        .await
        .is_err()
    {
        return;
    }
    let _ = stream.shutdown().await;
    // Hold the read side until the client closes, so a request written onto
    // this connection meets EOF rather than a reset.
    while let Ok(n) = stream.read(&mut chunk).await {
        if n == 0 {
            break;
        }
    }
}

/// Back-to-back GETs, each under [`DEADLINE`]; true when one missed it.
async fn chain_hung(transport: HttpTransport, route: Arc<Route>, uri: String) -> bool {
    for _ in 0..REQUESTS_PER_CHAIN {
        let request = http::Request::builder()
            .uri(uri.as_str())
            .body(empty_body())
            .unwrap();
        let settled = tokio::time::timeout(DEADLINE, async {
            // An error is a settled request: one written onto the closing
            // connection meets its EOF. Only a hang is the bug.
            if let Ok(response) = transport.send(&route, request).await {
                let _ = response.into_body().collect().await;
            }
        })
        .await;
        if settled.is_err() {
            return true;
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_racing_a_closing_pooled_connection_settles() {
    let port = answer_then_fin();
    let transport = transport(ProxySource::None);
    let route = Arc::new(transport.route(
        false,
        Duration::from_millis(250),
        oam_core::http_client::TlsRange::Both,
    ));
    let uri = format!("http://127.0.0.1:{port}/");
    let started = Instant::now();

    let chains: Vec<_> = (0..CHAINS)
        .map(|_| tokio::spawn(chain_hung(transport.clone(), route.clone(), uri.clone())))
        .collect();
    let mut hung = 0;
    for chain in chains {
        hung += usize::from(chain.await.unwrap());
    }

    assert_eq!(
        hung,
        0,
        "{hung} of {CHAINS} request chains stranded a request on a closing pooled \
         connection ({} requests, {:?})",
        CHAINS * REQUESTS_PER_CHAIN,
        started.elapsed()
    );
}
